//! hibernaut stands between Claude Code and whatever runs it. Once a session has been idle for a
//! while, it stops Claude, which frees all of its memory, MCP servers included, and brings the
//! session back with `claude --resume` the moment you return. Two ways in:
//! - a terminal: hibernaut runs claude in a pseudo-terminal it owns (tty.rs) and wakes it on focus
//!   or a keypress;
//! - an editor's chat panel: the extension runs hibernaut as its `claudeProcessWrapper`, and hibernaut
//!   relays the Agent SDK's JSON lines (pipe.rs), waking on the next message.
//!
//! Claude Code hooks, injected with --settings, say when a turn is really over. The tray icon
//! (tray.rs, its own `hibernaut-tray` program) shows every session and acts on them (control.rs).

mod adopt;
mod args;
mod config;
mod control;
mod hook;
mod install;
mod pipe;
mod proc;
mod status;
mod term;
mod tty;

#[cfg(any(windows, target_os = "macos"))]
pub mod tray;

#[cfg(not(any(windows, target_os = "macos")))]
pub mod tray {
    pub fn main() {
        eprintln!("hibernaut-tray: the tray icon is for Windows and macOS for now; use `hibernaut status`, `hibernaut nap` and `hibernaut wake`.");
    }
}

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

const HELP: &str = "hibernaut: puts idle Claude Code sessions to sleep to free memory, and wakes them when you return.

Use it in place of `claude` (any claude flags work), or run `hibernaut setup` once so every
terminal and editor chat panel goes through it.

  hibernaut setup       route terminals and VS Code-family editors through Hibernaut
  hibernaut uninstall   undo setup
  hibernaut status      list sessions: awake or napping, why, and the memory they hold or freed
  hibernaut nap <pid|all>      nap a session now (it refuses while Claude works or text is unsent)
  hibernaut wake <pid|all>     wake a napping session now
  hibernaut restart <pid>      restart a session to free leaked memory, conversation kept
  hibernaut adopt [--terminals]  move idle sessions that don't run through Hibernaut into it (Windows)
  hibernaut config      show where the settings file is, and what it says
  hibernaut version     print hibernaut's version (`hibernaut --version` prints Claude Code's)

The tray icon (hibernaut-tray, on Windows and macOS) does all of this from a menu.
";

/// The `hibernaut` program.
pub fn run() {
    let mut argv: Vec<OsString> = env::args_os().skip(1).collect();
    let state_env = env::var_os("HIBERNAUT_STATE");
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
    // Our own commands, unless we were started as the `claude` that `hibernaut setup` puts on PATH.
    if !install::invoked_as_claude() {
        let done = |r: io::Result<()>| -> ! { process::exit(report(r)) };
        match argv.first().and_then(|a| a.to_str()) {
            Some("setup") => done(install::setup()),
            Some("uninstall") => done(install::uninstall()),
            Some("status") => done(status::show()),
            Some("config") => done(show_config()),
            Some(cmd @ ("nap" | "wake" | "restart")) => {
                let target = argv.get(1).and_then(|a| a.to_str()).unwrap_or("");
                done(control::cli(control::Action::parse(cmd).unwrap(), target))
            }
            Some("adopt") => done(adopt::cli(argv.iter().any(|a| a == "--terminals"))),
            #[cfg(windows)]
            Some("__type") => {
                let (Some(pid), Some(text)) = (argv.get(1).and_then(|a| a.to_str()?.parse().ok()), argv.get(2)) else { process::exit(2) };
                done(adopt::type_here(pid, &text.to_string_lossy()))
            }
            Some("version") => done(writeln!(io::stdout(), "hibernaut {}", env!("CARGO_PKG_VERSION"))),
            Some("help") => done(write!(io::stdout(), "{HELP}")),
            _ => {}
        }
    }
    // An editor's claudeProcessWrapper runs `hibernaut <path to its claude> <args>`.
    let claude = if argv.first().is_some_and(|a| is_claude_exe(Path::new(a))) {
        PathBuf::from(argv.remove(0))
    } else {
        find_claude().unwrap_or_else(|e| {
            eprintln!("hibernaut: {e}");
            process::exit(127)
        })
    };
    let mut toks = args::parse(&argv);
    // Already inside a hibernaut session (e.g. run from Claude's own shell).
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
            eprintln!("hibernaut: {e}");
            process::exit(1)
        }
    }
}

fn report(r: io::Result<()>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("hibernaut: {e}");
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
    println!("adopt:         {}", if c.adopt { "auto" } else { "off" });
    println!("log:           {}", c.log.map_or("off".into(), |p| p.display().to_string()));
    Ok(())
}

fn is_claude_exe(p: &Path) -> bool {
    p.is_file()
        && p.file_stem().is_some_and(|s| s.eq_ignore_ascii_case("claude"))
        && p.extension().is_none_or(|x| x.eq_ignore_ascii_case("exe"))
}

fn temp_dir() -> PathBuf {
    env::temp_dir().join("hibernaut")
}

/// After a restart for memory, another one may happen only once a sample taken at least this
/// long after it was below the limit: a restart that didn't help (the new claude soon grows past
/// the limit again) isn't tried over and over.
const REARM_AFTER_MS: u64 = 10 * 60_000;

/// Per-session temp files: the hook state, the --settings that installs the hooks, and the
/// status that `hibernaut status` reads.
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
        for f in [&self.state, &self.settings, &self.status, &control::request_path(process::id())] {
            let _ = fs::remove_file(f);
        }
    }
}

/// Where to resume: the session may have moved (into a worktree, say) since it started.
fn session_cwd(st: &hook::State, started_in: &Path) -> PathBuf {
    Some(PathBuf::from(&st.cwd)).filter(|p| p.is_dir()).unwrap_or_else(|| started_in.to_path_buf())
}

fn find_claude() -> Result<PathBuf, String> {
    if let Some(p) = env::var_os("HIBERNAUT_CLAUDE") {
        return Ok(p.into());
    }
    let me = env::current_exe().and_then(fs::canonicalize).ok();
    // `hibernaut setup` puts a `claude` that is really hibernaut in here.
    // A terminal opened before ccnap became hibernaut still has ccnap's on its PATH.
    let shims: Vec<PathBuf> = [Some(install::bin_dir()), install::legacy_home().map(|h| h.join("bin"))]
        .into_iter()
        .flatten()
        .filter_map(|d| fs::canonicalize(d).ok())
        .collect();
    let names: &[&str] = if cfg!(windows) { &["claude.exe", "claude.cmd"] } else { &["claude"] };
    for dir in env::split_paths(&env::var_os("PATH").unwrap_or_default()) {
        if fs::canonicalize(&dir).is_ok_and(|d| shims.contains(&d)) {
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
    Err("can't find `claude` on PATH; set HIBERNAUT_CLAUDE to its full path".into())
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
    eprintln!("hibernaut: {}: {e}", claude.display());
    127
}

#[cfg(windows)]
fn passthrough(claude: &Path, argv: &[OsString]) -> i32 {
    term::ignore_ctrl_c();
    match process::Command::new(claude).args(argv).status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("hibernaut: {}: {e}", claude.display());
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

/// A trail for "why didn't it nap?": `log` in the settings file, or HIBERNAUT_LOG.
pub(crate) fn log(msg: impl std::fmt::Display) {
    let Some(path) = config::get().log else { return };
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{} hibernaut[{}] {msg}", now_ms(), process::id());
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
        let dir = env::temp_dir().join(format!("hibernaut-exe-test-{}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        for (name, is) in [("CLAUDE.md", false), ("claude.json", false), ("claude", true), ("claude.exe", true)] {
            fs::write(dir.join(name), "").unwrap();
            assert_eq!(is_claude_exe(&dir.join(name)), is, "{name}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settings_survive_a_cleaned_temp_dir() {
        let dir = env::temp_dir().join(format!("hibernaut-files-test-{}", process::id()));
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
