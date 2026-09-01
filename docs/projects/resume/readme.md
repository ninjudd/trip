---
status: ready
priority: now
---

# Resume

## 1. Outcome

`trip resume` brings back the sessions that died with the daemon. After a
daemon crash, kill, or reboot, it lists the sessions that were alive at the
moment of death, recreates them (same name, same cwd), and relaunches any
agent that was running inside each one — resumed into the same conversation
via `claude --resume <session-id>` / `codex resume <thread-id>`.

Motivating incident: on 2026-08-29 the daemon died silently (~12:31), taking
every session with it. The transcripts survived on disk in `~/.claude` and
`~/.codex`, but reconnecting each agent to its conversation meant manually
hunting down ids and retyping flags. PR #10 (`daemon-hardening`) added the
crash marker this project builds on.

## 2. What already exists

- **`meta.json`** (PR #10): written to the session dir by `Session::spawn`
  with `name`, `command` (original argv, null = default shell), `cwd`,
  `created_at`. Removed on every clean exit path (reaper, kill, shutdown).
  Its presence therefore means "this session died with the daemon" — the
  exact candidate set for resume. Old session dirs that predate PR #10 have
  no `meta.json` and are never offered.
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
- **Picker UI**: the `enter -a` picker (`ls-grouping-and-enter-picker`,
  `enter-all` branches, `src/client/mod.rs`) gives the interaction model for
  choosing among candidates.
- **Existing protocol suffices**: `CreateSession` + `SendInput` can recreate a
  session and type the resume command into it. v1 needs no daemon protocol
  changes.

## 3. Design

### 3.1 Enrich `agent.json` at registration time

Extend `AgentConfig` (all new fields `#[serde(default)]` so existing files
still parse):

```json
{
  "kind": "claude",
  "log_path": "/Users/.../.claude/projects/-Users-x-proj/<uuid>.jsonl",
  "resume_id": "<uuid>",
  "log_offset": 48291,
  "argv": ["claude", "--permission-mode", "auto"]
}
```

- `resume_id`: `trip on` fills it from, in order: the hook's stdin JSON
  `session_id` field, `CLAUDE_CODE_SESSION_ID` / `CODEX_THREAD_ID`, else
  parsed from the transcript path (claude: basename; codex: uuid in the
  rollout filename) or the transcript's first `session_meta` line.
- `log_offset`: the size of `log_path` at the moment `trip on` writes the
  file. `tail_agent_log` starts every tail at `last_size = 0`
  (`agent.rs:275`) and so reads the whole transcript on its first pass —
  harmless when the file is new, destructive when it is not, which is exactly
  the resumed case. See §3.4.
- `argv`: how the user actually launched the agent, so resume preserves flags
  like `--permission-mode auto` or `--yolo`. `trip on` runs as a descendant
  of the agent process (hook command), so it walks its own ppid chain until
  it finds a process whose name matches `kind`, and captures that process's
  argv. Requires a new `procinfo::get_argv(pid)` (macOS: `sysctl
  KERN_PROCARGS2`; linux: `/proc/<pid>/cmdline` — `procinfo.rs` already has
  the cfg split). On failure, `argv` stays null and resume falls back to the
  bare binary name.

Rejected alternative: having the daemon's agent watcher enrich `agent.json`
with the PTY's foreground argv when it first sees the file. It works without
hook cooperation, but writes the file from two processes (`trip on` client +
daemon) and the fg pid can transiently be a subprocess; the ppid walk in
`trip on` is a single atomic write with the agent guaranteed to be an
ancestor.

### 3.2 The `trip resume` command (client-only)

Candidate scan: walk `~/.trip/sessions/*/*` (and legacy unscoped dirs) for
dirs containing `meta.json`, minus names that already exist live in the
daemon (`ListSessions`). For each candidate show name, cwd, agent kind (from
`agent.json` if present), and last activity (`log.jsonl` mtime).

UX:

- `trip resume` — interactive picker over all candidates (reuse the
  `enter -a` picker style), with an option to resume all and an option to
  discard a candidate (deletes `meta.json` + `agent.json`, keeps the log).
- `trip resume <name>` — resume one session.
- `trip resume --all` — resume everything non-interactively.
- `trip resume --dry-run` — print exactly what would be created and typed,
  no side effects.

Per selected candidate:

1. If `meta.cwd` no longer exists, warn and skip (a recreated shell would
   silently land in the daemon's cwd — `session.rs` uses
   `set_current_dir(&cwd).ok()`).
2. Read `agent.json` into memory, then delete it **before** creating the
   session. Otherwise the daemon's agent watcher sees it, tails the *old*
   transcript from offset 0, and re-emits the entire prior conversation into
   `log.jsonl`.

   The read is not only for the command line the client has to build anyway
   (`kind`, `resume_id`, `argv`). Step 3 can fail — `CreateSession` rejects a
   name claimed since the scan (`daemon/mod.rs:251`, the same list staleness
   #11 dealt with), and `Session::spawn` can fail on `openpty` or `fork` — and
   a deleted `agent.json` is unrecoverable, dropping the session out of the
   candidate set as an agent session for good. So write it back if the create
   fails.

   Deleting *after* a successful create is the obvious alternative and is
   worse: the daemon polls for `agent.json` every 2s (`session.rs:256`) and
   would likely see the stale file first, which is the race step 2 exists to
   avoid.
3. `CreateSession` with the same name and `meta.cwd`. Reusing the name means
   `log.jsonl` continues in the same file — history stays contiguous — and
   `Session::spawn` overwrites `meta.json` with fresh metadata.
4. Launch the work:
   - Agent session (`agent.json` existed): create the session with the
     default shell, then `SendInput` the resume command line. Typing into a
     shell reproduces normal usage — preexec/`trip env` runs, the agent's
     SessionStart hook re-registers `trip on` against whatever transcript the
     resumed agent writes to (the README's recommended matcher is already
     `startup|resume`), and when the agent exits the user still has a shell.
     That transcript may be the pre-crash file rather than a fresh one, which
     is what §3.4 exists to handle. PTY input is buffered by the kernel, so
     sending before the shell prints its prompt is safe (same mechanism
     tmux-resurrect relies on).
   - No agent: recreate per `meta.command` (explicit argv → same argv;
     null → default shell).
5. Print a summary and an `trip enter <name>` hint (or enter directly when
   exactly one session was resumed and stdout is a tty).

Resume command construction, per kind:

- claude: original `argv` with any `--resume`/`--continue`/`--session-id`
  arguments stripped, plus `--resume <resume_id>`.
- codex: `codex resume <resume_id>` followed by the original flags with the
  original subcommand (if any) removed. Which flags `codex resume` accepts
  needs verification during implementation (`--yolo` is known-good: observed
  in real use as `codex resume <uuid> --yolo`).
- Fallback when `argv` is null: `claude --resume <id>` / `codex resume <id>`.

Rejected alternative: running the resume argv as the session command instead
of typing it into a shell. Deterministic, but the session dies when the agent
exits, no shell hooks run (so `trip on` re-registration depends entirely on
the agent-side hook), and it diverges from how users actually run agents in
trip. The shell + `SendInput` route was chosen; revisit only if input races
show up in practice.

### 3.3 Hook guidance (docs change)

No new hook *types* are required — the README's existing `SessionStart`
recommendation already covers re-registration on resume for both agents.
README gains a `trip resume` section and a note that the SessionStart hook
is what makes resumed agents re-register automatically.

### 3.4 Stop the replay at re-registration, not only before create

Step 2 deletes the stale `agent.json` so the *pre-resume* watcher cannot replay
the old transcript. The same mechanism then fires a second time, and nothing
above stops it: when the resumed agent's SessionStart hook writes a fresh
`agent.json`, the daemon starts a new `tail_agent_log`, and that one also
begins at `last_size = 0` (`agent.rs:275`). Whether it replays the conversation
depends on which file the resumed agent writes to — an open question (§7) whose
answer this design deliberately does not depend on.

So `trip on` records `log_offset` (§3.1) and the tailer seeks there instead of
to 0:

```rust
let mut last_size: u64 = cfg.log_offset;   // was: 0
```

For a genuinely new transcript `log_offset` is 0 and nothing changes; for a
reused one the tailer emits only what the resumed agent appends. This also
demotes step 2's delete-before-create from the only thing standing between
resume and a duplicated conversation to a belt-and-braces measure.

This is the one daemon-side change in v1. It adds no request to the protocol —
`trip resume` itself stays pure orchestration (§6) — but the tailer edit ships
with the command rather than after it, because without it §4's "no replayed
duplicate" criterion cannot be met.

## 4. Acceptance criteria

Reproduce the incident in an isolated `HOME` and verify:

- Start a session, run an agent in it, `trip on` registers with `resume_id`
  and `argv` populated; `kill -9` the daemon.
- `trip resume` lists exactly that session (sessions that exited cleanly
  before the kill are absent); `--dry-run` prints the create + send plan
  without touching anything.
- Resuming recreates the session with the same name and cwd and the agent
  opens the **same conversation** — verified by asking it for something only
  the pre-crash turns contain, rather than by assuming a transcript layout.
  Record which layout the agent actually used (§7): if it reuses the pre-crash
  file its id matches `resume_id`; if it opens a fresh one the id differs, and
  that file's opening turns are checked for a copy of the prior conversation.
- Either way `trip log <name>` shows pre-crash history followed by post-resume
  events with **no replayed duplicate** of the old transcript. Assert this on a
  reused transcript specifically — that is the case `last_size = 0` breaks and
  §3.4 fixes.
- A candidate whose cwd was deleted is skipped with a warning, not created
  in the wrong directory.
- `cargo test` covers: resume-argv construction for both kinds (including
  stripping a pre-existing `--resume`), candidate scanning (meta present /
  absent / session live), and `AgentConfig` parsing of legacy two-field
  files.

## 5. Implementation sequence

1. `procinfo::get_argv(pid)` (both platforms) + ppid-walk helper.
2. `AgentConfig` gains `resume_id`, `log_offset` + `argv`; `trip on` populates
   them; `tail_agent_log` seeks to `log_offset` (§3.4); unit tests for id
   derivation from hook stdin, env, and transcript paths, and for a tail that
   starts mid-file.
3. `trip resume` command: scan, dry-run, single-name resume (create + send).
4. Picker + `--all` + discard action.
5. README: command reference entry + hook note.

Steps 1–3 are useful on their own; 4–5 polish. Depends on `enter-all` for
picker plumbing only — steps 1–3 don't need it.

## 6. Decisions

- **Candidate marker is `meta.json` presence** (from PR #10), not a session
  registry the daemon maintains. One file per session, written and removed at
  the natural lifecycle points, and it degrades correctly: pre-feature dirs
  are simply never offered.
- **Same session name on resume**, continuing the existing log, rather than a
  `name.resumed` variant. Contiguous history is the point of trip's logs.
- **Client-only v1.** No new daemon requests; resume is orchestration over
  `CreateSession`/`SendInput`/`ListSessions`. One daemon-side edit rides along,
  the `tail_agent_log` seek in §3.4, but it is a behaviour fix rather than
  protocol.
- **`trip on` owns id/argv capture** (single writer of `agent.json`), not the
  daemon watcher — see §3.1.

## 7. Open questions

- **Lingering `agent.json` after a clean agent exit**: if the agent exits and
  the daemon dies before the user's next prompt (which is what deletes
  `agent.json`), resume will offer to relaunch a conversation the user had
  finished. Deliberately deferred: the picker shows last-activity time and
  the discard action covers it. If it bites in practice, teach `trip resume`
  to peek at the transcript tail for a terminal event.
- **Does `claude --resume` / `codex resume` continue the pre-crash transcript
  or open a fresh one?** Not guessed here, and deliberately not depended on:
  §3.4's `log_offset` seek is correct either way and §4 verifies the
  conversation by content rather than file identity. Worth recording the answer
  for both agents once observed (owner: whoever picks up step 3), since it
  decides whether the post-resume transcript id matches `resume_id`.
- **Codex flag passthrough on `resume`**: verify against the installed codex
  CLI during implementation (owner: whoever picks up step 3); the
  conservative fallback is `codex resume <id>` with no extra flags.
- **Waiting for shell readiness before `SendInput`**: kernel PTY buffering
  should make an immediate send safe; if startup banners interleave badly,
  add a short daemon-side "send after first output" option. Deferred until
  observed.
