//! `ccnap setup` routes every way Claude Code starts through ccnap; `ccnap uninstall` undoes it.
//! - The chat panel of VS Code-family editors: `claudeCode.claudeProcessWrapper` in each
//!   editor's user settings points at ccnap, which the extension then runs in place of claude.
//! - Terminals, the extensions' terminal mode, JetBrains: a `claude` that is really ccnap goes
//!   first on PATH. ccnap finds the real claude by skipping that directory.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

const WRAPPER_KEY: &str = "claudeCode.claudeProcessWrapper";
/// VS Code and its forks: the name of their user-data directory, and of the directory their
/// remote windows (SSH, WSL, containers) keep in the home of the remote machine, where known.
const EDITORS: &[(&str, Option<&str>)] = &[
    ("Code", Some(".vscode-server")),
    ("Code - Insiders", Some(".vscode-server-insiders")),
    ("VSCodium", Some(".vscodium-server")),
    ("Cursor", Some(".cursor-server")),
    ("Windsurf", Some(".windsurf-server")),
    ("Antigravity", Some(".antigravity-server")),
    ("Kiro", None),
    ("Trae", None),
    ("Positron", None),
    ("Void", None),
];
const EXE: &str = env::consts::EXE_SUFFIX;

/// Where ccnap installs itself: %LOCALAPPDATA%\ccnap or ~/.ccnap.
pub fn home() -> PathBuf {
    if let Some(h) = env::var_os("CCNAP_HOME") {
        return h.into();
    }
    if cfg!(windows) {
        env::var_os("LOCALAPPDATA").map_or_else(|| user_home().join("AppData").join("Local"), PathBuf::from).join("ccnap")
    } else {
        user_home().join(".ccnap")
    }
}

pub fn bin_dir() -> PathBuf {
    home().join("bin")
}

pub fn user_home() -> PathBuf {
    env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_default()
}

/// Started as the `claude` on PATH, where `claude install` and friends belong to Claude Code.
pub fn invoked_as_claude() -> bool {
    env::args_os().next().is_some_and(|a| Path::new(&a).file_stem().is_some_and(|s| s.eq_ignore_ascii_case("claude")))
}

pub fn setup() -> io::Result<()> {
    let bin = bin_dir();
    fs::create_dir_all(&bin)?;
    sweep_old(&bin);
    let exe = bin.join(format!("ccnap{EXE}"));
    let me = env::current_exe()?;
    if fs::canonicalize(&me).ok() != fs::canonicalize(&exe).ok() {
        replace(&me, &exe)?;
    }
    println!("ccnap installed at {}", exe.display());

    let shim = bin.join(format!("claude{EXE}"));
    #[cfg(unix)]
    {
        let _ = fs::remove_file(&shim);
        std::os::unix::fs::symlink(&exe, &shim)?;
    }
    #[cfg(windows)]
    replace(&exe, &shim)?;
    path::add(&bin)?;
    println!("terminals: `claude` now runs through ccnap ({})", shim.display());
    #[cfg(windows)]
    for dir in path::claude_ahead() {
        println!(
            "warning: {} has a claude too, and Windows puts system PATH folders ahead of yours, so terminals run that one, not ccnap. Remove it there, or run `ccnap` instead of `claude`.",
            dir.display()
        );
    }

    let editors = editor_settings();
    for (name, file) in &editors {
        println!("{name}: {}", set_wrapper(file, &exe)?);
    }
    if editors.is_empty() {
        println!("editors: no VS Code-family editor found");
    }
    match crate::find_claude() {
        Ok(real) => println!("the real claude: {}", real.display()),
        Err(e) => println!("note: {e} (editor panels bring their own, so they work regardless)"),
    }
    let cfg = crate::config::path();
    if !cfg.exists() {
        fs::write(&cfg, crate::config::TEMPLATE)?;
    }
    println!("settings: {} (idle time, memory limit, excluded folders)", cfg.display());
    println!("Done. New editor chat tabs use ccnap right away; open a new terminal (or restart the editor) for `claude`.");
    let old = crate::status::outdated_count();
    if old > 0 {
        println!("{old} open session{} still run the previous ccnap. {}", if old == 1 { "" } else { "s" }, crate::status::UPDATE_HINT);
    }
    if cfg!(any(windows, target_os = "macos")) {
        setup_tray()?;
    }
    Ok(())
}

/// Installs the tray icon next to ccnap, has it start at login, and starts it now, replacing
/// one that is already running so it shows this version.
fn setup_tray() -> io::Result<()> {
    let dst = tray_exe();
    let beside_me = env::current_exe()?.with_file_name(dst.file_name().unwrap());
    let src = if beside_me.is_file() { beside_me } else { dst.clone() };
    if !src.is_file() {
        println!("tray: ccnap-tray wasn't found next to ccnap, so there's no tray icon");
        return Ok(());
    }
    stop_tray();
    if fs::canonicalize(&src).ok() != fs::canonicalize(&dst).ok() {
        replace(&src, &dst)?;
    }
    login::set(Some(&dst))?;
    start_tray(&dst)?;
    println!("tray: the ccnap icon is by the clock, and starts at login (turn that off in its menu)");
    Ok(())
}

pub fn tray_exe() -> PathBuf {
    bin_dir().join(format!("ccnap-tray{EXE}"))
}

/// The running tray icon's heartbeat: its pid, rewritten every few seconds.
pub fn tray_heartbeat() -> PathBuf {
    crate::temp_dir().join("tray.json")
}

/// Written to ask the running tray icon to quit.
pub fn tray_quit() -> PathBuf {
    crate::temp_dir().join("tray.quit")
}

/// Ends the running tray icon, if any. It's asked to quit first: one ended by force leaves a
/// dead icon by the clock until the pointer passes over it.
fn stop_tray() -> bool {
    let Some(pid) = tray_pid() else { return false };
    let _ = fs::write(tray_quit(), "");
    for _ in 0..50 {
        if tray_pid().is_none() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    crate::proc::end_tree(pid);
    let _ = fs::remove_file(tray_heartbeat());
    let _ = fs::remove_file(tray_quit());
    true
}

/// The pid of a tray icon that is running now.
pub fn tray_pid() -> Option<u32> {
    let v: serde_json::Value = serde_json::from_slice(&fs::read(tray_heartbeat()).ok()?).ok()?;
    let fresh = crate::now_ms().saturating_sub(v["updated"].as_u64()?) < 10_000;
    let pid = fresh.then(|| v["pid"].as_u64()).flatten()? as u32;
    // A tray that was just ended leaves a fresh heartbeat behind.
    crate::proc::list().iter().any(|p| p.pid == pid && p.name.starts_with("ccnap-tray")).then_some(pid)
}

pub fn start_tray(exe: &Path) -> io::Result<()> {
    let mut cmd = std::process::Command::new(exe);
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x8;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().map(drop)
}

/// Starting the tray icon at login.
#[cfg(windows)]
pub mod login {
    use std::{io, path::Path, ptr};
    use windows_sys::Win32::{Foundation::*, System::Registry::*};

    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    pub fn enabled() -> bool {
        let mut size = 0u32;
        let code = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, wide(RUN).as_ptr(), wide("ccnap").as_ptr(), RRF_RT_REG_SZ, ptr::null_mut(), ptr::null_mut(), &mut size)
        };
        code == ERROR_SUCCESS
    }

    pub fn set(tray: Option<&Path>) -> io::Result<()> {
        let name = wide("ccnap");
        let code = unsafe {
            let mut key = ptr::null_mut();
            let code = RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN).as_ptr(), 0, KEY_SET_VALUE, &mut key);
            if code != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(code as i32));
            }
            let code = match tray {
                Some(exe) => {
                    let data = wide(&format!("\"{}\"", exe.display()));
                    RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32)
                }
                None => match RegDeleteValueW(key, name.as_ptr()) {
                    ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
                    c => c,
                },
            };
            RegCloseKey(key);
            code
        };
        if code == ERROR_SUCCESS { Ok(()) } else { Err(io::Error::from_raw_os_error(code as i32)) }
    }
}

#[cfg(target_os = "macos")]
pub mod login {
    use std::{fs, io, path::Path, path::PathBuf};

    fn agent() -> PathBuf {
        super::user_home().join("Library").join("LaunchAgents").join("dev.ccnap.tray.plist")
    }

    pub fn enabled() -> bool {
        agent().is_file()
    }

    pub fn set(tray: Option<&Path>) -> io::Result<()> {
        let Some(exe) = tray else {
            return match fs::remove_file(agent()) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        };
        let exe = exe.display().to_string().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>dev.ccnap.tray</string>
    <key>ProgramArguments</key><array><string>{exe}</string></array>
    <key>RunAtLoad</key><true/>
</dict>
</plist>
"#
        );
        fs::create_dir_all(agent().parent().unwrap())?;
        fs::write(agent(), plist)
    }
}

/// No tray icon here yet, so nothing to start at login.
#[cfg(not(any(windows, target_os = "macos")))]
pub mod login {
    pub fn set(_tray: Option<&std::path::Path>) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn uninstall() -> io::Result<()> {
    login::set(None)?;
    if stop_tray() {
        println!("tray: icon removed");
    }
    for (name, file) in editor_settings() {
        let Ok(text) = fs::read_to_string(&file) else { continue };
        if jsonc::get_string(&text, WRAPPER_KEY).is_some_and(|v| is_ours(&v)) {
            if let Some(new) = jsonc::remove(&text, WRAPPER_KEY) {
                fs::write(&file, new)?;
                println!("{name}: removed {WRAPPER_KEY}");
            }
        }
    }
    let bin = bin_dir();
    path::remove(&bin)?;
    // One still running in an open terminal can't be deleted, only moved out of the way.
    let shim = bin.join(format!("claude{EXE}"));
    match fs::remove_file(&shim).or_else(|_| fs::rename(&shim, shim.with_extension(format!("old{}", std::process::id())))) {
        Ok(()) => println!("terminals: `claude` is Claude Code again"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => println!("couldn't remove the claude shim ({e}); close running sessions and retry"),
    }
    println!("ccnap itself is still in {}; delete that folder to remove it.", home().display());
    Ok(())
}

fn is_ours(wrapper: &str) -> bool {
    Path::new(wrapper).file_stem().is_some_and(|s| s.eq_ignore_ascii_case("ccnap"))
}

/// Points one editor's Claude Code extension at ccnap, unless it already uses another wrapper.
fn set_wrapper(file: &Path, exe: &Path) -> io::Result<String> {
    let text = match fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Ok(format!("skipped: couldn't read {} ({e})", file.display())),
    };
    let exe = exe.to_string_lossy();
    match jsonc::get_string(&text, WRAPPER_KEY) {
        Some(v) if v == exe => return Ok("chat panel already uses ccnap".into()),
        Some(v) if !is_ours(&v) => return Ok(format!("left alone: {WRAPPER_KEY} already runs {v}")),
        _ => {}
    }
    let Some(new) = jsonc::set_string(&text, WRAPPER_KEY, &exe) else {
        return Ok(format!("skipped: couldn't read {}", file.display()));
    };
    fs::write(file, new)?;
    Ok("chat panel now runs through ccnap".into())
}

/// (name, settings.json) for each VS Code-family editor with a user profile on this machine, and
/// for each one whose remote windows connect to it. The wrapper setting is machine-scoped, so a
/// remote window reads it from the remote's Machine settings, not the user's.
fn editor_settings() -> Vec<(String, PathBuf)> {
    let base = if cfg!(windows) {
        env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        Some(user_home().join("Library").join("Application Support"))
    } else {
        Some(env::var_os("XDG_CONFIG_HOME").map_or_else(|| user_home().join(".config"), PathBuf::from))
    };
    let mut out = Vec::new();
    for (name, server) in EDITORS {
        if let Some(user) = base.as_ref().map(|b| b.join(name).join("User")).filter(|d| d.is_dir()) {
            out.push((name.to_string(), user.join("settings.json")));
        }
        if let Some(machine) = server.map(|s| user_home().join(s).join("data").join("Machine")).filter(|d| d.is_dir()) {
            out.push((format!("{name} (remote)"), machine.join("settings.json")));
        }
    }
    out
}

/// Copies `src` over `dst`, even while `dst` runs: Windows lets a running exe be renamed, not
/// overwritten.
fn replace(src: &Path, dst: &Path) -> io::Result<()> {
    if dst.exists() && fs::remove_file(dst).is_err() {
        fs::rename(dst, dst.with_extension(format!("old{}", std::process::id())))?;
    }
    fs::copy(src, dst)?;
    Ok(())
}

/// Removes the copies `replace` had to rename, once nothing runs them.
fn sweep_old(bin: &Path) {
    for e in fs::read_dir(bin).into_iter().flatten().flatten() {
        if e.path().extension().is_some_and(|x| x.to_string_lossy().starts_with("old")) {
            let _ = fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_editors_at_ccnap_but_respects_other_wrappers() {
        let dir = env::temp_dir().join(format!("ccnap-setup-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (file, exe) = (dir.join("settings.json"), dir.join("ccnap.exe"));
        let original = "// mine\n{\n    \"editor.fontSize\": 14,\n}\n";
        fs::write(&file, original).unwrap();
        assert_eq!(set_wrapper(&file, &exe).unwrap(), "chat panel now runs through ccnap");
        assert_eq!(set_wrapper(&file, &exe).unwrap(), "chat panel already uses ccnap");
        let text = fs::read_to_string(&file).unwrap();
        assert_eq!(jsonc::get_string(&text, WRAPPER_KEY).as_deref(), Some(&*exe.to_string_lossy()));
        assert_eq!(jsonc::remove(&text, WRAPPER_KEY).unwrap(), original);

        let theirs = "{ \"claudeCode.claudeProcessWrapper\": \"/opt/other\" }";
        fs::write(&file, theirs).unwrap();
        assert!(set_wrapper(&file, &exe).unwrap().starts_with("left alone"));
        assert_eq!(fs::read_to_string(&file).unwrap(), theirs);

        fs::remove_file(&file).unwrap(); // an editor that never saved a setting
        set_wrapper(&file, &exe).unwrap();
        assert!(fs::read_to_string(&file).unwrap().contains(WRAPPER_KEY));

        let unreadable = b"{ \"a\": \"\xff\" }"; // not UTF-8
        fs::write(&file, unreadable).unwrap();
        assert!(set_wrapper(&file, &exe).unwrap().starts_with("skipped"));
        assert_eq!(fs::read(&file).unwrap(), unreadable);
        fs::remove_dir_all(&dir).unwrap();
    }
}

/// Editing settings.json, which is JSON with comments and trailing commas, without disturbing
/// anything else in it.
mod jsonc {
    pub struct Member {
        key: String,
        start: usize,
        value: (usize, usize),
    }

    /// The top-level `{` and members of `text`, or None if it isn't an object.
    fn members(text: &str) -> Option<(usize, Vec<Member>)> {
        let b = text.as_bytes();
        let open = skip_ws(b, 0);
        if b.get(open) != Some(&b'{') {
            return None;
        }
        let mut out = Vec::new();
        let mut i = open + 1;
        loop {
            i = skip_ws(b, i);
            match b.get(i)? {
                b'}' => return Some((open, out)),
                b',' => i += 1,
                b'"' => {
                    let start = i;
                    i = skip_string(b, i)?;
                    let key = text[start + 1..i - 1].to_string();
                    i = skip_ws(b, i);
                    if b.get(i) != Some(&b':') {
                        return None;
                    }
                    let v = skip_ws(b, i + 1);
                    i = skip_value(b, v)?;
                    out.push(Member { key, start, value: (v, i) });
                }
                _ => return None,
            }
        }
    }

    fn skip_ws(b: &[u8], mut i: usize) -> usize {
        loop {
            match (b.get(i), b.get(i + 1)) {
                (Some(c), _) if c.is_ascii_whitespace() => i += 1,
                (Some(b'/'), Some(b'/')) => {
                    while b.get(i).is_some_and(|&c| c != b'\n') {
                        i += 1;
                    }
                }
                (Some(b'/'), Some(b'*')) => {
                    i += 2;
                    while i < b.len() && !b[i..].starts_with(b"*/") {
                        i += 1;
                    }
                    i = (i + 2).min(b.len());
                }
                _ => return i,
            }
        }
    }

    /// `i` is at an opening quote; returns the index after the closing one.
    fn skip_string(b: &[u8], mut i: usize) -> Option<usize> {
        i += 1;
        loop {
            match b.get(i)? {
                b'\\' => i += 2,
                b'"' => return Some(i + 1),
                _ => i += 1,
            }
        }
    }

    fn skip_value(b: &[u8], i: usize) -> Option<usize> {
        match b.get(i)? {
            b'"' => skip_string(b, i),
            b'{' | b'[' => {
                let mut depth = 0;
                let mut i = i;
                loop {
                    i = skip_ws(b, i);
                    match b.get(i)? {
                        b'"' => i = skip_string(b, i)?,
                        b'{' | b'[' => {
                            depth += 1;
                            i += 1;
                        }
                        b'}' | b']' => {
                            depth -= 1;
                            i += 1;
                            if depth == 0 {
                                return Some(i);
                            }
                        }
                        _ => i += 1,
                    }
                }
            }
            _ => {
                let mut i = i;
                while b.get(i).is_some_and(|&c| !matches!(c, b',' | b'}' | b'/') && !c.is_ascii_whitespace()) {
                    i += 1;
                }
                Some(i)
            }
        }
    }

    pub fn get_string(text: &str, key: &str) -> Option<String> {
        let (_, ms) = members(text)?;
        let m = ms.iter().find(|m| m.key == key)?;
        serde_json::from_str(&text[m.value.0..m.value.1]).ok()
    }

    /// `text` with `key` set to the string `value`, added as the first member if missing.
    pub fn set_string(text: &str, key: &str, value: &str) -> Option<String> {
        let lit = serde_json::to_string(value).ok()?;
        if text.trim().is_empty() {
            return Some(format!("{{\n    \"{key}\": {lit}\n}}\n"));
        }
        let (open, ms) = members(text)?;
        if let Some(m) = ms.iter().find(|m| m.key == key) {
            return Some(format!("{}{lit}{}", &text[..m.value.0], &text[m.value.1..]));
        }
        let comma = if ms.is_empty() { "" } else { "," };
        Some(format!("{}\n    \"{key}\": {lit}{comma}{}", &text[..=open], &text[open + 1..]))
    }

    /// `text` without `key`, and without the comma that separated it.
    pub fn remove(text: &str, key: &str) -> Option<String> {
        let b = text.as_bytes();
        let (_, ms) = members(text)?;
        let n = ms.iter().position(|m| m.key == key)?;
        let (mut start, mut end) = (ms[n].start, ms[n].value.1);
        let after = skip_ws(b, end);
        if b.get(after) == Some(&b',') {
            end = after + 1;
        } else if n > 0 {
            start = ms[n - 1].value.1;
        }
        // Take the whole line when the member had one to itself.
        let line_start = text[..start].rfind('\n').map_or(0, |p| p + 1);
        let line_end = text[end..].find('\n').map_or(text.len(), |p| end + p + 1);
        if text[line_start..start].trim().is_empty() && text[end..line_end].trim().is_empty() {
            (start, end) = (line_start, line_end);
        }
        Some(format!("{}{}", &text[..start], &text[end..]))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        const K: &str = "claudeCode.claudeProcessWrapper";

        #[test]
        fn edits_keep_everything_else() {
            let text = "// my settings\n{\n    \"editor.fontSize\": 14, // big\n    \"a\": {\"b\": [1, \"}\"]},\n}\n";
            let set = set_string(text, K, r"C:\x\ccnap.exe").unwrap();
            assert_eq!(get_string(&set, K).as_deref(), Some(r"C:\x\ccnap.exe"));
            assert!(set.contains("\"editor.fontSize\": 14, // big"));
            let again = set_string(&set, K, "/y/ccnap").unwrap();
            assert_eq!(get_string(&again, K).as_deref(), Some("/y/ccnap"));
            assert_eq!(again.matches(K).count(), 1);
            assert_eq!(remove(&again, K).unwrap(), text);
        }

        #[test]
        fn empty_and_missing_files() {
            assert_eq!(get_string(&set_string("", K, "v").unwrap(), K).as_deref(), Some("v"));
            let set = set_string("{}", K, "v").unwrap();
            assert_eq!(get_string(&set, K).as_deref(), Some("v"));
            assert!(!set.contains(','));
            assert_eq!(set_string("not json", K, "v"), None);
            // Removing the last member also drops the comma before it.
            let t = "{\n  \"a\": 1,\n  \"claudeCode.claudeProcessWrapper\": \"v\"\n}";
            assert_eq!(remove(t, K).unwrap(), "{\n  \"a\": 1\n}");
        }
    }
}

/// Putting ccnap's bin directory first on PATH.
#[cfg(unix)]
mod path {
    use super::user_home;
    use std::{env, fs, io, path::Path};

    const START: &str = "# >>> ccnap >>>";
    const END: &str = "# <<< ccnap <<<";

    /// Shell startup files to edit, with the line each needs.
    fn files(bin: &Path) -> Vec<(std::path::PathBuf, String)> {
        let home = user_home();
        let shell = env::var("SHELL").unwrap_or_default();
        let posix = format!("export PATH=\"{}:$PATH\"", bin.display());
        let mut out = Vec::new();
        for (name, used) in [
            (".bashrc", shell.ends_with("bash")),
            (".zshrc", shell.ends_with("zsh")),
            (".bash_profile", false),
            (".profile", false),
        ] {
            let f = home.join(name);
            if used || f.exists() {
                out.push((f, posix.clone()));
            }
        }
        let fish = home.join(".config").join("fish");
        if shell.ends_with("fish") || fish.exists() {
            out.push((fish.join("conf.d").join("ccnap.fish"), format!("set -gx PATH \"{}\" $PATH", bin.display())));
        }
        out
    }

    pub fn add(bin: &Path) -> io::Result<()> {
        for (f, line) in files(bin) {
            let text = fs::read_to_string(&f).unwrap_or_default();
            let new = with_block(&text, &line);
            if new == text {
                println!("PATH: {} already has {}", f.display(), bin.display());
                continue;
            }
            if let Some(dir) = f.parent() {
                fs::create_dir_all(dir)?;
            }
            fs::write(&f, new)?;
            println!("PATH: added {} in {}", bin.display(), f.display());
        }
        Ok(())
    }

    pub fn remove(bin: &Path) -> io::Result<()> {
        for (f, _) in files(bin) {
            let Ok(text) = fs::read_to_string(&f) else { continue };
            if text.contains(START) {
                fs::write(&f, strip(&text))?;
                println!("PATH: removed ccnap from {}", f.display());
            }
        }
        Ok(())
    }

    /// `text` with our block at the end, so ours is the last PATH change and comes first.
    fn with_block(text: &str, line: &str) -> String {
        format!("{}\n{START}\n{line}\n{END}\n", strip(text).trim_end()).trim_start().to_string()
    }

    /// `text` without our block.
    fn strip(text: &str) -> String {
        match (text.find(START), text.find(END)) {
            (Some(s), Some(e)) if s < e => format!("{}{}", &text[..s], text[e + END.len()..].trim_start_matches('\n')),
            _ => text.to_string(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn profile_block_round_trip() {
            let rc = "alias ll='ls -l'\nexport PATH=\"$HOME/.local/bin:$PATH\"\n";
            let added = with_block(rc, "export PATH=\"/h/.ccnap/bin:$PATH\"");
            assert!(added.ends_with("export PATH=\"/h/.ccnap/bin:$PATH\"\n# <<< ccnap <<<\n"));
            assert_eq!(with_block(&added, "export PATH=\"/h/.ccnap/bin:$PATH\""), added);
            assert_eq!(strip(&added), rc);
            assert_eq!(strip(&with_block("", "x")), "");
        }
    }
}

/// Putting ccnap's bin directory first on the user PATH in the registry.
#[cfg(windows)]
mod path {
    use std::{
        env, io,
        path::{Path, PathBuf},
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::*,
        System::Registry::*,
        UI::WindowsAndMessaging::{SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE},
    };

    pub fn add(bin: &Path) -> io::Result<()> {
        let bin = bin.to_string_lossy();
        let mut changed = false;
        edit("Path", |p| {
            let new = with_entry(p, &bin, true);
            changed = new.is_some();
            new
        })?;
        println!("PATH: {} {bin} for your user", if changed { "added" } else { "already has" });
        Ok(())
    }

    pub fn remove(bin: &Path) -> io::Result<()> {
        edit("Path", |p| with_entry(p, &bin.to_string_lossy(), false))
    }

    /// PATH with `dir` moved to the front (`add`) or taken out, or None if nothing changes.
    pub fn with_entry(path: &str, dir: &str, add: bool) -> Option<String> {
        let same = |e: &str| e.trim_end_matches('\\').eq_ignore_ascii_case(dir.trim_end_matches('\\'));
        let rest: Vec<&str> = path.split(';').filter(|e| !e.is_empty() && !same(e)).collect();
        let new = if add { [vec![dir], rest].concat().join(";") } else { rest.join(";") };
        (new != path).then_some(new)
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// System PATH folders that hold a claude. Windows puts the system PATH ahead of each
    /// user's, so that claude runs in place of ccnap's.
    pub fn claude_ahead() -> Vec<PathBuf> {
        env::split_paths(&system_path())
            .filter(|d| ["claude.exe", "claude.cmd"].iter().any(|n| d.join(n).is_file()))
            .collect()
    }

    fn system_path() -> String {
        let key = wide(r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment");
        let mut buf = vec![0u16; 32 * 1024];
        let mut size = (buf.len() * 2) as u32;
        // RRF_RT_REG_SZ also takes REG_EXPAND_SZ, expanded.
        let code = unsafe {
            RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), wide("Path").as_ptr(), RRF_RT_REG_SZ, ptr::null_mut(), buf.as_mut_ptr().cast(), &mut size)
        };
        if code != ERROR_SUCCESS {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..size as usize / 2]).trim_end_matches('\0').to_string()
    }

    /// Rewrites a value under HKCU\Environment, keeping its type (REG_EXPAND_SZ keeps
    /// `%USERPROFILE%`-style entries working), then tells running programs.
    pub(super) fn edit(name: &str, mut f: impl FnMut(&str) -> Option<String>) -> io::Result<()> {
        let err = |code: u32| io::Error::from_raw_os_error(code as i32);
        let name = wide(name);
        unsafe {
            let mut key = ptr::null_mut();
            let code = RegOpenKeyExW(HKEY_CURRENT_USER, wide("Environment").as_ptr(), 0, KEY_READ | KEY_WRITE, &mut key);
            if code != ERROR_SUCCESS {
                return Err(err(code));
            }
            let (mut kind, mut size) = (REG_EXPAND_SZ, 0u32);
            let mut buf: Vec<u16> = Vec::new();
            let code = RegQueryValueExW(key, name.as_ptr(), ptr::null(), &mut kind, ptr::null_mut(), &mut size);
            if code == ERROR_SUCCESS {
                buf.resize(size as usize / 2 + 1, 0);
                RegQueryValueExW(key, name.as_ptr(), ptr::null(), &mut kind, buf.as_mut_ptr().cast(), &mut size);
                buf.truncate(size as usize / 2);
            } else if code != ERROR_FILE_NOT_FOUND {
                RegCloseKey(key);
                return Err(err(code));
            }
            let old = String::from_utf16_lossy(&buf).trim_end_matches('\0').to_string();
            let result = match f(&old) {
                Some(new) => {
                    let data = wide(&new);
                    let kind = if kind == REG_SZ { REG_SZ } else { REG_EXPAND_SZ };
                    let code = RegSetValueExW(key, name.as_ptr(), 0, kind, data.as_ptr().cast(), (data.len() * 2) as u32);
                    let env = wide("Environment");
                    let mut out = 0;
                    SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, env.as_ptr() as isize, SMTO_ABORTIFHUNG, 1000, &mut out);
                    if code == ERROR_SUCCESS { Ok(()) } else { Err(err(code)) }
                }
                None => Ok(()),
            };
            RegCloseKey(key);
            result
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn path_entries() {
            let d = r"C:\Users\u\AppData\Local\ccnap\bin";
            assert_eq!(with_entry(r"%USERPROFILE%\.local\bin;C:\x", d, true).unwrap(), format!(r"{d};%USERPROFILE%\.local\bin;C:\x"));
            assert_eq!(with_entry(&format!(r"{d};C:\x"), d, true), None);
            assert_eq!(with_entry(&format!(r"C:\x;{d}\"), d, true).unwrap(), format!(r"{d};C:\x"));
            assert_eq!(with_entry(&format!(r"{d};C:\x"), d, false).unwrap(), r"C:\x");
            assert_eq!(with_entry("", d, true).unwrap(), d);
        }

        #[test]
        fn reads_the_system_path_expanded() {
            let p = system_path().to_lowercase();
            assert!(p.contains(r"\windows\system32") && !p.contains('%'), "{p}");
        }

        #[test]
        fn registry_round_trip() {
            // A scratch value, so the real PATH is never touched by tests.
            let name = format!("CcnapTest{}", std::process::id());
            edit(&name, |p| with_entry(p, r"C:\a", true)).unwrap();
            edit(&name, |p| with_entry(p, r"C:\b", true)).unwrap();
            let mut seen = String::new();
            edit(&name, |p| {
                seen = p.to_string();
                None
            })
            .unwrap();
            assert_eq!(seen, r"C:\b;C:\a");
            unsafe {
                let mut key = ptr::null_mut();
                RegOpenKeyExW(HKEY_CURRENT_USER, wide("Environment").as_ptr(), 0, KEY_WRITE, &mut key);
                RegDeleteValueW(key, wide(&name).as_ptr());
                RegCloseKey(key);
            }
        }
    }
}
