//! Pipe mode, for the chat panel of VS Code-family editors. With `claudeCode.claudeProcessWrapper`
//! pointing at hibernaut, the extension's Agent SDK runs `hibernaut <claude> --input-format stream-json
//! ...` and talks JSON lines over stdin and stdout. hibernaut relays every line unchanged. Once the
//! session has been idle long enough it closes claude's input, the way the SDK ends a session,
//! so claude shuts down and frees its memory. The next line from the SDK wakes it: claude starts
//! again with `--resume`, and the setup the SDK sent the first time (initialize, then model,
//! permission mode and the like) is replayed into it. The SDK never sees the swap.

use crate::{
    args, config,
    control::{self, Action},
    hook, log, now_ms, proc, session_cwd, status, term, Files, REARM_AFTER_MS,
};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    env,
    ffi::OsString,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    thread,
    time::{Duration, Instant},
};

/// Control requests that change session state; the latest of each is replayed after a wake.
const REPLAY: &[&str] = &[
    "set_model", "set_permission_mode", "set_max_thinking_tokens",
    "set_mcp_permission_mode_override", "select_chrome_browser", "apply_flag_settings", "mcp_toggle",
];
/// Request ids hibernaut uses for replays; their answers are not passed on.
const OWN_ID: &str = "hibernaut-";
/// A session past the memory limit naps after this much quiet instead of the full idle time.
const RECYCLE_QUIET_MS: u64 = 60_000;

enum Ev {
    /// A line from the SDK, or None at the end of its input.
    In(Option<Vec<u8>>),
    /// A line from claude number `.0`, or None when it closed its output.
    Out(u64, Option<Vec<u8>>),
}

struct Live {
    child: Child,
    stdin: Option<ChildStdin>,
    gen: u64,
}

struct Pipe {
    claude: PathBuf,
    toks: Vec<args::Tok>,
    files: Files,
    cwd: PathBuf,
    tx: Sender<Ev>,
    gen: u64,
    init: Option<Value>,
    replay: Vec<Value>,
    /// When the SDK last set the permission mode. Claude also changes it on its own (a tool
    /// allowed with "allow all edits", say); the hooks report that, and the newer one wins.
    mode_at: u64,
    /// Requests waiting for an answer: from the SDK to claude, and from claude to the SDK.
    pending_in: HashSet<String>,
    pending_out: HashSet<String>,
    /// From a prompt (or claude starting a turn on its own) until the turn's result.
    busy: bool,
    /// The SDK closed its input and claude is finishing.
    closing: bool,
    last_activity: u64,
}

pub fn run(claude: PathBuf, toks: Vec<args::Tok>, files: Files) -> io::Result<i32> {
    let (tx, rx) = mpsc::channel();
    pump(io::stdin(), tx.clone(), Ev::In);
    let cwd = env::current_dir().unwrap_or_default();
    let mut status = status::Status::new(files.status.clone());
    let mut p = Pipe {
        claude,
        toks,
        files,
        cwd: cwd.clone(),
        tx,
        gen: 0,
        init: None,
        replay: Vec::new(),
        mode_at: 0,
        pending_in: HashSet::new(),
        pending_out: HashSet::new(),
        busy: false,
        closing: false,
        last_activity: now_ms(),
    };
    let first = [args::flatten(&p.toks), p.files.settings_arg()?].concat();
    let mut live = Some(p.spawn(&first, &cwd)?);
    let mut st = hook::State::default();
    let (mut last_check, mut last_measure, mut since) = (0, 0, now_ms());
    let mut awake_because = None;
    // Memory of claude's process tree; what the last nap freed; whether a nap for memory may
    // happen, and when a sample may allow one again after one (see REARM_AFTER_MS).
    let (mut memory, mut freed, mut armed, mut rearm_at) = (0u64, 0u64, true, 0u64);
    let mut last_action = Value::Null;
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ev::In(Some(line))) => {
                let msg = parse(&line);
                let kind = msg["type"].as_str().unwrap_or_default();
                if live.is_none() && matches!(kind, "keep_alive" | "control_cancel_request") {
                    continue;
                }
                p.note_in(&msg);
                if live.is_none() {
                    live = Some(p.wake(&st)?);
                    since = now_ms();
                }
                send(live.as_mut().unwrap(), &line);
            }
            Ok(Ev::In(None)) => match live.as_mut() {
                // The SDK is done with the session: claude sees the end of input and finishes.
                Some(l) => {
                    l.stdin = None;
                    p.closing = true;
                }
                None => return Ok(0),
            },
            Ok(Ev::Out(gen, line)) if live.as_ref().is_some_and(|l| l.gen == gen) => match line {
                Some(line) => {
                    if p.note_out(&parse(&line)) {
                        write_out(&line);
                    }
                }
                None => return Ok(exit_code(&mut live.take().unwrap().child)),
            },
            Ok(Ev::Out(..)) => {} // what a napping claude said on its way out
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(0),
        }
        let now = now_ms();
        if now.abs_diff(last_check) < 1000 {
            continue;
        }
        last_check = now;
        let cfg = config::get();
        if let Some(s) = hook::read(&p.files.state).filter(|s| *s != st) {
            log(format_args!("state {s:?}"));
            st = s;
        }
        if let Some(l) = live.as_mut() {
            if let Ok(Some(exit)) = l.child.try_wait() {
                // Exited without closing its output (a child of its may hold it open).
                let gen = l.gen;
                p.drain(&rx, gen);
                return Ok(exit.code().unwrap_or(1));
            }
        }
        // A request from the tray or `hibernaut nap|wake|restart`: no waiting for idle time, but
        // the same checks as any nap, so nothing in flight is lost.
        if let Some(action) = control::take() {
            let result = match (action, live.is_some()) {
                (Action::Nap | Action::Restart, true) => match p.blocker(&st, now, 0) {
                    Some(why) => format!("not now: {why}"),
                    None => {
                        (freed, since) = (memory, now);
                        p.nap(live.take().unwrap());
                        if action == Action::Restart {
                            live = Some(p.wake(&st)?);
                            (memory, p.last_activity) = (0, now);
                            "restarted".into()
                        } else {
                            format!("napped, {} freed", status::size(freed))
                        }
                    }
                },
                (Action::Nap, false) => "already napping".into(),
                (Action::Wake | Action::Restart, false) => {
                    live = Some(p.wake(&st)?);
                    (p.last_activity, since) = (now, now);
                    "woke".into()
                }
                (Action::Wake, true) => "already awake".into(),
            };
            log(format_args!("{} requested: {result}", action.name()));
            last_action = control::outcome(action, &result, now);
        }
        let cwd = session_cwd(&st, &p.cwd);
        let pid = live.as_ref().map(|l| l.child.id());
        let mut why = None;
        if let Some(pid) = pid {
            if now.abs_diff(last_measure) >= cfg.idle_ms.min(15_000) {
                last_measure = now;
                memory = proc::tree_rss(pid);
                armed |= memory < cfg.memory_limit && now >= rearm_at;
            }
            let excluded = cfg.excludes(&cwd);
            why = if excluded { Some("excluded folder") } else { p.blocker(&st, now, cfg.idle_ms) };
            if why != awake_because {
                log(format_args!("awake because: {why:?}"));
                awake_because = why;
            }
            // Past the memory limit, nap after a shorter quiet spell: the next message brings
            // back a fresh claude.
            let leaking = armed && cfg.memory_limit > 0 && memory > cfg.memory_limit && !excluded;
            let recycle = leaking && p.blocker(&st, now, cfg.idle_ms.min(RECYCLE_QUIET_MS)).is_none();
            if why.is_none() || recycle {
                if why.is_some() {
                    log(format_args!("restarting: claude grew to {}", status::size(memory)));
                    (armed, rearm_at) = (false, now + REARM_AFTER_MS);
                }
                (freed, since) = (memory, now);
                p.nap(live.take().unwrap());
            }
        }
        // A newer hibernaut was installed. While napping nothing runs, so step aside: a clean exit
        // is how a session ends, the extension closes it quietly, and its next message starts
        // the session again through the new hibernaut.
        if live.is_none() && status.replaced() {
            log("a newer hibernaut is installed; handing the session over to it");
            return Ok(0);
        }
        let napping = live.is_none();
        status.update(
            now,
            json!({
                "pid": std::process::id(), "mode": "panel", "napping": napping,
                "why": if napping { None } else { why }, "claude_pid": pid, "since": since,
                "memory": if napping { 0 } else { memory }, "freed": freed,
                "cwd": cwd, "session": st.session_id, "last_action": last_action,
            }),
        );
    }
}

impl Pipe {
    fn spawn(&mut self, argv: &[OsString], cwd: &Path) -> io::Result<Live> {
        self.gen += 1;
        log(format_args!("spawn {} {argv:?} in {}", self.claude.display(), cwd.display()));
        let mut cmd = Command::new(&self.claude);
        cmd.args(argv)
            .current_dir(cwd)
            .env("HIBERNAUT_STATE", &self.files.state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[cfg(unix)]
        {
            // Its own process group, so a nap can stop its MCP servers and shells too.
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn()?;
        log(format_args!("claude pid {}", child.id()));
        let gen = self.gen;
        pump(child.stdout.take().unwrap(), self.tx.clone(), move |line| Ev::Out(gen, line));
        Ok(Live { stdin: child.stdin.take(), child, gen })
    }

    fn wake(&mut self, st: &hook::State) -> io::Result<Live> {
        let how = st.comeback().unwrap_or(hook::Comeback::Resume);
        log(format_args!("waking {} ({how:?})", st.session_id));
        let argv = [args::resume_sdk(&self.toks, &st.session_id, how), self.files.settings_arg()?].concat();
        let mut live = self.spawn(&argv, &session_cwd(st, &self.cwd))?;
        // Set the new claude up the way the SDK set up the first one, in the mode it was left in.
        let mode = (!st.mode.is_empty() && st.mode_at > self.mode_at)
            .then(|| json!({ "type": "control_request", "request": { "subtype": "set_permission_mode", "mode": st.mode } }));
        for (n, req) in self.init.iter().chain(&self.replay).chain(&mode).enumerate() {
            let mut req = req.clone();
            req["request_id"] = json!(format!("{OWN_ID}{}-{n}", self.gen));
            send(&mut live, format!("{req}\n").as_bytes());
        }
        Ok(live)
    }

    fn nap(&mut self, mut l: Live) {
        log("napping");
        // Closing its input is how the SDK ends a session: claude shuts down cleanly, MCP
        // servers included.
        l.stdin = None;
        if !wait_for(&mut l.child, Duration::from_secs(5)) {
            let pid = l.child.id();
            term::kill_tree(pid, false);
            // Only while it's still alive: once reaped, its pid can belong to someone else.
            if !wait_for(&mut l.child, Duration::from_secs(3)) {
                term::kill_tree(pid, true);
                let _ = l.child.kill();
                let _ = l.child.wait();
            }
        }
        proc::trim_self();
    }

    /// Why the session must stay awake, or None when it may nap after `idle_ms` of quiet.
    fn blocker(&self, st: &hook::State, now: u64, idle_ms: u64) -> Option<&'static str> {
        if self.closing {
            Some("session closing")
        } else if self.busy || st.state != "idle" {
            Some("claude is working")
        } else if !self.pending_in.is_empty() || !self.pending_out.is_empty() {
            Some("a request is in flight")
        } else if self.init.is_none() {
            Some("no initialize request to replay")
        } else if let Err(why) = st.comeback() {
            Some(why)
        } else if now.saturating_sub(self.last_activity.max(st.at)) < idle_ms {
            Some("used recently")
        } else {
            None
        }
    }

    /// Tracks a line from the SDK.
    fn note_in(&mut self, msg: &Value) {
        let kind = msg["type"].as_str().unwrap_or_default();
        if kind == "keep_alive" {
            return;
        }
        self.last_activity = now_ms();
        match kind {
            "user" => self.busy = true,
            "control_request" => {
                self.pending_in.insert(text(&msg["request_id"]));
                let subtype = msg["request"]["subtype"].as_str().unwrap_or_default();
                log(format_args!("sdk asks: {subtype}"));
                if subtype == "set_permission_mode" {
                    self.mode_at = now_ms();
                }
                if subtype == "initialize" {
                    self.init = Some(msg.clone());
                } else if REPLAY.contains(&subtype) {
                    // Setters only need their latest value; toggles and flag layers add up.
                    if subtype.starts_with("set_") || subtype.starts_with("select_") {
                        self.replay.retain(|r| r["request"]["subtype"] != subtype);
                    }
                    self.replay.push(msg.clone());
                }
            }
            "control_response" => {
                self.pending_out.remove(&text(&msg["response"]["request_id"]));
            }
            "control_cancel_request" => self.cancel(&text(&msg["request_id"])),
            _ => {}
        }
    }

    /// Tracks a line from claude; false for answers to our own replays.
    fn note_out(&mut self, msg: &Value) -> bool {
        let kind = msg["type"].as_str().unwrap_or_default();
        if kind == "keep_alive" {
            return true;
        }
        self.last_activity = now_ms();
        match kind {
            "control_response" => {
                let id = text(&msg["response"]["request_id"]);
                if id.starts_with(OWN_ID) {
                    if msg["response"]["subtype"] == "error" {
                        log(format_args!("replay {id} failed: {}", msg["response"]["error"]));
                    }
                    return false;
                }
                self.pending_in.remove(&id);
            }
            "control_request" => {
                self.pending_out.insert(text(&msg["request_id"]));
                log(format_args!("claude asks: {}", msg["request"]["subtype"]));
            }
            "control_cancel_request" => self.cancel(&text(&msg["request_id"])),
            "result" => self.busy = false,
            "assistant" | "user" | "stream_event" | "tool_progress" => self.busy = true,
            _ => {}
        }
        true
    }

    fn cancel(&mut self, id: &str) {
        self.pending_in.remove(id);
        self.pending_out.remove(id);
    }

    /// Passes on what claude `gen` still has to say, for up to a second.
    fn drain(&mut self, rx: &Receiver<Ev>, gen: u64) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while let Ok(ev) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            match ev {
                Ev::Out(g, Some(line)) if g == gen => {
                    if self.note_out(&parse(&line)) {
                        write_out(&line);
                    }
                }
                Ev::Out(g, None) if g == gen => break,
                _ => {}
            }
        }
    }
}

/// Sends each line read from `r` (None at the end) to `tx`.
fn pump<R: Read + Send + 'static>(r: R, tx: Sender<Ev>, wrap: impl Fn(Option<Vec<u8>>) -> Ev + Send + 'static) {
    thread::spawn(move || {
        let mut r = BufReader::new(r);
        loop {
            let mut line = Vec::new();
            let got = match r.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(line),
            };
            let end = got.is_none();
            if tx.send(wrap(got)).is_err() || end {
                break;
            }
        }
    });
}

fn parse(line: &[u8]) -> Value {
    serde_json::from_slice(line).unwrap_or(Value::Null)
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn send(l: &mut Live, line: &[u8]) {
    if let Some(stdin) = l.stdin.as_mut() {
        let _ = stdin.write_all(line).and_then(|()| stdin.flush());
    }
}

fn write_out(line: &[u8]) {
    let mut o = io::stdout().lock();
    let _ = o.write_all(line).and_then(|()| o.flush());
}

fn wait_for(child: &mut Child, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

fn exit_code(child: &mut Child) -> i32 {
    if !wait_for(child, Duration::from_secs(10)) {
        let _ = child.kill();
    }
    child.wait().ok().and_then(|s| s.code()).unwrap_or(1)
}
