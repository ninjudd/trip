# trip

Persistent terminal sessions.

Trip is a tiny daemon that owns your terminal sessions so they survive when you close a window. Terminal apps become lightweight clients that attach and detach. Think tmux, but radically simpler — no panes, no splits, no keybindings to memorize. Just close the terminal to detach.

## Install

```
./install.sh
```

For development (symlinks debug build so `cargo build` updates it instantly):

```
./install.sh --dev
```

## Quick start

```
cd ~/my-project
trip enter
```

That's it. If a session exists for this workspace, you're attached. If not, one is created. Close the terminal whenever you want — the session survives. Run `trip enter` again to pick up where you left off.

## Commands

### Sessions

**`trip enter [name]`** — Enter the canonical workspace session. Creates it if missing, attaches if it exists. Derives the session name from your git repo root when no name is given. If someone else is attached, prompts to take over.

**`trip return`** — Return to the previous session. Opposite of `trip enter` — pops the session stack.

**`trip new [name]`** — Open a fresh durable terminal. Auto-numbered (`.1`, `.2`, `.3`). Attaches immediately. Kept alive in the background; cleaned up when the shell exits.

**`trip create <name> [-- command]`** — Create a session without attaching. For scripting and automation.

**`trip ls`** — List sessions. Shows foreground command, git branch, cwd, and marks the current session with `*`.

**`trip attach <name>`** — Attach to a specific session by name.

**`trip detach [name]`** — Detach from a session. Defaults to current session.

**`trip kill <name>`** — Kill a session.

**`trip shutdown`** — Stop the daemon and kill all sessions.

### Observation

**`trip screen <name>`** — Show the current terminal screen (what you'd see if attached).

**`trip log <name>`** — Show what happened over time. Screen snapshots are captured on idle and diffed to show only new content.

**`trip log <name> --raw`** — Full JSONL event stream (output, input, resize, screen events).

**`trip log <name> --follow`** — Stream new events as they happen.

**`trip log <name> --since 10m`** — Events from the last 10 minutes.

**`trip screens <name> [index]`** — Browse captured screen snapshots.

### Interaction

**`trip send <name> <input>`** — Send input to a session without attaching. Auto-appends Enter. Use `--raw` for exact bytes.

**`trip current`** — Print the current session name (exit 1 if not in a session).

### Programmatic control

**`trip wrap [name] [-- command]`** — Wrap a command with a JSONL protocol. The wrapped process gets a real PTY internally, but stdin/stdout become structured events. This turns any interactive program into a durable, programmable session.

```
trip wrap -- claude
```

**Input** (JSONL on stdin):

```json
{"type":"send","text":"summarize this repo\n"}
{"type":"key","key":"ctrl-c"}
{"type":"resize","cols":120,"rows":40}
{"type":"screenshot"}
{"type":"close"}
```

- `send` — type text into the PTY. Include `\n` for Enter. Multi-line text is automatically bracketed-pasted.
- `key` — named special keys: `ctrl-c`, `escape`, `enter`, `up`, `down`, `tab`, `backspace`, etc.
- `resize` — resize the PTY.
- `screenshot` — request the current screen state.
- `close` — end the session.

**Output** (JSONL on stdout):

```json
{"type":"log","text":"Claude is thinking..."}
{"type":"screen","text":"full screen contents here"}
{"type":"exit","code":0}
{"type":"error","message":"unknown key: ctrl-bogus"}
```

- `log` — pushed automatically as meaningful screen changes occur (diffed snapshots).
- `screen` — full screen state, only in response to `screenshot`.
- `exit` — process exited.
- `error` — protocol or runtime errors.

Wrapped sessions are normal trip sessions — you can `trip attach`, `trip screen`, `trip log` them from another terminal.

### Shell integration

`./install.sh` adds a shell hook to `.zshrc` and `.bashrc` that runs `trip init` before each command. This keeps terminal environment variables (`TERM_PROGRAM`, `COLORTERM`, etc.) in sync when you switch between different terminal apps while attached to the same session.

## How it works

A single Rust binary acts as both daemon and client. The daemon auto-starts on first use and auto-exits when the last session ends.

```
terminal
  ↕
trip attach
  ↕
trip daemon (Unix socket)
  ↕
PTY master
  ↕
shell / claude / vim / anything
```

The daemon owns PTY sessions. Clients connect over a Unix domain socket, receive the current screen state (via a virtual terminal), and stream I/O. Closing the client doesn't affect the session.

### Virtual terminal

The daemon maintains a VT100 parser for each session. On attach, it renders the current screen — no raw scrollback replay, no garbled escape sequences. Just a clean screen.

### Recording

Every PTY event is logged to `~/.trip/sessions/<name>/log.jsonl`. Screen snapshots are captured when output settles (500ms idle or 5s max interval), diffed against the previous snapshot using LCS, and stored as derived events. Full screen snapshots are saved to `~/.trip/sessions/<name>/screens/`.

Raw events are the canonical source of truth. Screen diffs are the index. `trip log` is the view.

### Writer model

One writer per session. Additional clients attach read-only (monochrome output, no input). When the writer disconnects, the previous writer regains control (stack-based). Sessions can be taken over — the old writer is silently demoted to read-only.

### Session switching

`trip enter` from inside a trip session seamlessly switches your terminal to the target session — no nesting, no new processes. The daemon redirects the attach client's stream. `trip return` pops back to the previous session. Enter and return form a stack, so you can nest switches and unwind them.

## Design philosophy

Trip is intentionally not:

- A terminal multiplexer (no panes or splits)
- A terminal emulator (your terminal does the rendering)
- An IDE or cloud platform
- A tmux clone

Trip is infrastructure. It should feel tiny, durable, and boring in a good way.

The core primitive is a **persistent PTY-backed session**. Everything else is optional.

## Requirements

- macOS or Linux
- Rust 2021 edition

## License

MIT
