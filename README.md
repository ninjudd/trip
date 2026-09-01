# trip

Persistent terminal sessions.

Trip is a tiny daemon that keeps your terminal sessions alive when you close a window. Terminal apps become lightweight clients that attach and detach. Think tmux, but radically simpler — no panes, no splits, no keybindings to memorize. Just close the terminal to detach.

## Why?

Normal terminal tabs are disposable. The processes inside them usually are not.

Trip separates the terminal viewport from the runtime session. This means:

- sessions survive window closes
- workspaces become durable
- terminals become interchangeable clients

## Install

```
./install.sh
```

For development (symlinks debug build so `cargo build` updates it instantly):

```
./install.sh --dev
```

## Terminal setup

For the best experience, configure your terminal to run trip automatically on new tabs.

### macOS Terminal

Open **Settings → Profiles → Shell**:

- **Startup → Run command:** `/usr/local/bin/trip enter`
- **When the shell exits:** Close if the shell exited cleanly
- **Ask before closing:** Only if there are processes other than the login shell and: add `trip` and `-trip` to the list

### iTerm2

Open **Settings → Profiles → General**:

- **Command:** choose **Custom Shell**, set to `/usr/local/bin/trip enter`

Open **Settings → Profiles → Session**:

- **After a session ends:** Close

This way every tab is a trip session. Close the tab to detach, open a new tab to reattach.

## Quick start

```
cd ~/my-project
trip enter
```

That's it. If a session exists for this workspace, you're attached. If not, one is created. Close the terminal whenever you want — the session survives. Run `trip enter` again to pick up where you left off.

## Switching and detaching

Closing the terminal is the usual way out. To leave without closing the
window, press **Ctrl-\\** — the same key dtach and abduco use.

The key has two stops. The first shows the session chooser:

```
sessions:  ↑/↓ + enter · 0-9 · esc back · ^\ detach
  0) trip.2                     (new session)
> 1) trip          claude       (current)
  2) trip.1        cargo test
  3) acme/webapp   nvim         (attached)
```

**Enter** moves this terminal to the highlighted session. **Esc** goes back to
where you were. **Ctrl-\ again** detaches for real — the session keeps
running, the client exits, and your shell comes back, exactly as one press
used to do.

Only the terminal you pressed it in moves. Other terminals attached to the
same session keep streaming.

Row `0` is always a session that does not exist yet, so **Up, Enter** — or just
**0**, which works even when the list has scrolled past it — makes one:
the canonical session for the workspace while it is missing, otherwise the
next number, the way `trip new` would. It lands in the same directory as the
session you opened the chooser from.

Pasted text is never misread as the key: input inside a bracketed paste is
forwarded untouched. And the key survives programs that switch the terminal
into an enhanced keyboard protocol — Claude Code enables kitty CSI-u and
xterm modifyOtherKeys at startup, after which the terminal sends the
keystroke as an escape sequence rather than a byte; trip recognizes those
spellings of the configured key too, so it keeps working whatever is in the
foreground.

To change or disable the key, set `TRIP_DETACH_KEY`:

```
TRIP_DETACH_KEY='^z'    # detach on Ctrl-Z instead
TRIP_DETACH_KEY='^-'    # Ctrl+dash (sent by terminals as ^_, 0x1f)
TRIP_DETACH_KEY=none    # no detach key; close the terminal to detach
```

Whatever key you pick stops reaching programs inside the session — the
default Ctrl-\ sacrifices only SIGQUIT, which almost nothing wants.

## Titles

Every title a session emits is wrapped with the workspace, so it reads
`webapp Deliberating` rather than losing the workspace the moment a program sets
its own title. Programs keep saying what they are doing; you keep knowing where.
Numbered sessions share their workspace's wrapping, and a title already carrying
it is left alone, so it never nests.

`TRIP_TITLE` is the whole thing — a shell string, expanded once when you attach
with `TRIP_WORKSPACE`, `TRIP_SESSION` and `TITLE` in the environment. `$TITLE`
is where the session's own title goes. There is no template language to learn;
parameter expansion does the work:

```
TRIP_TITLE='${TRIP_WORKSPACE##*/} $TITLE'      webapp Deliberating       (the default)
TRIP_TITLE='$TITLE @${TRIP_WORKSPACE##*/}'     Deliberating @webapp
TRIP_TITLE='[$TITLE] ~${TRIP_WORKSPACE##*/}'   [Deliberating] ~webapp
TRIP_TITLE='$TITLE - $TRIP_SESSION'            Deliberating - acme/webapp.2
TRIP_TITLE='${TRIP_WORKSPACE##*/}'             webapp                    (title dropped)
TRIP_TITLE=''                                  Deliberating              (left alone)
```

**Put `$TITLE` first if your terminal truncates from the left.** iTerm keeps the
*end* of a long title, so whatever precedes `$TITLE` is the first thing to
disappear — and that is usually the part naming the workspace.

Note `##*/` strips up to the last slash (`webapp`) while `%%/*` strips from the
first (`acme`).

Everything around `$TITLE` is literal, so spacing and any divider are part of
the value. Omit `$TITLE` and the session title is replaced outright; set
`TRIP_TITLE=''` and titles pass through untouched.

Expansion happens once per attach rather than per title — a title changes far
too often to fork a shell for each one. The consequence is that `$TITLE` is
substituted positionally: a bare `$TITLE` works, but a construct that
*transforms* it, like `${TITLE:-idle}`, sees a placeholder rather than the real
title.

The title the terminal had before you attached is pushed onto the XTerm title
stack and restored when you detach — on error exits too — so it goes back to
whatever it said before rather than keeping the session's last title.

## Commands

### Sessions

**`trip enter`** — Choose a session from every workspace and enter it. Arrow keys (or j/k) move, 1-9 jump to a visible row, 0 creates the next session in your workspace (that is row 0 of the list), Enter selects, q/Esc cancels. Your own workspace leads the list with its canonical session already highlighted, so plain Enter still takes you where it always did. With stdin redirected there is nothing to choose with, so it takes the canonical session.

**`trip enter <name>`** — Enter that session directly, no chooser. Creates it if missing, attaches if it exists.

**`trip enter --pwd`** — Only this workspace, derived from your git repo root. Skips the chooser entirely when there is nothing to choose between — one canonical session and no numbered ones — which is what the flag is for.

**`trip return`** — Return to the previous session. Opposite of `trip enter`. Cancelling out of the chooser does not count as having gone anywhere, so it still takes you to the session you actually switched from.

**`trip new [name]`** — Open a fresh durable terminal for the current workspace. Auto-numbered (`.1`, `.2`, `.3`). Kept alive in the background; cleaned up when the shell exits.

**`trip create <name> [-- command]`** — Create a session without attaching. For scripting and automation.

**`trip ls`** — List every session, grouped by workspace. Shows foreground command, git branch, cwd, and marks the current session with `*`. Use `--pwd` to narrow to the current workspace, and `--attached` to show only attached sessions; the two compose.

**`trip attach <name>`** — Attach to a specific session by name.

**`trip detach [name]`** — Detach from a session. Defaults to current session.

**`trip kill <name>`** — Kill a session.

**`trip shutdown`** — Stop the daemon and kill all sessions.

### Observation

**`trip screen <name>`** — Show the current terminal screen (what you'd see if attached).

**`trip log <name>`** — Show what happened over time. Three modes depending on context: raw output for normal shell, screen diffs for full-screen TUIs, and structured agent events when `trip on` is active.

**`trip log <name> -v`** — Include tool results (hidden by default for agent sessions).

**`trip log <name> --raw`** — Full JSONL event stream (output, screen, agent events).

**`trip log <name> --follow`** — Stream new events as they happen.

**`trip log <name> --since 10m`** — Events from the last 10 minutes.

### Interaction

**`trip send <name> <input>`** — Send input to a session without attaching. Auto-appends Enter. Use `--raw` for exact bytes.

**`trip current`** — Print the current session name (exit 1 if not in a session).

### Agent integration

**`trip on`** — Register a running AI agent (Claude Code, Codex) with the current trip session. Reads `CLAUDE_CODE_SESSION_ID` or `CODEX_THREAD_ID` from the environment, locates the agent's JSONL log file, and tells the daemon to tail it for structured events. Raw terminal output is suppressed while agent logging is active.

Run this inside an agent session:

```
! trip on
```

Or automate it with a SessionStart hook:

**Claude Code** — add to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup|resume",
        "hooks": [
          {
            "type": "command",
            "command": "trip on"
          }
        ]
      }
    ]
  }
}
```

**Codex** — add to `~/.codex/config.toml`:

```toml
[[hooks.SessionStart]]

[[hooks.SessionStart.hooks]]
type = "command"
command = "trip on"
```

With `trip on` active, `trip log` shows structured output: assistant text, thinking blocks, tool calls, and turn boundaries — instead of raw terminal escape sequences.

### Programmatic control

**`trip wrap [name] [-- command]`** — Wrap a command with a JSONL protocol. The wrapped process gets a real PTY internally, but stdin/stdout become structured events.

```
trip wrap -- claude
```

Wrapped sessions are normal trip sessions. You can `trip attach`, `trip screen`, and `trip log` them from another terminal.

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

### Shell integration

`./install.sh` adds a shell hook to `.zshrc` and `.bashrc` that runs `trip env` before each command. This keeps terminal environment variables (`TERM_PROGRAM`, `COLORTERM`, etc.) in sync when you switch between different terminal apps while attached to the same session. It also cleans up agent and TUI markers when the user returns to a shell prompt.

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

`trip enter` from inside a trip session seamlessly switches to the target session — no nesting, no new processes. Your terminal is rebound to the new session. `trip return` switches back. Enter and return form a stack, so you can nest switches and unwind them.

The detach key switches the same way, but per terminal: it is intercepted by the client before it reaches the session, so the terminal that saw it is the one that moves. A `trip enter` typed into a shell cannot be that precise — with several terminals attached, they all share the one shell, so nothing in it says which terminal you typed into.

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
