//! Terminal mode: claude runs in a pseudo-terminal hibernaut owns. hibernaut watches keystrokes and the
//! terminal's focus reports, naps the session once it is idle and out of sight, and wakes it on
//! focus or a keypress, replaying what was typed. It also restarts a session that has grown
//! past the memory limit, labels napping tabs with 💤, and hands Ctrl+Z to the shell on Unix.

use crate::{
    args, config,
    control::{self, Action},
    hook, log, now_ms, proc, session_cwd, status, term, Files, REARM_AFTER_MS,
};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtyPair, PtySize};
use serde_json::{json, Value};
use std::{
    env,
    ffi::OsString,
    io::{self, Read, Write},
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
        mpsc::{self, RecvTimeoutError},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

const TICK: Duration = Duration::from_millis(250);
/// Never nap within this long of Claude drawing anything (its spinner redraws constantly while
/// it works), in case a hook was missed.
const OUTPUT_QUIET_MS: u64 = 30_000;
/// A restart for memory waits this long without input, even with the tab in focus.
const RECYCLE_QUIET_MS: u64 = 60_000;
/// Replayed keys go in once a woken claude has drawn its prompt and then paused this long.
const READY_QUIET_MS: u64 = 500;
/// Claude's prompt marker. Input sent before it shows up is lost while Claude sets up its input.
const PROMPT: &str = "❯";

pub struct Wrapper {
    claude: PathBuf,
    toks: Vec<args::Tok>,
    cwd: PathBuf,
    files: Files,
    size: (u16, u16),
    shared: Arc<Shared>,
}

/// Written by the output thread, read by the main loop.
#[derive(Default)]
struct Shared {
    /// Claude turned on focus reports itself (CSI ?1004h), so pass them through to it.
    wants_focus: AtomicBool,
    alt_screen: AtomicBool,
    last_output: AtomicU64,
    /// Claude has drawn its prompt since it started.
    prompt_drawn: AtomicBool,
    /// The window title Claude last set, to label the tab while it naps.
    title: Mutex<String>,
}

/// A running claude.
struct Live {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    /// Disconnects once the output thread has written everything.
    output_done: mpsc::Receiver<()>,
}

impl Wrapper {
    pub fn new(claude: PathBuf, toks: Vec<args::Tok>, files: Files) -> Wrapper {
        Wrapper { claude, toks, cwd: env::current_dir().unwrap_or_default(), files, size: term::size(), shared: Arc::default() }
    }

    pub fn run(mut self) -> io::Result<i32> {
        #[cfg_attr(not(unix), allow(unused_variables))]
        let raw = term::Raw::enter()?;
        term::ignore_ctrl_c();
        out(b"\x1b[?1004h"); // ask the terminal to report focus changes
        let (tx, keys) = mpsc::channel();
        #[cfg_attr(not(windows), allow(unused_variables))]
        let reader = term::spawn_reader(tx);
        let first = [args::flatten(&self.toks), self.files.settings_arg()?].concat();
        let mut live = Some(self.spawn(&first, &self.cwd)?);
        let mut status = status::Status::new(self.files.status.clone());
        let mut act = Activity::new(now_ms());
        let mut st = hook::State::default();
        let (mut last_check, mut last_measure, mut since) = (0, 0, now_ms());
        let mut awake_because = None;
        // Keys typed since a wake, held until the new claude is ready for them.
        // An older hibernaut that handed this session over passes on the keys that woke it.
        let mut replay: Option<(Vec<u8>, u64)> = env::var("HIBERNAUT_TYPED").ok().and_then(|h| unhex(&h)).map(|k| (k, now_ms()));
        // Memory of claude's process tree; what the last nap freed; whether a restart for memory
        // may happen, and when a sample may allow one again after a restart (see REARM_AFTER_MS).
        let (mut memory, mut freed, mut armed, mut rearm_at) = (0u64, 0u64, true, 0u64);
        let mut last_action = Value::Null;
        let code = loop {
            match keys.recv_timeout(TICK) {
                Ok(Some(bytes)) => {
                    let now = now_ms();
                    let k = scan_keys(&bytes);
                    if let Some(f) = k.focus {
                        (act.focus_known, act.focused, act.last_focus) = (true, f, now);
                        log(format_args!("focus {}", if f { "in" } else { "out" }));
                    }
                    if !k.keys.is_empty() {
                        act.last_input = now;
                        act.typed(&k.keys);
                    }
                    #[cfg(unix)]
                    if k.suspend {
                        self.suspend(&raw, live.as_ref());
                    }
                    match (live.as_mut(), replay.as_mut()) {
                        (Some(l), Some((queued, _))) => {
                            queued.extend_from_slice(&k.keys);
                            send(l, &k.reports);
                        }
                        (Some(l), None) => send(l, if self.shared.wants_focus.load(Relaxed) { &k.with_focus } else { &k.rest }),
                        (None, _) if (!k.keys.is_empty() || k.focus == Some(true)) && status.replaced() => {
                            // A newer hibernaut is installed: it takes the session from here.
                            return self.hand_over(raw, (&reader, &keys), &st, k.keys, status.exe());
                        }
                        (None, _) if !k.keys.is_empty() || k.focus == Some(true) => {
                            log(format_args!("woken by {} typed bytes, focus {:?}", k.keys.len(), k.focus));
                            act.focused = true;
                            live = Some(self.wake(&st, Some("waking Claude..."))?);
                            replay = Some((k.keys, now));
                            since = now;
                        }
                        (None, _) => {}
                    }
                }
                Ok(None) | Err(RecvTimeoutError::Disconnected) => {
                    // The terminal is gone.
                    if let Some(l) = live.take() {
                        self.stop(l);
                    }
                    break 0;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            let now = now_ms();
            let size = term::size();
            if size != self.size {
                self.size = size;
                if let Some(l) = &live {
                    let _ = l.master.resize(pty_size(size));
                }
            }
            if let Some(exit) = live.as_mut().and_then(|l| l.child.try_wait().ok().flatten()) {
                finish(live.take().unwrap());
                log(format_args!("claude exited with {}", exit.exit_code()));
                break exit.exit_code() as i32;
            }
            if let (Some(l), Some((_, at))) = (live.as_mut(), replay.as_ref()) {
                let ready = self.shared.prompt_drawn.load(Relaxed)
                    && now.saturating_sub(self.shared.last_output.load(Relaxed)) >= READY_QUIET_MS;
                if ready || now.saturating_sub(*at) >= 15_000 {
                    let typed = replay.take().unwrap().0;
                    log(format_args!("typing in {} bytes held during the wake (prompt drawn: {ready})", typed.len()));
                    type_in(l, &typed);
                }
            }
            if now.abs_diff(last_check) < 1000 {
                continue;
            }
            last_check = now;
            let cfg = config::get();
            if let Some(s) = hook::read(&self.files.state).filter(|s| *s != st) {
                log(format_args!("state {s:?}"));
                st = s;
            }
            // A request from the tray or `hibernaut nap|wake|restart`. Unlike a nap on its own, it
            // doesn't wait for idle time or focus, but it never loses anything either.
            if let Some(action) = control::take() {
                let result = match (action, live.is_some()) {
                    (Action::Nap | Action::Restart, true) => match act.unsafe_to_stop(&st) {
                        Some(why) => format!("not now: {why}"),
                        None if action == Action::Nap => {
                            freed = memory;
                            self.nap(live.take().unwrap(), freed);
                            since = now;
                            format!("napped, {} freed", status::size(freed))
                        }
                        None => {
                            self.stop(live.take().unwrap());
                            live = Some(self.wake(&st, Some("restarted on request. The conversation is unchanged."))?);
                            replay = Some((Vec::new(), now));
                            (memory, since) = (0, now);
                            "restarted".into()
                        }
                    },
                    (Action::Nap, false) => "already napping".into(),
                    (Action::Wake | Action::Restart, false) => {
                        live = Some(self.wake(&st, Some("woken on request"))?);
                        replay = Some((Vec::new(), now));
                        // Ready when you get there, not napping again a moment later.
                        (act.last_input, since) = (now, now);
                        "woke".into()
                    }
                    (Action::Wake, true) => "already awake".into(),
                };
                log(format_args!("{} requested: {result}", action.name()));
                last_action = control::outcome(action, &result, now);
            }
            let cwd = session_cwd(&st, &self.cwd);
            let pid = live.as_ref().and_then(|l| l.child.process_id());
            let mut why = None;
            if let Some(pid) = pid {
                if now.abs_diff(last_measure) >= cfg.idle_ms.min(15_000) {
                    last_measure = now;
                    memory = proc::tree_rss(pid);
                    armed |= memory < cfg.memory_limit && now >= rearm_at;
                }
                let drew = self.shared.last_output.load(Relaxed);
                let excluded = cfg.excludes(&cwd);
                why = if excluded { Some("excluded folder") } else { act.blocker(&st, drew, now, cfg.idle_ms) };
                if why != awake_because {
                    log(format_args!("awake because: {why:?}"));
                    awake_because = why;
                }
                let leaking = armed && cfg.memory_limit > 0 && memory > cfg.memory_limit;
                if why.is_none() {
                    freed = memory;
                    self.nap(live.take().unwrap(), freed);
                    since = now;
                } else if leaking && !excluded && act.recyclable(&st, drew, now, cfg.idle_ms) {
                    (armed, rearm_at) = (false, now + REARM_AFTER_MS);
                    log(format_args!("restarting: claude grew to {}", status::size(memory)));
                    self.stop(live.take().unwrap());
                    let note = format!("Claude had grown to {}, so Hibernaut restarted it. The conversation is unchanged.", status::size(memory));
                    live = Some(self.wake(&st, Some(&note))?);
                    replay = Some((Vec::new(), now));
                    memory = 0;
                }
            }
            let napping = live.is_none();
            status.update(
                now,
                json!({
                    "pid": std::process::id(), "mode": "terminal", "napping": napping,
                    "why": if napping { None } else { why }, "claude_pid": pid, "since": since,
                    "memory": if napping { 0 } else { memory }, "freed": freed,
                    "cwd": cwd, "session": st.session_id, "last_action": last_action,
                }),
            );
        };
        out(b"\x1b[?1004l");
        Ok(code)
    }

    fn spawn(&self, argv: &[OsString], cwd: &Path) -> io::Result<Live> {
        log(format_args!("spawn {} {argv:?} in {}", self.claude.display(), cwd.display()));
        let PtyPair { master, slave } = native_pty_system().openpty(pty_size(self.size)).map_err(io::Error::other)?;
        let mut cmd = CommandBuilder::new(&self.claude);
        cmd.args(argv);
        cmd.cwd(cwd);
        cmd.env("HIBERNAUT_STATE", &self.files.state);
        cmd.env_remove("HIBERNAUT_TYPED");
        let child = slave.spawn_command(cmd).map_err(io::Error::other)?;
        drop(slave);
        log(format_args!("claude pid {:?}", child.process_id()));
        let mut reader = master.try_clone_reader().map_err(io::Error::other)?;
        let writer = master.take_writer().map_err(io::Error::other)?;
        self.shared.wants_focus.store(false, Relaxed);
        self.shared.alt_screen.store(false, Relaxed);
        self.shared.prompt_drawn.store(false, Relaxed);
        self.shared.last_output.store(now_ms(), Relaxed);
        let shared = self.shared.clone();
        let (done, output_done) = mpsc::channel::<()>();
        thread::spawn(move || {
            let _done = done;
            let mut buf = [0u8; 16 * 1024];
            let mut tail = Vec::new();
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                shared.last_output.store(now_ms(), Relaxed);
                shared.note_modes(&mut tail, &buf[..n]);
                out(&buf[..n]);
            }
        });
        Ok(Live { child, master, writer, output_done })
    }

    fn wake(&self, st: &hook::State, note: Option<&str>) -> io::Result<Live> {
        log(format_args!("waking {}", st.session_id));
        let mut s = Vec::new();
        let title = self.title();
        if !title.is_empty() {
            // Take the 💤 off the tab: pop the title we pushed, or set Claude's again.
            s.extend_from_slice(format!("\x1b[23;0t\x1b]0;{title}\x07").as_bytes());
        }
        if let Some(note) = note {
            s.extend_from_slice(format!("\x1b[2m[hibernaut] {note}\x1b[0m\r\n").as_bytes());
        }
        out(&s);
        let how = st.comeback().unwrap_or(hook::Comeback::Resume);
        let argv = [args::resume(&self.toks, &st.session_id, &st.mode, how), self.files.settings_arg()?].concat();
        self.spawn(&argv, &session_cwd(st, &self.cwd))
    }

    fn nap(&self, l: Live, freed: u64) {
        log("napping");
        self.stop(l);
        let mut s = Vec::new();
        let title = self.title();
        if !title.is_empty() {
            s.extend_from_slice(format!("\x1b[22;0t\x1b]0;💤 {title}\x07").as_bytes());
        }
        let freed = if freed > 0 { format!(" ({} freed)", status::size(freed)) } else { String::new() };
        s.extend_from_slice(
            format!("\r\n\x1b[2m[hibernaut] napping: Claude is stopped to save memory{freed}. Focus this tab or start typing to resume.\x1b[0m\r\n")
                .as_bytes(),
        );
        out(&s);
        proc::trim_self();
    }

    /// Claude's last title, without the spinner it puts in front.
    fn title(&self) -> String {
        let t = self.shared.title.lock().unwrap();
        t.trim_start_matches(|c: char| !c.is_alphanumeric()).trim().to_string()
    }

    /// Stops claude and everything it started (MCP servers, shells), then undoes what it may
    /// have left on in the terminal.
    /// Passes this napping session to the newer hibernaut installed at `exe`, which resumes it and
    /// types in the keys that woke this one. On Unix it replaces this process; on Windows this
    /// one waits for it, so the terminal sees one program throughout.
    fn hand_over(
        &self,
        raw: term::Raw,
        #[cfg_attr(unix, allow(unused_variables))] (reader, keys): (&thread::JoinHandle<()>, &mpsc::Receiver<Option<Vec<u8>>>),
        st: &hook::State,
        #[cfg_attr(unix, allow(unused_mut))] mut typed: Vec<u8>,
        exe: &str,
    ) -> io::Result<i32> {
        log(format_args!("a newer hibernaut is installed; handing session {} to it", st.session_id));
        #[cfg(windows)]
        {
            term::stop_reader(reader);
            while let Ok(Some(b)) = keys.try_recv() {
                typed.extend(scan_keys(&b).keys);
            }
        }
        let how = st.comeback().unwrap_or(hook::Comeback::Resume);
        let mut cmd = std::process::Command::new(exe);
        cmd.args(args::resume(&self.toks, &st.session_id, &st.mode, how))
            .current_dir(session_cwd(st, &self.cwd))
            .env("HIBERNAUT_TYPED", hex(&typed))
            .env_remove("HIBERNAUT_STATE");
        drop(raw); // the new hibernaut sets the terminal up its own way
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            Err(cmd.exec())
        }
        #[cfg(windows)]
        {
            let mut child = cmd.spawn()?;
            let _ = std::fs::remove_file(&self.files.status); // the new one reports for the session now
            proc::trim_self();
            Ok(child.wait()?.code().unwrap_or(1))
        }
    }

    fn stop(&self, mut l: Live) {
        if let Some(pid) = l.child.process_id() {
            term::kill_tree(pid, false);
            let mut exited = false;
            for _ in 0..30 {
                if matches!(l.child.try_wait(), Ok(Some(_))) {
                    exited = true;
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            // Only while it's still alive: once reaped, its pid can belong to someone else.
            if !exited {
                term::kill_tree(pid, true);
                let _ = l.child.kill();
            }
        }
        finish(l);
        let mut s = Vec::new();
        if self.shared.alt_screen.load(Relaxed) {
            s.extend_from_slice(b"\x1b[?1049l");
        }
        // Colors, hidden cursor, mouse reports and kitty keyboard mode off. Focus reports and
        // bracketed paste on: hibernaut reads them while Claude is gone, and a paste typed in after
        // a wake must stay one paste, not a prompt per line.
        s.extend_from_slice(b"\x1b[0m\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[<u\x1b[?1004h\x1b[?2004h");
        out(&s);
    }

    /// Ctrl+Z: hand the terminal to the shell until `fg`, then have Claude redraw.
    #[cfg(unix)]
    fn suspend(&self, raw: &term::Raw, live: Option<&Live>) {
        let alt = self.shared.alt_screen.load(Relaxed);
        let (before, after): (&[u8], &[u8]) =
            if alt { (b"\x1b[?1004l\x1b[?25h\x1b[?1049l", b"\x1b[?1049h\x1b[?1004h") } else { (b"\x1b[?1004l\x1b[?25h", b"\x1b[?1004h") };
        out(before);
        term::suspend(raw);
        out(after);
        if let Some(l) = live {
            let (rows, cols) = term::size();
            let _ = l.master.resize(pty_size((rows, cols.saturating_sub(1).max(1))));
            thread::sleep(Duration::from_millis(50));
            let _ = l.master.resize(pty_size((rows, cols)));
        }
    }
}

fn send(l: &mut Live, bytes: &[u8]) {
    if !bytes.is_empty() {
        let _ = l.writer.write_all(bytes).and_then(|()| l.writer.flush());
    }
}

/// Keys passed to a newer hibernaut in an environment variable.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

/// Sends what was typed during a nap. Text arriving all at once looks like a paste to Claude,
/// and Enter inside a paste is a newline, so each Enter goes on its own, a moment later.
fn type_in(l: &mut Live, bytes: &[u8]) {
    for part in bytes.split_inclusive(|&b| b == b'\r') {
        let (text, enter) = match part.split_last() {
            Some((b'\r', text)) => (text, true),
            _ => (part, false),
        };
        send(l, text);
        if enter {
            thread::sleep(Duration::from_millis(100));
            send(l, b"\r");
        }
    }
}

fn finish(l: Live) {
    let Live { child, master, writer, output_done } = l;
    drop((writer, master, child));
    // Let the output thread flush Claude's last frame before anything else is drawn.
    let _ = output_done.recv_timeout(Duration::from_secs(1));
}

impl Shared {
    /// Tracks what Claude sets that matters once it is gone: terminal modes and the title.
    /// `tail` carries the end of the previous chunk so a mode switch split across reads counts.
    fn note_modes(&self, tail: &mut Vec<u8>, chunk: &[u8]) {
        let mut hay = std::mem::take(tail);
        hay.extend_from_slice(chunk);
        for (on, off, flag) in [
            (b"\x1b[?1004h", b"\x1b[?1004l", &self.wants_focus),
            (b"\x1b[?1049h", b"\x1b[?1049l", &self.alt_screen),
        ] {
            let find = |n: &[u8]| hay.windows(n.len()).rposition(|w| w == n);
            match (find(on), find(off)) {
                (Some(a), Some(b)) => flag.store(a > b, Relaxed),
                (Some(_), None) => flag.store(true, Relaxed),
                (None, Some(_)) => flag.store(false, Relaxed),
                (None, None) => {}
            }
        }
        if hay.windows(PROMPT.len()).any(|w| w == PROMPT.as_bytes()) {
            self.prompt_drawn.store(true, Relaxed);
        }
        // OSC 0 or 2 sets the title, up to BEL or ESC.
        if let Some(p) = hay.windows(4).rposition(|w| w == b"\x1b]0;" || w == b"\x1b]2;") {
            let rest = &hay[p + 4..];
            if let Some(end) = rest.iter().position(|&c| c == 0x07 || c == 0x1b) {
                *self.title.lock().unwrap() = String::from_utf8_lossy(&rest[..end]).into_owned();
            }
        }
        tail.extend_from_slice(&hay[hay.len().saturating_sub(7)..]);
    }
}

/// What the user is doing, judged from keystrokes and the terminal's focus reports.
struct Activity {
    last_input: u64,
    last_focus: u64,
    /// Set by the first focus report; terminals without them never send one.
    focus_known: bool,
    focused: bool,
    /// About how many characters of unsent text are in Claude's prompt; a nap would lose them.
    draft: usize,
    /// The last key was a lone Esc: Claude clears the prompt on a second one.
    esc: bool,
    /// Inside a bracketed paste, where Enter is a newline.
    paste: bool,
}

impl Activity {
    fn new(now: u64) -> Self {
        Activity { last_input: now, last_focus: now, focus_known: false, focused: true, draft: 0, esc: false, paste: false }
    }

    /// Follows what keys do to Claude's prompt: Enter sends it, and Ctrl+C or Esc twice clear it.
    fn typed(&mut self, keys: &[u8]) {
        for (_, r) in tokens(keys) {
            let esc = std::mem::take(&mut self.esc);
            match &keys[r] {
                b"\x1b[200~" => self.paste = true,
                b"\x1b[201~" => self.paste = false,
                [b'\r'] if !self.paste => self.draft = 0,
                [0x03] => self.draft = 0,
                [0x1b] if esc => self.draft = 0,
                [0x1b] => self.esc = true,
                [0x7f | 0x08] => self.draft = self.draft.saturating_sub(1),
                [b'\r'] => self.draft += 1, // a pasted newline
                // One per character, so not for UTF-8 continuation bytes.
                [c] if *c >= 0x20 && !(0x80..0xc0).contains(c) => self.draft += 1,
                _ => {}
            }
        }
    }

    /// Why the session must stay awake, or None when it may nap.
    fn blocker(&self, st: &hook::State, last_output: u64, now: u64, idle_ms: u64) -> Option<&'static str> {
        let quiet_since = |t: u64| now.saturating_sub(t);
        if st.state != "idle" {
            Some("claude is working")
        } else if let Err(why) = st.comeback() {
            Some(why)
        } else if self.draft > 0 {
            Some("unsent text in the prompt")
        } else if self.focus_known && self.focused {
            Some("tab has focus")
        } else if quiet_since(self.last_input.max(self.last_focus).max(st.at)) < idle_ms {
            Some("used recently")
        } else if quiet_since(last_output) < idle_ms.min(OUTPUT_QUIET_MS) {
            Some("claude drew recently")
        } else {
            None
        }
    }

    /// Whether a restart for memory is safe: what a nap needs, except that the tab may have
    /// focus, after a shorter quiet spell.
    fn recyclable(&self, st: &hook::State, last_output: u64, now: u64, idle_ms: u64) -> bool {
        let quiet = idle_ms.min(RECYCLE_QUIET_MS);
        self.unsafe_to_stop(st).is_none()
            && now.saturating_sub(self.last_input.max(st.at)) >= quiet
            && now.saturating_sub(last_output) >= quiet.min(OUTPUT_QUIET_MS)
    }

    /// What stopping claude right now would lose, or None if nothing.
    fn unsafe_to_stop(&self, st: &hook::State) -> Option<&'static str> {
        if st.state != "idle" {
            Some("claude is working")
        } else if let Err(why) = st.comeback() {
            Some(why)
        } else if self.draft > 0 {
            Some("unsent text in the prompt")
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Focus(bool),
    Key,
    /// An answer to a query Claude sent the terminal (cursor position, colors, modes).
    Report,
    Suspend,
}

/// Splits terminal input into focus reports, keys, terminal reports, and Ctrl+Z.
fn tokens(b: &[u8]) -> Vec<(Kind, Range<usize>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let kind = match &b[i..] {
            [0x1b, b'[', b'I', ..] => {
                i += 3;
                Kind::Focus(true)
            }
            [0x1b, b'[', b'O', ..] => {
                i += 3;
                Kind::Focus(false)
            }
            [0x1b, b'[', rest @ ..] => {
                // CSI: parameters up to a final byte.
                i = rest.iter().position(|c| (0x40..=0x7e).contains(c)).map_or(b.len(), |p| start + 3 + p);
                let seq = &b[start..i];
                match seq.last() {
                    _ if seq == b"\x1b[122;5u" => Kind::Suspend, // Ctrl+Z under the kitty keyboard protocol
                    Some(b'R' | b'c' | b'n' | b't' | b'y') => Kind::Report,
                    Some(b'u') if seq.get(2) == Some(&b'?') => Kind::Report,
                    _ => Kind::Key,
                }
            }
            [0x1b, b']' | b'P' | b'_' | b'^', body @ ..] => {
                // OSC, DCS, APC, PM: up to BEL or ESC \.
                i = match body.iter().position(|&c| c == 0x07 || c == 0x1b) {
                    Some(p) if body[p] == 0x07 => start + 3 + p,
                    Some(p) => (start + 4 + p).min(b.len()),
                    None => b.len(),
                };
                Kind::Report
            }
            [0x1b, b'O', _, ..] => {
                i += 3; // SS3 key
                Kind::Key
            }
            [0x1b, _, ..] => {
                i += 2; // Alt+key
                Kind::Key
            }
            [0x1a, ..] => {
                i += 1;
                Kind::Suspend
            }
            _ => {
                i += 1;
                Kind::Key
            }
        };
        out.push((kind, start..i));
    }
    out
}

/// One chunk of terminal input, sorted out.
#[derive(Debug, Default)]
struct Keys {
    /// What Claude gets: everything but focus reports, and Ctrl+Z on Unix (hibernaut handles it).
    rest: Vec<u8>,
    /// `rest` with focus reports, for a Claude that asked for them.
    with_focus: Vec<u8>,
    /// Only what the user typed: it counts as use, and waits for a woken claude.
    keys: Vec<u8>,
    /// Answers to Claude's terminal queries; they never wait.
    reports: Vec<u8>,
    focus: Option<bool>,
    suspend: bool,
}

fn scan_keys(b: &[u8]) -> Keys {
    let mut k = Keys::default();
    for (kind, range) in tokens(b) {
        let bytes = &b[range];
        match kind {
            Kind::Focus(f) => {
                k.focus = Some(f);
                k.with_focus.extend_from_slice(bytes);
                continue;
            }
            Kind::Suspend if cfg!(unix) => {
                k.suspend = true;
                continue;
            }
            Kind::Report => k.reports.extend_from_slice(bytes),
            Kind::Key | Kind::Suspend => k.keys.extend_from_slice(bytes),
        }
        k.rest.extend_from_slice(bytes);
        k.with_focus.extend_from_slice(bytes);
    }
    k
}

fn pty_size((rows, cols): (u16, u16)) -> PtySize {
    PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }
}

fn out(b: &[u8]) {
    let mut o = io::stdout().lock();
    // Windows consoles reject invalid UTF-8; one bad byte must not stall the session.
    if o.write_all(b).is_err() {
        let _ = o.write_all(String::from_utf8_lossy(b).as_bytes());
    }
    let _ = o.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process};

    #[test]
    fn keys_split_out_focus_reports() {
        let k = scan_keys(b"\x1b[Ihi\x1b[O");
        assert_eq!((k.rest.as_slice(), k.focus), (&b"hi"[..], Some(false)));
        assert_eq!(k.with_focus, b"\x1b[Ihi\x1b[O");
        let arrows_and_mouse = b"\x1b[A\x1bOB\x1b[<0;3;4M";
        let k = scan_keys(arrows_and_mouse);
        assert_eq!((k.keys.as_slice(), k.focus), (&arrows_and_mouse[..], None));
    }

    #[test]
    fn drafts_follow_what_claude_does_with_keys() {
        // Each case is typed in chunks, as keys arrive.
        for (chunks, draft) in [
            (&[&b"hi"[..]][..], 2),
            (&[b"hi", b"\r"], 0),
            (&[b"hi", b"\x03"], 0),
            (&[b"hi", b"\x1b"], 2), // one Esc only offers to clear
            (&[b"hi", b"\x1b", b"\x1b"], 0),
            (&[b"hi", b"\x7f", b"\x7f"], 0),
            (&["é".as_bytes(), b"\x7f"], 0),
            (&[b"\x1b[200~a\rb\x1b[201~"], 3), // a pasted newline doesn't send
            (&[b"\x1b[200~a\rb\x1b[201~", b"\r"], 0),
            (&[b"hi", b"\x1b[A\x1b[<0;3;4M"], 2),
        ] {
            let mut act = Activity::new(0);
            for c in chunks {
                act.typed(c);
            }
            assert_eq!(act.draft, draft, "{chunks:?}");
        }
    }

    #[test]
    fn terminal_answers_are_not_typing() {
        // Background color (OSC 11, both terminators), cursor position, device attributes,
        // kitty keyboard flags: Claude asks, the terminal answers through the keyboard stream.
        for answer in [&b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\"[..], b"\x1b]11;rgb:0/0/0\x07", b"\x1b[12;40R", b"\x1b[?62;22c", b"\x1b[?1u"] {
            let k = scan_keys(answer);
            assert_eq!((k.reports.as_slice(), k.keys.len()), (answer, 0), "{answer:?}");
        }
        let k = scan_keys(b"\x1b]11;rgb:0/0/0\x07ok");
        assert_eq!(k.keys, b"ok");
    }

    #[test]
    fn ctrl_z_goes_to_the_shell_on_unix() {
        for z in [&b"\x1a"[..], b"\x1b[122;5u"] {
            let k = scan_keys(z);
            assert_eq!(k.suspend, cfg!(unix));
            assert_eq!(k.rest.is_empty(), cfg!(unix));
        }
    }

    #[test]
    fn naps_only_when_idle_unseen_and_quiet() {
        let transcript = env::temp_dir().join(format!("hibernaut-test-{}.jsonl", process::id()));
        fs::write(&transcript, "").unwrap();
        let st = hook::State {
            state: "idle".into(),
            session_id: "S".into(),
            transcript: transcript.to_string_lossy().into(),
            ..Default::default()
        };
        let mut act = Activity::new(0);
        assert_eq!(act.blocker(&st, 0, 1000, 1000), None);
        assert_eq!(act.blocker(&st, 0, 999, 1000), Some("used recently"));
        assert_eq!(act.blocker(&st, 500, 1000, 1000), Some("claude drew recently"));
        act.focus_known = true;
        assert_eq!(act.blocker(&st, 0, 1000, 1000), Some("tab has focus"));
        // A restart for memory doesn't need the tab out of sight.
        assert!(act.recyclable(&st, 0, 1000, 1000));
        act.focused = false;
        assert_eq!(act.blocker(&st, 0, 1000, 1000), None);
        act.draft = 3;
        assert_eq!(act.blocker(&st, 0, 1000, 1000), Some("unsent text in the prompt"));
        assert!(!act.recyclable(&st, 0, 1000, 1000));
        act.draft = 0;
        for busy in ["busy", "bg", ""] {
            let st = hook::State { state: busy.into(), ..st.clone() };
            assert_eq!(act.blocker(&st, 0, 1000, 1000), Some("claude is working"));
            assert!(!act.recyclable(&st, 0, 1000, 1000));
        }
        fs::remove_file(&transcript).unwrap();
        // No transcript: fine while nothing was said (it comes back fresh), not after.
        assert_eq!(act.blocker(&st, 0, 1000, 1000), None);
        let st = hook::State { prompted: true, ..st };
        assert_eq!(act.blocker(&st, 0, 1000, 1000), Some("this chat isn't saved to disk"));
    }

    #[test]
    fn tracks_modes_and_title_across_chunks() {
        let sh = Shared::default();
        let mut tail = Vec::new();
        sh.note_modes(&mut tail, b"abc\x1b[?10");
        sh.note_modes(&mut tail, b"49h\x1b[?1004h\x1b]0;\xe2\x9c\xb3 Fix the bug\x07");
        assert!(sh.alt_screen.load(Relaxed) && sh.wants_focus.load(Relaxed));
        assert_eq!(*sh.title.lock().unwrap(), "✳ Fix the bug");
        sh.note_modes(&mut tail, b"\x1b[?1049l");
        assert!(!sh.alt_screen.load(Relaxed) && sh.wants_focus.load(Relaxed));
    }
}
