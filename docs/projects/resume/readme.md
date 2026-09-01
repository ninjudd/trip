---
status: ready
priority: now
---

# Resume

## 1. Outcome

Sessions that died with the daemon come back. After a crash, kill, or reboot,
the sessions that were alive at the moment of death appear in the `trip enter`
chooser alongside the live ones, marked dead. Picking one recreates it — same
name, same cwd — relaunches whatever was running inside it, and attaches. An
agent comes back resumed into the same conversation, via
`claude --resume <session-id>` / `codex resume <thread-id>`.

There is no `trip resume` command. Resurrecting a session is the same gesture
as entering one, in the same list, with the same keys — which is the whole
argument of the session-switcher plan (`66de6c6`) applied to one more row
type. A separate command would be a second way to reach sessions, discovered
separately and bound separately.

Motivating incident: on 2026-08-29 the daemon died silently (~12:31), taking
every session with it. The transcripts survived on disk in `~/.claude` and
`~/.codex`, but reconnecting each agent to its conversation meant manually
hunting down ids and retyping flags. PR #10 (`daemon-hardening`) added the
crash marker this project builds on.

## 2. What already exists

- **`meta.json`** (PR #10): `SessionMeta { name, command, cwd, created_at }`
  (`common.rs:41`), written by `Session::spawn` (`session.rs:226`) and removed
  at every clean exit path — reaper (`daemon/mod.rs:198`, `:203`), kill
  (`:599`), shutdown (`:631`). Its presence therefore means "this session died
  with the daemon" — the exact candidate set. Session dirs that predate PR #10
  have no `meta.json` and are never offered.
- **The daemon can already see what is running.**
  `procinfo::get_foreground_pid(master_fd)` is `tcgetpgrp` (`procinfo.rs:177`),
  and `daemon/mod.rs:275-278` already pairs it with `get_cwd` and `get_name` to
  answer `ListSessions`. It is read on demand and nothing is persisted, so
  after a `kill -9` there is no record of it. `get_argv` does not exist yet;
  it is the one new primitive this project needs (§3.1).
- **`agent.json`** (`src/daemon/agent.rs`): `{kind, log_path}`, written by
  `trip on` (from a SessionStart hook, hook stdin, or `CLAUDE_CODE_SESSION_ID`
  / `CODEX_THREAD_ID`), deleted by the `trip env` preexec hook once the agent
  env vars are gone. After a crash it survives, marking sessions where an
  agent was (probably — see §7) running.
- **Resume identity is already derivable**: the Claude session id is the
  transcript basename in `agent.json.log_path`
  (`~/.claude/projects/<cwd-encoded>/<session-id>.jsonl`); the Codex thread id
  is embedded in the rollout filename (`find_codex_log` in `src/client/mod.rs`
  matches it as a substring). `log.jsonl` also records it as
  `agent_session_start.continuation`.
- **The chooser**: the session-switcher plan (`66de6c6`) factors `select_choice`
  (`src/client/mod.rs:196`) and `read_key` into a `Chooser` in
  `src/client/chooser.rs` — `feed`/`render`/`tick` over `rows: Vec<String>`,
  returning `Outcome::Pick(usize)`. It already carries the precedent this
  project needs: `(new session)` is a row that is not a live session and whose
  selection performs an action (its §3.5).
- **Existing protocol suffices**: `CreateSession` + `SendInput` can recreate a
  session and type the resume command into it. v1 adds no daemon request.

## 3. Design

### 3.1 Record the running job in `meta.json`, for every session

`SessionMeta` carries what a session was *created as*. Resume needs what is
running in it *now*: a session created as a plain shell that has had `claude`
in it for an hour should come back as claude, not as a bare shell.

```rust
pub struct SessionMeta {
    pub name: String,
    pub command: Option<Vec<String>>,   // as created
    pub cwd: String,
    pub created_at: u64,
    pub fg_argv: Option<Vec<String>>,   // as currently running
    pub fg_cwd: Option<String>,
    pub updated_at: u64,        // when fg_argv last changed
}
```

`updated_at` is a job-transition timestamp and nothing reads it as liveness:
because the rewrite is change-triggered, a session given one long-running job
on Monday and still alive on Friday carries Monday's `updated_at`. "Last
active" comes from `log.jsonl`'s mtime instead, which the pty loop appends to
on every chunk of output (`session.rs:445`) and which therefore keeps moving
for the life of the session.

A sampler in its own `tokio::spawn`, beside the agent watcher, reads the
foreground process group every 2s and rewrites `meta.json` when `fg_argv`
changes. Requires `procinfo::get_argv(pid)` (macOS: `sysctl KERN_PROCARGS2`;
linux: `/proc/<pid>/cmdline` — `procinfo.rs` already has the cfg split). New
fields are `#[serde(default)]`, so a `meta.json` written by PR #10 still
parses.

**Not the agent watcher's loop**, though it is the obvious place to hang this
and it looks like a 2s tick:

```rust
loop {
    if read_agent_config(&name).is_some() {
        tail_agent_log(name.clone()).await;   // returns only when agent.json goes
    }
    sleep(2s).await;
}
```

`tail_agent_log` runs its own 300ms loop and breaks only when `agent.json` is
removed or its `log_path` changes (`agent.rs:278-284`), so the outer 2s sleep
happens only while **no** agent is registered. Sharing it would sample a plain
shell every 2s and then stop the moment `trip on` fires — and since the
SessionStart hook runs immediately after the pgid becomes claude, the sample
would usually never see claude at all, leaving `fg_argv` holding the shell for
the whole session. The flags this project exists to preserve would be missing
exactly when an agent is involved, degrading silently to the bare
`claude --resume <id>` fallback.

This is what makes resume general. A session running `vim`, `psql`, a dev
server or an agent all come back the same way, with no cooperation from the
program and no hook of its own. Agents stop being a special case for
*recreating* a session and are special only in having a conversation to rejoin
(§3.2).

**Why the daemon and not `trip on`.** `tcgetpgrp` returns the foreground
process *group*, which is the job the shell launched — a subprocess that
claude or vim spawns inherits that pgid, so a sample does not catch a
transient `rg`. An earlier draft of this plan rejected daemon-side capture on
the grounds that "the fg pid can transiently be a subprocess" and had `trip on`
walk its own ppid chain instead; that was wrong about `tcgetpgrp`, and it is
why the capture moved. Generalising `trip on` itself was the other option and
is worse: a generic command does not know trip exists and will never call it,
so the mechanism has to live somewhere that needs no cooperation.

### 3.2 What stays agent-specific, in `agent.json`

`AgentConfig` (`agent.rs:8-11`) keeps only the things no generic command has:

```json
{
  "kind": "claude",
  "log_path": "/Users/.../.claude/projects/-Users-x-proj/<uuid>.jsonl",
  "log_offset": 48291,
  "resume_id": "<uuid>"
}
```

- `resume_id`: `trip on` fills it from, in order: the hook's stdin JSON
  `session_id` field, `CLAUDE_CODE_SESSION_ID` / `CODEX_THREAD_ID`, else
  parsed from the transcript path (claude: basename; codex: uuid in the
  rollout filename) or the transcript's first `session_meta` line.
- `log_offset`: the size of `log_path` when `trip on` writes the file.
  `tail_agent_log` starts every tail at `last_size = 0` (`agent.rs:275`) and so
  reads the whole transcript on its first pass — harmless when the file is new,
  destructive when it is not, which is exactly the resumed case. See §3.4.

No `argv` here: the daemon already has it in `meta.json` for every session.

Resume command construction, per kind:

- claude: `meta.fg_argv` with any `--resume`/`--continue`/`--session-id`
  arguments stripped, plus `--resume <resume_id>`.
- codex: `codex resume <resume_id>` followed by the original flags with the
  original subcommand (if any) removed. Which flags `codex resume` accepts
  needs verification during implementation (`--yolo` is known-good: observed
  in real use as `codex resume <uuid> --yolo`).
- Fallback when `fg_argv` is null: `claude --resume <id>` / `codex resume <id>`.

### 3.3 Dead rows in the chooser

Candidate scan (client-side): walk `~/.trip/sessions/*/*` (and legacy unscoped
dirs) for dirs holding a `meta.json`, minus the names the daemon reports live.
Each becomes a chooser row below the live ones, showing name, cwd, what it will
relaunch (from `fg_argv`), and when it was last active (`log.jsonl` mtime).

"Last active" rather than "died" is not just honesty about the timestamp: every
session in one crash died at the same instant, so time-of-death cannot tell two
rows apart, while last-active is exactly the signal for deciding which ones are
worth bringing back.

Selecting a dead row:

1. If `meta.cwd` no longer exists, say so and leave the row selected rather
   than creating the session — `session.rs:143` uses
   `set_current_dir(&cwd).ok()`, so a recreated shell would silently land in
   the daemon's cwd.
2. Read `agent.json` into memory, then delete it **before** creating the
   session. Otherwise the daemon's agent watcher sees it, tails the *old*
   transcript from offset 0, and re-emits the entire prior conversation into
   `log.jsonl`.

   The read is not only for the resume command. Step 3 can fail —
   `CreateSession` rejects a name claimed since the scan (`daemon/mod.rs:251`,
   the same list staleness #11 dealt with), and `Session::spawn` can fail on
   `openpty` or `fork` — and a deleted `agent.json` is unrecoverable, dropping
   the session out of the candidate set as an agent session for good. So write
   it back if the create fails.

   Deleting *after* a successful create is the obvious alternative and is
   worse: the daemon polls for `agent.json` every 2s (`session.rs:256`) and
   would likely see the stale file first, which is the race this ordering
   exists to avoid.
3. `CreateSession` with the same name and `meta.cwd`. Reusing the name means
   `log.jsonl` continues in the same file — history stays contiguous — and
   `Session::spawn` overwrites `meta.json` with fresh metadata.
4. Relaunch the work:
   - Agent session (`agent.json` existed): create with the default shell, then
     `SendInput` the resume command line. Typing into a shell reproduces normal
     usage — preexec/`trip env` runs, the agent's SessionStart hook
     re-registers `trip on` against whatever transcript the resumed agent
     writes to (the README's recommended matcher is already `startup|resume`),
     and when the agent exits the user still has a shell. That transcript may
     be the pre-crash file rather than a fresh one, which is what §3.4 exists
     to handle. PTY input is buffered by the kernel, so sending before the
     shell prints its prompt is safe (the mechanism tmux-resurrect relies on).
   - Non-agent with `fg_argv`: same shape — default shell, then `SendInput` the
     argv. A resurrected `vim foo.rs` or `npm run dev` is one line of typing,
     and the user keeps a shell when it exits.
   - Neither: recreate per `meta.command` (explicit argv → same argv; null →
     default shell).
5. Attach, exactly as picking a live row does.

Rejected alternative: running the resume argv as the session command rather
than typing it into a shell. Deterministic, but the session dies when the
program exits, no shell hooks run (so `trip on` re-registration depends
entirely on the agent-side hook), and it diverges from how programs are
actually run in trip. Revisit only if input races show up in practice.

The `Chooser` needs one addition for §3.5: a discard key, which is an
`Outcome::Discard(usize)` variant beside `Pick`/`Cancel`/`Detach`.

### 3.4 Stop the replay at re-registration, not only before create

Step 2 deletes the stale `agent.json` so the *pre-resume* watcher cannot replay
the old transcript. The same mechanism then fires a second time, and nothing
above stops it: when the resumed agent's SessionStart hook writes a fresh
`agent.json`, the daemon starts a new `tail_agent_log`, and that one also
begins at `last_size = 0` (`agent.rs:275`). Whether it replays the conversation
depends on which file the resumed agent writes to — an open question (§7) whose
answer this design deliberately does not depend on.

So `trip on` records `log_offset` (§3.2) and the tailer seeks there instead of
to 0:

```rust
let mut last_size: u64 = cfg.log_offset;   // was: 0
```

For a genuinely new transcript `log_offset` is 0 and nothing changes; for a
reused one the tailer emits only what the resumed agent appends. This also
demotes the delete-before-create ordering from the only thing standing between
resume and a duplicated conversation to a belt-and-braces measure.

### 3.5 Keeping the dead list from growing

Dead rows accumulate — every crash adds its whole session set, and a row is
only interesting until the user decides about it. Three things retire them,
none of which is a chore the user has to remember:

- **Resurrecting consumes the row.** `Session::spawn` overwrites `meta.json`
  with fresh metadata, so a resurrected session is simply live again.
- **A discard key in the chooser** removes one: delete `meta.json` and
  `agent.json`, keep `log.jsonl`. This is the deliberate "no, that one is
  finished" gesture, and it is the answer to a lingering `agent.json` from an
  agent that had already exited (§7).
- **Age.** A dead session not active within `TRIP_RESUME_TTL` (default 7 days,
  measured from `log.jsonl`'s mtime) stops being offered.

  Expiry **hides; it never deletes.** An earlier draft had it remove
  `meta.json` on the next scan, and §3.3 puts that scan on the path that draws
  the chooser — so merely opening `trip enter` to switch sessions would
  permanently retire a resurrectable session, irreversibly and silently, as a
  side effect of a read. Nothing that destroys resume state should be triggered
  by looking. The files are a few hundred bytes beside a `log.jsonl` that is
  kept anyway; only the *list* needs pruning, and hiding prunes it. Deletion
  happens on exactly two deliberate acts: discard, and a successful resurrect
  (which overwrites `meta.json` because the session is live again).

Lazy resurrection is what makes this affordable: after a reboot the user
revives the two sessions they need rather than all eight, and the rest age out
without ever costing a relaunched agent.

### 3.6 Hook guidance (docs change)

No new hook *types* are required — the README's existing `SessionStart`
recommendation already covers re-registration on resume for both agents.
README gains a section on dead rows in the chooser and a note that the
SessionStart hook is what makes resumed agents re-register automatically.

## 4. Acceptance criteria

Reproduce the incident in an isolated `HOME` and verify:

- Start a session, run an agent in it, `trip on` registers with `resume_id` and
  `log_offset` populated and the daemon records `fg_argv` in `meta.json`;
  `kill -9` the daemon.
- `trip enter` shows exactly that session as a dead row (sessions that exited
  cleanly before the kill are absent), naming what it will relaunch.
- Picking it recreates the session with the same name and cwd and the agent
  opens the **same conversation** — verified by asking it for something only
  the pre-crash turns contain, rather than by assuming a transcript layout.
  Record which layout the agent actually used (§7): if it reuses the pre-crash
  file its id matches `resume_id`; if it opens a fresh one the id differs, and
  that file's opening turns are checked for a copy of the prior conversation.
- Either way `trip log <name>` shows pre-crash history followed by post-resume
  events with **no replayed duplicate** of the old transcript. Assert this on a
  reused transcript specifically — that is the case `last_size = 0` breaks and
  §3.4 fixes.
- The same flow for a **non-agent** session: `vim` running at the moment of
  death comes back as `vim`, from `fg_argv` alone, with no `agent.json`
  involved.
- A candidate whose cwd was deleted is reported, not created in the wrong
  directory.
- A candidate whose `log.jsonl` has not moved within `TRIP_RESUME_TTL` is not
  offered, and drawing the chooser deletes nothing — its `meta.json` and
  `log.jsonl` both survive. Only discard removes `meta.json`.
- An agent session running for hours has `fg_argv` holding the agent, not the
  shell — the regression the sampler's own task exists to prevent. Assert it
  with `agent.json` present, since that is when the shared-loop version broke.
- `cargo test` covers: resume-argv construction for both kinds (including
  stripping a pre-existing `--resume`), candidate scanning (meta present /
  absent / live / expired), a tail that starts mid-file, and `SessionMeta` /
  `AgentConfig` parsing of the older field sets.

## 5. Implementation sequence

1. `procinfo::get_argv(pid)` (both platforms).
2. `SessionMeta` gains `fg_argv` / `fg_cwd` / `updated_at`, refreshed on change
   by a sampler in its own task (§3.1 — *not* the agent watcher's loop). Useful on its own — `trip ls` can show the running
   job rather than just the process name.
3. `AgentConfig` gains `resume_id` + `log_offset`; `trip on` populates them;
   `tail_agent_log` seeks to `log_offset` (§3.4). Unit tests for id derivation
   from hook stdin, env, and transcript paths, and for a mid-file tail.
4. Candidate scan + expiry, surfaced first in `trip ls` (a plain listing needs
   no chooser and makes 1–3 testable end to end).
5. Dead rows in the chooser, resurrect-on-pick, and `Outcome::Discard`.
6. README: dead rows, `TRIP_RESUME_TTL`, hook note.

Steps 1–4 carry the substance and are shippable without any chooser work. Only
step 5 depends on the session-switcher plan's `Chooser` (its §5 step 1:
`src/client/chooser.rs`, a pure refactor that is shippable alone). Land
`Chooser` first and step 5 inherits its viewport and unit tests instead of
porting `select_choice` a second time.

## 6. Decisions

- **Candidate marker is `meta.json` presence** (from PR #10), not a session
  registry the daemon maintains. One file per session, written and removed at
  the natural lifecycle points, and it degrades correctly: pre-feature dirs are
  simply never offered.
- **No `trip resume` command.** Dead sessions are rows in the chooser the
  session-switcher plan already makes the hub. One list, one set of keys, and
  resurrection is discovered by the same person who was going to type
  `trip enter` anyway. The cost is that bulk resurrection has no home — see §7.
- **The daemon captures the running job, not `trip on`.** It needs no
  cooperation from the program, which is the only way `vim` and `psql` are ever
  covered, and `tcgetpgrp` gives the job rather than a transient subprocess
  (§3.1). Generalising `trip on` was the alternative and does not work: a
  generic command will never call it.
- **Same session name on resume**, continuing the existing log, rather than a
  `name.resumed` variant. Contiguous history is the point of trip's logs.
- **No new daemon requests.** Resurrection is orchestration over
  `CreateSession`/`SendInput`/`ListSessions`. Two daemon-side edits ride along
  — the `meta.json` refresh (§3.1) and the `tail_agent_log` seek (§3.4) — but
  both are behaviour, not protocol.

## 7. Open questions

- **Does `claude --resume` / `codex resume` continue the pre-crash transcript
  or open a fresh one?** Not guessed here, and deliberately not depended on:
  §3.4's `log_offset` seek is correct either way and §4 verifies the
  conversation by content rather than file identity. Worth recording the answer
  for both agents once observed (owner: whoever picks up step 3), since it
  decides whether the post-resume transcript id matches `resume_id`.
- **Codex flag passthrough on `resume`**: verify against the installed codex
  CLI during implementation (owner: whoever picks up step 3); the conservative
  fallback is `codex resume <id>` with no extra flags.
- **Bulk resurrection after a reboot.** Dropping `trip resume` drops
  `--all` with it. The bet is that lazy revival is better — you bring back the
  two sessions you need rather than relaunching eight agents — and that expiry
  (§3.5) handles the rest. If picking rows one at a time turns out to be the
  wrong shape after a real reboot, the cheap answer is a multi-select in the
  chooser rather than a new command.
- **A job that changes its own process group.** `tcgetpgrp` reads the
  foreground pgid, so a program that calls `setpgid`/`tcsetpgrp` itself — a
  nested shell doing job control — would be recorded as whatever it handed the
  terminal to. Harmless for the cases this targets; worth knowing before
  trusting `fg_argv` for anything beyond resume.
- **Lingering `agent.json` after a clean agent exit**: if the agent exits and
  the daemon dies before the user's next prompt (which is what deletes
  `agent.json`), the row will offer to relaunch a conversation the user had
  finished. Deliberately deferred: the row shows when it was last active and
  the discard key covers it. If it bites in practice, teach the scan to peek at the
  transcript tail for a terminal event.
- **Waiting for shell readiness before `SendInput`**: kernel PTY buffering
  should make an immediate send safe; if startup banners interleave badly, add
  a short daemon-side "send after first output" option. Deferred until
  observed.
