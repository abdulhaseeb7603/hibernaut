//! `hibernaut status`: every running session, awake or napping and why, with the memory it holds
//! or freed, plus claude processes that don't run through Hibernaut (left behind by closed editor
//! tabs, say). Each session keeps a small status file beside its other temp files.

use crate::{install, now_ms, proc, temp_dir};
use serde_json::Value;
use std::{collections::HashSet, env, fs, io, path::PathBuf, time::UNIX_EPOCH};

/// A session's status file, rewritten when anything changes and every few seconds, so
/// `hibernaut status` can tell running sessions from ones that died without cleaning up.
pub struct Status {
    path: PathBuf,
    last: Value,
    written: u64,
    /// Which hibernaut binary this session runs, and its file's timestamp when the session started:
    /// once `hibernaut setup` replaces the file, the session is running an older build.
    exe: String,
    build: u64,
}

const HEARTBEAT_MS: u64 = 5_000;

fn modified_ms(exe: &str) -> u64 {
    fs::metadata(exe).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64)
}

impl Status {
    pub fn new(path: PathBuf) -> Status {
        let exe = env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        Status { path, last: Value::Null, written: 0, build: modified_ms(&exe), exe }
    }

    /// A newer hibernaut was installed over the one this session runs.
    pub fn replaced(&self) -> bool {
        self.build != 0 && modified_ms(&self.exe) != self.build
    }

    /// The path this session's hibernaut was started from, which holds the newer one once replaced.
    pub fn exe(&self) -> &str {
        &self.exe
    }

    pub fn update(&mut self, now: u64, mut v: Value) {
        if v == self.last && now.saturating_sub(self.written) < HEARTBEAT_MS {
            return;
        }
        self.last = v.clone();
        self.written = now;
        v["updated"] = now.into();
        v["exe"] = self.exe.clone().into();
        v["build"] = self.build.into();
        if let Some(dir) = self.path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = fs::write(&self.path, v.to_string());
    }
}

/// The running sessions, from their status files.
pub fn sessions(now: u64) -> Vec<Value> {
    // Sessions still running ccnap, hibernaut's old name, report in a folder of that name.
    let legacy = env::temp_dir().join(crate::install::LEGACY);
    [(temp_dir(), false), (legacy, true)]
        .into_iter()
        .flat_map(|(dir, legacy)| {
            fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".status.json"))
                .filter_map(|e| serde_json::from_slice::<Value>(&fs::read(e.path()).ok()?).ok())
                .map(move |mut v| {
                    if legacy {
                        v["legacy"] = true.into();
                    }
                    v
                })
        })
        .filter(|v| now.saturating_sub(v["updated"].as_u64().unwrap_or(0)) < 4 * HEARTBEAT_MS)
        .collect()
}

/// A session started before its hibernaut binary was replaced, so it still runs the old code.
pub fn outdated(v: &Value) -> bool {
    if v["legacy"] == true {
        return true;
    }
    match (v["exe"].as_str(), v["build"].as_u64()) {
        (Some(exe), Some(build)) => modified_ms(exe) != build,
        _ => true, // from a build that didn't record itself, so older still
    }
}

/// How many running sessions use an older hibernaut than the one installed now.
pub fn outdated_count() -> usize {
    sessions(now_ms()).iter().filter(|v| outdated(v)).count()
}

pub const UPDATE_HINT: &str = if cfg!(windows) {
    "Panel sessions move to the new one by themselves once idle; for terminal sessions, use the tray icon or `hibernaut adopt --terminals`."
} else {
    "They update the next time they start: in your editor run \"Developer: Reload Window\", and restart terminal sessions."
};

pub fn show() -> io::Result<()> {
    let now = now_ms();
    let mut rows = sessions(now);
    rows.sort_by_key(|v| (v["napping"].as_bool(), v["since"].as_u64()));
    let u = |v: &Value, k: &str| v[k].as_u64().unwrap_or(0);
    let napping: Vec<&Value> = rows.iter().filter(|v| v["napping"] == true).collect();
    let awake_mem: u64 = rows.iter().filter(|v| v["napping"] != true).map(|v| u(v, "memory")).sum();
    let freed: u64 = napping.iter().map(|v| u(v, "freed")).sum();

    println!(
        "{} session{}: {} awake using {}, {} napping (freed {})",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" },
        rows.len() - napping.len(),
        size(awake_mem),
        napping.len(),
        size(freed)
    );
    if !rows.is_empty() {
        println!();
        println!("  {:<8} {:<9} {:<10} {:<6} {:<32} NOTE", "STATE", "WHERE", "MEMORY", "FOR", "FOLDER");
        for v in &rows {
            let nap = v["napping"] == true;
            let memory = if nap { format!("-{}", size(u(v, "freed"))) } else { size(u(v, "memory")) };
            println!(
                "  {:<8} {:<9} {:<10} {:<6} {:<32} {}",
                if nap { "napping" } else { "awake" },
                v["mode"].as_str().unwrap_or("?"),
                memory,
                age(now.saturating_sub(u(v, "since"))),
                short_dir(v["cwd"].as_str().unwrap_or("")),
                format!("{}{}", v["why"].as_str().unwrap_or(""), if outdated(v) { " [older version]" } else { "" }).trim()
            );
        }
        let old = rows.iter().filter(|v| outdated(v)).count();
        if old > 0 {
            println!();
            println!("{old} session{} still run an older version. {UPDATE_HINT}", if old == 1 { "" } else { "s" });
        }
    }

    let others = leftovers(&rows);
    if !others.is_empty() {
        let total: u64 = others.iter().map(|(_, m)| m).sum();
        let pids: Vec<String> = others.iter().map(|(p, m)| format!("{p} ({}, {})", size(*m), crate::adopt::origin(*p).label)).collect();
        println!();
        println!("{} claude process{} not running through Hibernaut, using {}: {}", others.len(), if others.len() == 1 { "" } else { "es" }, size(total), pids.join(", "));
        println!("  These never nap. Sessions opened after `hibernaut setup` do; leftovers from closed tabs can be ended.");
    }
    if rows.is_empty() && others.is_empty() {
        println!("No Claude Code sessions running. Settings: {}", crate::config::path().display());
    }
    Ok(())
}

/// Claude processes that hibernaut doesn't manage, such as leftovers from closed editor tabs, with
/// the memory each one's process tree holds.
pub fn leftovers(sessions: &[Value]) -> Vec<(u32, u64)> {
    let u = |v: &Value, k: &str| v[k].as_u64().unwrap_or(0) as u32;
    let ours: HashSet<u32> = sessions.iter().flat_map(|v| [u(v, "pid"), u(v, "claude_pid")]).collect();
    let all = proc::list();
    all.iter()
        .filter(|p| proc::is_claude(&p.name) && !ours.contains(&p.pid))
        // A claude started by another claude (a subagent or `claude -p` from a hook) belongs to its parent.
        .filter(|p| !all.iter().any(|q| q.pid == p.ppid && proc::is_claude(&q.name)))
        .map(|p| (p.pid, proc::tree_rss_in(&all, p.pid)))
        .collect()
}

pub fn size(bytes: u64) -> String {
    let mb = bytes as f64 / (1u64 << 20) as f64;
    if mb >= 1024.0 { format!("{:.1} GB", mb / 1024.0) } else { format!("{mb:.0} MB") }
}

fn age(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

fn short_dir(dir: &str) -> String {
    let home = install::user_home().to_string_lossy().into_owned();
    let d = match dir.strip_prefix(home.as_str()) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => dir.to_string(),
    };
    if d.chars().count() > 32 { format!("…{}", d.chars().rev().take(31).collect::<Vec<_>>().into_iter().rev().collect::<String>()) } else { d }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(size(343 << 20), "343 MB");
        assert_eq!(size(3 << 29), "1.5 GB");
        assert_eq!(age(90_000), "1m");
        assert_eq!(age(7_200_000), "2h");
        assert!(short_dir(&"x/".repeat(40)).starts_with('…'));
    }
}
