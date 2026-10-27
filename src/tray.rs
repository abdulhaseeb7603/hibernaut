//! The tray icon (`ccnap-tray`): every ccnap session at a glance, and what you can do about
//! each, in its menu. It reads the status files sessions write and sends them requests
//! (control.rs). It never stops Claude itself, so each session keeps all its safety checks.
//! Settings changes go to config.toml, which running sessions pick up within seconds.

use crate::{
    adopt::{self, Candidate, Origin},
    config::{self, parse_size, Config},
    control::{self, Action},
    install, now_ms, parse_duration, proc, status,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
use tao::{
    event::{Event, StartCause},
    event_loop::{ControlFlow, EventLoopBuilder},
};
use tray_icon::{
    menu::{CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu},
    Icon, TrayIcon, TrayIconBuilder,
};

const REFRESH: Duration = Duration::from_secs(2);
/// Leftover claude processes are looked for less often: that walks every process.
const LEFTOVERS_EVERY: Duration = Duration::from_secs(10);
/// How long the result of an action stays at the top of the menu.
const NOTE_MS: u64 = 60_000;
const IDLE_CHOICES: &[(&str, &str)] = &[("5 minutes", "5m"), ("10 minutes", "10m"), ("30 minutes", "30m"), ("1 hour", "1h")];
const MEMORY_CHOICES: &[(&str, &str)] = &[("1 GB", "1GB"), ("2 GB", "2GB"), ("4 GB", "4GB"), ("Never", "off")];
const TERMINAL_WARNING: &str = "In a terminal, Claude stops and starts again through ccnap, in the same terminal and with the same chat. Text typed into its prompt but not sent is lost. Continue?";

/// A claude outside ccnap: its pid, its memory, and what started it.
type Leftover = (u32, u64, Origin);

/// What a menu entry does.
#[derive(Debug, Clone, PartialEq)]
enum Do {
    Session(u32, Action),
    NapAll(Vec<u32>),
    /// Move one session that doesn't run through ccnap into it, by its process id.
    MoveIn(u32),
    /// Move every one that's ready.
    MoveAll,
    Exclude(PathBuf, bool),
    End(u32, u64),
    /// A setting: its key and TOML value.
    Set(&'static str, String),
    OpenSettings,
    Login(bool),
    Quit,
}

/// The menu as data, so it can be compared (to rebuild it only when it changes) and tested.
#[derive(Debug, Clone, PartialEq)]
enum Entry {
    Text(String),
    Separator,
    Item(String, Do),
    Check(String, bool, Do),
    Sub(String, Vec<Entry>),
}

pub fn main() {
    if std::env::args().any(|a| a == "--preview") {
        return preview();
    }
    if install::tray_pid().is_some_and(|pid| pid != std::process::id()) {
        return; // one tray icon is plenty
    }
    let _ = fs::remove_file(install::tray_quit()); // meant for a tray that's gone
    #[allow(unused_mut)]
    let mut event_loop = EventLoopBuilder::<MenuEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        // A menu bar item, with no Dock icon.
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = proxy.send_event(e);
    }));
    let mut tray: Option<TrayIcon> = None;
    let mut actions: HashMap<MenuId, Do> = HashMap::new();
    let mut shown: Option<Vec<Entry>> = None;
    let mut tip_shown = String::new();
    let mut note: Option<(u64, String)> = None;
    // Claude processes outside ccnap, and the sessions among them that can move in.
    let mut outside: (Option<Instant>, Vec<Leftover>, Vec<Candidate>) = (None, Vec::new(), Vec::new());
    let mut refreshed: Option<Instant> = None;
    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::WaitUntil(Instant::now() + REFRESH);
        match event {
            Event::NewEvents(StartCause::Init) => {
                tray = TrayIconBuilder::new().with_icon(icon()).with_tooltip("ccnap").build().ok();
                // macOS draws the new icon once its run loop wakes.
                #[cfg(target_os = "macos")]
                if let Some(rl) = objc2_core_foundation::CFRunLoop::main() {
                    rl.wake_up();
                }
            }
            Event::UserEvent(e) => {
                let Some(what) = actions.get(&e.id).cloned() else { return };
                if what == Do::Quit {
                    tray.take(); // takes the icon away with it
                    let _ = fs::remove_file(install::tray_heartbeat());
                    *flow = ControlFlow::Exit;
                    return;
                }
                if let Some(text) = perform(what) {
                    note = Some((now_ms(), text));
                }
                (shown, refreshed) = (None, None);
            }
            _ => {}
        }
        if refreshed.is_some_and(|t| t.elapsed() < REFRESH) {
            return;
        }
        refreshed = Some(Instant::now());
        // `ccnap setup` or `uninstall` asks this tray to make way.
        if fs::remove_file(install::tray_quit()).is_ok() {
            tray.take();
            let _ = fs::remove_file(install::tray_heartbeat());
            *flow = ControlFlow::Exit;
            return;
        }
        let Some(tray) = &tray else { return };
        let now = now_ms();
        let _ = fs::write(install::tray_heartbeat(), json!({ "pid": std::process::id(), "updated": now }).to_string());
        let sessions = status::sessions(now);
        let cfg = config::get();
        if outside.0.is_none_or(|t| t.elapsed() >= LEFTOVERS_EVERY) {
            let mut candidates = adopt::candidates(&sessions);
            // Panel sessions move in on their own once idle: nothing is lost, and the next
            // message brings each one back through ccnap.
            if cfg.adopt {
                let moved = candidates.iter().filter(|c| c.panel && c.ready() && adopt::move_in(c).is_ok()).count();
                if moved > 0 {
                    note = Some((now, format!("moved {moved} session{} into ccnap", if moved == 1 { "" } else { "s" })));
                    candidates = adopt::candidates(&status::sessions(now));
                }
            }
            let leftovers = status::leftovers(&sessions).into_iter().map(|(pid, memory)| (pid, memory, adopt::origin(pid))).collect();
            outside = (Some(Instant::now()), leftovers, candidates);
        }
        let note = note.as_ref().filter(|(at, _)| now.saturating_sub(*at) < NOTE_MS).map(|(at, t)| (*at, t.as_str()));
        let (tip, entries) = model(&sessions, &outside.1, &outside.2, &cfg, install::login::enabled(), note, now);
        if tip != tip_shown && !menu_open() {
            let _ = tray.set_tooltip(Some(&tip));
            tip_shown = tip;
        }
        // Replacing the menu while it's open would close it under the pointer: wait till it shuts.
        if shown.as_ref() != Some(&entries) && !menu_open() {
            let mut new_actions = HashMap::new();
            let menu = Menu::new();
            for e in &entries {
                let _ = menu.append(build(e, &mut new_actions).as_ref());
            }
            tray.set_menu(Some(Box::new(menu)));
            (actions, shown) = (new_actions, Some(entries));
        }
    });
}

/// `ccnap-tray --preview`: a second icon whose menu, filled with made-up sessions, opens at once,
/// to see how the menu looks. Its items do nothing, and it quits once the menu closes.
fn preview() {
    let session = |pid: u32, cwd: &str, mode: &str, napping: bool, mb: u64, why: &str| {
        let exe = std::env::current_exe().unwrap_or_default();
        let build = fs::metadata(&exe).and_then(|m| m.modified()).map_or(0, |t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64);
        json!({ "pid": pid, "cwd": cwd, "mode": mode, "napping": napping, "memory": mb << 20, "freed": mb << 20, "why": why,
                "exe": exe, "build": build })
    };
    let sessions = [
        session(1, r"C:\code\voicetest", "panel", false, 800, "tab has focus"),
        session(2, r"C:\code\New folder (21)", "terminal", false, 290, "claude is working"),
        session(3, r"C:\code\New folder (21)", "panel", true, 290, ""),
        session(4, r"D:\postiz", "terminal", true, 300, ""),
    ];
    let candidates = [Candidate {
        pid: 5,
        session: "s".into(),
        cwd: r"C:\code\betterzone".into(),
        panel: true,
        old_ccnap: false,
        memory: 310 << 20,
        wait: None,
        shell: None,
    }];
    let python = Origin { label: "Python program", about: "Started by a Python program using the Claude Agent SDK, not a chat", cost: "" };
    let now = now_ms();
    let (tip, entries) = model(&sessions, &[(6, 141 << 20, python)], &candidates, &Config::default(), true, Some((now, "moved 2 sessions into ccnap")), now);
    let menu = Menu::new();
    for e in &entries {
        let _ = menu.append(build(e, &mut HashMap::new()).as_ref());
    }
    let Ok(tray) = TrayIconBuilder::new().with_icon(icon()).with_tooltip(tip).with_menu(Box::new(menu.clone())).build() else { return };
    // At the pointer: by the icon fails when Windows tucks a new icon in its overflow.
    #[cfg(windows)]
    unsafe {
        use tray_icon::menu::ContextMenu;
        menu.show_context_menu_for_hwnd(tray.window_handle() as isize, None);
    }
    #[cfg(not(windows))]
    tray.show_menu();
}

/// The tooltip and the menu for these sessions and settings. `leftovers` are claude processes
/// outside ccnap; `candidates`, the sessions that can move into it (some of them leftovers).
fn model(
    sessions: &[Value],
    leftovers: &[Leftover],
    candidates: &[Candidate],
    cfg: &Config,
    login: bool,
    note: Option<(u64, &str)>,
    now: u64,
) -> (String, Vec<Entry>) {
    let u = |v: &Value, k: &str| v[k].as_u64().unwrap_or(0);
    let napping = |v: &Value| v["napping"] == true;
    let saved: u64 = sessions.iter().filter(|v| napping(v)).map(|v| u(v, "freed")).sum();
    let asleep = sessions.iter().filter(|v| napping(v)).count();
    let (summary, tip) = match (sessions.len(), asleep) {
        (0, _) => ("No Claude Code sessions running".to_string(), "ccnap".to_string()),
        (n, 0) => (format!("{n} session{}, all awake", plural(n)), format!("ccnap: {n} session{} awake", plural(n))),
        (n, a) => (format!("Saving {} · {a} of {n} napping", steady(saved)), format!("ccnap: saving {}", steady(saved))),
    };

    let mut menu = vec![Entry::Text(summary)];
    // What happened to the last request, from the session that answered or from the tray.
    let answered = sessions
        .iter()
        .filter_map(|v| {
            let a = &v["last_action"];
            Some((a["at"].as_u64()?, format!("{}: {}", folder_name(v), a["result"].as_str()?)))
        })
        .chain(note.map(|(at, t)| (at, t.to_string())))
        .filter(|(at, _)| now.saturating_sub(*at) < NOTE_MS)
        .max_by_key(|(at, _)| *at);
    if let Some((_, text)) = answered {
        menu.push(Entry::Text(format!("Last: {text}")));
    }
    // Every session that can move in, or update, in one click.
    let ready: Vec<&Candidate> = candidates.iter().filter(|c| c.ready()).collect();
    if let n @ 1.. = ready.len() {
        let what = if ready.iter().all(|c| c.old_ccnap) {
            format!("Update {n} session{} to this ccnap", plural(n))
        } else {
            format!("Move {n} session{} into ccnap", plural(n))
        };
        let asks = if ready.iter().any(|c| !c.panel) { "…" } else { "" };
        menu.push(Entry::Item(format!("{what}{asks}"), Do::MoveAll));
    }

    // The sessions, awake ones first: they hold memory now.
    let mut order: Vec<&Value> = sessions.iter().collect();
    order.sort_by_key(|v| (napping(v), folder_name(v).to_lowercase()));
    let names = tell_apart(&order.iter().map(|v| (folder_name(v), place(v["mode"] == "panel"))).collect::<Vec<_>>());
    if !order.is_empty() {
        menu.push(Entry::Separator);
    }
    for (v, name) in order.iter().zip(&names) {
        menu.push(session_entry(v, name, cfg, candidates));
    }

    // Claude processes outside ccnap, out of the way in one submenu.
    let outside: Vec<Entry> = candidates
        .iter()
        .filter(|c| !c.old_ccnap)
        .map(outsider_entry)
        .chain(leftovers.iter().filter(|(pid, ..)| !candidates.iter().any(|c| c.pid == *pid)).map(leftover_entry))
        .collect();
    if !outside.is_empty() {
        let mut sub = vec![Entry::Text("These never nap, as they don't run through ccnap.".into())];
        if cfg.adopt && candidates.iter().any(|c| !c.old_ccnap && c.panel && !c.ready()) {
            sub.push(Entry::Text("Editor chats move in by themselves once idle.".into()));
        }
        sub.push(Entry::Separator);
        let n = outside.len();
        sub.extend(outside);
        menu.push(Entry::Separator);
        menu.push(Entry::Sub(format!("Outside ccnap ({n})"), sub));
    }

    menu.push(Entry::Separator);
    let awake: Vec<u32> = sessions.iter().filter(|v| !napping(v) && !status::outdated(v)).map(|v| u(v, "pid") as u32).collect();
    if !awake.is_empty() {
        menu.push(Entry::Item("Nap idle sessions now".into(), Do::NapAll(awake)));
    }
    menu.push(Entry::Sub("Settings".into(), settings(cfg, login)));
    menu.push(Entry::Separator);
    menu.push(Entry::Item("Quit tray icon".into(), Do::Quit));
    (tip, menu)
}

fn settings(cfg: &Config, login: bool) -> Vec<Entry> {
    let choices = |list: &[(&str, &str)], key: &'static str, current: &dyn Fn(&str) -> bool| {
        list.iter().map(|(label, v)| Entry::Check(label.to_string(), current(v), Do::Set(key, format!("\"{v}\"")))).collect()
    };
    vec![
        Entry::Sub("Nap after".into(), choices(IDLE_CHOICES, "idle", &|v| parse_duration(v) == Some(cfg.idle_ms))),
        Entry::Sub("Restart sessions above".into(), choices(MEMORY_CHOICES, "memory_limit", &|v| parse_size(v) == Some(cfg.memory_limit))),
        Entry::Separator,
        Entry::Check(
            "Move other sessions in automatically".into(),
            cfg.adopt,
            Do::Set("adopt", if cfg.adopt { "\"off\"" } else { "\"auto\"" }.into()),
        ),
        Entry::Check("Start at login".into(), login, Do::Login(!login)),
        Entry::Separator,
        Entry::Item("Open settings file".into(), Do::OpenSettings),
    ]
}

fn session_entry(v: &Value, name: &str, cfg: &Config, candidates: &[Candidate]) -> Entry {
    let pid = v["pid"].as_u64().unwrap_or(0) as u32;
    let cwd = PathBuf::from(v["cwd"].as_str().unwrap_or(""));
    let bytes = |k: &str| steady(v[k].as_u64().unwrap_or(0));
    let at = if v["mode"] == "panel" { "In the editor" } else { "In a terminal" };
    let napping = v["napping"] == true;
    let (icon, right) = if napping { ("💤", format!("{} saved", bytes("freed"))) } else { ("●", bytes("memory")) };
    let mut sub = vec![Entry::Text(cwd.display().to_string())];

    if status::outdated(v) {
        // It doesn't take requests, but restarting it brings it to the installed version.
        sub.push(Entry::Text(format!("{at} · runs an older ccnap")));
        sub.push(Entry::Separator);
        sub.push(match candidates.iter().find(|c| c.pid == pid) {
            Some(c) if c.ready() && c.panel => Entry::Item("Update now (keeps the chat)".into(), Do::MoveIn(pid)),
            Some(c) if c.ready() => Entry::Item("Update now (restarts it in its terminal)…".into(), Do::MoveIn(pid)),
            Some(c) => Entry::Text(format!("Updates once idle ({})", c.wait.unwrap_or("busy"))),
            None => Entry::Text("Reload the editor window, or restart the terminal session, to update it".into()),
        });
        return Entry::Sub(row(&format!("{icon} {name} (old version)"), &right), sub);
    }

    if napping {
        sub.push(Entry::Text(format!("{at} · napping, {right}")));
        sub.push(Entry::Separator);
        sub.push(Entry::Item("Wake now".into(), Do::Session(pid, Action::Wake)));
    } else {
        let why = v["why"].as_str().map_or(String::new(), |w| format!(": {w}"));
        sub.push(Entry::Text(format!("{at} · awake{why}")));
        sub.push(Entry::Separator);
        sub.push(Entry::Item("Nap now".into(), Do::Session(pid, Action::Nap)));
        sub.push(Entry::Item("Restart (frees memory, keeps the chat)".into(), Do::Session(pid, Action::Restart)));
    }
    let excluded = cfg.excludes(&cwd);
    sub.push(Entry::Separator);
    sub.push(Entry::Check("Never nap this folder".into(), excluded, Do::Exclude(cwd, !excluded)));
    Entry::Sub(row(&format!("{icon} {name}"), &right), sub)
}

/// A session that never ran through ccnap.
fn outsider_entry(c: &Candidate) -> Entry {
    let name = Path::new(&c.cwd).file_name().map_or_else(|| c.cwd.clone(), |n| n.to_string_lossy().into_owned());
    let at = if c.panel { "In the editor" } else { "In a terminal" };
    let mut sub = vec![Entry::Text(c.cwd.clone()), Entry::Text(format!("{at} · not in ccnap yet")), Entry::Separator];
    sub.push(match c.wait {
        None if c.panel => Entry::Item("Move into ccnap (keeps the chat)".into(), Do::MoveIn(c.pid)),
        None => Entry::Item("Move into ccnap (restarts it in its terminal)…".into(), Do::MoveIn(c.pid)),
        Some(why) => Entry::Text(format!("Can move in once idle ({why})")),
    });
    sub.push(Entry::Item("End this process…".into(), Do::End(c.pid, c.memory)));
    Entry::Sub(row(&format!("↻ {name} · {}", place(c.panel)), &steady(c.memory)), sub)
}

/// A claude outside ccnap that can't move in: a program's, or one nobody accounts for.
fn leftover_entry(&(pid, memory, origin): &Leftover) -> Entry {
    Entry::Sub(
        row(&format!("○ {}", origin.label), &steady(memory)),
        vec![
            Entry::Text(origin.about.into()),
            Entry::Text(format!("Process {pid}")),
            Entry::Separator,
            Entry::Item("End this process…".into(), Do::End(pid, memory)),
        ],
    )
}

/// `left`, then `right` in a column at the menu's right edge (a tab does that on Windows).
fn row(left: &str, right: &str) -> String {
    if cfg!(windows) {
        format!("{left}\t{right}")
    } else {
        format!("{left}   {right}")
    }
}

/// Sizes rounded down to 10 MB, so the menu doesn't change with every sample.
fn steady(bytes: u64) -> String {
    status::size(bytes / (10 << 20) * (10 << 20))
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn place(panel: bool) -> &'static str {
    if panel { "editor" } else { "terminal" }
}

/// Folder names, told apart where sessions share one: by where they run, then by number.
fn tell_apart(items: &[(String, &str)]) -> Vec<String> {
    let base: Vec<String> = items
        .iter()
        .map(|(name, at)| if items.iter().filter(|(n, _)| n == name).count() > 1 { format!("{name} · {at}") } else { name.clone() })
        .collect();
    base.iter()
        .enumerate()
        .map(|(i, b)| match base[..i].iter().filter(|o| *o == b).count() {
            0 => b.clone(),
            before => format!("{b} {}", before + 1),
        })
        .collect()
}

fn folder_name(v: &Value) -> String {
    let cwd = v["cwd"].as_str().unwrap_or("");
    Path::new(cwd).file_name().map_or_else(|| cwd.to_string(), |n| n.to_string_lossy().into_owned())
}

/// Does what a menu entry says; returns a note for the menu when the result is ours to tell.
fn perform(what: Do) -> Option<String> {
    let result = match what {
        // The session answers in its status file; the menu shows that.
        Do::Session(pid, action) => return control::send(pid, action).err().map(|e| format!("couldn't reach it: {e}")),
        Do::NapAll(pids) => {
            for &pid in &pids {
                let _ = control::send(pid, Action::Nap);
            }
            Ok(format!("asked {} session{} to nap", pids.len(), plural(pids.len())))
        }
        Do::MoveIn(pid) => {
            let Some(c) = adopt::candidates(&status::sessions(now_ms())).into_iter().find(|c| c.pid == pid) else {
                return Some("that session is gone".into());
            };
            if !c.panel && !confirm(TERMINAL_WARNING) {
                return None;
            }
            let name = Path::new(&c.cwd).file_name().map_or_else(|| c.cwd.clone(), |n| n.to_string_lossy().into_owned());
            adopt::move_in(&c).map_err(std::io::Error::other).map(|()| {
                if c.panel { format!("{name} moved into ccnap; your next message there resumes it") } else { format!("{name} restarted in its terminal through ccnap") }
            })
        }
        Do::MoveAll => {
            let found = adopt::candidates(&status::sessions(now_ms()));
            let ready: Vec<&Candidate> = found.iter().filter(|c| c.ready()).collect();
            if ready.iter().any(|c| !c.panel) && !confirm(TERMINAL_WARNING) {
                return None;
            }
            let (moved, failed): (Vec<_>, Vec<_>) = ready.into_iter().map(adopt::move_in).partition(Result::is_ok);
            let failed = if failed.is_empty() { String::new() } else { format!(", {} couldn't", failed.len()) };
            Ok(format!("moved {} session{} into ccnap{failed}", moved.len(), plural(moved.len())))
        }
        Do::Exclude(dir, on) => config::set_excluded(&dir, on).map(|()| {
            let name = dir.file_name().map_or_else(|| dir.display().to_string(), |n| n.to_string_lossy().into_owned());
            if on { format!("sessions in {name} won't nap") } else { format!("sessions in {name} can nap again") }
        }),
        Do::End(pid, memory) => {
            let origin = adopt::origin(pid);
            let question = format!("End Claude process {pid}? It uses {}. {}. {}", status::size(memory), origin.about, origin.cost);
            if !confirm(&question) {
                return None;
            }
            proc::end_tree(pid);
            Ok(format!("ended a Claude process ({} freed)", status::size(memory)))
        }
        Do::Set(key, value) => config::set(key, &value).map(|()| "settings saved".to_string()),
        Do::OpenSettings => {
            let path = config::path();
            if !path.exists() {
                let _ = config::set("idle", &format!("\"{}s\"", config::get().idle_ms / 1000));
            }
            open(&path).map(|()| String::new())
        }
        Do::Login(on) => install::login::set(on.then(|| std::env::current_exe().ok()).flatten().as_deref())
            .map(|()| if on { "starts at login".into() } else { "won't start at login".into() }),
        Do::Quit => return None,
    };
    match result {
        Ok(text) if text.is_empty() => None,
        Ok(text) => Some(text),
        Err(e) => Some(format!("failed: {e}")),
    }
}

/// A native menu item for `e`, noting what each clickable one does.
fn build(e: &Entry, actions: &mut HashMap<MenuId, Do>) -> Box<dyn IsMenuItem> {
    // Windows menus read `&` as a shortcut marker.
    let text = |s: &str| if cfg!(windows) { s.replace('&', "&&") } else { s.to_string() };
    match e {
        Entry::Text(t) => Box::new(MenuItem::new(text(t), false, None)),
        Entry::Separator => Box::new(PredefinedMenuItem::separator()),
        Entry::Item(t, what) => {
            let item = MenuItem::new(text(t), true, None);
            actions.insert(item.id().clone(), what.clone());
            Box::new(item)
        }
        Entry::Check(t, checked, what) => {
            let item = CheckMenuItem::new(text(t), true, *checked, None);
            actions.insert(item.id().clone(), what.clone());
            Box::new(item)
        }
        Entry::Sub(t, children) => {
            let sub = Submenu::new(text(t), true);
            for c in children {
                let _ = sub.append(build(c, actions).as_ref());
            }
            Box::new(sub)
        }
    }
}

/// Whether one of this tray's menus is open now. Open menus are windows of class "#32768".
#[cfg(windows)]
fn menu_open() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowExW, GetWindowThreadProcessId, IsWindowVisible};
    let class: Vec<u16> = "#32768".encode_utf16().chain([0]).collect();
    let mut w = std::ptr::null_mut();
    loop {
        w = unsafe { FindWindowExW(std::ptr::null_mut(), w, class.as_ptr(), std::ptr::null()) };
        if w.is_null() {
            return false;
        }
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(w, &mut pid) };
        if pid == std::process::id() && unsafe { IsWindowVisible(w) } != 0 {
            return true;
        }
    }
}

// ponytail: always "no" on macOS, untested there; if a refresh closes an open menu on a Mac,
// track it with an NSMenu delegate.
#[cfg(not(windows))]
fn menu_open() -> bool {
    false
}

/// Opens a file with the system's text editor.
fn open(path: &Path) -> std::io::Result<()> {
    let mut cmd = if cfg!(windows) { Command::new("notepad") } else { Command::new("open") };
    if cfg!(target_os = "macos") {
        cmd.arg("-t");
    }
    cmd.arg(path).spawn().map(drop)
}

/// Asks a yes/no question; true for yes.
#[cfg(windows)]
fn confirm(question: &str) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, IDYES, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO};
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (text, caption) = (wide(question), wide("ccnap"));
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), MB_YESNO | MB_ICONWARNING | MB_SETFOREGROUND | MB_TOPMOST) == IDYES }
}

#[cfg(target_os = "macos")]
fn confirm(question: &str) -> bool {
    let q = question.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(r#"display dialog "{q}" buttons {{"Cancel", "Continue"}} default button "Cancel" cancel button "Cancel" with icon caution"#);
    Command::new("osascript").args(["-e", &script]).status().is_ok_and(|s| s.success())
}

/// A 32×32 icon: Claude's orange with a crescent moon.
fn icon() -> Icon {
    const N: usize = 32;
    let mut px = vec![0u8; N * N * 4];
    for y in 0..N {
        for x in 0..N {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let dist = |cx: f32, cy: f32| ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
            let disc = (15.5 - dist(16.0, 16.0)).clamp(0.0, 1.0);
            let moon = (9.5 - dist(14.5, 16.5)).clamp(0.0, 1.0) * (dist(19.0, 12.5) - 8.0).clamp(0.0, 1.0);
            let mix = |a: f32, b: f32| (a + (b - a) * moon) as u8;
            let i = (y * N + x) * 4;
            px[i..i + 4].copy_from_slice(&[mix(217.0, 255.0), mix(119.0, 255.0), mix(87.0, 255.0), (disc * 255.0) as u8]);
        }
    }
    Icon::from_rgba(px, N as u32, N as u32).expect("a 32x32 RGBA icon")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(pid: u32, napping: bool, cwd: &str) -> Value {
        // Built by this ccnap, so not outdated.
        let exe = std::env::current_exe().unwrap().to_string_lossy().into_owned();
        let build = fs::metadata(&exe).unwrap().modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        json!({ "pid": pid, "napping": napping, "cwd": cwd, "mode": "panel", "memory": 300 << 20, "freed": 280 << 20,
                "why": "tab has focus", "exe": exe, "build": build })
    }

    fn find<'a>(entries: &'a [Entry], label: &str) -> &'a Entry {
        entries.iter().find(|e| matches!(e, Entry::Sub(t, _) | Entry::Item(t, _) | Entry::Text(t) if t.starts_with(label))).unwrap_or_else(|| panic!("no {label} in {entries:#?}"))
    }

    fn sub<'a>(entries: &'a [Entry], label: &str) -> &'a [Entry] {
        let Entry::Sub(_, items) = find(entries, label) else { panic!("{label} isn't a submenu") };
        items
    }

    fn label<'a>(entries: &'a [Entry], start: &str) -> &'a str {
        let (Entry::Sub(t, _) | Entry::Item(t, _) | Entry::Text(t)) = find(entries, start) else { panic!() };
        t
    }

    #[test]
    fn menu_offers_what_each_session_can_do() {
        let cfg = Config { exclude_dirs: vec!["/w/b".into()], ..Config::default() };
        let sessions = [session(1, true, "/w/a"), session(2, false, "/w/b")];
        let (tip, menu) = model(&sessions, &[(9, 3 << 30, adopt::LEFTOVER)], &[], &cfg, true, None, 0);
        assert_eq!(tip, "ccnap: saving 280 MB");
        assert_eq!(menu[0], Entry::Text("Saving 280 MB · 1 of 2 napping".into()));
        // Awake sessions first, with their memory; napping ones with what they saved.
        let at = |l: &str| menu.iter().position(|e| matches!(e, Entry::Sub(t, _) if t.starts_with(l))).unwrap();
        assert!(at("● b") < at("💤 a"));
        assert_eq!(label(&menu, "● b"), row("● b", "300 MB"));
        assert_eq!(label(&menu, "💤 a"), row("💤 a", "280 MB saved"));
        let napped = sub(&menu, "💤 a");
        assert!(napped.contains(&Entry::Text("In the editor · napping, 280 MB saved".into())));
        assert!(napped.contains(&Entry::Item("Wake now".into(), Do::Session(1, Action::Wake))));
        assert!(napped.contains(&Entry::Check("Never nap this folder".into(), false, Do::Exclude("/w/a".into(), true))));
        let awake = sub(&menu, "● b");
        assert!(awake.contains(&Entry::Text("In the editor · awake: tab has focus".into())));
        assert!(awake.contains(&Entry::Item("Nap now".into(), Do::Session(2, Action::Nap))));
        assert!(awake.contains(&Entry::Check("Never nap this folder".into(), true, Do::Exclude("/w/b".into(), false))));
        // Processes outside ccnap are tucked away in one submenu.
        let leftover = sub(sub(&menu, "Outside ccnap (1)"), "○ Leftover Claude");
        assert!(leftover.contains(&Entry::Item("End this process…".into(), Do::End(9, 3 << 30))));
        assert_eq!(*find(&menu, "Nap idle"), Entry::Item("Nap idle sessions now".into(), Do::NapAll(vec![2])));
        // Settings too.
        let settings = sub(&menu, "Settings");
        assert!(sub(settings, "Nap after").contains(&Entry::Check("10 minutes".into(), true, Do::Set("idle", "\"10m\"".into()))));
        assert!(settings.contains(&Entry::Check("Start at login".into(), true, Do::Login(false))));
        assert_eq!(menu.len(), 11, "{menu:#?}");
    }

    #[test]
    fn menu_shows_the_latest_answer_and_older_sessions() {
        let mut answered = session(3, false, "/w/c");
        answered["last_action"] = json!({ "action": "nap", "result": "not now: claude is working", "at": 5_000 });
        let mut old = session(4, false, "/w/d");
        old["build"] = json!(1);
        let (_, menu) = model(&[answered, old], &[], &[], &Config::default(), false, Some((4_000, "settings saved")), 6_000);
        assert_eq!(menu[0], Entry::Text("2 sessions, all awake".into()));
        assert_eq!(menu[1], Entry::Text("Last: c: not now: claude is working".into()));
        let items = sub(&menu, "● d (old version)");
        assert!(!items.iter().any(|e| matches!(e, Entry::Item(..) | Entry::Check(..))), "an older ccnap can't take requests");
        assert_eq!(*find(&menu, "Nap idle"), Entry::Item("Nap idle sessions now".into(), Do::NapAll(vec![3])));
        // Results go stale after a minute.
        let (_, menu) = model(&[], &[], &[], &Config::default(), false, Some((0, "settings saved")), NOTE_MS + 1);
        assert_eq!(menu[0], Entry::Text("No Claude Code sessions running".into()));
        assert!(!menu.iter().any(|e| matches!(e, Entry::Text(t) if t.starts_with("Last"))));
    }

    #[test]
    fn sessions_in_the_same_folder_are_told_apart() {
        let mut terminal = session(2, false, "/x/app");
        terminal["mode"] = json!("terminal");
        let sessions = [session(1, false, "/x/app"), terminal, session(3, false, "/y/app"), session(4, false, "/z/web")];
        let (_, menu) = model(&sessions, &[], &[], &Config::default(), false, None, 0);
        for name in ["● app · editor", "● app · terminal", "● app · editor 2", "● web"] {
            let want = row(name, "300 MB");
            assert!(menu.iter().any(|e| matches!(e, Entry::Sub(t, _) if *t == want)), "no {want:?} in {menu:#?}");
        }
    }

    #[test]
    fn menu_moves_other_sessions_into_ccnap() {
        let mut old = session(4, false, "/w/d");
        old["build"] = json!(1);
        let c = |pid, panel, old_ccnap, wait, cwd: &str| Candidate {
            pid, session: "S".into(), cwd: cwd.into(), panel, old_ccnap, memory: 1 << 30, wait, shell: None,
        };
        let candidates = [
            c(4, true, true, None, "/w/d"),
            c(7, true, false, None, "/w/e"),
            c(8, false, false, None, "/w/f"),
            c(9, true, false, Some("claude is working"), "/w/g"),
        ];
        let leftovers = [(7, 1 << 30, adopt::LEFTOVER), (5, 2 << 30, adopt::LEFTOVER)];
        let (_, menu) = model(&[old.clone()], &leftovers, &candidates, &Config::default(), false, None, 0);
        // One click for all of them; it asks first, as one is in a terminal.
        assert_eq!(*find(&menu, "Move 3"), Entry::Item("Move 3 sessions into ccnap…".into(), Do::MoveAll));
        assert!(sub(&menu, "● d (old version)").contains(&Entry::Item("Update now (keeps the chat)".into(), Do::MoveIn(4))));
        // A leftover that can move in is listed once, as a session.
        let outside = sub(&menu, "Outside ccnap (4)");
        assert!(outside.contains(&Entry::Text("Editor chats move in by themselves once idle.".into())));
        assert!(sub(outside, "↻ e · editor").contains(&Entry::Item("Move into ccnap (keeps the chat)".into(), Do::MoveIn(7))));
        assert!(sub(outside, "↻ g").contains(&Entry::Text("Can move in once idle (claude is working)".into())));
        assert_eq!(outside.iter().filter(|e| matches!(e, Entry::Sub(t, _) if t.starts_with('○'))).count(), 1);
        let settings = sub(&menu, "Settings");
        assert!(settings.contains(&Entry::Check("Move other sessions in automatically".into(), true, Do::Set("adopt", "\"off\"".into()))));
        // Only older ccnaps ready: that's an update.
        let (_, menu) = model(&[old], &[], &candidates[..1], &Config::default(), false, None, 0);
        assert_eq!(*find(&menu, "Update 1"), Entry::Item("Update 1 session to this ccnap".into(), Do::MoveAll));
    }

    #[test]
    fn icon_is_built() {
        icon();
    }
}
