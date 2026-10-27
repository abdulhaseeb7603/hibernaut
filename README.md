# ccnap

Puts idle Claude Code sessions to sleep so you get your RAM back, and wakes them the moment you return. It works in every terminal and in the chat panel of VS Code-family editors, on Windows, macOS and Linux.

If you keep a dozen Claude Code sessions open, each idle one keeps hundreds of MB resident, plus a process for every MCP server, and some leak far past that. ccnap sits between Claude Code and whatever started it:

- **Naps idle sessions.** Once a session has finished its turn and nobody has touched it for 10 minutes, ccnap stops Claude and its MCP servers. When you come back, it resumes the session with `claude --resume`, and your conversation is right where you left it.
- **Restarts leaking sessions.** A session that grows past 2 GB is restarted at its next quiet moment, even while you're looking at it, with the conversation intact.
- **Shows you everything.** `ccnap status` lists every session, whether it's awake or napping and why, and how much memory it holds or freed. It also lists claude processes left running outside ccnap, such as leftovers from closed editor tabs.

Measured on Windows with Claude Code 2.1.301:

- An idle session's process tree (claude plus its MCP servers) used **343 MB across 6 processes**.
- While it napped, only ccnap remained, at **0.7 MB**.
- On the same machine, `ccnap status` found 5 claude processes left running outside ccnap holding **6.0 GB**, one of them at 5.0 GB.

It is a single binary of under 1 MB, with no runtime.

## Install

```sh
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/abdulhaseeb7603/ccnap/main/install.sh | sh
```

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/abdulhaseeb7603/ccnap/main/install.ps1 | iex
```

With Rust, run `cargo install --git https://github.com/abdulhaseeb7603/ccnap`, then `ccnap setup`.

The binaries aren't code-signed yet, so Windows SmartScreen or your antivirus may ask before the first run.

`ccnap setup` copies ccnap to `~/.ccnap` (on Windows, `%LOCALAPPDATA%\ccnap`) and routes Claude Code through it:

- **Editor chat panels:** in each VS Code-family editor it finds (VS Code, VS Code Insiders, VSCodium, Cursor, Windsurf, Antigravity, Kiro, Trae, Positron, Void), it sets `claudeCode.claudeProcessWrapper` to ccnap. The Claude Code extension then starts ccnap instead of its built-in claude, passing it the path to that claude. If you already set a wrapper of your own, ccnap leaves it alone.
- **Everything that runs `claude` from a shell:** it puts a `claude` that is really ccnap first on your PATH. On Windows this goes in your user PATH; elsewhere it goes in your shell's startup files. That covers every terminal, tmux, SSH, Claude Code's terminal mode in editors, JetBrains IDEs, and editor plugins that open `claude` in a terminal.

New chat tabs use ccnap right away. Open a new terminal, or restart the editor, for the PATH change. `ccnap uninstall` reverses everything.

## Commands

| Command | What it does |
| --- | --- |
| `ccnap` | Same as `claude`; all claude flags work |
| `ccnap setup` / `ccnap uninstall` | Route every terminal and editor through ccnap, or stop |
| `ccnap status` | Every session: awake or napping and why, memory held or freed, and claude processes ccnap doesn't manage |
| `ccnap config` | Where the settings file is, and its current values |
| `ccnap version` | ccnap's version (`ccnap --version` prints Claude Code's) |

## When a session naps

A session naps only when all of these are true:

- Claude has finished its turn, and has no background shell, agent, monitor or `/loop` cron still running.
- Nothing is waiting on an answer, such as a permission prompt or a request between the extension and Claude.
- The session can come back: either it has a saved transcript to resume from, or nothing has been sent in it yet. A new chat tab nobody typed in naps too, and wakes as a fresh chat under the same session ID. Only a chat with messages but no saved transcript (saving turned off) stays awake, since a restart would lose it.
- Nobody has typed in it, focused it, or sent it anything for the idle time (10 minutes by default).
- **In a terminal**, also: there is no unsent text in the prompt, Claude hasn't drawn anything for 30 seconds, and the tab doesn't have focus. ccnap skips the focus check in terminals that don't report focus.

ccnap knows when a turn is over from Claude Code's own hooks, not by reading the screen, so a redesigned dialog can't make a busy session look idle. A napping terminal tab is marked with 💤 in its title.

## When it wakes

- **In a terminal**, focusing the tab or typing wakes it. Whatever you type or paste is held until Claude's prompt is ready, then typed in for you, and a pasted block stays one paste. ccnap runs `claude --resume <id>` with your original flags, in the session's last working directory, and in the permission mode it was last in.
- **In an editor panel**, the next message you send wakes it. The panel never notices the swap. ccnap starts claude again with `--resume`, then replays the setup the extension sent the first time: the initial handshake, the model, thinking settings, and MCP toggles. It also puts back the permission mode the session was last in, even one Claude changed itself, such as after you allowed all edits. That first reply takes about as long as opening a new chat, because MCP servers have to reconnect.

In a Unix terminal, Ctrl+Z suspends the session and `fg` brings it back, as with any other program.

## Settings

`ccnap setup` creates `config.toml` in ccnap's folder (`ccnap config` shows where). Edits apply to running sessions within a few seconds:

```toml
idle = "10m"            # how long a session sits untouched before it naps
memory_limit = "2GB"    # restart a session that grows past this; "off" to disable
exclude_dirs = []       # sessions working in these folders never nap or restart
# log = "~/ccnap.log"   # log every decision, to see why a session stayed awake
```

Environment variables override the file: `CCNAP_IDLE`, `CCNAP_MEMORY_LIMIT`, `CCNAP_EXCLUDE` (a PATH-style list), `CCNAP_LOG`, and `CCNAP_CLAUDE` (the claude to run). To set them for editor panels, use the extension's `claudeCode.environmentVariables` setting.

## How it works

Both modes inject Claude Code hooks through a temporary `--settings` file. The hooks are `SessionStart`, `UserPromptSubmit`, `Stop`, `StopFailure` and `Notification` (`idle_prompt`), and they call back into `ccnap hook`. Claude Code merges these hooks with yours, and your settings files are never touched. The hooks report the session ID, working directory and permission mode, whether a turn is running, and whether background tasks are still going.

- **Terminal mode:** ccnap runs claude inside a pseudo-terminal of its own. It turns on terminal focus reporting (`CSI ?1004h`) and sorts the input into keystrokes, focus changes, and the terminal's answers to Claude's queries. Only real keystrokes count as use or as a draft.
- **Panel mode:** the extension talks to Claude through the Agent SDK's JSON-lines protocol, and ccnap relays every line unchanged. Along the way it tracks turns and control requests still waiting for an answer, and records the requests that set up the session so it can replay them. Answers to those replays are never passed on to the extension.
- **Memory:** ccnap measures claude's whole process tree every 15 seconds. That number shows in `ccnap status`, appears in the nap message as "300 MB freed", and decides when a leaking session gets restarted. After a restart for memory, another one happens only if the new claude first stays under the limit for 10 minutes, so a legitimately large session isn't restarted over and over.

## Similar tools

- [claude-hibernate](https://github.com/novarii/claude-hibernate) (Rust) does the terminal half of this on Linux and macOS: it replays typed keys, handles Ctrl+Z, and has a config file and a Herdr plugin.
- [tmux-agent-hibernator](https://github.com/amack87/tmux-agent-hibernator) does it for tmux panes, and also handles Codex and Cursor.

ccnap adds:

- **Windows**
- **editor chat panels**
- **hook-based detection** instead of reading the screen, which also works with your own `--settings`
- **waking on focus**
- **memory-leak restarts**
- **`ccnap status`**, which also reports claude processes running outside it
- the **💤 tab title**

## Limitations

- **Not covered:** the Claude Desktop app, background agents (`claude --bg`), cloud sessions, and other apps that embed the Agent SDK with their own Claude binary. `ccnap status` still shows such processes if they run claude.
- **VS Code Remote (SSH, WSL, Dev Containers):** the extension runs on the remote machine, so install ccnap there too. `ccnap setup` there points the chat panel at it for VS Code, VS Code Insiders, VSCodium, Cursor, Windsurf and Antigravity.
- **Stopped turns in a panel:** if you stop Claude in a panel and send nothing after, that session stays awake. A stopped turn skips the `Stop` hook, so ccnap can't be sure no background tasks are left. It naps normally after your next completed turn.
- **Extra panel history:** with a process wrapper set, the extension also lists sessions started outside it, such as `claude -p` runs, in its session history.
- **Hooks must be able to run:** a session never naps under `--bare` or `--safe-mode`, with `disableAllHooks`, or when managed settings set `allowManagedHooksOnly`. ccnap also needs a Claude Code version with exec-form hooks (`"args"` in a hook entry). It was developed against 2.1.301 to 2.1.311.

## Development

```sh
cargo test
```

This runs the unit tests and end-to-end tests around a fake `claude` (`examples/fake_claude.rs`), on a real pseudo-terminal and real pipes:

- **Terminal naps and wakes:** ccnap stays awake while focused, naps when the tab loses focus, shows up in `ccnap status`, resumes on focus with keys typed right after it kept, and resumes on a paste that arrives as one paste rather than a prompt per line.
- **Leak restart:** a bloated claude is restarted even with the tab in focus, and only once when the restarted claude grows past the limit again.
- **Panel mode:** ccnap naps while only keep-alives flow, and resumes with the same session, initialize request, model, and the permission mode a tool approval left it in, without leaking replay answers.

## License

MIT
