# drip

Persistent terminal sessions.

Drip is a tiny daemon that owns your terminal sessions so they survive when you close a window. Terminal apps become lightweight clients that attach and detach. Think tmux, but radically simpler — no panes, no splits, no keybindings to memorize. Just close the terminal to detach.

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
drip enter
```

That's it. If a session exists for this workspace, you're attached. If not, one is created. Close the terminal whenever you want — the session survives. Run `drip enter` again to pick up where you left off.

## Commands

### Sessions

**`drip enter [name]`** — Enter the canonical workspace session. Creates it if missing, attaches if it exists. Derives the session name from your git repo root when no name is given. If someone else is attached, prompts to take over.

**`drip new [name]`** — Open a fresh durable terminal. Auto-numbered (`.1`, `.2`, `.3`). Attaches immediately. Cleaned up automatically when you detach if only the shell is running.

**`drip create <name> [-- command]`** — Create a session without attaching. For scripting and automation.

**`drip ls`** — List sessions. Shows foreground command, git branch, cwd, and marks the current session with `*`.

**`drip attach <name>`** — Attach to a specific session by name.

**`drip detach [name]`** — Detach from a session. Defaults to current session.

**`drip kill <name>`** — Kill a session.

**`drip shutdown`** — Stop the daemon and kill all sessions.

### Observation

**`drip screen <name>`** — Show the current terminal screen (what you'd see if attached).

**`drip log <name>`** — Show what happened over time. Screen snapshots are captured on idle and diffed to show only new content.

**`drip log <name> --raw`** — Full JSONL event stream (output, input, resize, screen events).

**`drip log <name> --follow`** — Stream new events as they happen.

**`drip log <name> --since 10m`** — Events from the last 10 minutes.

**`drip screens <name> [index]`** — Browse captured screen snapshots.

### Interaction

**`drip send <name> <input>`** — Send input to a session without attaching. Auto-appends Enter. Use `--raw` for exact bytes.

**`drip current`** — Print the current session name (exit 1 if not in a session).

## How it works

A single Rust binary acts as both daemon and client. The daemon auto-starts on first use and auto-exits when the last session ends.

```
terminal
  ↕
drip attach
  ↕
drip daemon (Unix socket)
  ↕
PTY master
  ↕
shell / claude / vim / anything
```

The daemon owns PTY sessions. Clients connect over a Unix domain socket, receive the current screen state (via a virtual terminal), and stream I/O. Closing the client doesn't affect the session.

### Virtual terminal

The daemon maintains a VT100 parser for each session. On attach, it renders the current screen — no raw scrollback replay, no garbled escape sequences. Just a clean screen.

### Recording

Every PTY event is logged to `~/.drip/sessions/<name>/log.jsonl`. Screen snapshots are captured when output settles (500ms idle or 5s max interval), diffed against the previous snapshot using LCS, and stored as derived events. Full screen snapshots are saved to `~/.drip/sessions/<name>/screens/`.

Raw events are the canonical source of truth. Screen diffs are the index. `drip log` is the view.

### Writer model

One writer per session. Additional clients attach read-only (monochrome output, no input). When the writer disconnects, the previous writer regains control (stack-based). Sessions can be taken over — the old writer is silently demoted to read-only.

### Session switching

`drip enter` from inside a drip session seamlessly switches your terminal to the target session — no nesting, no new processes. The daemon redirects the attach client's stream.

## Design philosophy

Drip is intentionally not:

- A terminal multiplexer (no panes or splits)
- A terminal emulator (your terminal does the rendering)
- An IDE or cloud platform
- A tmux clone

Drip is infrastructure. It should feel tiny, durable, and boring in a good way.

The core primitive is a **persistent PTY-backed session**. Everything else is optional.

## Requirements

- macOS or Linux
- Rust 2021 edition

## License

MIT
