---
status: in-progress
priority: now
---

# Releases

`trip` gets numbered releases, an installer that builds a tagged release into `~/.local/bin` without `sudo`, and `trip upgrade`, which moves you to the newest release without disturbing the sessions you have open.

## 1. Problem

`trip` has no releases. Every install builds whatever `main` holds today: `install.sh` runs `cargo build --release` in a checkout and copies the binary to `/usr/local/bin/trip` with `sudo`. So there is no version anyone can name, no way to say which commit a machine runs, and no command that upgrades one. `trip` has no `--version`, and `Cargo.toml` still says `0.1.0`, which it has said since the first commit.

The installer also puts `trip` where it is not used. On the machines that run it, `trip` is `~/.local/bin/trip`, a binary copied there by hand, while `install.sh` writes `/usr/local/bin/trip`, which does not exist there. The README's terminal setup names `/usr/local/bin/trip enter`.

Upgrading has a cost that nothing tells you about. The daemon owns every session's terminal, so it can't restart without ending them. `install.sh` already replaces the binary by renaming it, so a running daemon keeps the old file and keeps working. But nothing says that the daemon is now older than the client, and the only way to move it to the new binary, `trip shutdown --yes`, ends every session.

The repository has no CI either. Its only workflow publishes the project site, so nothing checks a pull request before it merges.

## 2. Solution

`trip` releases the way Projector does: a hand-started workflow opens a version-only release pull request, and merging it tags the release and publishes it. Releases publish no binaries. The installer downloads a tag's source and builds it on your machine with `cargo`. It builds each version into its own directory under `~/.local/share/trip/`, switches `~/.local/bin/trip` to it only once the whole build succeeds, and keeps the previous version. `trip upgrade` finds the newest release, runs that tag's installer, and checks that what it installed matches the tag. It never restarts the daemon. It tells you the daemon still runs the old version and how many sessions it holds, and you restart it with `trip shutdown --yes` when nothing you care about is running. A new `check.yml` workflow runs the build, lints, and tests on every pull request, the release pull request included.

### 2.1 Cutting a release

1. You start the release with `gh workflow run release.yml -f bump=patch`, `minor`, or `major`.
2. The workflow's propose job refuses any ref other than `main`. It bumps the version in every file that holds it (§ 2.5), commits `Release X.Y.Z` on a `release/vX.Y.Z` branch, and refuses the commit if it touches any file but those. It pushes the branch and opens the release pull request, or reuses one that is already open.
3. A pull request that the workflow's own token opens starts no workflow by itself, so the propose job dispatches `check.yml` onto the release branch. The pull request then shows its checks.
4. You approve and merge the release pull request. Merging it is the decision to ship.
5. The push to `main` that changes the version starts the publish job. It checks whether the release is already complete and does only the steps still missing, in this order:
   1. Creates and pushes the annotated tag `vX.Y.Z`. It never moves an existing one.
   2. Creates the GitHub release for that tag, with notes generated from the merged pull requests.
   3. Moves the major tag `v0` to the new tag, last, so the installer that `v0` serves and the release `trip upgrade` finds always agree.
6. When a step fails, rerunning the job finishes the release without repeating what already happened.

`trip`'s `main` ruleset requires a pull request with one approving review, resolved review threads, and linear history, but no status check, so the release commit needs no review status of its own. It does need that approving review. You can give it yourself, because the workflow's token, not you, opened the pull request and pushed its last commit. The workflow needs one repository setting: "Allow GitHub Actions to create and approve pull requests".

### 2.2 The installed layout

| Path | What it is |
| --- | --- |
| `~/.local/share/trip/<version>/trip` | The binary built for one version |
| `~/.local/share/trip/current` | A symlink to the current version's directory, switched in one rename |
| `~/.local/bin/trip` | A symlink to `~/.local/share/trip/current/trip`, or, after `./install.sh --dev`, to the checkout's `target/debug/trip` (§ 2.3) |
| `~/.local/share/trip/release.json` | The repository and ref the installer used, so `trip upgrade` follows the same fork |

The installer keeps the current version and the one before it, so you can switch back by hand, and deletes older ones. It never deletes the version a running daemon runs, which it learns from the daemon's version record (§ 2.5).

### 2.3 The installer

`install.sh` has three modes. A `Cargo.toml` beside it chooses a checkout mode, and `--dev` chooses the linked one:

| Mode | How you run it | What it installs |
| --- | --- | --- |
| Release | `curl -fsSL https://raw.githubusercontent.com/ninjudd/trip/v0/install.sh \| bash` | The source of `TRIP_REF`, which defaults to `v0`, from `TRIP_REPO`, which defaults to `ninjudd/trip`, built as a release |
| Checkout | `./install.sh` in a clone | The checkout, built as version `<version>-dev`, into the same versioned layout |
| Checkout, linked | `./install.sh --dev` in a clone | No copy: `~/.local/bin/trip` becomes a symlink to the checkout's `target/debug/trip`, as today's `--dev` does, so each `cargo build` updates it |

Only the release mode marks a build as a release. It builds with `TRIP_RELEASE=1` in the environment, which `trip` reads at compile time with `option_env!`, and `trip --version` prints the bare version only when that was set. Every other build, the checkout mode's, the `cargo build` after `--dev`, and any build you run yourself, prints `<version>-dev`. So a binary always says whether it is a release, whichever way it was built.

In the release and checkout modes it:

1. Refuses to start without `cargo`, and says how to install Rust.
2. Builds with `cargo build --release --locked` in a temporary directory, with `TRIP_RELEASE=1` in the release mode only.
3. On success only, moves the binary into `~/.local/share/trip/<version>/`, switches `current`, and makes `~/.local/bin/trip` the symlink to it. On failure it says which version did not install, leaves the current one in place, and removes the partial build.

In the linked mode it refuses to start without `cargo`, runs `cargo build` in the checkout, and points `~/.local/bin/trip` at `target/debug/trip`. It leaves `~/.local/share/trip/` as it is, so a later release or checkout install switches back to `current`.

In every mode it then:

4. Adds the `trip env` shell hook to `~/.zshrc` and `~/.bashrc`, as today, except that the hook runs `trip env` only when `command -v trip` finds `trip`. A session the terminal starts with the absolute path `~/.local/bin/trip enter` can run a shell whose `PATH` lacks `~/.local/bin`, and the hook then does nothing instead of printing `command not found: trip` before every command.
5. Warns when `~/.local/bin` is not on `PATH`, as Projector's installer does, and prints the line to add to your shell's startup file: `export PATH="$HOME/.local/bin:$PATH"`. It never edits `PATH` itself.
6. Warns when `/usr/local/bin/trip` exists from an earlier install, because a terminal that names it would keep running the old binary, and prints the `sudo rm` that removes it. It never runs `sudo` itself.

### 2.4 Upgrading

`trip upgrade`:

1. Finds the newest release's tag through `gh api repos/<repo>/releases/latest`, falling back to the same API anonymously through `curl`. It reads the repository from `release.json`.
2. Downloads that tag's `install.sh` and runs it with `TRIP_REF` set to the tag, so the installer and the source come from one immutable tag.
3. Runs the new `~/.local/bin/trip --version` and checks it against the tag. A mismatch prints both and exits 1.
4. Reads the running daemon's version (§ 2.5). When it is older, it prints, for example: `the daemon still runs 0.2.0 with 4 sessions; it moves to 0.3.0 when its last session ends or you run trip shutdown --yes`.

`trip upgrade --check` prints the installed version and the newest release without installing anything. When `trip --version` ends in `-dev`, which covers both checkout modes, `trip upgrade` says it is a checkout install, points to `./install.sh`, and installs nothing, because a checkout follows the checkout, not releases. So it never replaces a `--dev` link with a release.

### 2.5 Versions and the daemon

- **The version** lives in `Cargo.toml`'s `[package]` and in `trip`'s entry in `Cargo.lock`, and the release workflow bumps both. `trip --version` prints it bare for a build the release installer made, and as `<version>-dev` for every other build (§ 2.3).
- **The daemon records its version** in `~/.trip/daemon.json` when it starts, beside its pid, and removes the file when it exits. The client and the installer ignore a record whose pid is not running, so a record an exited daemon left behind never vouches for a daemon started later, such as one from a stale `/usr/local/bin/trip`. A daemon from before this change writes no record, and the client treats it as older than any release.
- **The client notices an older daemon.** `trip enter`, `attach`, and `ls` print one line when the daemon's version is older than the client's, with the same advice as `trip upgrade`, and go on working. A request the older daemon doesn't understand fails with that line, instead of an opaque decode error.
- **Nothing restarts the daemon** for you. A dev version never counts as older or newer than a release.

### 2.6 Changes by file

| File | Change |
| --- | --- |
| `Cargo.toml` | Adds `description`, `license = "MIT"`, `repository`, and `rust-version` |
| `src/cli.rs` | Adds `--version`, which appends `-dev` unless `TRIP_RELEASE` was set at compile time, and the `upgrade` subcommand with `--check` |
| `src/client/upgrade.rs` | New. Finds the release, runs its installer, verifies the result, and reports the daemon. Refuses a `-dev` install |
| `src/daemon/mod.rs` | Writes `~/.trip/daemon.json` with the version and pid at start, and removes it on exit |
| `src/client/mod.rs` | Reads the daemon's version, ignoring a record whose pid is not running, prints the older-daemon line, and maps a failed request on an older daemon to it |
| `install.sh` | Rewritten for § 2.2 and § 2.3: the release, checkout, and linked modes, `TRIP_RELEASE=1` for release builds only, the versioned layout, the hook that runs only when `trip` resolves, the `PATH` warning, no `sudo` |
| `.github/workflows/check.yml` | New. `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and the end-to-end scripts in `tests/`, on pull requests, on pushes to `main`, and by dispatch |
| `.github/workflows/release.yml` | New. The propose and publish jobs of § 2.1 |
| `.github/scripts/release.py` | New. Bumps the version files, checks that a release commit touches only them, and runs the publish steps idempotently, as Projector's does |
| `tests/release_test.py`, `tests/install_e2e.py` | New. § 4's checks of the release script and the installer |
| `README.md` | The release install command, `trip upgrade`, and the terminal setup naming `~/.local/bin/trip` |
| `LICENSE` | New: the MIT license, as Projector uses |

## 3. Why this design

- **Projector's release flow, reused.** It already solves the hard parts: a release pull request that the workflow opens and still gets checked, a publish job that can be rerun safely, and a major tag that moves last so the installer and the newest release never disagree. Copying it keeps one release process across the tools.
- **Build from source, not binaries.** Publishing binaries means a build matrix for each platform and architecture, uploaded assets and their checksums, and, for binaries downloaded with `curl` on macOS, code signing and notarization or quarantine handling. Building with `cargo` needs none of that, and the people installing `trip` already have Rust. The cost is a build of a minute or two on each upgrade, measured in § 5.
- **A directory per version, switched last.** A failed build can then never leave you without a working `trip`, the version before stays one rename away, and a running daemon keeps its own version's binary on disk while a newer one installs beside it.
- **The daemon is never restarted for you.** It holds every session's terminal, so restarting it ends them. `trip shutdown --yes` already exists for the moment you choose, and the daemon exits by itself when its last session ends. Restarting it automatically when no client is attached was considered and rejected, because it would still end sessions you meant to come back to. Keeping sessions alive across a restart, by passing their terminals to the new daemon, is a project of its own (§ 7).
- **A version file, not a new request.** A daemon from before this change can't answer a version request, but a missing file is unambiguous. Reading a file also costs the client nothing when the daemon is busy.
- **`~/.local/bin`, no `sudo`.** That is where `trip` already runs, where Projector installs, and where nothing needs a password.
- **The MIT license.** The README's License section already names MIT, but the repository has no `LICENSE` file, so the license text a public release needs is missing. Adding the MIT text matches what the README says and what Projector uses.

## 4. Acceptance criteria

| Do | Expect | Catches |
| --- | --- | --- |
| Run `release.py bump minor` on a tree at `0.1.0` | `Cargo.toml` and `Cargo.lock` both say `0.2.0`, and nothing else changed | A bump that misses `Cargo.lock`, so `--locked` builds fail |
| Run the release check on a commit that also touches `src/` | It refuses the commit | A release pull request that carries code past review |
| Run the publish steps twice, the second time after a failure between the tag and the release | The tag exists once, the release exists, and `v0` names the tag | A rerun that fails on the existing tag, or moves `v0` before the release exists |
| Run the release installer in a temporary `HOME` against a local source archive | `~/.local/bin/trip --version` prints the archive's version with no `-dev`, through `current` | An install that switches to the wrong directory |
| Run `cargo build` in a checkout, then the built `trip --version` | `<version>-dev` | A build made outside the release installer read as a release |
| Run `./install.sh --dev` in a temporary `HOME` with an existing release install, then `trip upgrade` | `~/.local/bin/trip` links to `target/debug/trip` and prints `<version>-dev`, `current` still names the release, and `trip upgrade` says it is a checkout install, exits without installing, and leaves the link | A `--dev` install that runs the release steps, or an upgrade that replaces the dev link |
| Make the build fail, then run the installer again | It prints which version did not install, `current` still names the old version, and no partial directory remains | A failed build that leaves no working `trip` |
| Install three versions in turn | Only the last two remain | Unbounded old versions, or deleting the one before |
| Install while a daemon from the oldest version runs | That version is kept | Deleting the binary a running daemon uses |
| Run the installer with a `PATH` that lacks `~/.local/bin` | A warning naming `~/.local/bin` and the `export PATH` line to add, and no startup file's `PATH` changed | A fresh install that leaves `trip` unreachable without saying so |
| In a session started as `~/.local/bin/trip enter`, run a command in a shell whose `PATH` lacks `~/.local/bin` | The command runs with no `command not found: trip` | A shell hook that fails before every command |
| Run `trip upgrade` when the installed binary reports a version other than the tag | Both versions printed, exit 1 | An upgrade that reports success for the wrong build |
| Start a daemon of an older version, then run the new `trip ls` | Its output plus one line naming both versions and the session count | An older daemon nobody notices |
| Run `trip ls` against a daemon with no version record | The same line, naming it as from before releases | A daemon from today's install read as current |
| Stop a daemon, then start one with no version record while the old record names a pid that is no longer running | `daemon.json` is gone after the stop, and `trip ls` names the new daemon as from before releases | A stale record vouching for an older daemon |
| Run the installer with `/usr/local/bin/trip` present | A warning with the `sudo rm` command, and nothing removed | A stale binary shadowing the new one, or `sudo` run unasked |
| Open a pull request | `check.yml` runs and must pass | A release with nothing checking it |

## 5. Cost

- **Upgrade time.** An upgrade builds `trip` from source. Measure a release build on an Apple Silicon Mac from a clean temporary directory, and say the time in the README. `--locked` keeps the dependency set fixed, so the time is stable from one release to the next.
- **CI time.** `check.yml` runs on each pull request and push to `main`. The end-to-end scripts start real daemons in temporary homes, so measure the whole job and keep it under 10 minutes.
- **Client overhead.** Reading `daemon.json` is one small file read for `enter`, `attach`, and `ls`.

## 6. Rollout

1. `check.yml`, the `Cargo.toml` metadata, `LICENSE`, and `trip --version`, so every later change is checked.
2. The installer and the layout, with the README's install section and terminal setup.
3. The release workflow and `release.py`. You then turn on "Allow GitHub Actions to create and approve pull requests" in the repository settings.
4. `trip upgrade`, the daemon's version record, and the older-daemon line.
5. The first release, `gh workflow run release.yml -f bump=minor`, which publishes `0.2.0`. Install it on each machine with the release command once, which also moves it off the hand-copied binary.

Steps 1 to 4 can ship as one stack. The first release waits for all of them, so its installer and `trip upgrade` already exist.

## 7. Open questions

- **Prebuilt binaries.** Deferred: releases build from source (§ 3). Prebuilt binaries for macOS and Linux can follow as a later option if installing without Rust comes to matter.
- **Linux.** `check.yml` runs on macOS. Add a Linux runner once the tests are known to pass there. Owner: whoever implements step 1.
- **Keeping sessions across a daemon restart.** Passing the sessions' terminals and screen state to a new daemon would let an upgrade restart the daemon without ending anything. Deferred to its own project.
- **A hosted installer address.** `raw.githubusercontent.com` at `v0` serves the installer for now. A short address can come later.

## 8. Background

- **Projector's release system**, the model for § 2.1: `.github/workflows/release.yml` (the propose and publish jobs), `.github/scripts/release.py` (the version files, the bump, and the idempotent publish steps, with `v0` moved last), `install.sh` (two modes chosen by what sits beside it, `PROJECTOR_REF` defaulting to `v0`), and `project upgrade` in `src/projector/cli.py` (the newest release's tag, that tag's installer, and the check of what it installed). Its "Release an update" section in `docs/plugins.md` describes the process.
- **`trip` today**: `Cargo.toml` at `0.1.0` with no metadata and no `LICENSE`; no tags; one workflow, the project site; `install.sh` building `--release` and copying to `/usr/local/bin` with `sudo` by a rename that leaves a running daemon on the old file; no `--version`. The client starts the daemon from `current_exe()` (`src/client/launch.rs`), and the daemon owns each session's terminal (`src/daemon/session.rs`), so a restart ends them. `trip shutdown` ends every session (`README.md`).

## 9. What implementation settled

- **The minimum Rust version is 1.85.** `clap` 4.6 and `vt100` 0.18 use the 2024 edition and declare `rust-version = "1.85"`, the first release with that edition, and nothing in `trip`'s own code needs newer than 1.82 (`Option::is_none_or`). `check.yml` has an `msrv` job that reads `rust-version` from `Cargo.toml` and runs `cargo check --locked --all-targets` on that toolchain, so a dependency bump or newer API that raises it fails a pull request. With `rust-version` set, clippy's `incompatible_msrv` lint also flags a standard library API newer than it.
- **Only `TRIP_RELEASE=1` marks a release.** Any other value, `0` included, builds `<version>-dev`. `tests/version.rs` runs the built binary's `--version` and `-V` and expects the version its own build's `TRIP_RELEASE` calls for, so `check.yml` runs it three times: in `cargo test`, as `TRIP_RELEASE=1 cargo test --test version`, and as `TRIP_RELEASE=0 cargo test --test version`, so a change that let any value mark a release fails. Cargo tracks the variable, so a build after a change to it recompiles `trip` and reports the new answer.
- **The `check` job takes about 4 minutes**, well under § 5's budget of 10. Its first run, on `macos-latest`, took 4 minutes 4 seconds, end-to-end scripts included, and the `msrv` job beside it took under 30 seconds. A push to a pull request cancels that pull request's earlier run. Every other run, a push to `main` or a `workflow_dispatch`, takes a concurrency group of its own keyed on `github.run_id`, so nothing cancels or replaces it and every merged commit keeps its own result. A shared group with `cancel-in-progress: false` would not be enough, because GitHub keeps only one pending run per group, and a third merge inside one run's four minutes would cancel the second merge's run.
- **Linux stays deferred** (§ 7). `check.yml` runs on `macos-latest` only.
