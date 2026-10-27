//! ccnap stands between Claude Code and whatever runs it. Once a session has been idle for a
//! while, it stops Claude, which frees all of its memory, MCP servers included, and brings the
//! session back with `claude --resume` the moment you return. Two ways in:
//! - a terminal: ccnap runs claude in a pseudo-terminal it owns (tty.rs) and wakes it on focus
//!   or a keypress;
//! - an editor's chat panel: the extension runs ccnap as its `claudeProcessWrapper`, and ccnap
//!   relays the Agent SDK's JSON lines (pipe.rs), waking on the next message.
//!
//! Claude Code hooks, injected with --settings, say when a turn is really over.

mod args;
mod config;
mod hook;
mod install;
mod pipe;
mod proc;
mod status;
mod term;
mod tty;

use serde_json::Value;
use std::{
    env,
    ffi::OsString,
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const HELP: &str = "ccnap: puts idle Claude Code sessions to sleep to free memory, and wakes them when you return.

Use it in place of `claude` (any claude flags work), or run `ccnap setup` once so every
terminal and editor chat panel goes through it.

  ccnap setup       route terminals and VS Code-family editors through ccnap
  ccnap uninstall   undo setup
  ccnap status      list sessions: awake or napping, why, and the memory they hold or freed
  ccnap config      show where the settings file is, and what it says
  ccnap version     print ccnap's version (`ccnap --version` prints Claude Code's)
";

fn main() {
    let mut argv: Vec<OsString> = env::args_os().skip(1).collect();
    let state_env = env::var_os("CCNAP_STATE");
    // Hooks get their input piped; `claude hook ...` typed in a terminal is a prompt.
    if argv.first().is_some_and(|a| a == "hook") && !io::stdin().is_terminal() {
        if let Some(path) = argv.get(1).cloned().or(state_env) {
            hook::run(Path::new(&path), now_ms());
        }
        return;
    }
    // Claude Code builds without exec-form hooks run the command without its arguments.
    if argv.is_empty() && !io::stdin().is_terminal() {
        if let Some(path) = &state_env {
            hook::run(Path::new(path), now_ms());
            return;
        }
    }
    // Our own commands, unless we were started as the `claude` that `ccnap setup` puts on PATH.
    if !install::invoked_as_claude() {
        let done = |r: io::Result<()>| -> ! { process::exit(report(r)) };
        match argv.first().and_then(|a| a.to_str()) {
            Some("setup") => done(install::setup()),
            Some("uninstall") => done(install::uninstall()),
            Some("status") => done(status::show()),
            Some("config") => done(show_config()),
            Some("version") => done(writeln!(io::stdout(), "ccnap {}", env!("CARGO_PKG_VERSION"))),
            Some("help") => done(write!(io::stdout(), "{HELP}")),
            _ => {}
        }
    }
    // An editor's claudeProcessWrapper runs `ccnap <path to its claude> <args>`.
    let claude = if argv.first().is_some_and(|a| is_claude_exe(Path::new(a))) {
        PathBuf::from(argv.remove(0))
    } else {
        find_claude().unwrap_or_else(|e| {
            eprintln!("ccnap: {e}");
            process::exit(127)
        })
    };
    let mut toks = args::parse(&argv);
    // Already inside a ccnap session (e.g. run from Claude's own shell).
    if state_env.is_some() {
        process::exit(passthrough(&claude, &argv));
    }
    let result = if args::stream_json(&toks) {
        let user_settings = args::take_settings(&mut toks);
        Files::new(user_settings).and_then(|files| pipe::run(claude, toks, files))
    } else if args::passthrough(&toks) || !term::is_tty() {
        process::exit(passthrough(&claude, &argv));
    } else {
        let user_settings = args::take_settings(&mut toks);
        Files::new(user_settings).and_then(|files| tty::Wrapper::new(claude, toks, files).run())
    };
    match result {
        Ok(code) => process::exit(code),
        Err(e) => {
            eprintln!("ccnap: {e}");
            process::exit(1)
        }
    }
}

fn report(r: io::Result<()>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("ccnap: {e}");
            1
        }
    }
}

fn show_config() -> io::Result<()> {
    let c = config::get();
    println!("settings file: {}", config::path().display());
    println!("idle:          {}s", c.idle_ms / 1000);
    let limit = if c.memory_limit == 0 { "off".into() } else { status::size(c.memory_limit) };
    println!("memory_limit:  {limit}");
    println!("exclude_dirs:  {:?}", c.exclude_dirs);
    println!("log:           {}", c.log.map_or("off".into(), |p| p.display().to_string()));
    Ok(())
}

fn is_claude_exe(p: &Path) -> bool {
    p.is_file()
        && p.file_stem().is_some_and(|s| s.eq_ignore_ascii_case("claude"))
        && p.extension().is_none_or(|x| x.eq_ignore_ascii_case("exe"))
}

fn temp_dir() -> PathBuf {
    env::temp_dir().join("ccnap")
}

/// After a restart for memory, another one may happen only once a sample taken at least this
/// long after it was below the limit: a restart that didn't help (the new claude soon grows past
/// the limit again) isn't tried over and over.
const REARM_AFTER_MS: u64 = 10 * 60_000;

/// Per-session temp files: the hook state, the --settings that installs the hooks, and the
/// status that `ccnap status` reads.
struct Files {
    state: PathBuf,
    settings: PathBuf,
    settings_json: String,
    status: PathBuf,
}

impl Files {
    fn new(user_settings: Option<OsString>) -> io::Result<Files> {
        let user = user_settings.map(|s| load_settings(&s)).transpose()?;
        let dir = temp_dir();
        fs::create_dir_all(&dir)?;
        // Sessions closed along with their terminal window never get to clean up.
        let stale = |e: &fs::DirEntry| {
            let age = e.metadata().and_then(|m| m.modified()).map(|t| t.elapsed().unwrap_or_default());
            age.is_ok_and(|age| age > Duration::from_secs(3 * 24 * 3600))
        };
        for e in fs::read_dir(&dir)?.flatten().filter(stale) {
            let _ = fs::remove_file(e.path());
        }
        let id = process::id();
        let state = dir.join(format!("{id}.state.json"));
        let _ = fs::remove_file(&state);
        Ok(Files {
            settings_json: hook::settings(&env::current_exe()?, &state, user).to_string(),
            settings: dir.join(format!("{id}.settings.json")),
            status: dir.join(format!("{id}.status.json")),
            state,
        })
    }

    /// `--settings <file>`, written fresh each time: temp cleaners may remove it during a nap.
    fn settings_arg(&self) -> io::Result<Vec<OsString>> {
        if let Some(dir) = self.settings.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&self.settings, &self.settings_json)?;
        Ok(vec!["--settings".into(), self.settings.clone().into_os_string()])
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        for f in [&self.state, &self.settings, &self.status] {
            let _ = fs::remove_file(f);
        }
    }
}

/// Where to resume: the session may have moved (into a worktree, say) since it started.
fn session_cwd(st: &hook::State, started_in: &Path) -> PathBuf {
    Some(PathBuf::from(&st.cwd)).filter(|p| p.is_dir()).unwrap_or_else(|| started_in.to_path_buf())
}

fn find_claude() -> Result<PathBuf, String> {
    if let Some(p) = env::var_os("CCNAP_CLAUDE") {
        return Ok(p.into());
    }
    let me = env::current_exe().and_then(fs::canonicalize).ok();
    // `ccnap setup` puts a `claude` that is really ccnap in here.
    let shims = fs::canonicalize(install::bin_dir()).ok();
    let names: &[&str] = if cfg!(windows) { &["claude.exe", "claude.cmd"] } else { &["claude"] };
    for dir in env::split_paths(&env::var_os("PATH").unwrap_or_default()) {
        if shims.is_some() && fs::canonicalize(&dir).ok() == shims {
            continue;
        }
        for name in names {
            let p = dir.join(name);
            if !p.is_file() || fs::canonicalize(&p).ok() == me {
                continue;
            }
            if name.ends_with(".cmd") {
                if let Some(exe) = npm_shim_target(&p) {
                    return Ok(exe);
                }
                continue;
            }
            return Ok(p);
        }
    }
    Err("can't find `claude` on PATH; set CCNAP_CLAUDE to its full path".into())
}

/// npm installs `claude.cmd`, which runs `"%dp0%\node_modules\...\claude.exe" %*`. A pty needs a
/// real executable, so look through the shim.
fn npm_shim_target(shim: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(shim).ok()?;
    let rel = text.split("\"%dp0%\\").nth(1)?.split('"').next()?;
    let exe = shim.parent()?.join(rel);
    (exe.file_stem()? == "claude" && exe.extension()? == "exe" && exe.is_file()).then_some(exe)
}

#[cfg(unix)]
fn passthrough(claude: &Path, argv: &[OsString]) -> i32 {
    use std::os::unix::process::CommandExt;
    let e = process::Command::new(claude).args(argv).exec();
    eprintln!("ccnap: {}: {e}", claude.display());
    127
}

#[cfg(windows)]
fn passthrough(claude: &Path, argv: &[OsString]) -> i32 {
    term::ignore_ctrl_c();
    match process::Command::new(claude).args(argv).status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("ccnap: {}: {e}", claude.display());
            127
        }
    }
}

/// The user's own --settings (inline JSON or a file), so our hooks can be merged into it.
fn load_settings(s: &OsString) -> io::Result<Value> {
    let text = s.to_string_lossy();
    let json = if text.trim_start().starts_with('{') { text.to_string() } else { fs::read_to_string(s)? };
    serde_json::from_str(&json).map_err(|e| io::Error::other(format!("--settings {text}: {e}")))
}

/// "10m", "90s", "1h", or plain seconds.
fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, unit) = match s.char_indices().last()? {
        (i, 'h') => (&s[..i], 3_600_000),
        (i, 'm') => (&s[..i], 60_000),
        (i, 's') => (&s[..i], 1_000),
        _ => (s, 1_000),
    };
    num.trim().parse::<u64>().ok().filter(|&n| n > 0).map(|n| n * unit)
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// A trail for "why didn't it nap?": `log` in the settings file, or CCNAP_LOG.
fn log(msg: impl std::fmt::Display) {
    let Some(path) = config::get().log else { return };
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{} ccnap[{}] {msg}", now_ms(), process::id());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("10m"), Some(600_000));
        assert_eq!(parse_duration("90"), Some(90_000));
        assert_eq!(parse_duration("2s"), Some(2_000));
        assert_eq!(parse_duration("soon"), None);
    }

    #[test]
    fn only_executables_count_as_claude() {
        let dir = env::temp_dir().join(format!("ccnap-exe-test-{}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        for (name, is) in [("CLAUDE.md", false), ("claude.json", false), ("claude", true), ("claude.exe", true)] {
            fs::write(dir.join(name), "").unwrap();
            assert_eq!(is_claude_exe(&dir.join(name)), is, "{name}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settings_survive_a_cleaned_temp_dir() {
        let dir = env::temp_dir().join(format!("ccnap-files-test-{}", process::id()));
        let _ = fs::remove_dir_all(&dir);
        let files = Files {
            state: dir.join("1.state.json"),
            settings: dir.join("1.settings.json"),
            settings_json: "{}".into(),
            status: dir.join("1.status.json"),
        };
        files.settings_arg().unwrap();
        drop(files);
        fs::remove_dir_all(&dir).unwrap();
    }
}
