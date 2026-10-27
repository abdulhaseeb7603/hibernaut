//! Drives the real ccnap binary around a fake claude (examples/fake_claude.rs), both ways it
//! runs: in a terminal it must nap once the tab is idle and unfocused and resume on focus; as an
//! editor's process wrapper it must nap when idle and resume, set up as before, on the next
//! message.

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde_json::{json, Value};
use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime},
};

/// ccnap as an editor's process wrapper around the fake claude, with the SDK's end of the pipes.
struct Panel {
    ccnap: std::process::Child,
    sdk: Option<std::process::ChildStdin>,
    lines: Arc<Mutex<Vec<Value>>>,
    dir: PathBuf,
}

impl Panel {
    fn start(name: &str, idle: &str) -> Panel {
        Panel::start_from(name, idle, false)
    }

    /// With `own_copy`, ccnap runs from a copy of its own in the test's folder, to update.
    fn start_from(name: &str, idle: &str, own_copy: bool) -> Panel {
        // Editors run `<wrapper> <path to claude> <args>`, so the fake has to be called claude.
        let dir = scratch(name);
        let claude = dir.join(format!("claude{}", env::consts::EXE_SUFFIX));
        fs::copy(fake_claude(), &claude).unwrap();
        let exe = if own_copy { copy_ccnap(&dir) } else { PathBuf::from(env!("CARGO_BIN_EXE_ccnap")) };
        let mut ccnap = Command::new(exe)
            .arg(&claude)
            .args(["--output-format", "stream-json", "--verbose", "--input-format", "stream-json", "--model=m1"])
            .env("CCNAP_IDLE", idle)
            .env("CCNAP_LOG", dir.join("ccnap.log"))
            .env("CCNAP_HOME", env::temp_dir().join("ccnap-e2e-home"))
            .env_remove("CCNAP_STATE")
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let lines = Arc::new(Mutex::new(Vec::<Value>::new()));
        let (sink, out) = (lines.clone(), ccnap.stdout.take().unwrap());
        thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                sink.lock().unwrap().push(serde_json::from_str(&line.unwrap()).unwrap());
            }
        });
        Panel { sdk: ccnap.stdin.take(), ccnap, lines, dir }
    }

    fn send(&mut self, v: Value) {
        writeln!(self.sdk.as_mut().unwrap(), "{v}").unwrap();
    }

    fn wait_for(&self, what: &str, f: &dyn Fn(&Value) -> bool) -> Value {
        let start = Instant::now();
        loop {
            if let Some(v) = self.lines.lock().unwrap().iter().rev().find(|v| f(v)) {
                return v.clone();
            }
            assert!(start.elapsed() < Duration::from_secs(15), "no {what} in {:?}", self.lines.lock().unwrap());
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// The text of the assistant's reply to `prompt`.
    fn reply(&self, prompt: &str) -> String {
        let echo = format!("echo:{prompt} ");
        let v = self.wait_for(&echo, &|v: &Value| v["type"] == "assistant" && v.to_string().contains(&echo));
        v["message"]["content"][0]["text"].as_str().unwrap().to_string()
    }

    fn close(mut self) {
        let seen = self.lines.lock().unwrap().clone();
        assert!(!seen.iter().any(|v| v.to_string().contains("ccnap-")), "replay answers leaked");
        self.sdk = None;
        let start = Instant::now();
        let status = loop {
            if let Some(s) = self.ccnap.try_wait().unwrap() {
                break s;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "ccnap did not exit");
            thread::sleep(Duration::from_millis(50));
        };
        assert!(status.success(), "{status}");
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn answered(id: &'static str) -> impl Fn(&Value) -> bool {
    move |v: &Value| v["response"]["request_id"] == id
}

#[test]
fn panel_naps_when_idle_and_resumes_on_the_next_message() {
    let mut p = Panel::start("resume", "2s");
    p.send(json!({ "type": "control_request", "request_id": "i1", "request": { "subtype": "initialize" } }));
    p.wait_for("initialize answer", &answered("i1"));
    p.send(json!({ "type": "control_request", "request_id": "m1", "request": { "subtype": "set_model", "model": "m2" } }));
    p.wait_for("set_model answer", &answered("m1"));
    p.send(json!({ "type": "control_request", "request_id": "p1", "request": { "subtype": "set_permission_mode", "mode": "default" } }));
    p.wait_for("set_permission_mode answer", &answered("p1"));
    // Allowing a tool with "allow all edits" changes the mode inside claude, with no request.
    p.send(json!({ "type": "user", "message": { "role": "user", "content": "edit" } }));
    let ask = p.wait_for("permission request", &|v: &Value| v["request"]["subtype"] == "can_use_tool");
    let allow = json!({ "behavior": "allow", "updatedPermissions": [{ "type": "setMode", "mode": "acceptEdits", "destination": "session" }] });
    p.send(json!({ "type": "control_response", "response": { "subtype": "success", "request_id": ask["request_id"], "response": allow } }));
    let first = p.reply("edit");
    assert!(first.contains("resumed=none fresh=none model=m2 mode=acceptEdits inits=1"), "{first}");

    // Idle for longer than CCNAP_IDLE (keep-alives don't count): claude is stopped.
    thread::sleep(Duration::from_secs(5));
    p.send(json!({ "type": "user", "message": { "role": "user", "content": "again" } }));
    let second = p.reply("again");
    // A new process, resumed, set up like the first and in the mode the first was left in.
    assert!(second.contains("resumed=sess-1 fresh=none model=m2 mode=acceptEdits inits=1"), "{second}");
    assert_ne!(first.rsplit("pid=").next(), second.rsplit("pid=").next());
    let seen = p.lines.lock().unwrap().clone();
    assert_eq!(seen.iter().filter(|v| answered("i1")(v)).count(), 1, "initialize answered twice");
    p.close();
}

#[test]
fn panel_naps_an_empty_tab_and_brings_it_back_fresh() {
    // A new chat tab: set up, but nothing sent yet, so there's no transcript to resume.
    let mut p = Panel::start("empty", "2s");
    p.send(json!({ "type": "control_request", "request_id": "i1", "request": { "subtype": "initialize" } }));
    p.wait_for("initialize answer", &answered("i1"));
    thread::sleep(Duration::from_secs(5));
    p.send(json!({ "type": "user", "message": { "role": "user", "content": "hello" } }));
    let reply = p.reply("hello");
    // A new claude under the same session id, set up like the first.
    assert!(reply.contains("resumed=none fresh=sess-1 model=m1 mode=default inits=1"), "{reply}");
    p.close();
}

#[test]
fn terminal_naps_an_empty_tab_and_brings_it_back_fresh() {
    let t = Term::start(&[("CCNAP_IDLE", "2s"), ("FAKE_EMPTY", "1")]);
    t.wait_for("FAKE READY resume=none fresh=none", 1);
    t.send(b"\x1b[O"); // never typed in, and left behind
    t.wait_for("[ccnap] napping", 1);
    t.send(b"\x1b[I");
    t.wait_for("FAKE READY resume=none fresh=sess-1", 1);
    t.quit();
}

/// An empty folder of this test's own.
fn scratch(name: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!("ccnap-e2e-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn copy_ccnap(dir: &Path) -> PathBuf {
    let exe = dir.join(format!("ccnap{}", env::consts::EXE_SUFFIX));
    fs::copy(env!("CARGO_BIN_EXE_ccnap"), &exe).unwrap();
    exe
}

/// Installs a "newer" ccnap over `exe` while it runs, the way `ccnap setup` does.
fn update(exe: &Path) {
    fs::rename(exe, exe.with_extension("old")).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_ccnap"), exe).unwrap();
    fs::File::options().write(true).open(exe).unwrap().set_modified(SystemTime::now()).unwrap();
}

fn wait_exit(child: &mut std::process::Child, what: &str) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return s;
        }
        assert!(start.elapsed() < Duration::from_secs(15), "{what} did not exit");
        thread::sleep(Duration::from_millis(50));
    }
}

fn fake_claude() -> PathBuf {
    // Cargo builds examples into target/<profile>/examples, next to the tests' deps/.
    let mut dir = env::current_exe().unwrap();
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    dir.join("examples").join(format!("fake_claude{}", env::consts::EXE_SUFFIX))
}

/// ccnap running in a pty around the fake claude, with a terminal on the other end.
struct Term {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    screen: Arc<Mutex<String>>,
    keys: Arc<Mutex<Box<dyn Write + Send>>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Term {
    fn start(vars: &[(&str, &str)]) -> Term {
        Term::run(Path::new(env!("CARGO_BIN_EXE_ccnap")), &[], vars)
    }

    /// `program args`, in a pty with the fake claude set up.
    fn run(program: &Path, args: &[&str], vars: &[(&str, &str)]) -> Term {
        let pair = native_pty_system().openpty(PtySize { rows: 30, cols: 120, pixel_width: 0, pixel_height: 0 }).unwrap();
        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        cmd.cwd(env::temp_dir());
        cmd.env("CCNAP_CLAUDE", fake_claude());
        cmd.env("CCNAP_HOME", env::temp_dir().join("ccnap-e2e-home"));
        cmd.env_remove("CCNAP_STATE");
        for (k, v) in vars {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).unwrap();
        let screen = Arc::new(Mutex::new(String::new()));
        let keys = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (sink, terminal) = (screen.clone(), keys.clone());
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                // ConPTY asks for the cursor position and waits; answer like a real terminal.
                if buf[..n].windows(4).any(|w| w == b"\x1b[6n") {
                    let _ = terminal.lock().unwrap().write_all(b"\x1b[1;1R");
                }
                sink.lock().unwrap().push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        Term { child, screen, keys, _master: pair.master }
    }

    fn send(&self, b: &[u8]) {
        let mut keys = self.keys.lock().unwrap();
        keys.write_all(b).unwrap();
        keys.flush().unwrap();
    }

    /// Pastes like a terminal does: in brackets only while the program has bracketed paste on.
    fn paste(&self, text: &str) {
        let on = {
            let s = self.screen.lock().unwrap();
            s.rfind("\x1b[?2004h") > s.rfind("\x1b[?2004l")
        };
        self.send(if on { format!("\x1b[200~{text}\x1b[201~") } else { text.into() }.as_bytes());
    }

    fn count(&self, needle: &str) -> usize {
        self.screen.lock().unwrap().matches(needle).count()
    }

    /// Waits until `needle` has appeared `n` times.
    fn wait_for(&self, needle: &str, n: usize) {
        let start = Instant::now();
        while self.count(needle) < n {
            assert!(start.elapsed() < Duration::from_secs(20), "no {needle:?} x{n} in:\n{}", self.screen.lock().unwrap());
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn quit(mut self) {
        self.send(b"q\r");
        let start = Instant::now();
        let status = loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                break s;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "ccnap did not exit");
            thread::sleep(Duration::from_millis(50));
        };
        assert!(status.success(), "exit code {}", status.exit_code());
        assert_eq!(self.count("\\u{1b}[I") + self.count("\\u{1b}[O"), 0, "focus reports leaked through to claude");
    }
}

#[test]
fn terminal_naps_when_idle_and_unfocused_and_wakes_on_focus_or_typing() {
    let t = Term::start(&[("CCNAP_IDLE", "2s")]);
    t.wait_for("FAKE READY resume=none", 1);
    t.send(b"\x1b[I"); // the tab has focus: never nap
    thread::sleep(Duration::from_secs(4));
    assert_eq!(t.count("[ccnap] napping"), 0, "napped while focused");
    t.send(b"\x1b[O"); // the user switched to another tab
    t.wait_for("[ccnap] napping", 1);

    // `ccnap status` sees the napping session.
    let start = Instant::now();
    loop {
        let out = Command::new(env!("CARGO_BIN_EXE_ccnap")).arg("status").env("CCNAP_HOME", env::temp_dir().join("ccnap-e2e-home")).output().unwrap();
        let out = String::from_utf8_lossy(&out.stdout).into_owned();
        if out.contains("napping ") && out.contains("terminal") {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "status:\n{out}");
        thread::sleep(Duration::from_millis(200));
    }

    // Back on the tab, typing a moment later: the keys wait until the new claude can take them.
    t.send(b"\x1b[I");
    thread::sleep(Duration::from_millis(200));
    t.send(b"hi\r");
    t.wait_for("FAKE READY resume=sess-1", 1);
    t.wait_for("SUBMIT \"hi\"", 1);
    t.send(b"\x1b[O");
    t.wait_for("[ccnap] napping", 2);
    // Pasting wakes it too, and two pasted lines arrive as one paste, not as two prompts.
    t.paste("a\rb");
    t.send(b"\r");
    t.wait_for("FAKE READY resume=sess-1", 2);
    t.wait_for("SUBMIT \"a\\nb\"", 1);
    assert_eq!(t.count("SUBMIT \"a\""), 0, "the paste was typed in line by line");
    t.quit();
}

#[test]
fn terminal_restarts_a_leaking_claude_even_in_focus_but_never_loops() {
    // Every claude grows past the limit 3s after it starts, the restarted one too.
    let t = Term::start(&[
        ("CCNAP_IDLE", "2s"),
        ("CCNAP_MEMORY_LIMIT", "40MB"),
        ("FAKE_BLOAT_MB", "120"),
        ("FAKE_BLOAT_AFTER_MS", "3000"),
    ]);
    t.wait_for("FAKE READY resume=none", 1);
    t.send(b"\x1b[I"); // in focus, so it never naps; the leak still gets handled
    t.wait_for("had grown to", 1);
    t.wait_for("FAKE READY resume=sess-1", 1);
    // A restart didn't help, so it isn't tried again and again.
    thread::sleep(Duration::from_secs(10));
    assert_eq!(t.count("had grown to"), 1);
    assert_eq!(t.count("[ccnap] napping"), 0);
    t.quit();
}

/// `ccnap <args>`: what it prints.
fn ccnap(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_ccnap")).args(args).env("CCNAP_HOME", env::temp_dir().join("ccnap-e2e-home")).output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn menu_actions_nap_wake_and_restart_a_terminal_session() {
    // A long idle time and the tab in focus: nothing naps unless asked to.
    let dir = scratch("menu-terminal");
    let log = dir.join("ccnap.log");
    let t = Term::start(&[("CCNAP_IDLE", "1h"), ("CCNAP_LOG", log.to_str().unwrap())]);
    t.wait_for("FAKE READY resume=none", 1);
    t.send(b"\x1b[I");
    let pid = t.child.process_id().unwrap().to_string();
    // Newer Windows consoles drop keys that arrive in the same instant as a focus report; a
    // person never types that fast.
    thread::sleep(Duration::from_millis(300));
    t.send(b"abc");
    // Ask only once ccnap has seen the text, which a slow machine takes a while to pass on.
    let start = Instant::now();
    loop {
        let status = ccnap(&["status"]);
        if status.contains("unsent text") {
            break;
        }
        if start.elapsed() > Duration::from_secs(15) {
            let screen = t.screen.lock().unwrap().clone();
            panic!("the typed text never showed in the status:\n{status}\nlog:\n{}\nscreen:\n{screen:?}", fs::read_to_string(&log).unwrap_or_default());
        }
        thread::sleep(Duration::from_millis(200));
    }
    let refused = ccnap(&["nap", &pid]);
    assert!(refused.contains("not now: unsent text in the prompt"), "{refused}");
    t.send(b"\r");
    t.wait_for("SUBMIT \"abc\"", 1);
    let napped = ccnap(&["nap", &pid]);
    assert!(napped.contains("napped"), "{napped}");
    t.wait_for("[ccnap] napping", 1);
    assert!(ccnap(&["wake", &pid]).contains("woke"));
    t.wait_for("FAKE READY resume=sess-1", 1);
    assert!(ccnap(&["restart", &pid]).contains("restarted"));
    t.wait_for("FAKE READY resume=sess-1", 2);
    t.quit();
}

#[test]
fn menu_actions_nap_and_wake_a_panel_session() {
    let mut p = Panel::start("menu", "1h");
    p.send(json!({ "type": "control_request", "request_id": "i1", "request": { "subtype": "initialize" } }));
    p.wait_for("initialize answer", &answered("i1"));
    p.send(json!({ "type": "user", "message": { "role": "user", "content": "hello" } }));
    let first = p.reply("hello");
    let pid = p.ccnap.id().to_string();
    thread::sleep(Duration::from_millis(1500));
    assert!(ccnap(&["nap", &pid]).contains("napped"));
    assert!(ccnap(&["wake", &pid]).contains("woke"));
    // Woken ahead of the next message, and set up as before.
    p.send(json!({ "type": "user", "message": { "role": "user", "content": "again" } }));
    let second = p.reply("again");
    assert!(second.contains("resumed=sess-1") && second.contains("inits=1"), "{second}");
    assert_ne!(first.rsplit("pid=").next(), second.rsplit("pid=").next());
    p.close();
}

#[test]
fn panel_hands_a_napping_session_to_a_newer_ccnap() {
    let mut p = Panel::start_from("update", "2s", true);
    p.send(json!({ "type": "control_request", "request_id": "i1", "request": { "subtype": "initialize" } }));
    p.wait_for("initialize answer", &answered("i1"));
    p.send(json!({ "type": "user", "message": { "role": "user", "content": "hello" } }));
    p.reply("hello");
    thread::sleep(Duration::from_secs(5)); // napping
    update(&p.dir.join(format!("ccnap{}", env::consts::EXE_SUFFIX)));
    // It ends the way a session ends, so the editor starts the next message through the new one.
    let status = wait_exit(&mut p.ccnap, "the older ccnap");
    assert!(status.success(), "{status}");
    let log = fs::read_to_string(p.dir.join("ccnap.log")).unwrap();
    assert!(log.contains("handing the session over"), "{log}");
    p.close();
}

#[test]
fn terminal_moves_to_a_newer_ccnap_when_woken_keeping_what_was_typed() {
    let dir = scratch("handover");
    let exe = copy_ccnap(&dir);
    let log = dir.join("ccnap.log");
    let t = Term::run(&exe, &[], &[("CCNAP_IDLE", "2s"), ("CCNAP_LOG", log.to_str().unwrap())]);
    t.wait_for("FAKE READY resume=none", 1);
    t.send(b"\x1b[O");
    t.wait_for("[ccnap] napping", 1);
    update(&exe);
    t.send(b"hi\r"); // typing wakes it: the newer ccnap takes over and gets the keys
    t.wait_for("FAKE READY resume=sess-1", 1);
    t.wait_for("SUBMIT \"hi\"", 1);
    let text = fs::read_to_string(&log).unwrap();
    assert!(text.contains("handing session"), "{text}");
    t.quit();
    let _ = fs::remove_dir_all(dir);
}

/// Types into a terminal from outside it, the way a terminal session moves into ccnap.
#[cfg(windows)]
#[test]
fn types_into_another_terminal() {
    let t = Term::run(Path::new("cmd.exe"), &["/q", "/k", "prompt $g"], &[]);
    thread::sleep(Duration::from_millis(1500));
    let pid = t.child.process_id().unwrap().to_string();
    let typed = Command::new(env!("CARGO_BIN_EXE_ccnap")).args(["__type", &pid, "echo typed-%OS%"]).status().unwrap();
    assert!(typed.success(), "{typed}");
    t.wait_for("typed-Windows_NT", 1);
    t.send(b"exit\r");
}

/// A Claude panel session started before ccnap: ended the way a session ends, once idle.
#[cfg(windows)]
#[test]
fn moves_an_idle_panel_session_into_ccnap() {
    let dir = scratch("adopt");
    let claude = dir.join("claude.exe");
    fs::copy(fake_claude(), &claude).unwrap();
    let now = || SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    fs::write(dir.join("settings.json"), "{}").unwrap();
    let started = now();
    let mut running = Command::new(&claude)
        .args(["--output-format", "stream-json", "--verbose", "--input-format", "stream-json", "--settings"])
        .arg(dir.join("settings.json"))
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    // Claude's record of it, in a Claude home of this test's own; and ccnap's status files in a
    // temp folder of its own, so no real session is touched.
    let record = |status: &str, at: u64| {
        json!({ "pid": running.id(), "sessionId": "s1", "cwd": dir, "startedAt": started, "kind": "interactive",
                "entrypoint": "claude-vscode", "status": status, "statusUpdatedAt": at })
    };
    fs::create_dir_all(dir.join("claude").join("sessions")).unwrap();
    let write = |r: Value| fs::write(dir.join("claude").join("sessions").join(format!("{}.json", running.id())), r.to_string()).unwrap();
    let adopt = || {
        let out = Command::new(env!("CARGO_BIN_EXE_ccnap"))
            .arg("adopt")
            .env("CLAUDE_CONFIG_DIR", dir.join("claude"))
            .env("TMP", &dir)
            .env("TEMP", &dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    write(record("busy", now()));
    assert!(adopt().contains("not now: claude is working"));
    write(record("idle", now()));
    assert!(adopt().contains("not now: used in the last 2 minutes"));
    write(record("idle", now() - 5 * 60_000));
    let out = adopt();
    assert!(out.contains("moved"), "{out}");
    let status = wait_exit(&mut running, "the claude");
    assert_eq!(status.code(), Some(0), "the editor would report a crash");
    let _ = fs::remove_dir_all(dir);
}
