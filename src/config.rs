//! Settings: `config.toml` in hibernaut's home, overridden by HIBERNAUT_* environment variables. The
//! file is read again whenever it changes, so edits apply to sessions that are already running.

use crate::{install, parse_duration};
use serde_json::Value;
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    sync::RwLock,
    time::SystemTime,
};

pub const DEFAULT_IDLE_MS: u64 = 10 * 60_000;
pub const DEFAULT_MEMORY_LIMIT: u64 = 2 << 30;

pub const TEMPLATE: &str = r#"# hibernaut settings. Edits apply to running sessions within a few seconds.

# How long a session sits untouched before it naps: "90s", "10m", "1h".
idle = "10m"

# Claude Code has had memory leaks. A session that grows past this is restarted at its next
# quiet moment, conversation intact. "off" to disable.
memory_limit = "2GB"

# Sessions working in these folders never nap or restart.
exclude_dirs = []

# Claude sessions that don't run through Hibernaut yet (started before it was installed, or on an
# older hibernaut) move into it once idle, in editor chat panels: "auto", or "off" to move them
# only from the tray menu.
adopt = "auto"

# Log every decision here, to see why a session stayed awake.
# log = "~/hibernaut.log"
"#;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub idle_ms: u64,
    /// Restart a session at its next quiet moment once it holds this much; 0 never does.
    pub memory_limit: u64,
    pub exclude_dirs: Vec<PathBuf>,
    pub log: Option<PathBuf>,
    /// Move idle sessions that don't run through Hibernaut into it, without being asked.
    pub adopt: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { idle_ms: DEFAULT_IDLE_MS, memory_limit: DEFAULT_MEMORY_LIMIT, exclude_dirs: Vec::new(), log: None, adopt: true }
    }
}

impl Config {
    pub fn excludes(&self, dir: &Path) -> bool {
        let dir = norm(dir);
        self.exclude_dirs.iter().any(|x| {
            let x = norm(x);
            dir == x || dir.starts_with(&format!("{x}/"))
        })
    }
}

/// A folder as compared for exclusion: forward slashes, no trailing one, any case on Windows.
fn norm(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/").trim_end_matches('/').to_string();
    if cfg!(windows) { s.to_lowercase() } else { s }
}

pub fn path() -> PathBuf {
    install::home().join("config.toml")
}

/// Sets `key = value` (a TOML value) in the settings file, keeping everything else in it.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))] // used by the tray
pub fn set(key: &str, value: &str) -> io::Result<()> {
    let path = path();
    let text = fs::read_to_string(&path).unwrap_or_else(|_| TEMPLATE.to_string());
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&path, set_in(&text, key, value))
}

/// Puts `dir` on the settings' exclude_dirs list, or takes it off.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))] // used by the tray
pub fn set_excluded(dir: &Path, excluded: bool) -> io::Result<()> {
    let mut dirs = fs::read_to_string(path()).map(|t| parse(&t).exclude_dirs).unwrap_or_default();
    dirs.retain(|d| norm(d) != norm(dir));
    if excluded {
        dirs.push(dir.to_path_buf());
    }
    let items: Vec<String> = dirs.iter().map(|d| toml_string(&d.to_string_lossy())).collect();
    set("exclude_dirs", &format!("[{}]", items.join(", ")))
}

/// `s` as a TOML string: a literal one ('…', no escapes, which suits Windows paths) when it can be.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))] // used by the tray
pub fn toml_string(s: &str) -> String {
    if s.contains(['\'', '\n']) { serde_json::to_string(s).unwrap() } else { format!("'{s}'") }
}

/// `text` with `key` set to `value`, or with the line added if the key isn't there. An array
/// value may run over several lines, up to its `]`.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))] // used by the tray
fn set_in(text: &str, key: &str, value: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let value_of = |l: &str| strip_comment(l).split_once('=').map(|(k, v)| (k.trim().to_string(), v.trim().to_string()));
    let line = format!("{key} = {value}");
    let Some(start) = lines.iter().position(|l| value_of(l).is_some_and(|(k, _)| k == key)) else {
        let text = text.trim_end();
        return if text.is_empty() { format!("{line}\n") } else { format!("{text}\n{line}\n") };
    };
    let mut end = start;
    let (_, mut v) = value_of(lines[start]).unwrap();
    if v.starts_with('[') {
        while !v.ends_with(']') && end + 1 < lines.len() {
            end += 1;
            v.push_str(strip_comment(lines[end]).trim());
        }
    }
    let mut out: Vec<&str> = lines[..start].to_vec();
    out.push(&line);
    out.extend_from_slice(&lines[end + 1..]);
    out.join("\n") + "\n"
}

static CURRENT: RwLock<Option<(Option<SystemTime>, Config)>> = RwLock::new(None);

/// The current settings, read again if the file changed since last time.
pub fn get() -> Config {
    let mtime = fs::metadata(path()).and_then(|m| m.modified()).ok();
    if let Some((seen, c)) = &*CURRENT.read().unwrap() {
        if *seen == mtime {
            return c.clone();
        }
    }
    let mut c = fs::read_to_string(path()).map(|t| parse(&t)).unwrap_or_default();
    for (key, var) in [("idle", "HIBERNAUT_IDLE"), ("memory_limit", "HIBERNAUT_MEMORY_LIMIT"), ("log", "HIBERNAUT_LOG")] {
        if let Ok(v) = env::var(var) {
            apply(&mut c, key, &Value::String(v));
        }
    }
    if let Some(dirs) = env::var_os("HIBERNAUT_EXCLUDE") {
        c.exclude_dirs = env::split_paths(&dirs).collect();
    }
    *CURRENT.write().unwrap() = Some((mtime, c.clone()));
    c
}

/// The part of TOML hibernaut needs: `key = value` lines whose value is a string, a number, or an
/// array of strings, which may run over several lines. Unknown keys and lines are skipped.
pub fn parse(text: &str) -> Config {
    let mut c = Config::default();
    let mut lines = text.lines().map(|l| strip_comment(l).trim());
    while let Some(line) = lines.next() {
        let Some((key, value)) = line.split_once('=') else { continue };
        let mut value = value.trim().to_string();
        if value.starts_with('[') {
            while !value.ends_with(']') {
                let Some(more) = lines.next() else { break };
                value.push_str(more);
            }
        }
        apply(&mut c, key.trim(), &toml_value(&value));
    }
    c
}

/// A string ("…" or '…'), a number, or an array of those, where a trailing comma is fine.
fn toml_value(s: &str) -> Value {
    let s = s.trim();
    if let Some(items) = s.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        let commas: Vec<usize> = unquoted(items).filter(|&(_, c)| c == ',').map(|(i, _)| i).collect();
        let mut start = 0;
        let mut out = Vec::new();
        for end in commas.into_iter().chain([items.len()]) {
            if !items[start..end].trim().is_empty() {
                out.push(toml_value(&items[start..end]));
            }
            start = end + 1;
        }
        return Value::Array(out);
    }
    let inner = || Value::String(s[1..s.len() - 1].into());
    match s.as_bytes() {
        [b'\'', .., b'\''] => inner(),
        // Escapes as in JSON; a lone backslash, as in "C:\work", is kept as it is.
        [b'"', .., b'"'] => serde_json::from_str(s).unwrap_or_else(|_| inner()),
        _ => serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.into())),
    }
}

fn strip_comment(line: &str) -> &str {
    unquoted(line).find(|&(_, c)| c == '#').map_or(line, |(i, _)| &line[..i])
}

/// The characters of `s` outside "…" and '…' strings, with their byte offsets.
fn unquoted(s: &str) -> impl Iterator<Item = (usize, char)> + '_ {
    let mut quote = None;
    s.char_indices().filter(move |&(_, c)| match quote {
        Some(q) => {
            if c == q {
                quote = None;
            }
            false
        }
        None if c == '"' || c == '\'' => {
            quote = Some(c);
            false
        }
        None => true,
    })
}

fn apply(c: &mut Config, key: &str, value: &Value) {
    let text = value.as_str().map(str::to_string).or_else(|| value.as_f64().map(|n| n.to_string()));
    match key {
        "idle" => {
            if let Some(ms) = text.and_then(|t| parse_duration(&t)) {
                c.idle_ms = ms;
            }
        }
        "idle_minutes" => {
            if let Some(n) = value.as_f64().filter(|n| *n > 0.0) {
                c.idle_ms = (n * 60_000.0) as u64;
            }
        }
        "memory_limit" => {
            if let Some(b) = text.and_then(|t| parse_size(&t)) {
                c.memory_limit = b;
            }
        }
        "exclude_dirs" => {
            c.exclude_dirs = value.as_array().into_iter().flatten().filter_map(Value::as_str).map(expand).collect();
        }
        "log" => c.log = text.filter(|t| !t.is_empty()).map(|t| expand(&t)),
        "adopt" => c.adopt = value.as_bool().unwrap_or_else(|| value.as_str() != Some("off")),
        _ => {}
    }
}

/// "2GB", "1500MB", "800M", "off", or plain megabytes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    if matches!(s.as_str(), "off" | "none" | "0") {
        return Some(0);
    }
    let s = s.trim_end_matches('b').trim_end_matches('i');
    let (num, unit) = match s.char_indices().last()? {
        (i, 'g') => (&s[..i], 1u64 << 30),
        (i, 'm') => (&s[..i], 1 << 20),
        (i, 'k') => (&s[..i], 1 << 10),
        _ => (s, 1 << 20),
    };
    let n: f64 = num.trim().parse().ok()?;
    (n > 0.0).then_some((n * unit as f64) as u64)
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix('~') {
        Some(rest) => install::user_home().join(rest.trim_start_matches(['/', '\\'])),
        None => PathBuf::from(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_template_and_overrides() {
        assert_eq!(parse(TEMPLATE), Config::default());
        let c = parse(
            "idle = \"90s\" # short\nmemory_limit = 1.5\nexclude_dirs = [\"~/secret\", \"/tmp/x\"]\nlog = /tmp/hibernaut.log\n[ignored]\nnonsense",
        );
        assert_eq!(c.idle_ms, 90_000);
        assert_eq!(c.memory_limit, 3 << 19);
        assert_eq!(c.exclude_dirs[1], PathBuf::from("/tmp/x"));
        assert!(c.exclude_dirs[0].ends_with("secret"));
        assert_eq!(c.log, Some(PathBuf::from("/tmp/hibernaut.log")));
        assert_eq!(parse("idle_minutes = 30").idle_ms, 30 * 60_000);
        assert_eq!(parse("memory_limit = \"off\"").memory_limit, 0);
    }

    #[test]
    fn reads_multi_line_arrays_and_literal_strings() {
        let c = parse("exclude_dirs = [\n  \"~/a\", # mine\n  'C:\\work',\n]\nlog = 'C:\\logs\\hibernaut.log'\nidle = \"5m\"\n");
        assert_eq!(c.exclude_dirs.len(), 2);
        assert_eq!(c.exclude_dirs[1], PathBuf::from(r"C:\work"));
        assert_eq!(c.log, Some(PathBuf::from(r"C:\logs\hibernaut.log")));
        assert_eq!(c.idle_ms, 5 * 60_000);
        assert_eq!(parse(r#"exclude_dirs = ["a", "b",]"#).exclude_dirs.len(), 2);
        // Not valid TOML, but the obvious way to write a Windows path.
        assert_eq!(parse(r#"exclude_dirs = ["C:\work"]"#).exclude_dirs, [PathBuf::from(r"C:\work")]);
    }

    #[test]
    fn sizes_and_exclusions() {
        assert_eq!(parse_size("2GB"), Some(2 << 30));
        assert_eq!(parse_size("1500mb"), Some(1500 << 20));
        assert_eq!(parse_size("512MiB"), Some(512 << 20));
        assert_eq!(parse_size("lots"), None);
        let c = Config { exclude_dirs: vec!["/work/secret/".into()], ..Config::default() };
        assert!(c.excludes(Path::new("/work/secret")));
        assert!(c.excludes(Path::new("/work/secret/app")));
        assert!(!c.excludes(Path::new("/work/secretive")));
    }

    #[test]
    fn edits_keep_the_rest_of_the_file() {
        let set = set_in(TEMPLATE, "idle", "\"30m\"");
        assert_eq!(parse(&set).idle_ms, 30 * 60_000);
        assert_eq!(set.lines().count(), TEMPLATE.lines().count());
        assert!(set.contains("# How long a session sits untouched"));
        // A multi-line array is replaced whole; a missing key is added; comments aren't keys.
        let text = "exclude_dirs = [\n  'C:\\a',\n  'C:\\b',\n]\n# log = \"x\"\nidle = \"1m\"\n";
        let set = set_in(text, "exclude_dirs", &format!("[{}]", toml_string(r"C:\Users\me\work")));
        assert_eq!(set, "exclude_dirs = ['C:\\Users\\me\\work']\n# log = \"x\"\nidle = \"1m\"\n");
        assert_eq!(parse(&set).exclude_dirs, [PathBuf::from(r"C:\Users\me\work")]);
        assert!(set_in(&set, "log", "'/tmp/l'").ends_with("idle = \"1m\"\nlog = '/tmp/l'\n"));
        assert_eq!(set_in("", "idle", "\"5m\""), "idle = \"5m\"\n");
        assert_eq!(parse(&format!("exclude_dirs = [{}]", toml_string("/it's"))).exclude_dirs, [PathBuf::from("/it's")]);
    }
}
