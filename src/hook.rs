//! The hook half of ccnap. Claude Code runs `ccnap hook <state-file>` on session events (we
//! inject the hooks with --settings), and the wrapper reads that file to learn whether Claude
//! is in the middle of a turn.

use serde_json::{json, Value};
use std::{fs, io::Read, path::Path};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct State {
    /// "idle", "busy", or "bg" (the turn ended but background tasks or crons still run).
    pub state: String,
    pub session_id: String,
    pub cwd: String,
    pub transcript: String,
    pub mode: String,
    /// When `state` last changed, in unix ms.
    pub at: u64,
    /// When `mode` last changed, in unix ms.
    pub mode_at: u64,
    /// A prompt was sent in this session. Until then there's no transcript, and nothing to lose.
    pub prompted: bool,
}

/// How a napped session comes back.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Comeback {
    /// `--resume <id>`, from its transcript.
    Resume,
    /// Nothing was said yet: a new claude under the same `--session-id`.
    Fresh,
}

impl State {
    /// How this session can come back after a nap, or why it can't.
    pub fn comeback(&self) -> Result<Comeback, &'static str> {
        if self.session_id.is_empty() {
            Err("session not started yet")
        } else if Path::new(&self.transcript).is_file() {
            Ok(Comeback::Resume)
        } else if !self.prompted {
            Ok(Comeback::Fresh)
        } else {
            // Prompts but no transcript: saving is off, so a restart would lose the chat.
            Err("this chat isn't saved to disk")
        }
    }
}

pub fn read(path: &Path) -> Option<State> {
    let v: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    Some(State {
        state: s("state"),
        session_id: s("session_id"),
        cwd: s("cwd"),
        transcript: s("transcript"),
        mode: s("mode"),
        at: v["at"].as_u64().unwrap_or(0),
        mode_at: v["mode_at"].as_u64().unwrap_or(0),
        prompted: v["prompted"].as_bool().unwrap_or(false),
    })
}

/// `ccnap hook`. Never prints and never fails: stdout from some hooks becomes Claude's
/// context, and a broken hook must not get in the user's way.
pub fn run(path: &Path, now: u64) {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let Ok(input) = serde_json::from_str::<Value>(&input) else { return };
    let mut st = fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({}));
    if !apply(&mut st, &input, now) {
        return;
    }
    // Write-then-rename so the wrapper never reads half a file. A temp cleaner may have taken
    // the folder.
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    if fs::write(&tmp, st.to_string()).is_err() || fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

/// Folds one hook event into the state. Returns false for events we don't track.
pub fn apply(st: &mut Value, input: &Value, now: u64) -> bool {
    let prev = st["state"].as_str().unwrap_or_default().to_string();
    let running = |k: &str| input[k].as_array().is_some_and(|a| !a.is_empty());
    let next = match input["hook_event_name"].as_str().unwrap_or_default() {
        // Compaction can happen mid-turn, so it says nothing about being idle.
        "SessionStart" if input["source"] == "compact" => prev.clone(),
        "SessionStart" => "idle".into(),
        "UserPromptSubmit" => "busy".into(),
        "Stop" | "StopFailure" if running("background_tasks") || running("session_crons") => {
            "bg".into()
        }
        "Stop" | "StopFailure" => "idle".into(),
        // Registered for idle_prompt only: ~60s after a turn ends, and also after an Esc
        // interrupt, which skips Stop.
        "Notification" if prev != "bg" => "idle".into(),
        "Notification" => prev.clone(),
        _ => return false,
    };
    if !st.is_object() {
        *st = json!({});
    }
    if next != prev {
        st["at"] = json!(now);
    }
    st["state"] = json!(next);
    match input["hook_event_name"].as_str() {
        Some("UserPromptSubmit") => st["prompted"] = json!(true),
        // A new conversation (startup, /clear, a resume) starts out with nothing said in it.
        Some("SessionStart") if input["source"] != "compact" => st["prompted"] = json!(false),
        _ => {}
    }
    if input["permission_mode"].is_string() && input["permission_mode"] != st["mode"] {
        st["mode_at"] = json!(now);
    }
    for (key, src) in [
        ("session_id", "session_id"),
        ("cwd", "cwd"),
        ("transcript", "transcript_path"),
        ("mode", "permission_mode"),
    ] {
        if let Some(v) = input[src].as_str() {
            st[key] = json!(v);
        }
    }
    true
}

/// The --settings we pass to claude: the user's own --settings (if any) plus our hooks.
/// Claude Code merges hooks from every settings source, so the user's hooks keep running.
pub fn settings(exe: &Path, state: &Path, user: Option<Value>) -> Value {
    let handler = json!({
        "type": "command",
        "command": exe.to_string_lossy(),
        "args": ["hook", state.to_string_lossy()],
        "timeout": 10,
    });
    let mut s = match user {
        Some(v @ Value::Object(_)) => v,
        _ => json!({}),
    };
    if !s["hooks"].is_object() {
        s["hooks"] = json!({});
    }
    for (event, matcher) in [
        ("SessionStart", None),
        ("UserPromptSubmit", None),
        ("Stop", None),
        ("StopFailure", None),
        ("Notification", Some("idle_prompt")),
    ] {
        let mut group = json!({ "hooks": [handler.clone()] });
        if let Some(m) = matcher {
            group["matcher"] = json!(m);
        }
        let list = &mut s["hooks"][event];
        if !list.is_array() {
            *list = json!([]);
        }
        list.as_array_mut().unwrap().push(group);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(name: &str, extra: Value) -> Value {
        let mut v = json!({
            "hook_event_name": name, "session_id": "S", "cwd": "/w",
            "transcript_path": "/t.jsonl", "permission_mode": "plan",
        });
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    }

    #[test]
    fn follows_the_turn_lifecycle() {
        let mut st = json!({});
        let mut step = |name: &str, extra: Value, now: u64| {
            assert!(apply(&mut st, &ev(name, extra), now));
            (st["state"].as_str().unwrap().to_string(), st["at"].as_u64().unwrap())
        };
        assert_eq!(step("SessionStart", json!({"source": "startup"}), 1), ("idle".into(), 1));
        assert_eq!(step("UserPromptSubmit", json!({}), 2), ("busy".into(), 2));
        assert_eq!(step("SessionStart", json!({"source": "compact"}), 3), ("busy".into(), 2));
        let bg = json!({"background_tasks": [{"type": "shell"}], "session_crons": []});
        assert_eq!(step("Stop", bg, 4), ("bg".into(), 4));
        assert_eq!(step("Notification", json!({}), 5), ("bg".into(), 4));
        let done = json!({"background_tasks": [], "session_crons": []});
        assert_eq!(step("Stop", done, 6), ("idle".into(), 6));
        assert_eq!(step("UserPromptSubmit", json!({}), 7), ("busy".into(), 7));
        // Esc interrupt: no Stop, but idle_prompt arrives later.
        assert_eq!(step("Notification", json!({}), 8), ("idle".into(), 8));
        assert_eq!(st["session_id"], "S");
        assert_eq!(st["transcript"], "/t.jsonl");
        assert_eq!(st["mode"], "plan");
        assert!(!apply(&mut st, &ev("PreToolUse", json!({})), 9));
    }

    #[test]
    fn empty_sessions_come_back_fresh_and_saved_ones_resume() {
        let dir = std::env::temp_dir().join(format!("ccnap-hook-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join("t.jsonl");
        let at = |v: Value| {
            let mut e = ev("SessionStart", json!({ "transcript_path": transcript.to_string_lossy() }));
            for (k, x) in v.as_object().unwrap() {
                e[k] = x.clone();
            }
            e
        };
        let state = |st: &Value| {
            let p = dir.join("state.json");
            fs::write(&p, st.to_string()).unwrap();
            read(&p).unwrap()
        };
        let mut st = json!({});
        apply(&mut st, &at(json!({ "source": "startup" })), 1);
        assert_eq!(state(&st).comeback(), Ok(Comeback::Fresh), "a new tab nobody typed in");
        apply(&mut st, &at(json!({ "hook_event_name": "UserPromptSubmit" })), 2);
        assert_eq!(state(&st).comeback(), Err("this chat isn't saved to disk"));
        fs::write(&transcript, "{}").unwrap();
        assert_eq!(state(&st).comeback(), Ok(Comeback::Resume));
        fs::remove_file(&transcript).unwrap();
        apply(&mut st, &at(json!({ "source": "compact" })), 3);
        assert!(state(&st).prompted, "compaction keeps what was said");
        apply(&mut st, &at(json!({ "source": "clear" })), 4);
        assert_eq!(state(&st).comeback(), Ok(Comeback::Fresh), "after /clear");
        assert_eq!(State::default().comeback(), Err("session not started yet"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn notes_when_the_permission_mode_changed() {
        let mut st = json!({});
        apply(&mut st, &ev("SessionStart", json!({})), 1);
        apply(&mut st, &ev("UserPromptSubmit", json!({})), 2);
        assert_eq!((&st["mode"], &st["mode_at"]), (&json!("plan"), &json!(1)));
        apply(&mut st, &ev("Stop", json!({"permission_mode": "acceptEdits"})), 3);
        assert_eq!((&st["mode"], &st["mode_at"]), (&json!("acceptEdits"), &json!(3)));
    }

    #[test]
    fn settings_merge_into_user_settings() {
        let user = json!({"model": "opus", "hooks": {"Stop": [{"hooks": []}]}});
        let s = settings(Path::new("/bin/ccnap"), Path::new("/tmp/1.json"), Some(user));
        assert_eq!(s["model"], "opus");
        assert_eq!(s["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(s["hooks"]["Notification"][0]["matcher"], "idle_prompt");
        assert_eq!(s["hooks"]["SessionStart"][0]["hooks"][0]["args"][1], "/tmp/1.json");
    }
}
