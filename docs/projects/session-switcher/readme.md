---
status: completed
priority: now
---

# Session Switcher

## 1. Outcome

The session list becomes the hub of trip rather than a special case of
`enter`. Three changes, one interaction model:

- **The detach key opens a chooser.** Pressing it inside an attached session
  (`TRIP_DETACH_KEY`, `^\` by default) detaches the *view* and shows the
  interactive session list. Enter switches this terminal to the highlighted
  session; the key again fully detaches (today's behaviour, one keystroke
  later); Esc/q goes back to the session you left.
- **`trip ls` lists every session by default**, with `--pwd` narrowing to the
  current workspace — the inverse of today's `-a`.
- **`trip enter` shows that same chooser**, over every workspace's sessions,
  unless `--pwd` narrows it.

The chooser is one component with one set of keys, reached three ways: a new
terminal (`trip enter`), a keystroke inside a session, and a command inside a
session (`trip enter` from a session shell).

## 2. What already exists

- **The picker** (`src/client/mod.rs`): `select_choice` (:196) renders rows
  and owns a raw-mode read loop over stdin; `read_key` (:150) parses one
  keypress; `session_choices` (:263) builds `(name, command, tag)` rows and is
  already pure and testable; `pick_session` (:314) wires them to the daemon's
  session list. Added by `enter -a` (PR #9).
- **The detach key** (`src/client/attach.rs`): `DEFAULT_DETACH_KEY` (:83),
  `parse_detach_key` (:96), and `DetachScanner` (:124), which scans client
  input for the key while tracking bracketed paste so a pasted blob
  containing the byte is forwarded untouched. `Scan::Detach(at)` is returned
  to the attach loop (:564), which flushes, breaks, and exits.
- **Terminal restoration** (`src/client/attach.rs`): `RawModeGuard` (:16) and
  `TerminalCleanup` (:66) already put the terminal back on every exit path,
  including the error paths, and balance the XTerm title push from :477.
- **Daemon-side switching** (`src/daemon/mod.rs`): `Request::SwitchSession`
  (:481) creates the target if missing, pushes `from` onto the target's
  `return_stack`, and signals the attached client. The `Attach` handler
  (:641) is a loop: when `stream_session` returns `StreamExit::SwitchTo`
  (:786) it re-attaches the same socket to the new session and re-renders.
  It does *not* send `Response::SessionName`: the client has handled that
  variant since titles were added, but nothing ever produced it — see §9.1.
- **Grouped listing** (`src/client/mod.rs:518`): `trip ls -a` already sorts by
  `(session_base, group_order)` and prints workspace headers. The default-all
  view is that renderer with the filter inverted.
- **Multi-writer sessions** (`0ec45ee`): several terminals can hold one
  session, all writable. This is what makes per-client switching necessary
  (§3.3).

## 3. Design

### 3.1 The chooser becomes a feedable state machine

`select_choice` reads stdin's fd directly. An attached client cannot call it:
`attach()` already has a thread blocked on stdin, forwarding bytes into an
mpsc channel (`attach.rs:498`). Two readers of one fd is not a fixable
arrangement, so the chooser has to stop owning the terminal.

New `src/client/chooser.rs` (`mod.rs` is 1165 lines already):

```rust
pub struct Chooser { rows: Vec<String>, selected: usize, top: usize, /* ... */ }

pub enum Outcome { Pick(usize), Cancel, Detach }

impl Chooser {
    pub fn new(rows: Vec<String>, selected: usize, viewport: u16) -> Self;
    /// Bytes to paint. First call draws; later calls redraw in place.
    pub fn render(&mut self) -> Vec<u8>;
    /// Consume input. Returns Some(..) when the user has decided.
    pub fn feed(&mut self, data: &[u8]) -> Option<Outcome>;
    /// Resolve a pending lone ESC after an idle interval (see below).
    pub fn tick(&mut self) -> Option<Outcome>;
}
```

`render` returning bytes rather than printing lets the attached caller write
to stdout alongside session data, and the standalone caller keep writing to
stderr as it does today.

`read_key` disambiguates a lone ESC from an arrow prefix by polling the fd for
25ms (:176). A feedable machine cannot poll, so a pending ESC becomes state:
resolved by the next byte, or by `tick()` when the caller sees ~25ms of
silence. Standalone uses `poll` for that; attached uses
`tokio::time::timeout` on the stdin channel.

`Outcome::Detach` is how the chooser reports the detach key. The attached
caller passes the configured key in; the standalone caller passes `None` and
never sees it.

### 3.2 The detach key opens the chooser

`DetachScanner` keeps its paste-aware scan unchanged; only the attach loop's
response changes. On `Scan::Detach(at)` the client:

1. Forwards `data[..at]` as it does today, then stops writing session output
   to the terminal. Frames still arrive and are dropped — whatever exit the
   chooser takes ends in a full re-render from the daemon, so buffering them
   would only replay a screen that is about to be overwritten.
2. Clears the screen and paints the chooser, having fetched the session list
   over a second short-lived connection (`get_session_list`, `mod.rs:80`).
3. Feeds the existing stdin channel into `Chooser::feed`.

Outcomes:

| Outcome | Action |
| --- | --- |
| `Pick(i)` on another session | `SwitchTo { to }` on the attached socket (§3.3) |
| `Pick(i)` on the current session | `SwitchTo { to: current }` — the re-render is the redraw |
| `Cancel` (Esc/q) | `SwitchTo { to: current }`; back where you were |
| `Detach` (the key again) | today's path: flush, break, `[detached: name]`, exit |

Cancel and same-session pick are deliberately the same operation: the daemon's
attach loop already re-adds the client, re-sends `screen_contents()`, and
resends `SessionName`, which is exactly the repaint a cancel needs. No new
"refresh" request, and one code path gets exercised by both.

The hint line names all four: `↑/↓ + enter · 1-9 · esc back · ^\ detach`,
with the configured key rendered in caret notation rather than a hardcoded
`^\`.

### 3.3 Switching becomes per-client

Today the switch signal lives on the session, not the client
(`session.rs:43`): `SwitchSession` sets one `switch_target` and calls
`switch_notify.notify_waiters()` (`mod.rs:516`), waking every client attached
to that session. They race for a single-slot `Option`; the first to `.take()`
switches and the rest get `None` and keep streaming. Since `0ec45ee` made
several terminals on one session ordinary, `trip enter webapp` typed in
terminal A can move terminal B instead. The request carries a session name,
and a session no longer identifies a terminal.

Worse for a keystroke: `notify_waiters()` stores no permit. A client that is
mid-write when the notify fires was not registered as a waiter and misses it
entirely; the switch is silently dropped while the caller has already been
told `Ok`.

The key does not have this problem, because it is intercepted in the client's
own input path before the bytes reach the PTY — the client that saw the key is
the exact task sitting in `stream_session`. So it sends the switch over its
own socket:

- New `Request::SwitchTo { to: String, command: Option<Vec<String>>, cwd:
  String, env: HashMap<String, String> }`. The extra fields mirror
  `SwitchSession` so the target can be created if it does not exist — the
  chooser offers a `(new session)` row (§3.5).
- `stream_session` currently treats any inbound control frame as a hangup
  (`mod.rs:988`). Parse the payload as a `Request` instead: `SwitchTo` creates
  the target if missing, pushes the current session onto the target's
  `return_stack` **unless `to` is the session the client is already on**, and
  returns `StreamExit::SwitchTo(to)`; anything else keeps today's
  `Disconnected`.

  The condition is not incidental. §3.2 routes both Cancel and a same-session
  `Pick` through `SwitchTo { to: current }`, and an unconditional push would
  put a session onto *its own* `return_stack` — an entry today's flow cannot
  produce, since `SwitchSession`'s push is `target.return_stack.push(from)`
  (`mod.rs:511`) and `enter` returns early rather than switching a session to
  itself. `ReturnSession` pops the topmost entry that still exists
  (`mod.rs:536-548`); a self-entry passes that check, so `trip return` would
  switch the session to itself, consuming one entry per cancel and leaving the
  real target underneath:

  ```
  key-switch a -> b        b.return_stack = [a]
  open chooser, Esc        b.return_stack = [a, b]      (without the condition)
  trip return              -> b, a no-op
  trip return              -> a
  ```

  The push is provisional in a second sense too — see §6 and §8, where the
  history moves to the client. The condition survives that move: a history
  cursor does not want a self-entry either.
- No `Notify`, no shared `Option`, no race, and the enclosing attach loop
  (:786) already handles the variant.

`SwitchSession` stays as it is for the out-of-band command path. Making *that*
per-client is not possible in principle: with multi-writer there is one shell
behind the PTY shared by every attached terminal, so nothing in the session's
environment can say which terminal you typed into. Left as an open question
(§7).

### 3.4 `trip ls` lists everything by default

- `--pwd` narrows to the current workspace: the existing
  `session_base(&s.name) == derive_session_name()?` filter, unchanged in
  substance and only inverted in default.
- `-a` / `--all` is removed outright, not kept as an accepted no-op.
- `--attached` stops implying `--all` (`mod.rs:540`) and becomes a plain
  filter that composes with `--pwd`.
- Grouping follows the scope as it does now: headers when listing more than
  one workspace, flat when `--pwd`.
- Empty-list messages invert: `no sessions` for the wide view, and
  `no sessions for '<base>' (try: trip ls)` for `--pwd`.

### 3.5 `trip enter` opens the chooser

- `trip enter` — chooser over every workspace's sessions.
- `trip enter --pwd` — today's scoped behaviour, *including* the fast path
  that skips the chooser when the workspace has nothing but the canonical
  session (`pick_session`, `mod.rs:333`). Skipping the chooser is the point of
  the flag.
- `trip enter <name>` — unchanged, direct.
- `-a` / `--all` is removed here too, which takes its `conflicts_with =
  "name"` (`cli.rs:20`) with it.
- Not a tty: fall back to the canonical session, as `pick_session` does today
  (:319). The `--all needs a terminal to choose with` bail disappears with the
  flag's meaning.

`session_choices` grows two responsibilities and returns the preselected index
alongside the rows:

1. **A `(new session)` row leading the current workspace's group, in the wide
   view too.** Scoped mode already synthesises one (:282); the wide branch has
   no create affordance at all, so a first tab in a fresh repo would have
   nothing to pick.

   It is always row 1 and always sits directly above the preselected row, so
   **Up, Enter** is how you make a session — a fixed gesture that does not
   move with the contents of the list. What it creates depends on what is
   there, which is what makes the position stable:

   | Current workspace | Row 1 | Preselected | Up + Enter |
   | --- | --- | --- | --- |
   | no sessions | `trip` `(new session)` | row 1 itself | n/a — Enter creates `trip` |
   | `trip` exists | `trip.1` `(new session)` | `trip` | creates `trip.1` |
   | `trip`, `trip.1`, `trip.2` | `trip.3` `(new session)` | `trip` | creates `trip.3` |
   | `trip.1`, `trip.2`, no `trip` | `trip` `(new session)` | `trip.1` | creates `trip` |

   So the row means "the next session in this workspace": the canonical one
   when it is missing, otherwise the next number that `trip new` would take
   (`next_available_name`, `mod.rs:125`). That folds `trip new` into the
   chooser without a second kind of row, and the empty-workspace case collapses
   to exactly today's one-keystroke flow rather than being a special case.

   Only the current workspace gets the row. Every other workspace would add a
   row apiece for something nobody scrolls the list to do.

2. **Current workspace first, then other workspaces alphabetically**, with the
   current workspace's canonical session preselected; failing that its first
   surviving session, and failing that the `(new session)` row. The middle
   clause is the last row of the table above — `trip` killed while `trip.1`
   lives, which is ordinary — where there is no canonical session to select
   and the workspace is not empty either. Preselecting row 2 there is what
   keeps "the `(new session)` row always sits directly above the preselected
   row" true in every state, and it reads sensibly: with the canonical session
   gone, Up + Enter puts it back. Enter therefore lands where
   it lands today, and your own sessions hold the low digits. The cost is that
   row positions depend on where you launched from; see §6.

Creating from the chooser has two details the plain-name path does not:

- **The displayed number can go stale.** `next_available_name` is computed
  when the list is drawn; another terminal can take `trip.3` before you press
  Enter. Treat it as `enter` already treats the analogous race (`mod.rs:455`):
  on `session '<name>' already exists`, recompute and retry once.
- **A new session's cwd should come from the session you were in**, not from
  the client. `trip new` inherits the shell's directory because it runs inside
  it; the attach client's cwd is wherever the terminal was launched, which may
  be months stale. The daemon already tracks each session's cwd
  (`SessionInfo.cwd`, via `procinfo`), so `SwitchTo` creating a session should
  prefer the source session's cwd and fall back to the client's.

Both need the current workspace in the wide view, which `scope:
Option<&str>` cannot express — it currently means "this workspace" and "narrow
to it" at once. Split it: `workspace: &str` (always known, from
`derive_session_name`) plus a `Scope::Pwd | Scope::All`.

Two existing tests encode the behaviour being inverted and must be rewritten
rather than merely extended: `all_choices_never_offer_a_new_session`
(`mod.rs:1144`), whose comment asserts there is no canonical session to create
across workspaces, and `all_choices_span_workspaces_grouped_and_ordered`
(`mod.rs:1122`), which expects strict alphabetical ordering.

The daemon-side switch that `enter` performs from inside a session
(`mod.rs:414`) is untouched — only the name it is given now comes from a wider
list.

### 3.6 The chooser gets a viewport

`select_choice` prints every row and redraws with `\r\x1b[{rows.len()}A`
(:215). Once the wide list is the default, a list taller than the window walks
the cursor off the top and corrupts the screen. So:

- Window height is `terminal_size().1` minus the hint line and a margin.
  `terminal_size` (`attach.rs:426`) moves to `client/mod.rs` and becomes
  `pub(crate)`.
- The window scrolls to keep the selection visible; `↑`/`↓` wrap as they do
  now, which means wrapping also jumps the window to the far end.
- Truncated ends are marked (`⋯`) so a short window does not look like the
  whole list.
- Digits number the rows *as rendered*, so `3` is always the third row you can
  see.

Type-to-filter is deliberately deferred (§6).

### 3.7 Restore input modes on re-render

`Session::screen_contents` (`session.rs:291`) uses vt100's
`contents_formatted()`, which writes cell contents only.
`state_formatted()` = contents + `write_input_mode_formatted` + title, and the
input modes are exactly what a chooser's screen teardown clears: bracketed
paste, application keypad/cursor, mouse protocol.

Today this is invisible, because a re-render only happens when you attach. Once
Esc from the chooser re-renders a *live* app, an editor would come back having
silently lost mouse reporting and bracketed paste. So `screen_contents` must
emit the input modes too — either `state_formatted()`, or
`contents_formatted()` plus modes rebuilt from `bracketed_paste()`,
`application_keypad()`, `application_cursor()` and `mouse_protocol_mode()`.
The rebuild is preferred: `state_formatted` also emits a title, which would
race the `TitlePrefixer` and set a title for sessions that never had one.

## 4. Acceptance criteria

Interactive, in a terminal:

- Inside a session, the detach key shows the chooser; Enter on another session
  moves *this* terminal to it and leaves the terminal's title correct; Esc
  returns to the original session with its screen intact; the key a second
  time prints `[detached: <name>]` and exits, exactly as it does today.
- With two terminals attached to one session, the key in terminal A moves only
  terminal A. Terminal B keeps streaming, and the PTY resizes to fit whoever
  is left.
- Esc out of the chooser while a full-screen app (vim, claude) is running in
  the session leaves the app usable: mouse reporting and bracketed paste still
  work, and a paste is still bracketed.
- After a key-switch from `a` to `b`, `trip return` inside `b` goes back to
  `a`.
- Cancelling the chooser leaves `trip return` going to the session you
  key-switched from, however many times the chooser has been opened and
  dismissed: switch `a` → `b`, then open and Esc out of the chooser three
  times, and one `trip return` still lands on `a`. The same holds for picking
  the session you are already on rather than cancelling.
- A chooser over more sessions than the terminal has rows scrolls instead of
  corrupting the screen; the truncation marker appears; digits select the row
  they label.
- `trip enter` in a workspace with no sessions preselects its `(new session)`
  row, so Enter creates and attaches — the same keystroke count as today.
- In a workspace that already has sessions, Up then Enter creates the next
  numbered one (`trip.1` where only `trip` exists, `trip.3` where `trip.1` and
  `trip.2` do) and attaches to it, in the same cwd as the session the chooser
  was opened from. Two terminals racing on the same displayed number both end
  up in a session, on different numbers.
- In a workspace where the canonical session is gone but numbered ones
  survive (kill `trip`, keep `trip.1`), the chooser preselects `trip.1` and
  row 1 offers to create `trip`, so Up then Enter puts the canonical session
  back.
- `trip enter --pwd` in a workspace whose only session is the canonical one
  attaches directly, with no chooser.
- `trip enter` with stdin redirected attaches to the canonical session without
  printing a chooser.

Non-interactive:

- `trip ls` lists every workspace grouped; `trip ls --pwd` lists only this
  one; `trip ls -a` is a clap unrecognized-argument error; `trip ls --attached --pwd` filters on
  both.
- The two `session_choices` tests named in §3.5 are rewritten to the new
  contract, not deleted.
- `cargo test` covers, as pure functions: `Chooser::feed` (arrows, j/k,
  digits, Enter, the detach key, a lone ESC via `tick`, and an arrow whose
  bytes arrive split across two `feed` calls); viewport scrolling (selection
  above/below the window, wrap to either end, list shorter than the window);
  and `session_choices` (wide ordering with the current workspace first, the
  synthesised `(new session)` row, and the preselected index in each case).

## 5. Implementation sequence

1. `Chooser` in `src/client/chooser.rs`, with the viewport and its unit tests.
   Port `select_choice`/`read_key` onto it and keep `pick_session` working
   unchanged — pure refactor, no behaviour change, shippable alone.
2. `Request::SwitchTo` + control-frame handling in `stream_session`, with the
   `return_stack` push. No client uses it yet.
3. `screen_contents` emits input modes (§3.7).
4. The detach key opens the chooser in `attach()`: chooser mode, dropped
   frames, the four outcomes.
5. `session_choices` gains wide-view ordering, the `(new session)` row, and
   the preselect index.
6. `trip ls` default-all + `--pwd`; `trip enter` default-chooser + `--pwd`;
   `-a` dropped from both.
7. README: `Detaching` gains the chooser, `Commands` updates `ls` and `enter`.

Steps 1–3 are independently useful; 4 is the feature; 5–6 are the CLI surface
and can land separately from the keystroke.

## 6. Decisions

- **The detach key gains a first stop rather than a second key being added.**
  A dedicated switch key would take another binding away from every program
  in the session, and the obvious candidate is not free: `^_` is `undo` in
  both zsh and bash. One key with a progressive meaning costs nothing new and
  keeps "two taps to get out" as the fast path. `TRIP_DETACH_KEY` keeps its
  name and its parsing.
- **Esc returns to the session; only the key detaches.** The alternative —
  Esc detaching, matching what `q` means in today's standalone picker —
  leaves no way back from an accidental keypress.
- **The chooser is client-side and per-client, over the attached socket**,
  rather than reusing `SwitchSession`'s session-wide notify. That notify is
  racy with multi-writer and drops signals when no client is parked on it
  (§3.3).
- **Cancel is implemented as a switch to the current session**, not a new
  refresh request, so the repaint uses the same daemon path as a real switch.
- **The `(new session)` row leads the list rather than trailing it.** Up from
  the preselected row is a shorter reach than scrolling to the end, it stays
  on screen once the viewport lands (§3.6), and it does not depend on the
  selection wrapping to be reachable. Rejected: labelling it `+ new session`
  as a bare action row. Showing the concrete name it will create (`trip.3`)
  says what you are about to get, matches the row format everywhere else, and
  keeps the existing `(new session)` tag; the cost is a name that can go stale
  between draw and Enter, handled by the retry in §3.5.
- **Current workspace first and preselected**, rather than one stable global
  order. Enter keeps landing where it lands today and the low digits stay
  useful; the cost is that row positions move with the launch directory.
- **Viewport now, type-to-filter later.** The viewport is forced by
  default-all; filtering is not, and it needs the `j`/`k` bindings settled
  first (probably `/` to enter filter mode). Revisit once the wide list has
  been lived with.
- **`-a` is deleted rather than kept as a no-op.** It is documented and it
  would cost one line to honour, but nobody uses it, and a flag that silently
  does nothing is worse to meet than one that errors.
- **A client's session history belongs to the client, not to the session.**
  `return_stack` lives on `Session` (`session.rs:50`), so switching A→B pushes
  `A` onto *B's* stack. With one terminal per session that reads as "where I
  came from"; with several it is a shared list of where *anyone* came from,
  and `trip return` from B pops whichever entry happens to be on top. The
  daemon's `Attach` handler is already a per-connection loop holding
  `current_name` (`mod.rs:641`), so a client's history is a `Vec<String>`
  local in that same scope — simpler than reaching into the target session,
  and per-client by construction. v1 does not move it (§8): the key-switch
  keeps pushing to the session stack, which is correct for the single-client
  case that is nearly all of them. That leaves `trip return` behaving as it
  does today only because §3.3 skips the push when `to` is the session the
  client is already on — without that condition the chooser's own Cancel would
  put a self-entry on the stack, which is a corruption today's flow cannot
  produce, and the deferral would be resting on it. The condition is not
  blocked on the client-addressing problem that defers the rest of §8: it
  holds wherever the history lives.
- **`SwitchSession` is left alone.** Per-client targeting is impossible for a
  command typed into a shell that every attached terminal shares (§3.3).

## 7. Open questions

- **What should `trip enter <name>` do from a multi-writer session?** Today a
  random attached terminal wins. The honest options are moving every viewer
  together (deterministic, and arguably right since they share the shell that
  ran the command) or refusing when a session has more than one client. Not
  blocking: the key path is unaffected, and the current behaviour is no worse
  than before this project. Owner: whoever picks up step 2, who will already
  be in `stream_session`.

  `trip return` is the *same* question, and worth deciding at the same time.
  It is a command typed into the same shared shell, and it is ambiguous at
  both ends: `ReturnSession` (`mod.rs:530`) pops from the session's stack and
  then delivers through the same session-wide `switch_notify`, so with several
  clients attached it can move an arbitrary terminal to an arbitrary previous
  session. Whatever addresses a client for `enter` addresses one for
  `return`.
- **Does the wide chooser want the workspace headers `ls` prints?** Rejected
  for now — the name column already carries the workspace and headers
  complicate the digit numbering and the viewport arithmetic. Revisit if the
  wide list reads badly in practice.
- **Should the chooser be able to kill a session (`x`)?** It is the natural
  place for it and the list is right there. Deferred: it wants a confirmation
  and a redraw path, and neither belongs in the first version.

## 8. Follow-ups

Recorded here rather than built, and not blocking anything in §5.

- **Move the history onto the client** (§6). The `Vec<String>` in the `Attach`
  loop becomes the source of truth; `SwitchTo` pushes to it instead of to the
  target session's `return_stack`. The catch is `trip return`, which arrives
  on a different socket and cannot name a client — the same problem as §7's
  open question, and it should be solved once for both.
- **Walk the history with ← and →.** Once the history is the client's, the
  chooser can navigate it: ← selects the session you were in before this one,
  → moves forward again, browser-style, so returning through several switches
  is repeated taps rather than finding each name in the list. This wants the
  history to be a cursor into a list rather than a stack that pops, since
  "forward" has nothing to pop from — worth building the client-side history
  that way from the start even though v1 only ever pushes.
- **`trip return` becomes the command form of ←**, rather than a separate
  mechanism with its own stack.

## 9. What implementation settled

Written while building §5; nothing here changes the outcome in §1.

### 9.1 `Response::SessionName` was dead code

§2 said the daemon sends it after a switch. It never did. Nothing produced
the variant — `trip enter` prints the title itself, from the client that ran
the command, so the gap never showed. The key-driven switch has no such
client, and with an allocated name (§9.2) the client does not even know what
it switched to, so the daemon now sends it and the client retitles and tracks
its own name from it. §2 is corrected.

### 9.2 Allocating the new session's number, instead of retrying a stale one

§3.5 handled the displayed number going stale by retrying once on
`session '<name>' already exists`. That error never arrives: `SwitchTo`
creates the target only when it is missing, so a name taken between draw and
Enter means the client silently *joins* the other terminal's session rather
than failing — and the retry has nothing to trigger it. The acceptance
criterion that two racing terminals "both end up in a session, on different
numbers" would have failed quietly.

`SwitchTo` instead carries `allocate`, and the daemon takes the next free name
while the session table is locked. Race-free rather than retried, and the
`(new session)` row still shows the concrete name it expects to create. The
canonical session is still requested by name: joining the one somebody else
just created is the right answer there.

### 9.3 Digits number only the rows a digit can reach

§3.6 says digits number the rows as rendered. Rows past the ninth visible one
are left unnumbered rather than carrying a number no key selects — the old
renderer numbered all of them, including the unreachable ones.

Two Escs in a row now cancel immediately instead of waiting out the idle
timeout; the old reader swallowed the second one.

### 9.4 The attached chooser's workspace is the session's, not the client's

§3.5 derives the workspace from `derive_session_name`, which reads the
process's cwd. That is right for `trip enter`, which the user just ran, and
wrong inside an attached client, whose cwd is wherever its terminal was
launched — possibly months ago and unrelated to where the session has been
since. The attached chooser takes the workspace from the session's own name.

### 9.5 Smaller things

- `--pwd` keeps `conflicts_with = "name"` on `enter`, which §3.5 dropped along
  with `-a`. Naming a session means not choosing one, so narrowing the chooser
  is meaningless there, and §6's own reasoning says a flag that silently does
  nothing is worse than one that errors.
- `--pwd`'s fast path reads the session list rather than the built choices,
  since the create row means the choices are never bare.
- §3.7's input modes are rebuilt with vt100's `input_mode_diff` against a
  pristine screen rather than by hand: it covers exactly the modes the crate
  tracks, uses the crate's own encoder, and still avoids `state_formatted`'s
  title.

### 9.6 Evidence

`tests/switcher_e2e.py` drives a real PTY in a throwaway `HOME` and covers
every interactive criterion in §4: the chooser opening, Esc returning, the key
twice detaching, per-terminal switching with a second terminal attached, the
PTY refitting to whoever is left, `trip return` surviving three cancels, cwd
inheritance, two terminals racing to the same displayed number, the viewport
and its marker, and bracketed paste surviving a cancel into a live app. 30
checks. `cargo test` covers the parser, the viewport, the renderer, the
choice ordering and preselection, and the input-mode rebuild.

Two things are not covered end to end: mouse reporting across a cancel, which
is unit-tested alongside bracketed paste and shares its code path, and the
`(exited)` tag, which no criterion asks for.

## 10. Outcome

Shipped, as §1 described it. The detach key's first press opens the chooser,
`trip ls` and `trip enter` default to every workspace with `--pwd` to narrow,
and the same component serves all three ways in.

Every §4 criterion has evidence. The interactive ones are covered by
`tests/switcher_e2e.py`, which drives a real PTY in a throwaway `HOME` (30
checks); the pure ones by `cargo test` (94 tests, 37 of them new). Two
criteria are proven by unit test rather than end to end, and deliberately:

- **Mouse reporting surviving a cancel** shares `input_modes` with bracketed
  paste, which *is* covered end to end. Driving a mouse-tracking app through a
  PTY would test the app.
- **Preselecting the survivor when the canonical session is gone** is decided
  entirely by `session_choices`, which is tested directly across all five
  workspace states, including the invariant that the create row sits above
  the selection in each.

Three things changed shape during implementation, all recorded in §9: the
stale-number retry became an allocation under the daemon's lock (§9.2),
because the error it retried on never arrives; `Response::SessionName` turned
out to be dead code that §2 claimed was live (§9.1); and the attached
chooser's workspace comes from the session rather than the client's cwd
(§9.4).

§7's first open question is still open and still not blocking: `trip enter
<name>` from a multi-writer session, and `trip return` with it, remain as racy
as they were before this project. Nothing here made them worse — the keystroke
path deliberately routes around the mechanism they use — and answering them
means deciding how a command names a client, which is §8's work. The other two
questions were answered as written. §8's follow-ups are untouched by design.
