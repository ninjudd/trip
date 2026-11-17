#!/usr/bin/env bash
#
# Build trip with cargo and install it into ~/.local, without sudo.
#
#   curl -fsSL https://raw.githubusercontent.com/ninjudd/trip/v0/install.sh | bash
#   ./install.sh          from a checkout: build the checkout as <version>-dev
#   ./install.sh --dev    from a checkout: link ~/.local/bin/trip to target/debug
#
# Without a checkout beside it, as when bash reads it from curl, the installer
# builds the source archive of TRIP_REF (default v0, the tag every release
# moves) from TRIP_REPO (default ninjudd/trip). TRIP_ARCHIVE, a URL such as
# file:///tmp/trip.tar.gz, replaces that archive.
#
# Each version lives in ~/.local/share/trip/<version>/. The installer switches
# ~/.local/share/trip/current to a version only once its build succeeds, keeps
# the version before it, and never removes the one a running daemon uses.

set -euo pipefail

# The directory holding this script, or nothing when bash reads it from stdin.
SCRIPT_DIR=""
if [ -n "${BASH_SOURCE[0]:-}" ] && [ -f "${BASH_SOURCE[0]}" ]; then
  SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fi
# A checkout is this script with trip's Cargo.toml beside it. A copy of the
# script on its own, such as the one `trip upgrade` downloads, is not one.
REPO=""
if [ -n "$SCRIPT_DIR" ] && grep -qs '^name = "trip"' "$SCRIPT_DIR/Cargo.toml"; then
  REPO="$SCRIPT_DIR"
fi
TRIP_REPO="${TRIP_REPO:-ninjudd/trip}"
TRIP_REF="${TRIP_REF:-v0}"
SHARE_DIR="$HOME/.local/share/trip"
BIN_DIR="$HOME/.local/bin"
DAEMON_RECORD="$HOME/.trip/daemon.json"
# Where install.sh used to put trip with sudo. Overridable so a test can check
# the warning without that file.
LEGACY_BIN="${TRIP_LEGACY_BIN:-/usr/local/bin/trip}"

MARKER="# trip shell hook"
# The hook asks `command -v` first: a terminal that starts the absolute path
# ~/.local/bin/trip can run a shell whose PATH lacks ~/.local/bin, and an
# unguarded hook would print `command not found: trip` before every command.
HOOK='# trip shell hook
if [ -n "$TRIP_SESSION" ]; then
  _trip_preexec() { if command -v trip >/dev/null 2>&1; then eval "$(trip env)"; fi; }
  if [ -n "$ZSH_VERSION" ]; then
    preexec_functions+=(_trip_preexec)
  elif [ -n "$BASH_VERSION" ]; then
    trap '"'"'_trip_preexec'"'"' DEBUG
  fi
fi'

TMP=""
STAGED=""
cleanup() {
  [ -z "$TMP" ] || rm -rf "$TMP"
  [ -z "$STAGED" ] || rm -rf "$STAGED"
}
trap cleanup EXIT

die() {
  printf 'install.sh: %s\n' "$1" >&2
  exit 1
}

usage() {
  printf 'usage: install.sh [--dev]\n' >&2
  exit 2
}

require_cargo() {
  command -v cargo >/dev/null 2>&1 && return 0
  die "trip builds from source and needs cargo, which is not on PATH.
Install Rust with rustup (https://rustup.rs), then run the installer again:
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
}

# The [package] version in a Cargo.toml, to name a build that never produced a
# binary to ask.
package_version() {
  sed -n '/^\[package\]/,/^\[/s/^version *= *"\([^"]*\)".*/\1/p' "$1" | head -n 1
}

# The version a link in ~/.local/share/trip names, or nothing when there is no
# link. The installer writes a bare version, but a link switched by hand, as
# `ln -sfn ~/.local/share/trip/0.2.0 current` makes, holds an absolute path.
link_version() {
  local target
  [ -L "$SHARE_DIR/$1" ] || return 0
  target="$(readlink "$SHARE_DIR/$1")"
  target="${target%/}"
  printf '%s\n' "${target##*/}"
}

# The version the current install runs, or nothing when there is none.
current_version() {
  link_version current
}

# Point a symlink at a target with one rename, so it never goes missing and a
# process running the old target keeps it. BSD mv spells "do not follow a link
# to a directory" -h and GNU mv spells it -T.
replace_link() {
  local target="$1" link="$2" new="$2.new.$$"
  rm -f "$new"
  ln -s "$target" "$new"
  mv -T -f "$new" "$link" 2>/dev/null || mv -h -f "$new" "$link"
}

# Say which version did not install and what still runs, then stop.
fail_install() {
  local label="$1" reason="$2" current
  current="$(current_version)"
  if [ -n "$current" ]; then
    die "trip $label did not install: $reason. trip $current is still current."
  fi
  die "trip $label did not install: $reason. Nothing was installed."
}

# The version a running daemon reports in ~/.trip/daemon.json. A record whose
# pid is not running is left over from a daemon that exited and vouches for
# nothing.
daemon_version() {
  [ -f "$DAEMON_RECORD" ] || return 0
  local pid version
  pid="$(sed -n 's/.*"pid"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$DAEMON_RECORD" | head -n 1)"
  version="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$DAEMON_RECORD" | head -n 1)"
  [ -n "$pid" ] && [ -n "$version" ] || return 0
  kill -0 "$pid" 2>/dev/null || return 0
  printf '%s\n' "$version"
}

# Move a built binary into its version's directory, switch current to it, and
# keep only the current version, the one before it, and a running daemon's.
install_version() {
  local built="$1" label="$2" output version previous daemon name dir
  output="$("$built" --version 2>/dev/null)" || fail_install "$label" "the built binary does not run"
  version="${output##* }"
  case "$version" in
    "" | *[!0-9A-Za-z.+-]* | [!0-9]*) fail_install "$label" "the built binary reports an unexpected version: $output" ;;
  esac

  mkdir -p "$SHARE_DIR"
  if [ ! -d "$SHARE_DIR/$version" ]; then
    STAGED="$SHARE_DIR/.$version.new.$$"
    mkdir "$STAGED"
    cp "$built" "$STAGED/trip"
    chmod 755 "$STAGED/trip"
    mv "$STAGED" "$SHARE_DIR/$version"
    STAGED=""
  else
    # Reinstalling a version, as a checkout does after each change. A rename
    # leaves a daemon that runs the old file on it.
    cp "$built" "$SHARE_DIR/$version/.trip.new.$$"
    chmod 755 "$SHARE_DIR/$version/.trip.new.$$"
    mv -f "$SHARE_DIR/$version/.trip.new.$$" "$SHARE_DIR/$version/trip"
  fi

  previous="$(current_version)"
  if [ -n "$previous" ] && [ "$previous" != "$version" ]; then
    replace_link "$previous" "$SHARE_DIR/previous"
  fi
  replace_link "$version" "$SHARE_DIR/current"
  mkdir -p "$BIN_DIR"
  replace_link "$SHARE_DIR/current/trip" "$BIN_DIR/trip"
  printf 'Installed trip %s to %s\n' "$version" "$SHARE_DIR/$version"

  previous="$(link_version previous)"
  daemon="$(daemon_version)"
  for dir in "$SHARE_DIR"/[0-9]*/; do
    dir="${dir%/}"
    [ -d "$dir" ] && [ ! -L "$dir" ] || continue
    name="${dir##*/}"
    if [ "$name" = "$version" ] || [ "$name" = "$previous" ]; then
      continue
    fi
    if [ "$name" = "$daemon" ]; then
      printf 'Kept trip %s, which the running daemon uses\n' "$name"
      continue
    fi
    rm -rf "$dir"
    printf 'Removed trip %s\n' "$name"
  done
}

install_release() {
  local url="${TRIP_ARCHIVE:-https://github.com/$TRIP_REPO/archive/$TRIP_REF.tar.gz}" label version
  case "$TRIP_REPO" in
    */*/* | *[!A-Za-z0-9._/-]* | /* | */) die "TRIP_REPO must be owner/name, not $TRIP_REPO" ;;
    */*) ;;
    *) die "TRIP_REPO must be owner/name, not $TRIP_REPO" ;;
  esac
  case "$TRIP_REF" in
    "" | *[!A-Za-z0-9._/-]*) die "TRIP_REF is not a tag or branch name: $TRIP_REF" ;;
  esac
  command -v curl >/dev/null 2>&1 || die "the release installer needs curl to download trip's source."
  require_cargo

  TMP="$(mktemp -d "${TMPDIR:-/tmp}/trip-install.XXXXXX")"
  printf 'Downloading %s\n' "$url"
  curl -fsSL "$url" -o "$TMP/source.tar.gz" || fail_install "$TRIP_REF" "could not download $url"
  mkdir "$TMP/src"
  tar -xzf "$TMP/source.tar.gz" -C "$TMP/src" --strip-components 1 \
    || fail_install "$TRIP_REF" "$url is not a source archive"
  # tar extracts any repository's archive, so a TRIP_REPO naming another
  # project gets this far.
  grep -qs '^name = "trip"' "$TMP/src/Cargo.toml" || fail_install "$TRIP_REF" "$url is not trip's source"
  version="$(package_version "$TMP/src/Cargo.toml")"
  label="${version:-$TRIP_REF} ($TRIP_REPO $TRIP_REF)"

  printf 'Building trip %s, which takes a few minutes\n' "$label"
  (cd "$TMP/src" && TRIP_RELEASE=1 CARGO_TARGET_DIR="$TMP/target" cargo build --release --locked) \
    || fail_install "$label" "the build failed"
  install_version "$TMP/target/release/trip" "$label"

  printf '{"repo": "%s", "ref": "%s"}\n' "$TRIP_REPO" "$TRIP_REF" > "$SHARE_DIR/.release.json.new"
  mv -f "$SHARE_DIR/.release.json.new" "$SHARE_DIR/release.json"
}

# The checkout's target directory, where `cargo build` there writes.
target_dir() {
  case "${CARGO_TARGET_DIR:-}" in
    "") printf '%s\n' "$REPO/target" ;;
    /*) printf '%s\n' "$CARGO_TARGET_DIR" ;;
    *) printf '%s\n' "$REPO/$CARGO_TARGET_DIR" ;;
  esac
}

install_checkout() {
  local label target
  require_cargo
  label="$(package_version "$REPO/Cargo.toml")-dev"
  target="$(target_dir)"
  printf 'Building trip %s from %s\n' "$label" "$REPO"
  # Only the release installer marks a build as a release.
  (cd "$REPO" && env -u TRIP_RELEASE cargo build --release --locked) \
    || fail_install "$label" "the build failed"
  install_version "$target/release/trip" "$label"
}

install_dev() {
  local target
  require_cargo
  target="$(target_dir)"
  printf 'Building trip (debug) in %s\n' "$REPO"
  (cd "$REPO" && env -u TRIP_RELEASE cargo build) || die "the debug build failed; $BIN_DIR/trip is unchanged."
  mkdir -p "$BIN_DIR"
  replace_link "$target/debug/trip" "$BIN_DIR/trip"
  printf 'Linked %s to %s\n' "$BIN_DIR/trip" "$target/debug/trip"
}

# Drop any trip hook block, from its marker through the `fi` that closes it.
# Fail and leave the file as it is when a block has no such `fi`, as when a
# user edited it to `fi  # trip`, rather than drop everything after it.
remove_hook() {
  local file="$1" stripped
  stripped="$(awk -v marker="$MARKER" '
    $0 == marker { skipping = 1; next }
    skipping && $0 == "fi" { skipping = 0; next }
    !skipping { print }
    END { if (skipping) exit 1 }
  ' "$file")" || return 1
  # Write through the file rather than replacing it, so a startup file that
  # is a symlink, as dotfile managers make, stays one.
  printf '%s\n' "$stripped" > "$file"
}

# Whether a startup file holds exactly one hook block, and it is this one.
hook_current() {
  [ -f "$1" ] && [ "$(grep -cxF "$MARKER" "$1")" = 1 ] || return 1
  [ "$(awk -v marker="$MARKER" '
    $0 == marker { on = 1 }
    on { print }
    on && $0 == "fi" { exit }
  ' "$1")" = "$HOOK" ]
}

install_hook() {
  local file="$1"
  if hook_current "$file"; then
    printf 'Shell hook already in %s\n' "$file"
  elif [ -f "$file" ] && grep -qxF "$MARKER" "$file"; then
    if ! remove_hook "$file"; then
      printf '\nwarning: %s has a trip shell hook with no closing fi, so it is unchanged.\nDelete the block that starts with "%s" and run the installer again.\n' \
        "$file" "$MARKER" >&2
      return 0
    fi
    printf '\n%s\n' "$HOOK" >> "$file"
    printf 'Updated shell hook in %s\n' "$file"
  else
    printf '\n%s\n' "$HOOK" >> "$file"
    printf 'Added shell hook to %s\n' "$file"
  fi
}

warn_path() {
  case ":$PATH:" in
    *":$BIN_DIR:"* | *":$BIN_DIR/:"*) return ;;
  esac
  # shellcheck disable=SC2016 # the line to paste keeps $HOME and $PATH.
  printf '\nwarning: %s is not on PATH. Add this line to your shell startup file:\n  export PATH="$HOME/.local/bin:$PATH"\n' "$BIN_DIR"
}

warn_legacy() {
  [ -e "$LEGACY_BIN" ] || [ -L "$LEGACY_BIN" ] || return 0
  printf '\nwarning: %s is left from an earlier install, and a terminal that runs it\nruns that old trip. Remove it with:\n  sudo rm %s\n' "$LEGACY_BIN" "$LEGACY_BIN"
}

main() {
  local dev=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --dev) dev=1 ;;
      -h | --help) printf 'usage: install.sh [--dev]\n'; exit 0 ;;
      *) usage ;;
    esac
    shift
  done

  if [ -n "$dev" ]; then
    [ -n "$REPO" ] || die "--dev links a checkout's debug build; run ./install.sh --dev in a clone of trip."
    install_dev
  elif [ -n "$REPO" ]; then
    install_checkout
  else
    install_release
  fi

  install_hook "$HOME/.zshrc"
  install_hook "$HOME/.bashrc"
  warn_path
  warn_legacy
  printf "\nDone. Run 'trip enter' to get started.\n"
}

# Everything above only defines functions, so bash reading this from curl has
# the whole script before any of it runs.
main "$@"
