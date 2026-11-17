---
status: draft
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
4. You merge the release pull request. Merging it is the decision to ship.
5. The push to `main` that changes the version starts the publish job. It checks whether the release is already complete and does only the steps still missing, in this order:
   1. Creates and pushes the annotated tag `vX.Y.Z`. It never moves an existing one.
   2. Creates the GitHub release for that tag, with notes generated from the merged pull requests.
   3. Moves the major tag `v0` to the new tag, last, so the installer that `v0` serves and the release `trip upgrade` finds always agree.
6. When a step fails, rerunning the job finishes the release without repeating what already happened.

`trip`'s `main` requires a pull request and linear history but no status check, so the release commit needs no review status of its own. The workflow needs one repository setting: "Allow GitHub Actions to create and approve pull requests".

### 2.2 The installed layout

| Path | What it is |
| --- | --- |
| `~/.local/share/trip/<version>/trip` | The binary built for one version |
| `~/.local/share/trip/current` | A symlink to the current version's directory, switched in one rename |
| `~/.local/bin/trip` | A symlink to `~/.local/share/trip/current/trip` |
| `~/.local/share/trip/release.json` | The repository and ref the installer used, so `trip upgrade` follows the same fork |

The installer keeps the current version and the one before it, so you can switch back by hand, and deletes older ones. It never deletes the version a running daemon runs, which it learns from the daemon's version record (§ 2.5).

### 2.3 The installer

`install.sh` has two modes, chosen by whether a `Cargo.toml` sits beside it:

| Mode | How you run it | What it builds |
| --- | --- | --- |
| Release | `curl -fsSL https://raw.githubusercontent.com/ninjudd/trip/v0/install.sh \| bash` | The source of `TRIP_REF`, which defaults to `v0`, from `TRIP_REPO`, which defaults to `ninjudd/trip` |
| Checkout | `./install.sh` in a clone | The checkout, as version `<version>-dev`, so a development build is never mistaken for a release |
| Checkout, linked | `./install.sh --dev` | Links `~/.local/bin/trip` to `target/debug/trip`, as today's `--dev` does, so each `cargo build` updates it |

In each mode it:

1. Refuses to start without `cargo`, and says how to install Rust.
2. Builds with `cargo build --release --locked` in a temporary directory.
3. On success only, moves the binary into `~/.local/share/trip/<version>/`, switches `current`, and makes `~/.local/bin/trip` the symlink. On failure it says which version did not install, leaves the current one in place, and removes the partial build.
4. Adds the `trip env` shell hook to `~/.zshrc` and `~/.bashrc`, as today.
5. Warns when `/usr/local/bin/trip` exists from an earlier install, because a terminal that names it would keep running the old binary, and prints the `sudo rm` that removes it. It never runs `sudo` itself.

### 2.4 Upgrading

`trip upgrade`:

1. Finds the newest release's tag through `gh api repos/<repo>/releases/latest`, falling back to the same API anonymously through `curl`. It reads the repository from `release.json`.
2. Downloads that tag's `install.sh` and runs it with `TRIP_REF` set to the tag, so the installer and the source come from one immutable tag.
3. Runs the new `~/.local/bin/trip --version` and checks it against the tag. A mismatch prints both and exits 1.
4. Reads the running daemon's version (§ 2.5). When it is older, it prints, for example: `the daemon still runs 0.2.0 with 4 sessions; it moves to 0.3.0 when its last session ends or you run trip shutdown --yes`.

`trip upgrade --check` prints the installed version and the newest release without installing anything. In a checkout install, `trip upgrade` says so and points to `./install.sh`, because a checkout follows the checkout, not releases.

### 2.5 Versions and the daemon

- **The version** lives in `Cargo.toml`'s `[package]` and in `trip`'s entry in `Cargo.lock`, and the release workflow bumps both. `trip --version` prints it.
- **The daemon records its version** in `~/.trip/daemon.json` when it starts, beside its pid. A daemon from before this change writes no record, and the client treats it as older than any release.
- **The client notices an older daemon.** `trip enter`, `attach`, and `ls` print one line when the daemon's version is older than the client's, with the same advice as `trip upgrade`, and go on working. A request the older daemon doesn't understand fails with that line, instead of an opaque decode error.
- **Nothing restarts the daemon** for you. A dev version never counts as older or newer than a release.

### 2.6 Changes by file

| File | Change |
| --- | --- |
| `Cargo.toml` | Adds `description`, `license = "MIT"`, `repository`, and `rust-version` |
| `src/cli.rs` | Adds `--version`, and the `upgrade` subcommand with `--check` |
| `src/client/upgrade.rs` | New. Finds the release, runs its installer, verifies the result, and reports the daemon |
| `src/daemon/mod.rs` | Writes `~/.trip/daemon.json` with the version and pid at start |
| `src/client/mod.rs` | Reads the daemon's version, prints the older-daemon line, and maps a failed request on an older daemon to it |
| `install.sh` | Rewritten for § 2.2 and § 2.3: release and checkout modes, the versioned layout, no `sudo` |
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
- **The daemon is never restarted for you.** It holds every session's terminal, so restarting it ends them. `trip shutdown --yes` already exists for the moment you choose, and the daemon exits by itself when its last session ends. Keeping sessions alive across a restart, by passing their terminals to the new daemon, is a project of its own (§ 7).
- **A version file, not a new request.** A daemon from before this change can't answer a version request, but a missing file is unambiguous. Reading a file also costs the client nothing when the daemon is busy.
- **`~/.local/bin`, no `sudo`.** That is where `trip` already runs, where Projector installs, and where nothing needs a password.
- **The MIT license.** `trip` has had no license, so a public release would leave others no right to use it. MIT matches Projector.

## 4. Acceptance criteria

| Do | Expect | Catches |
| --- | --- | --- |
| Run `release.py bump minor` on a tree at `0.1.0` | `Cargo.toml` and `Cargo.lock` both say `0.2.0`, and nothing else changed | A bump that misses `Cargo.lock`, so `--locked` builds fail |
| Run the release check on a commit that also touches `src/` | It refuses the commit | A release pull request that carries code past review |
| Run the publish steps twice, the second time after a failure between the tag and the release | The tag exists once, the release exists, and `v0` names the tag | A rerun that fails on the existing tag, or moves `v0` before the release exists |
| Run the release installer in a temporary `HOME` against a local source archive | `~/.local/bin/trip --version` prints the archive's version, through `current` | An install that switches to the wrong directory |
| Make the build fail, then run the installer again | It prints which version did not install, `current` still names the old version, and no partial directory remains | A failed build that leaves no working `trip` |
| Install three versions in turn | Only the last two remain | Unbounded old versions, or deleting the one before |
| Install while a daemon from the oldest version runs | That version is kept | Deleting the binary a running daemon uses |
| Run `trip upgrade` when the installed binary reports a version other than the tag | Both versions printed, exit 1 | An upgrade that reports success for the wrong build |
| Start a daemon of an older version, then run the new `trip ls` | Its output plus one line naming both versions and the session count | An older daemon nobody notices |
| Run `trip ls` against a daemon with no version record | The same line, naming it as from before releases | A daemon from today's install read as current |
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

- **Prebuilt binaries.** Building from source is the recommendation (§ 3). Prebuilt binaries for macOS and Linux could follow if installing without Rust matters. Owner: the user.
- **What an upgrade does to the daemon.** Leaving it running and saying so is the recommendation (§ 2.4). The alternative restarts it when no client is attached, which ends idle sessions. Owner: the user.
- **Linux.** `check.yml` runs on macOS. Add a Linux runner once the tests are known to pass there. Owner: whoever implements step 1.
- **Keeping sessions across a daemon restart.** Passing the sessions' terminals and screen state to a new daemon would let an upgrade restart the daemon without ending anything. Deferred to its own project.
- **A hosted installer address.** `raw.githubusercontent.com` at `v0` serves the installer for now. A short address can come later.

## 8. Background

- **Projector's release system**, the model for § 2.1: `.github/workflows/release.yml` (the propose and publish jobs), `.github/scripts/release.py` (the version files, the bump, and the idempotent publish steps, with `v0` moved last), `install.sh` (two modes chosen by what sits beside it, `PROJECTOR_REF` defaulting to `v0`), and `project upgrade` in `src/projector/cli.py` (the newest release's tag, that tag's installer, and the check of what it installed). Its "Release an update" section in `docs/plugins.md` describes the process.
- **`trip` today**: `Cargo.toml` at `0.1.0` with no metadata and no `LICENSE`; no tags; one workflow, the project site; `install.sh` building `--release` and copying to `/usr/local/bin` with `sudo` by a rename that leaves a running daemon on the old file; no `--version`. The client starts the daemon from `current_exe()` (`src/client/launch.rs`), and the daemon owns each session's terminal (`src/daemon/session.rs`), so a restart ends them. `trip shutdown` ends every session (`README.md`).
