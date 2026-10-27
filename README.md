<p align="center"><img src="assets/logo.svg" width="128" height="128" alt="Hibernaut: a dormouse curled up asleep under its tail"></p>

# Hibernaut

Puts idle Claude Code sessions to sleep so you get your RAM back, and wakes them the moment you return. It works in every terminal and in the chat panel of VS Code-family editors, on Windows, macOS and Linux.

Hibernaut was called ccnap until October 2026. Installing it takes over from ccnap by itself: it moves your settings, takes ccnap off PATH and out of your editors, and moves ccnap's open sessions over like any older session.

If you keep a dozen Claude Code sessions open, each idle one keeps hundreds of MB resident, plus a process for every MCP server, and some leak far past that. Hibernaut sits between Claude Code and whatever started it:

- **Naps idle sessions.** Once a session has finished its turn and nobody has touched it for 10 minutes, Hibernaut stops Claude and its MCP servers. When you come back, it resumes the session with `claude --resume`, and your conversation is right where you left it.
- **Restarts leaking sessions.** A session that grows past 2 GB is restarted at its next quiet moment, even while you're looking at it, with the conversation intact.
- **Shows you everything.** `hibernaut status` lists every session, whether it's awake or napping and why, and how much memory it holds or freed. It also lists claude processes left running outside Hibernaut, such as leftovers from closed editor tabs.

Measured on Windows with Claude Code 2.1.301:

- An idle session's process tree (claude plus its MCP servers) used **343 MB across 6 processes**.
- While it napped, only Hibernaut remained, at **0.7 MB**.
- On the same machine, `hibernaut status` found 5 claude processes left running outside Hibernaut holding **6.0 GB**, one of them at 5.0 GB.

It is a single binary of about 1 MB, with no runtime.

## Install

```sh
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/abdulhaseeb7603/hibernaut/main/install.sh | sh
```

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/abdulhaseeb7603/hibernaut/main/install.ps1 | iex
```

With Rust, run `cargo install --git https://github.com/abdulhaseeb7603/hibernaut`, then `hibernaut setup`.

The binaries aren't code-signed yet, so Windows SmartScreen or your antivirus may ask before the first run.

`hibernaut setup` copies Hibernaut to `~/.hibernaut` (on Windows, `%LOCALAPPDATA%\hibernaut`) and routes Claude Code through it:

- **Editor chat panels:** in each VS Code-family editor it finds (VS Code, VS Code Insiders, VSCodium, Cursor, Windsurf, Antigravity, Kiro, Trae, Positron, Void), it sets `claudeCode.claudeProcessWrapper` to Hibernaut. The Claude Code extension then starts Hibernaut instead of its built-in claude, passing it the path to that claude. If you already set a wrapper of your own, Hibernaut leaves it alone.
- **Everything that runs `claude` from a shell:** it puts a `claude` that is really Hibernaut first on your PATH. On Windows this goes in your user PATH; elsewhere it goes in your shell's startup files. That covers every terminal, tmux, SSH, Claude Code's terminal mode in editors, JetBrains IDEs, and editor plugins that open `claude` in a terminal.

- **The tray icon (Windows and macOS):** it installs `hibernaut-tray`, starts it, and has it start at login.

New chat tabs use Hibernaut right away. Open a new terminal, or restart the editor, for the PATH change. Sessions that were already open move in on their own (see [Sessions that were already running](#sessions-that-were-already-running)). `hibernaut uninstall` reverses everything.

## The tray icon

A small icon by the clock (in the menu bar on macOS) shows every session, and its menu lets you act on them:

- **At the top:** what Hibernaut saves right now, for example "Saving 884 MB · 3 of 5 napping", and the result of your last action.
- **Move N sessions into Hibernaut:** shown when sessions started before Hibernaut (or on an older version) can move in. One click moves them all and keeps every chat; it asks first if any of them is in a terminal.
- **Your sessions:** awake ones (●) first with the memory they use, then napping ones (💤) with what they save. Two sessions in the same folder are told apart by where they run. Open one to:
  - **Wake now**, so it's ready before you get there.
  - **Nap now**, without waiting for the idle time. It still refuses while Claude is working or there's unsent text in the prompt, and says why.
  - **Restart**, which frees leaked memory and keeps the chat.
  - **Never nap this folder.**
- **Outside Hibernaut (N):** Claude processes that don't nap, kept out of the way: sessions that can still move in (↻), and others (○), such as a program built on the Claude Agent SDK, or a leftover from a closed tab, with **End this process…** (it asks first).
- **Nap idle sessions now.**
- **Settings:** the idle time (5, 10 or 30 minutes, or 1 hour), the memory limit for restarts (1, 2 or 4 GB, or never), **Move other sessions in automatically**, **Start at login**, and **Open settings file**.

`hibernaut-tray --preview` opens the menu filled with sample sessions, to see how it looks.

The tray is its own small program and uses about 9 MB. Sessions don't depend on it: each still uses under 1 MB while napping, and does all its own safety checks. On Linux there's no tray icon yet; use the commands below.

## Commands

| Command | What it does |
| --- | --- |
| `hibernaut` | Same as `claude`; all claude flags work |
| `hibernaut setup` / `hibernaut uninstall` | Route every terminal and editor through Hibernaut, or stop |
| `hibernaut status` | Every session: awake or napping and why, memory held or freed, and claude processes Hibernaut doesn't manage |
| `hibernaut nap <pid\|all>` | Nap a session now; it refuses while Claude works or text is unsent, and says why |
| `hibernaut wake <pid\|all>` | Wake a napping session now |
| `hibernaut restart <pid>` | Restart a session to free leaked memory, conversation kept |
| `hibernaut adopt [--terminals]` | Move idle sessions that don't run through Hibernaut into it (Windows) |
| `hibernaut config` | Where the settings file is, and its current values |
| `hibernaut version` | Hibernaut's version (`hibernaut --version` prints Claude Code's) |

## When a session naps

A session naps only when all of these are true:

- Claude has finished its turn, and has no background shell, agent, monitor or `/loop` cron still running.
- Nothing is waiting on an answer, such as a permission prompt or a request between the extension and Claude.
- The session can come back: either it has a saved transcript to resume from, or nothing has been sent in it yet. A new chat tab nobody typed in naps too, and wakes as a fresh chat under the same session ID. Only a chat with messages but no saved transcript (saving turned off) stays awake, since a restart would lose it.
- Nobody has typed in it, focused it, or sent it anything for the idle time (10 minutes by default).
- **In a terminal**, also: there is no unsent text in the prompt, Claude hasn't drawn anything for 30 seconds, and the tab doesn't have focus. Hibernaut skips the focus check in terminals that don't report focus.

Hibernaut knows when a turn is over from Claude Code's own hooks, not by reading the screen, so a redesigned dialog can't make a busy session look idle. A napping terminal tab is marked with 💤 in its title.

## When it wakes

- **In a terminal**, focusing the tab or typing wakes it. Whatever you type or paste is held until Claude's prompt is ready, then typed in for you, and a pasted block stays one paste. Hibernaut runs `claude --resume <id>` with your original flags, in the session's last working directory, and in the permission mode it was last in.
- **In an editor panel**, the next message you send wakes it. The panel never notices the swap. Hibernaut starts claude again with `--resume`, then replays the setup the extension sent the first time: the initial handshake, the model, thinking settings, and MCP toggles. It also puts back the permission mode the session was last in, even one Claude changed itself, such as after you allowed all edits. That first reply takes about as long as opening a new chat, because MCP servers have to reconnect.

In a Unix terminal, Ctrl+Z suspends the session and `fg` brings it back, as with any other program.

## Updates

Installing a newer version (running the install command again) updates open sessions too, with nothing to reload:

- **In an editor panel**, a napping session hands itself to the new Hibernaut: it ends the way a session normally ends, and your next message starts it through the new one.
- **In a terminal**, a napping session hands itself over when you come back to it, and what you typed to wake it is passed along.

Awake sessions update the next time they nap.

## Sessions that were already running

If you had 30 Claude Code sessions open when you installed Hibernaut, you don't have to restart them one by one. Claude Code records each running session (its ID, folder, and whether it's idle), so Hibernaut knows which chat each process holds.

- **Editor panels move in by themselves** while the tray icon runs. Once a session has been idle for 2 minutes, with no background task running, Hibernaut ends its claude the way a session normally ends. The panel stays as it is, with any unsent text, and your next message resumes the chat through Hibernaut. This also moves sessions still running a Hibernaut from before self-updating existed. Turn it off with `adopt = "off"`, or from the tray.
- **Terminals move in with one click,** from the tray or with `hibernaut adopt --terminals`. Hibernaut ends claude and types `hibernaut --resume <id>` into the same terminal. This isn't automatic, because text typed but not sent in claude's prompt would be lost.

Moving sessions in is Windows-only for now. On macOS and Linux, one process can't choose another's exit code, so the editor would report a crash, and nothing can type into another terminal. There, reload the editor window and restart terminal sessions once.

## Settings

`hibernaut setup` creates `config.toml` in Hibernaut's folder (`hibernaut config` shows where). Edits apply to running sessions within a few seconds:

```toml
idle = "10m"            # how long a session sits untouched before it naps
memory_limit = "2GB"    # restart a session that grows past this; "off" to disable
exclude_dirs = []       # sessions working in these folders never nap or restart
adopt = "auto"          # move idle panel sessions that don't run through Hibernaut into it; "off" to only do it from the tray
# log = "~/hibernaut.log"   # log every decision, to see why a session stayed awake
```

Environment variables override the file: `HIBERNAUT_IDLE`, `HIBERNAUT_MEMORY_LIMIT`, `HIBERNAUT_EXCLUDE` (a PATH-style list), `HIBERNAUT_LOG`, and `HIBERNAUT_CLAUDE` (the claude to run). To set them for editor panels, use the extension's `claudeCode.environmentVariables` setting.

## How it works

Both modes inject Claude Code hooks through a temporary `--settings` file. The hooks are `SessionStart`, `UserPromptSubmit`, `Stop`, `StopFailure` and `Notification` (`idle_prompt`), and they call back into `hibernaut hook`. Claude Code merges these hooks with yours, and your settings files are never touched. The hooks report the session ID, working directory and permission mode, whether a turn is running, and whether background tasks are still going.

- **Terminal mode:** Hibernaut runs claude inside a pseudo-terminal of its own. It turns on terminal focus reporting (`CSI ?1004h`) and sorts the input into keystrokes, focus changes, and the terminal's answers to Claude's queries. Only real keystrokes count as use or as a draft.
- **Panel mode:** the extension talks to Claude through the Agent SDK's JSON-lines protocol, and Hibernaut relays every line unchanged. Along the way it tracks turns and control requests still waiting for an answer, and records the requests that set up the session so it can replay them. Answers to those replays are never passed on to the extension.
- **Memory:** Hibernaut measures claude's whole process tree every 15 seconds. That number shows in `hibernaut status`, appears in the nap message as "300 MB freed", and decides when a leaking session gets restarted. After a restart for memory, another one happens only if the new claude first stays under the limit for 10 minutes, so a legitimately large session isn't restarted over and over.

## Similar tools

- [claude-hibernate](https://github.com/novarii/claude-hibernate) (Rust) does the terminal half of this on Linux and macOS: it replays typed keys, handles Ctrl+Z, and has a config file and a Herdr plugin.
- [tmux-agent-hibernator](https://github.com/amack87/tmux-agent-hibernator) does it for tmux panes, and also handles Codex and Cursor.

Hibernaut adds:

- **Windows**
- **editor chat panels**
- **hook-based detection** instead of reading the screen, which also works with your own `--settings`
- **waking on focus**
- **memory-leak restarts**
- **`hibernaut status`**, which also reports claude processes running outside it
- a **tray icon** that shows every session and lets you nap, wake, restart or exclude it, and end leftovers
- **moving sessions that were already running** into it, without losing the chat
- **updates that reach open sessions**, with no reload
- the **💤 tab title**

## Limitations

- **Not covered:** the Claude Desktop app, background agents (`claude --bg`), cloud sessions, and other apps that embed the Agent SDK with their own Claude binary. `hibernaut status` still shows such processes if they run claude.
- **VS Code Remote (SSH, WSL, Dev Containers):** the extension runs on the remote machine, so install Hibernaut there too. `hibernaut setup` there points the chat panel at it for VS Code, VS Code Insiders, VSCodium, Cursor, Windsurf and Antigravity.
- **Stopped turns in a panel:** if you stop Claude in a panel and send nothing after, that session stays awake. A stopped turn skips the `Stop` hook, so Hibernaut can't be sure no background tasks are left. It naps normally after your next completed turn.
- **Extra panel history:** with a process wrapper set, the extension also lists sessions started outside it, such as `claude -p` runs, in its session history.
- **Hooks must be able to run:** a session never naps under `--bare` or `--safe-mode`, with `disableAllHooks`, or when managed settings set `allowManagedHooksOnly`. Hibernaut also needs a Claude Code version with exec-form hooks (`"args"` in a hook entry). It was developed against 2.1.301 to 2.1.311.

## Development

```sh
cargo test
```

This runs the unit tests and end-to-end tests around a fake `claude` (`examples/fake_claude.rs`), on a real pseudo-terminal and real pipes:

- **Terminal naps and wakes:** Hibernaut stays awake while focused, naps when the tab loses focus, shows up in `hibernaut status`, resumes on focus with keys typed right after it kept, and resumes on a paste that arrives as one paste rather than a prompt per line.
- **Leak restart:** a bloated claude is restarted even with the tab in focus, and only once when the restarted claude grows past the limit again.
- **Panel mode:** Hibernaut naps while only keep-alives flow, and resumes with the same session, initialize request, model, and the permission mode a tool approval left it in, without leaking replay answers.
- **Empty tabs:** a tab nobody typed in naps and comes back as a fresh chat under the same session ID.
- **Menu actions:** `hibernaut nap`, `wake` and `restart` act on terminal and panel sessions, and a nap is refused while there's unsent text.
- **Updates:** replacing the Hibernaut binary while a session naps makes a panel session exit cleanly, and a terminal session hand itself to the new binary on wake with the waking keys intact.
- **Moving sessions in (Windows):** a claude that isn't under Hibernaut is left alone while busy or recently used, then ended with exit code 0 once idle; and `hibernaut __type` types into another terminal.

The tray's menu is built as plain data and unit-tested; `cargo run --bin hibernaut-tray` shows the real one.

## License

MIT
