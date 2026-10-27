<p align="center"><img src="assets/logo.svg" width="128" height="128" alt="Hibernaut logo: a dormouse curled up asleep under its tail"></p>

<h1 align="center">Hibernaut</h1>

<p align="center"><b>Idle Claude Code sessions go to sleep and give your RAM back. They wake up the moment you return, with the chat exactly where you left it.</b></p>

<p align="center">Windows · macOS · Linux &nbsp;|&nbsp; Terminals · VS Code, Cursor, Antigravity and other VS Code-family editors</p>

---

Every open Claude Code session holds hundreds of MB of memory, plus one process per MCP server, even when you haven't touched it in hours. Some sessions slowly grow to several GB. With 10 to 40 sessions open, your machine runs out of memory.

Hibernaut fixes that without changing how you work:

- 💤 **Sleeps idle sessions.** A session nobody has used for 10 minutes is stopped, MCP servers included.
- ⚡ **Wakes them instantly.** Focus the tab, type, or send a message, and it resumes the same chat.
- 🔁 **Restarts leaking sessions.** A session that grows past 2 GB is restarted at a quiet moment, chat kept.
- 🐭 **Tray icon.** See every session and what it uses; sleep, wake or restart any of them in one click.
- 🧹 **Finds leftovers.** Claude processes left behind by closed tabs show up, and you can end them.

Measured on Windows: one idle session with its MCP servers used **343 MB across 6 processes**. While it slept, only Hibernaut remained, at **0.7 MB**. Hibernaut is a single binary of about 1 MB with no runtime to install.

## Install

**1. Run one command.**

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/abdulhaseeb7603/hibernaut/main/install.ps1 | iex
```

macOS / Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/abdulhaseeb7603/hibernaut/main/install.sh | sh
```

**2. Open a new terminal**, or a new Claude Code chat tab in your editor. That's it.

From then on, every Claude Code session runs through Hibernaut: terminals, tmux, SSH, JetBrains terminals, and the chat panel of VS Code, VS Code Insiders, VSCodium, Cursor, Windsurf, Antigravity, Kiro, Trae, Positron and Void. On Windows and macOS the dormouse icon appears by the clock and starts with your computer.

> The binaries aren't code-signed yet, so Windows SmartScreen or your antivirus may ask once before the first run.
>
> With Rust you can build it instead: `cargo install --git https://github.com/abdulhaseeb7603/hibernaut`, then `hibernaut setup`.

### Already have Claude Code sessions open?

You don't need to restart them one by one.

- **Editor chats** move into Hibernaut by themselves once they've been idle for 2 minutes. Nothing changes in the panel, unsent text included, and your next message continues the same chat.
- **Terminals:** click **Move N sessions into Hibernaut** in the tray (or run `hibernaut adopt --terminals`). Claude restarts in the same terminal with the same chat. It asks first, because text you typed but didn't send would be lost.

This works on Windows. On macOS and Linux, reload your editor window and restart terminal sessions once.

### Updating

Run the install command again. Open sessions switch to the new version on their own, with no reload: a sleeping editor chat on your next message, a sleeping terminal when you come back to it, and awake sessions the next time they sleep.

### Uninstall

```sh
hibernaut uninstall
```

This takes Hibernaut out of your editors, PATH and startup, and closes the tray icon. Open sessions keep working until you close them, and no chat is ever lost. Then delete `%LOCALAPPDATA%\hibernaut` (Windows) or `~/.hibernaut`.

## The tray icon

Click the dormouse by the clock (in the menu bar on macOS):

```
Saving 590 MB · 2 of 4 napping
Move 1 session into Hibernaut
───────────────────────────────────────────
● voicetest                         800 MB  ›
● New folder (21) · terminal        290 MB  ›
💤 New folder (21) · editor    290 MB saved  ›
💤 postiz                      300 MB saved  ›
───────────────────────────────────────────
Outside Hibernaut (2)                       ›
───────────────────────────────────────────
Nap idle sessions now
Settings                                    ›
───────────────────────────────────────────
Quit tray icon
```

- **Each session** opens to **Wake now**, **Nap now**, **Restart** (frees leaked memory, keeps the chat) and **Never nap this folder**.
- **Outside Hibernaut** lists Claude processes that don't sleep: sessions that can still move in, programs built on the Claude Agent SDK, and leftovers from closed tabs, which you can end.
- **Settings** has the idle time, the memory limit, moving other sessions in automatically, and starting at login.

The tray uses about 9 MB, and sessions don't depend on it. Linux has no tray icon yet; use the commands below.

## Is it safe?

Hibernaut only puts a session to sleep when nothing can be lost:

- Claude has finished its turn, and no background shell, agent, monitor or `/loop` is still running.
- Nothing is waiting on you, such as a permission prompt.
- The chat is saved to disk, so it can be resumed. A new tab nobody typed in sleeps too, and comes back as the same empty chat.
- Nobody has typed, focused it or sent it anything for the idle time.
- In a terminal, also: no unsent text in the prompt, and the tab isn't focused.

It learns when a turn is over from Claude Code's own hooks, not by reading the screen. When it wakes a session, it restores the model, the permission mode and your editor's settings, and whatever you typed in the meantime is typed in for you.

## Commands

| Command | What it does |
| --- | --- |
| `hibernaut status` | Every session, awake or asleep and why, with the memory it holds or freed |
| `hibernaut nap <pid\|all>` | Put a session to sleep now (it refuses if that would lose anything, and says why) |
| `hibernaut wake <pid\|all>` | Wake a sleeping session now |
| `hibernaut restart <pid>` | Restart a session to free leaked memory, chat kept |
| `hibernaut adopt [--terminals]` | Move sessions that don't run through Hibernaut into it (Windows) |
| `hibernaut config` | Show where the settings file is and what it says |
| `hibernaut setup` / `uninstall` | Install, or remove everything |
| `hibernaut` | Same as `claude`, with every claude flag |

## Settings

Change them from the tray, or edit `config.toml` (`hibernaut config` shows where it is). Changes apply to running sessions within seconds.

```toml
idle = "10m"            # how long a session sits untouched before it sleeps
memory_limit = "2GB"    # restart a session that grows past this; "off" to disable
exclude_dirs = []       # sessions in these folders never sleep or restart
adopt = "auto"          # move idle editor chats into Hibernaut by themselves; "off" to use the tray only
# log = "~/hibernaut.log"   # record every decision, to see why a session stayed awake
```

Environment variables override the file: `HIBERNAUT_IDLE`, `HIBERNAUT_MEMORY_LIMIT`, `HIBERNAUT_EXCLUDE` (a PATH-style list), `HIBERNAUT_LOG` and `HIBERNAUT_CLAUDE` (the claude to run). For editor chats, set them in the extension's `claudeCode.environmentVariables` setting.

## Good to know

- **Not covered:** the Claude Desktop app, background agents (`claude --bg`), cloud sessions, and apps that bundle their own Claude. `hibernaut status` still lists them if they run claude.
- **VS Code Remote (SSH, WSL, Dev Containers):** the extension runs on the remote machine, so install Hibernaut there too.
- **A turn you stopped in an editor chat** keeps that session awake until your next completed turn, because Claude Code skips its end-of-turn hook then.
- **Hooks must be allowed:** a session never sleeps under `--bare` or `--safe-mode`, with `disableAllHooks`, or when managed settings allow only managed hooks.
- **Editor chat history** also lists sessions started outside the editor, such as `claude -p` runs, once a process wrapper is set.
- Tested with Claude Code 2.1.301 to 2.1.311.

## How it works

`hibernaut setup` sets `claudeCode.claudeProcessWrapper` in each editor, and puts a `claude` that is really Hibernaut first on your PATH. If you already use a wrapper of your own, Hibernaut leaves it alone.

Each session gets Claude Code hooks through a temporary `--settings` file. Your own settings files are never changed. The hooks report when a turn starts and ends, the session ID, the permission mode, and whether background tasks are running.

- **In a terminal**, Hibernaut runs claude in a pseudo-terminal of its own and turns on focus reporting, so it can tell focus changes, real keystrokes and unsent text apart.
- **In an editor chat**, it relays the extension's JSON-lines protocol unchanged, and records the setup requests so it can replay them when the session wakes.
- **Memory** is measured across claude's whole process tree every 15 seconds. After a restart for memory, another happens only once the new claude has first stayed under the limit for 10 minutes.

## Compared with similar tools

[claude-hibernate](https://github.com/novarii/claude-hibernate) handles terminals on Linux and macOS, and [tmux-agent-hibernator](https://github.com/amack87/tmux-agent-hibernator) handles tmux panes. Hibernaut also covers Windows, editor chat panels, waking on focus, memory-leak restarts, leftover processes, a tray icon, moving sessions that were already open, and updates that reach open sessions. It detects idle sessions through hooks rather than by reading the screen.

## Development

```sh
cargo test                            # unit tests, plus end-to-end tests around a fake claude
cargo run --bin hibernaut-tray -- --preview   # the tray menu, filled with sample sessions
```

The end-to-end tests (`tests/e2e.rs`, with `examples/fake_claude.rs`) drive the real binary on a real pseudo-terminal and real pipes. They cover sleeping and waking in terminals and editor chats, typed and pasted keys, empty tabs, memory restarts, the nap, wake and restart commands, updates while a session sleeps, and, on Windows, moving sessions in. CI runs them on Windows, macOS and Linux.

## License

MIT
