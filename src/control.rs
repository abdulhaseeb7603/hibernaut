//! Acting on running sessions, from the tray or from `ccnap nap|wake|restart`. A request is a
//! small file the session picks up on its next check, within a second. The session decides
//! whether the action is safe, so nothing outside it can stop Claude mid-task, and records the
//! outcome in its status file for whoever asked.

use crate::{now_ms, status, temp_dir};
use serde_json::{json, Value};
use std::{fs, io, path::PathBuf, thread, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Nap,
    Wake,
    Restart,
}

impl Action {
    pub fn parse(s: &str) -> Option<Action> {
        match s {
            "nap" => Some(Action::Nap),
            "wake" => Some(Action::Wake),
            "restart" => Some(Action::Restart),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Action::Nap => "nap",
            Action::Wake => "wake",
            Action::Restart => "restart",
        }
    }
}

pub fn request_path(pid: u32) -> PathBuf {
    temp_dir().join(format!("{pid}.request"))
}

pub fn send(pid: u32, action: Action) -> io::Result<()> {
    fs::create_dir_all(temp_dir())?;
    fs::write(request_path(pid), action.name())
}

/// The request waiting for this session, taken so it runs once.
pub fn take() -> Option<Action> {
    let path = request_path(std::process::id());
    let text = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(&path);
    Action::parse(text.trim())
}

/// How a session reports what it did with a request, in its status file.
pub fn outcome(action: Action, result: &str, now: u64) -> Value {
    json!({ "action": action.name(), "result": result, "at": now })
}

/// `ccnap nap|wake|restart <pid|all>`: asks, then waits for the sessions to answer.
pub fn cli(action: Action, target: &str) -> io::Result<()> {
    let now = now_ms();
    let sessions = status::sessions(now);
    let napping = |v: &Value| v["napping"] == true;
    let pids: Vec<u32> = match target {
        "all" => sessions
            .iter()
            .filter(|v| if action == Action::Wake { napping(v) } else { !napping(v) })
            .filter_map(|v| v["pid"].as_u64().map(|p| p as u32))
            .collect(),
        pid => match pid.parse::<u32>() {
            Ok(pid) if sessions.iter().any(|v| v["pid"] == pid) => vec![pid],
            Ok(pid) => return Err(io::Error::other(format!("no ccnap session with pid {pid}; see `ccnap status`"))),
            Err(_) => return Err(io::Error::other(format!("usage: ccnap {} <pid|all>", action.name()))),
        },
    };
    if pids.is_empty() {
        println!("No session to {}.", action.name());
        return Ok(());
    }
    for &pid in &pids {
        send(pid, action)?;
    }
    for pid in pids {
        let answer = (0..50).find_map(|_| {
            let v = status::sessions(now_ms()).into_iter().find(|v| v["pid"] == pid)?;
            let done = &v["last_action"];
            let answered = done["at"].as_u64().is_some_and(|at| at >= now) && done["action"] == action.name();
            if !answered {
                thread::sleep(Duration::from_millis(100));
                return None;
            }
            Some(done["result"].as_str().unwrap_or("").to_string())
        });
        println!("{pid}: {}", answer.unwrap_or_else(|| "no answer (is it an older ccnap? see `ccnap status`)".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_taken_once() {
        send(std::process::id(), Action::Restart).unwrap();
        assert_eq!(take(), Some(Action::Restart));
        assert_eq!(take(), None);
        for a in [Action::Nap, Action::Wake, Action::Restart] {
            assert_eq!(Action::parse(a.name()), Some(a));
        }
    }
}
