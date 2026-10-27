//! How hibernaut reads a `claude` command line: which invocations to leave alone, and how to
//! rebuild one as `claude --resume <id>` after a nap.

use crate::hook::Comeback;
use std::ffi::OsString;

/// Flags that take exactly one value.
const VALUE: &[&str] = &[
    "--advisor", "--agent", "--agents", "--append-subagent-system-prompt",
    "--append-subagent-system-prompt-file", "--append-system-prompt",
    "--append-system-prompt-file", "--autocompact", "--debug-file", "--effort", "--environment",
    "--exec", "--fallback-model", "--input-format", "--json-schema", "--max-budget-usd",
    "--max-turns", "--model", "-n", "--name", "--output-format", "--permission-mode",
    "--permission-prompt-tool", "--permission-prompts", "--plugin-dir", "--plugin-url", "--ref",
    "--resume-drops-turn", "--resume-session-at",
    "--remote-control-session-name-prefix", "--session-id", "--setting-sources", "--settings",
    "--system-prompt", "--system-prompt-file", "--system-prompt-snapshot", "--teammate-mode",
];
/// Flags that take values up to the next flag (commander's `<values...>`).
const VARIADIC: &[&str] = &[
    "--add-dir", "--allowedTools", "--allowed-tools", "--betas", "--channels",
    "--dangerously-load-development-channels", "--disallowedTools", "--disallowed-tools",
    "--file", "--mcp-config", "--tools",
];
/// Flags whose value is optional (commander's `[value]`).
const OPTIONAL: &[&str] = &[
    "--cloud", "-d", "--debug", "--from-pr", "--prompt-suggestions", "--rc", "--remote",
    "--remote-control", "-r", "--resume", "--teleport", "-w", "--worktree",
];
/// Non-interactive or short-lived runs: nothing to put to sleep.
const PASSTHROUGH: &[&str] = &[
    "-p", "--print", "-h", "--help", "-v", "--version", "--bg", "--background", "--cloud",
    "--remote", "--desktop", "--init-only",
];
const SUBCOMMANDS: &[&str] = &[
    "agents", "attach", "auth", "auto-mode", "daemon", "doctor", "gateway", "import", "install",
    "kill", "logs", "mcp", "plugin", "plugins", "purge", "remote-control", "respawn", "rm",
    "self-hosted-runner", "setup-token", "stop", "ultrareview", "update", "upgrade",
];
/// Dropped on resume: they prompt, pick, or create a session, and `--resume <id>` names it now.
const NOT_ON_RESUME: &[&str] = &[
    "", "--", "-c", "--continue", "-r", "--resume", "--session-id", "--fork-session",
    "--from-pr", "--teleport", "-w", "--worktree", "--tmux",
];

/// One flag with its values, or one positional argument (empty `name`).
#[derive(Debug, Clone)]
pub struct Tok {
    pub name: String,
    pub raw: Vec<OsString>,
}

pub fn parse(args: &[OsString]) -> Vec<Tok> {
    let is_value = |a: &OsString| !a.to_string_lossy().starts_with('-');
    let mut toks = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let s = args[i].to_string_lossy();
        if s == "--" {
            toks.push(Tok { name: "--".into(), raw: args[i..].to_vec() });
            break;
        }
        if !s.starts_with('-') || s == "-" {
            toks.push(Tok { name: String::new(), raw: vec![args[i].clone()] });
            i += 1;
            continue;
        }
        let (name, inline) = match s.split_once('=') {
            Some((n, _)) if n.starts_with("--") => (n.to_string(), true),
            _ => (s.to_string(), false),
        };
        let start = i;
        i += 1;
        if !inline {
            if VALUE.contains(&name.as_str()) {
                i = (i + 1).min(args.len());
            } else if VARIADIC.contains(&name.as_str()) {
                while i < args.len() && is_value(&args[i]) {
                    i += 1;
                }
            } else if OPTIONAL.contains(&name.as_str()) && i < args.len() && is_value(&args[i]) {
                i += 1;
            }
        }
        toks.push(Tok { name, raw: args[start..i].to_vec() });
    }
    toks
}

pub fn passthrough(toks: &[Tok]) -> bool {
    toks.iter().any(|t| PASSTHROUGH.contains(&t.name.as_str()))
        || toks
            .iter()
            .find(|t| t.name.is_empty())
            .is_some_and(|t| SUBCOMMANDS.contains(&t.raw[0].to_string_lossy().as_ref()))
}

/// A flag's value, from `--flag value` or `--flag=value`.
fn value(t: &Tok) -> Option<OsString> {
    match t.raw.as_slice() {
        [only] => only.to_string_lossy().split_once('=').map(|(_, v)| v.into()),
        [_, v, ..] => Some(v.clone()),
        [] => None,
    }
}

/// Removes `--settings` from the command line and returns its (last) value.
pub fn take_settings(toks: &mut Vec<Tok>) -> Option<OsString> {
    let mut found = None;
    toks.retain(|t| {
        if t.name != "--settings" {
            return true;
        }
        found = value(t).or(found.take());
        false
    });
    found
}

/// The Agent SDK's command line: JSON messages over stdin and stdout.
pub fn stream_json(toks: &[Tok]) -> bool {
    toks.iter().any(|t| t.name == "--input-format" && value(t).is_some_and(|v| v == "stream-json"))
}

/// An Agent SDK command line rebuilt to bring `session` back. Everything else is kept as is,
/// unknown flags included, since the SDK passes values as `--flag=value`.
pub fn resume_sdk(toks: &[Tok], session: &str, how: Comeback) -> Vec<OsString> {
    const PICKS_SESSION: &[&str] = &[
        "-c", "--continue", "-r", "--resume", "--session-id", "--fork-session",
        "--resume-session-at", "--resume-drops-turn",
    ];
    let mut out: Vec<OsString> = toks
        .iter()
        .filter(|t| !PICKS_SESSION.contains(&t.name.as_str()))
        .flat_map(|t| t.raw.iter().cloned())
        .collect();
    out.push(format!("{}={session}", session_flag(how)).into());
    out
}

fn session_flag(how: Comeback) -> &'static str {
    match how {
        Comeback::Resume => "--resume",
        Comeback::Fresh => "--session-id",
    }
}

pub fn flatten(toks: &[Tok]) -> Vec<OsString> {
    toks.iter().flat_map(|t| t.raw.iter().cloned()).collect()
}

/// The command line that brings `session` back: the same flags without the prompt or session
/// pickers, and the permission mode the session was last in (Shift+Tab changes it mid-session).
pub fn resume(toks: &[Tok], session: &str, mode: &str, how: Comeback) -> Vec<OsString> {
    let has = |name: &str| toks.iter().any(|t| t.name == name);
    let skips = has("--dangerously-skip-permissions");
    // Still bypassing after starting with the flag: the original flags say so already.
    let set_mode = !mode.is_empty() && (!skips || mode != "bypassPermissions");
    let mut out: Vec<OsString> = toks
        .iter()
        .filter(|t| !NOT_ON_RESUME.contains(&t.name.as_str()))
        // --dangerously-skip-permissions would start it bypassing whatever mode it was in.
        .filter(|t| !set_mode || !["--permission-mode", "--dangerously-skip-permissions"].contains(&t.name.as_str()))
        .flat_map(|t| t.raw.iter().cloned())
        .collect();
    if set_mode {
        if skips && !has("--allow-dangerously-skip-permissions") {
            // Keeps bypass a Shift+Tab away, as it was.
            out.push("--allow-dangerously-skip-permissions".into());
        }
        out.extend(["--permission-mode".into(), mode.into()]);
    }
    out.extend([session_flag(how).into(), session.into()]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    fn strs(v: Vec<OsString>) -> Vec<String> {
        v.into_iter().map(|s| s.into_string().unwrap()).collect()
    }

    #[test]
    fn resume_keeps_flags_and_drops_prompt_and_pickers() {
        let toks = parse(&os(&[
            "fix the bug", "--model", "opus", "-c", "--add-dir", "a", "b", "-r", "--verbose",
            "--permission-mode", "plan",
        ]));
        assert_eq!(
            strs(resume(&toks, "S", "acceptEdits", Comeback::Resume)),
            [
                "--model", "opus", "--add-dir", "a", "b", "--verbose", "--permission-mode",
                "acceptEdits", "--resume", "S",
            ]
        );
        // Back in plan mode, not bypassing, with bypass still a Shift+Tab away.
        let toks = parse(&os(&["-r", "abc", "--dangerously-skip-permissions", "--", "hi"]));
        assert_eq!(
            strs(resume(&toks, "S", "plan", Comeback::Resume)),
            ["--allow-dangerously-skip-permissions", "--permission-mode", "plan", "--resume", "S"]
        );
        assert_eq!(strs(resume(&toks, "S", "bypassPermissions", Comeback::Resume)), ["--dangerously-skip-permissions", "--resume", "S"]);
        let toks = parse(&os(&["--allow-dangerously-skip-permissions"]));
        assert_eq!(
            strs(resume(&toks, "S", "bypassPermissions", Comeback::Resume)),
            ["--allow-dangerously-skip-permissions", "--permission-mode", "bypassPermissions", "--resume", "S"]
        );
    }

    #[test]
    fn passthrough_only_for_non_interactive_runs() {
        assert!(passthrough(&parse(&os(&["-p", "hi"]))));
        assert!(passthrough(&parse(&os(&["--verbose", "mcp", "list"]))));
        assert!(!passthrough(&parse(&os(&["--model", "mcp"]))));
        assert!(!passthrough(&parse(&os(&["update the docs"]))));
    }

    #[test]
    fn sdk_command_lines() {
        let toks = parse(&os(&[
            "--output-format", "stream-json", "--verbose", "--input-format", "stream-json",
            "--resume=old", "--resume-session-at=u1", "--fork-session", "--max-thinking-tokens=9",
            "--permission-prompt-tool", "stdio", "--model=opus",
        ]));
        assert!(stream_json(&toks));
        assert!(!stream_json(&parse(&os(&["--input-format=text"]))));
        assert_eq!(
            strs(resume_sdk(&toks, "S", Comeback::Resume)),
            [
                "--output-format", "stream-json", "--verbose", "--input-format", "stream-json",
                "--max-thinking-tokens=9", "--permission-prompt-tool", "stdio", "--model=opus",
                "--resume=S",
            ]
        );
        // A session nobody typed in comes back as a new one under the same id.
        assert_eq!(strs(resume_sdk(&toks, "S", Comeback::Fresh)).last().unwrap(), "--session-id=S");
        let toks = parse(&os(&["--session-id", "old", "--model", "opus"]));
        assert_eq!(strs(resume(&toks, "S", "", Comeback::Fresh)), ["--model", "opus", "--session-id", "S"]);
    }

    #[test]
    fn take_settings_handles_both_forms() {
        let mut toks = parse(&os(&["--settings", "x.json", "--model", "m"]));
        assert_eq!(take_settings(&mut toks), Some("x.json".into()));
        assert_eq!(strs(flatten(&toks)), ["--model", "m"]);
        let mut toks = parse(&os(&[r#"--settings={"a":1}"#]));
        assert_eq!(take_settings(&mut toks), Some(r#"{"a":1}"#.into()));
        assert!(toks.is_empty());
    }
}
