//! Stand-in for `claude` in tests/e2e.rs. It fires the hooks hibernaut injected the way Claude Code
//! would. In a terminal it acts like Claude's prompt: input that arrives while it starts up is
//! lost, Enter sends the line (`SUBMIT "<line>"`) unless it comes inside a bracketed paste, and
//! `q` quits. With `--input-format stream-json` it speaks just enough of the Agent SDK protocol,
//! and reports what it was told (resumed session, model, permission mode, how many initialize
//! requests) in each reply.

use serde_json::{json, Value};
use std::{
    env, fs,
    io::{self, BufRead, Read, Write},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    // Both `--flag value` and `--flag=value`.
    let flag = |f: &str| {
        args.iter().enumerate().find_map(|(i, a)| match a.strip_prefix(f) {
            Some("") => args.get(i + 1).cloned(),
            Some(v) => v.strip_prefix('=').map(str::to_string),
            None => None,
        })
    };
    let settings = fs::read_to_string(flag("--settings").expect("--settings")).unwrap();
    let settings: Value = serde_json::from_str(&settings).unwrap();
    let resumed = flag("--resume");
    // `--session-id`: hibernaut bringing back a session nobody had typed in yet.
    let fresh = flag("--session-id");
    let session = resumed.clone().or(fresh.clone()).unwrap_or_else(|| "sess-1".into());
    let fresh = fresh.as_deref().unwrap_or("none");
    // FAKE_BLOAT_MB: grow to that much FAKE_BLOAT_AFTER_MS after every start, like a leaking claude.
    if let Ok(mb) = env::var("FAKE_BLOAT_MB") {
        let mb: usize = mb.parse().unwrap();
        let after = env::var("FAKE_BLOAT_AFTER_MS").map_or(0, |ms| ms.parse().unwrap());
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(after));
            let held = vec![1u8; mb << 20];
            loop {
                std::hint::black_box(&held);
                thread::park();
            }
        });
    }
    // Like Claude, the transcript exists once something was said (or the session is resumed).
    let transcript = env::temp_dir().join(format!("hibernaut-fake-{}.jsonl", std::process::id()));
    if resumed.is_some() {
        fs::write(&transcript, "{}\n").unwrap();
    }

    let fire = |event: &str, extra: Value| {
        if event == "UserPromptSubmit" {
            fs::write(&transcript, "{}\n").unwrap();
        }
        let mut input = json!({
            "hook_event_name": event, "session_id": session, "cwd": env::current_dir().unwrap(),
            "transcript_path": transcript, "permission_mode": "default",
        });
        for (k, v) in extra.as_object().unwrap() {
            input[k] = v.clone();
        }
        for group in settings["hooks"][event].as_array().into_iter().flatten() {
            for h in group["hooks"].as_array().unwrap() {
                let args = h["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap());
                let mut c = Command::new(h["command"].as_str().unwrap())
                    .args(args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .spawn()
                    .unwrap();
                c.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
                c.wait().unwrap();
            }
        }
    };
    fire("SessionStart", json!({ "source": if resumed.is_some() { "resume" } else { "startup" } }));

    if flag("--input-format").as_deref() == Some("stream-json") {
        let say = |v: Value| println!("{v}");
        // Claude Code sends these while idle; hibernaut must not count them as activity.
        thread::spawn(move || loop {
            say(json!({ "type": "keep_alive" }));
            thread::sleep(Duration::from_millis(300));
        });
        let (mut model, mut inits) = (flag("--model").unwrap_or_default(), 0);
        let mut mode = flag("--permission-mode").unwrap_or_else(|| "default".into());
        let reply = |text: &str, mode: &str, model: &str, inits: u32| {
            let text = format!(
                "echo:{text} resumed={} fresh={fresh} model={model} mode={mode} inits={inits} pid={}",
                resumed.as_deref().unwrap_or("none"),
                std::process::id()
            );
            say(json!({ "type": "assistant", "message": { "content": [{ "type": "text", "text": text }] } }));
            fire("Stop", json!({ "background_tasks": [], "session_crons": [], "permission_mode": mode }));
            say(json!({ "type": "result", "subtype": "success", "session_id": session }));
        };
        // A prompt waiting on the answer to our permission request.
        let mut asked: Option<String> = None;
        for line in io::stdin().lock().lines() {
            let msg: Value = serde_json::from_str(&line.unwrap()).unwrap();
            match msg["type"].as_str().unwrap() {
                "control_request" => {
                    match msg["request"]["subtype"].as_str().unwrap() {
                        "initialize" => inits += 1,
                        "set_model" => model = msg["request"]["model"].as_str().unwrap().into(),
                        "set_permission_mode" => mode = msg["request"]["mode"].as_str().unwrap().into(),
                        _ => {}
                    }
                    let id = &msg["request_id"];
                    say(json!({ "type": "control_response", "response": { "subtype": "success", "request_id": id, "response": {} } }));
                }
                // The answer may change the mode, as the extension's "allow all edits" does.
                "control_response" => {
                    let updates = msg["response"]["response"]["updatedPermissions"].as_array().cloned().unwrap_or_default();
                    for u in updates.iter().filter(|u| u["type"] == "setMode") {
                        mode = u["mode"].as_str().unwrap().into();
                    }
                    if let Some(text) = asked.take() {
                        reply(&text, &mode, &model, inits);
                    }
                }
                "user" => {
                    let text = msg["message"]["content"].as_str().unwrap().to_string();
                    fire("UserPromptSubmit", json!({ "permission_mode": mode }));
                    if text == "edit" {
                        let input = json!({ "file_path": "x", "content": "y" });
                        let request = json!({ "subtype": "can_use_tool", "tool_name": "Write", "input": input });
                        say(json!({ "type": "control_request", "request_id": "perm-1", "request": request }));
                        asked = Some(text);
                    } else {
                        reply(&text, &mode, &model, inits);
                    }
                }
                _ => {}
            }
        }
        let _ = fs::remove_file(&transcript);
        return;
    }

    // FAKE_EMPTY: a new tab nobody typed in, so no prompt and no transcript.
    if resumed.is_none() && env::var_os("FAKE_EMPTY").is_none() {
        fire("UserPromptSubmit", json!({ "prompt": "hi" }));
        fire("Stop", json!({ "background_tasks": [], "session_crons": [] }));
    }
    raw_mode();
    // Like Claude: bracketed paste on, and whatever was typed before the prompt is lost.
    print!("\x1b[?2004h");
    io::stdout().flush().unwrap();
    let (tx, typed) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; 256];
        while let Ok(n @ 1..) = io::stdin().read(&mut buf) {
            if tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    thread::sleep(Duration::from_millis(1000));
    while typed.try_recv().is_ok() {}
    print!("FAKE READY resume={} fresh={fresh}\r\n❯ ", resumed.as_deref().unwrap_or("none"));
    io::stdout().flush().unwrap();

    let (mut line, mut paste) = (Vec::new(), false);
    'read: for chunk in typed {
        let mut rest = &chunk[..];
        while let Some(&b) = rest.first() {
            if let Some(after) = rest.strip_prefix(b"\x1b[200~") {
                (paste, rest) = (true, after);
                continue;
            }
            if let Some(after) = rest.strip_prefix(b"\x1b[201~") {
                (paste, rest) = (false, after);
                continue;
            }
            rest = &rest[1..];
            match b {
                b'\r' if paste => line.push(b'\n'),
                b'\r' => {
                    let text = String::from_utf8_lossy(&line).into_owned();
                    print!("SUBMIT {text:?}\r\n");
                    io::stdout().flush().unwrap();
                    if text == "q" {
                        break 'read;
                    }
                    line.clear();
                }
                _ => line.push(b),
            }
        }
    }
    let _ = fs::remove_file(&transcript);
}

/// Keys as they are typed, the way Claude reads them.
#[cfg(unix)]
fn raw_mode() {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        libc::tcgetattr(0, &mut t);
        libc::cfmakeraw(&mut t);
        libc::tcsetattr(0, libc::TCSANOW, &t);
    }
}

#[cfg(windows)]
fn raw_mode() {
    use windows_sys::Win32::System::Console::*;
    unsafe { SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), ENABLE_VIRTUAL_TERMINAL_INPUT) };
}
