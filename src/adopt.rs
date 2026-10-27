//! Moving Claude Code sessions into Hibernaut that don't run through it yet: ones started before
//! hibernaut was installed, and ones still running an older hibernaut. Claude Code keeps a record of
//! each running session in `~/.claude/sessions/<pid>.json`: its id, its folder, how it was
//! started, and whether it is idle. So hibernaut knows which chat each process holds, and when
//! moving it loses nothing.
//!
//! - In an editor's chat panel: once the session has been idle for a while, its process is
//!   ended with exit code 0, the way a session ends normally. The extension closes it without a
//!   word, and the next message starts it again with `--resume`, through Hibernaut now. Unsent text
//!   lives in the panel, not in claude, so it stays.
//! - In a terminal (only when asked, since its prompt may hold unsent text): claude is ended and
//!   `hibernaut --resume <id>` is typed into the same terminal.
//!
//! Windows only for now: elsewhere another process's exit code can't be chosen, so the editor
//! would report a crash, and nothing can type into another terminal.

use crate::{install, now_ms, proc, status};
use serde_json::Value;
use std::{collections::HashSet, env, fs, io, path::PathBuf};

/// A session moves in only after being idle this long.
pub const QUIET_MS: u64 = 2 * 60_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShellKind {
    PowerShell,
    Cmd,
    Bash,
}

impl ShellKind {
    fn of(exe: &str) -> Option<ShellKind> {
        match exe.to_ascii_lowercase().as_str() {
            "powershell.exe" | "pwsh.exe" => Some(ShellKind::PowerShell),
            "cmd.exe" => Some(ShellKind::Cmd),
            "bash.exe" | "sh.exe" | "zsh.exe" => Some(ShellKind::Bash),
            _ => None,
        }
    }

    /// The command that resumes `session` through Hibernaut, as typed into this shell.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn resume(self, hibernaut: &str, session: &str) -> String {
        match self {
            ShellKind::PowerShell => format!("& '{}' --resume {session}", hibernaut.replace('\'', "''")),
            ShellKind::Cmd => format!("\"{hibernaut}\" --resume {session}"),
            ShellKind::Bash => format!("'{}' --resume {session}", hibernaut.replace('\\', "/")),
        }
    }
}

/// A session that could move into Hibernaut.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The process the editor or the terminal started: claude, or an older hibernaut.
    pub pid: u32,
    pub session: String,
    pub cwd: String,
    pub panel: bool,
    pub old_hibernaut: bool,
    pub memory: u64,
    /// Why it can't move right now, or None when it can.
    pub wait: Option<&'static str>,
    /// For a terminal session: the shell to type into.
    pub shell: Option<(u32, ShellKind)>,
}

impl Candidate {
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub fn ready(&self) -> bool {
        self.wait.is_none()
    }
}

/// Programs Claude's Bash and PowerShell tools run in. One still running under an idle claude
/// is a background task, which moving would kill.
const SHELLS: &[&str] = &["bash.exe", "sh.exe", "zsh.exe", "fish.exe", "powershell.exe", "pwsh.exe"];

fn claude_dir() -> PathBuf {
    env::var_os("CLAUDE_CONFIG_DIR").map_or_else(|| install::user_home().join(".claude"), PathBuf::from)
}

/// Sessions that don't run through this hibernaut. `sessions` are hibernaut's own status files.
pub fn candidates(sessions: &[Value]) -> Vec<Candidate> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let all = proc::list();
    let now = now_ms();
    let alive = |pid: u32| all.iter().any(|p| p.pid == pid);
    let u = |v: &Value, k: &str| v[k].as_u64().unwrap_or(0) as u32;
    // Claude's records, for processes that are still the ones they describe.
    let records: Vec<Value> = fs::read_dir(claude_dir().join("sessions"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_slice::<Value>(&fs::read(e.path()).ok()?).ok())
        .filter(|r| {
            let pid = u(r, "pid");
            let is_claude = all.iter().any(|p| p.pid == pid && proc::is_claude(&p.name));
            is_claude && same_process(pid, r["startedAt"].as_u64().unwrap_or(0))
        })
        .collect();
    let record = |pid: u32| records.iter().find(|r| u(r, "pid") == pid);
    let busy_shell = |claude: u32| {
        all.iter()
            .any(|p| p.ppid == claude && SHELLS.iter().any(|s| p.name.eq_ignore_ascii_case(s)))
            .then_some("a background task is running")
    };
    let ours: HashSet<u32> = sessions.iter().flat_map(|v| [u(v, "pid"), u(v, "claude_pid")]).collect();
    let mut out = Vec::new();

    // Older hibernaut sessions: its process is what the editor or the terminal started.
    for v in sessions.iter().filter(|v| status::outdated(v) && alive(u(v, "pid"))) {
        let claude = u(v, "claude_pid");
        let panel = v["mode"] == "panel";
        let wait = if v["napping"] == true {
            None
        } else {
            match record(claude) {
                Some(r) => idle(r, now).or_else(|| busy_shell(claude)),
                None => Some("claude is working"),
            }
        };
        out.push(Candidate {
            pid: u(v, "pid"),
            session: v["session"].as_str().unwrap_or("").into(),
            cwd: v["cwd"].as_str().unwrap_or("").into(),
            panel,
            old_hibernaut: true,
            memory: proc::tree_rss_in(&all, u(v, "pid")),
            wait,
            shell: if panel { None } else { shell_of(&all, u(v, "pid")) },
        });
    }

    // Sessions that never ran through Hibernaut.
    for r in records.iter().filter(|r| !ours.contains(&u(r, "pid"))) {
        let pid = u(r, "pid");
        // How it was started: an editor's chat panel, or `claude` in a terminal. Not an app
        // built on the Agent SDK: that's someone's program, not ours to restart.
        let panel = match (r["entrypoint"].as_str(), r["kind"].as_str()) {
            (Some("claude-vscode"), Some("interactive")) => true,
            (Some("cli"), Some("interactive")) => false,
            _ => continue,
        };
        let shell = if panel { None } else { shell_of(&all, pid) };
        let wait = idle(r, now)
            .or_else(|| busy_shell(pid))
            .or((!panel && shell.is_none()).then_some("its terminal isn't one Hibernaut can type into"));
        out.push(Candidate {
            pid,
            session: r["sessionId"].as_str().unwrap_or("").into(),
            cwd: r["cwd"].as_str().unwrap_or("").into(),
            panel,
            old_hibernaut: false,
            memory: proc::tree_rss_in(&all, pid),
            wait,
            shell,
        });
    }
    out.retain(|c| !c.session.is_empty());
    out
}

/// Whether claude's record says it's been idle long enough to move.
fn idle(record: &Value, now: u64) -> Option<&'static str> {
    if record["status"] != "idle" {
        Some("claude is working")
    } else if now.saturating_sub(record["statusUpdatedAt"].as_u64().unwrap_or(now)) < QUIET_MS {
        Some("used in the last 2 minutes")
    } else {
        None
    }
}

/// What started a claude that runs outside Hibernaut, and what ending it costs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Origin {
    pub label: &'static str,
    pub about: &'static str,
    pub cost: &'static str,
}

const PROGRAM: &str = "Ending it stops whatever that program is doing.";
pub const LEFTOVER: Origin = Origin {
    label: "Leftover Claude",
    about: "Likely left behind by a closed tab or window",
    cost: "If it's a chat you still use, anything Claude is doing in it stops.",
};

/// Tells apart, from Claude's record of `pid`, a program built on the Agent SDK (which hibernaut
/// never touches) from a chat it couldn't move in and a claude nobody accounts for.
pub fn origin(pid: u32) -> Origin {
    let record = fs::read(claude_dir().join("sessions").join(format!("{pid}.json")))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(|r| !cfg!(windows) || same_process(pid, r["startedAt"].as_u64().unwrap_or(0)));
    let Some(r) = record else { return LEFTOVER };
    match r["entrypoint"].as_str().unwrap_or("") {
        "sdk-py" => Origin { label: "Python program", about: "Started by a Python program using the Claude Agent SDK, not a chat", cost: PROGRAM },
        "sdk-ts" => Origin { label: "Node.js program", about: "Started by a Node.js program using the Claude Agent SDK, not a chat", cost: PROGRAM },
        "cli" | "claude-vscode" if r["kind"] == "interactive" => Origin {
            label: "Claude chat outside Hibernaut",
            about: "A Claude Code chat that doesn't run through Hibernaut",
            cost: "Its chat is saved, but anything Claude is doing in it stops.",
        },
        _ => Origin { label: "Other program", about: "Started by another program, or by `claude -p`", cost: PROGRAM },
    }
}

/// The record was written by this process, not by an earlier one with the same pid.
fn same_process(pid: u32, started_at: u64) -> bool {
    #[cfg(windows)]
    return proc::started_ms(pid).is_some_and(|t| t.abs_diff(started_at) < 30_000);
    #[cfg(not(windows))]
    {
        let _ = (pid, started_at);
        false
    }
}

/// The shell that started `pid`. PowerShell and bash run npm's `claude.cmd` through a cmd.exe
/// of their own, so look past that one.
fn shell_of(all: &[proc::Proc], pid: u32) -> Option<(u32, ShellKind)> {
    let find = |pid: u32| all.iter().find(|p| p.pid == pid);
    let parent = find(find(pid)?.ppid)?;
    let kind = ShellKind::of(&parent.name)?;
    if kind == ShellKind::Cmd {
        if let Some(gp) = find(parent.ppid).filter(|gp| ShellKind::of(&gp.name).is_some_and(|k| k != ShellKind::Cmd)) {
            return Some((gp.pid, ShellKind::of(&gp.name).unwrap()));
        }
    }
    Some((parent.pid, kind))
}

/// Moves one session into Hibernaut.
pub fn move_in(c: &Candidate) -> Result<(), String> {
    if let Some(why) = c.wait {
        return Err(format!("not now: {why}"));
    }
    #[cfg(windows)]
    {
        let tree = proc::tree(&proc::list(), c.pid);
        // Exit code 0, as when a session ends: the editor closes it quietly, ready to resume.
        if !proc::terminate(c.pid, 0) {
            return Err("couldn't end its process".into());
        }
        for &pid in &tree[1..] {
            proc::terminate(pid, 0); // its MCP servers
        }
        if let Some((shell, kind)) = c.shell {
            std::thread::sleep(std::time::Duration::from_millis(600)); // back at the shell's prompt
            let hibernaut = install::bin_dir().join("hibernaut.exe");
            let hibernaut = if hibernaut.is_file() { hibernaut } else { env::current_exe().map_err(|e| e.to_string())? };
            type_into(shell, &kind.resume(&hibernaut.to_string_lossy(), &c.session))?;
        }
        Ok(())
    }
    #[cfg(not(windows))]
    Err("moving sessions in is for Windows for now".into())
}

/// Types `text` and Enter into the terminal `shell` runs in. A process with a console can't
/// attach to another one, so a console-less helper (`hibernaut __type`) does the typing.
#[cfg(windows)]
fn type_into(shell: u32, text: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x8;
    let hibernaut = install::bin_dir().join("hibernaut.exe");
    let hibernaut = if hibernaut.is_file() { hibernaut } else { env::current_exe().map_err(|e| e.to_string())? };
    let done = std::process::Command::new(hibernaut)
        .args(["__type", &shell.to_string(), text])
        .creation_flags(DETACHED_PROCESS)
        .status()
        .map_err(|e| e.to_string())?;
    if done.success() { Ok(()) } else { Err("couldn't type into its terminal".into()) }
}

/// `hibernaut __type <pid> <text>`: types into the console `pid` is attached to.
#[cfg(windows)]
pub fn type_here(pid: u32, text: &str) -> io::Result<()> {
    use std::{fs::OpenOptions, os::windows::io::AsRawHandle};
    use windows_sys::Win32::{
        System::Console::{AttachConsole, FreeConsole, WriteConsoleInputW, INPUT_RECORD, INPUT_RECORD_0, KEY_EVENT, KEY_EVENT_RECORD, KEY_EVENT_RECORD_0, SHIFT_PRESSED},
        UI::Input::KeyboardAndMouse::{MapVirtualKeyW, VkKeyScanW, MAPVK_VK_TO_VSC, VK_RETURN},
    };
    unsafe {
        FreeConsole();
        if AttachConsole(pid) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let input = OpenOptions::new().read(true).write(true).open("CONIN$")?;
    let key = |unit: u16, vk: u16, shift: bool, down: bool| INPUT_RECORD {
        EventType: KEY_EVENT as u16,
        Event: INPUT_RECORD_0 {
            KeyEvent: KEY_EVENT_RECORD {
                bKeyDown: down as i32,
                wRepeatCount: 1,
                wVirtualKeyCode: vk,
                wVirtualScanCode: unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16,
                uChar: KEY_EVENT_RECORD_0 { UnicodeChar: unit },
                dwControlKeyState: if shift { SHIFT_PRESSED } else { 0 },
            },
        },
    };
    let mut records = Vec::new();
    for unit in text.encode_utf16() {
        let scan = unsafe { VkKeyScanW(unit) };
        let (vk, shift) = if scan == -1 { (0, false) } else { ((scan & 0xff) as u16, scan & 0x100 != 0) };
        records.push(key(unit, vk, shift, true));
        records.push(key(unit, vk, shift, false));
    }
    records.push(key(b'\r' as u16, VK_RETURN, false, true));
    records.push(key(b'\r' as u16, VK_RETURN, false, false));
    let mut written = 0;
    let ok = unsafe { WriteConsoleInputW(input.as_raw_handle() as _, records.as_ptr(), records.len() as u32, &mut written) };
    unsafe { FreeConsole() };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// `hibernaut adopt [--terminals]`: moves every idle session into Hibernaut that can move.
pub fn cli(terminals: bool) -> io::Result<()> {
    let found = candidates(&status::sessions(now_ms()));
    if found.is_empty() {
        println!("Every Claude Code session already runs through Hibernaut.");
        return Ok(());
    }
    for c in &found {
        let what = if c.old_hibernaut { "older version" } else { "not in Hibernaut" };
        let place = if c.panel { "panel" } else { "terminal" };
        let result = if !c.panel && !terminals {
            "skipped: a terminal session (add --terminals; unsent text in its prompt would be lost)".to_string()
        } else {
            match move_in(c) {
                Ok(()) if c.panel => "moved: your next message there resumes it through Hibernaut".into(),
                Ok(()) => "moved: restarted in its terminal through Hibernaut".into(),
                Err(e) => e,
            }
        };
        println!("{:>6}  {place:<8} {what:<12} {:>8}  {}  {result}", c.pid, status::size(c.memory), c.cwd);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn idle_long_enough() {
        let r = |status: &str, at: u64| json!({ "status": status, "statusUpdatedAt": at });
        assert_eq!(idle(&r("busy", 0), QUIET_MS * 2), Some("claude is working"));
        assert_eq!(idle(&r("idle", QUIET_MS), QUIET_MS + 1), Some("used in the last 2 minutes"));
        assert_eq!(idle(&r("idle", 0), QUIET_MS), None);
    }

    #[test]
    fn resume_commands_per_shell() {
        let exe = r"C:\Users\O'Neil\hibernaut\bin\hibernaut.exe";
        assert_eq!(ShellKind::PowerShell.resume(exe, "S"), r"& 'C:\Users\O''Neil\hibernaut\bin\hibernaut.exe' --resume S");
        assert_eq!(ShellKind::Cmd.resume(exe, "S"), r#""C:\Users\O'Neil\hibernaut\bin\hibernaut.exe" --resume S"#);
        assert_eq!(ShellKind::Bash.resume(r"C:\a b\hibernaut.exe", "S"), "'C:/a b/hibernaut.exe' --resume S");
        assert_eq!(ShellKind::of("PWSH.EXE"), Some(ShellKind::PowerShell));
        assert_eq!(ShellKind::of("node.exe"), None);
    }
}
