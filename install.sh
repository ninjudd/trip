#!/bin/sh
set -e

if [ "$1" = "--dev" ]; then
    echo "Building trip (debug)..."
    cargo build
    echo "Linking to /usr/local/bin/trip..."
    sudo ln -sf "$(pwd)/target/debug/trip" /usr/local/bin/trip
else
    echo "Building trip..."
    cargo build --release
    echo "Installing to /usr/local/bin/trip..."
    # Copying onto the destination fails with ETXTBSY while a daemon or client
    # is executing it, and follows a --dev symlink back into target/ if one is
    # there. Write alongside and rename: rename is atomic, replaces a symlink
    # with a real file, and leaves running processes on the old inode.
    sudo cp target/release/trip /usr/local/bin/.trip.new
    sudo chmod 755 /usr/local/bin/.trip.new
    sudo mv -f /usr/local/bin/.trip.new /usr/local/bin/trip
fi

HOOK='# trip shell hook
if [ -n "$TRIP_SESSION" ]; then
  _trip_preexec() { eval "$(trip env)"; }
  if [ -n "$ZSH_VERSION" ]; then
    preexec_functions+=(_trip_preexec)
  elif [ -n "$BASH_VERSION" ]; then
    trap '"'"'_trip_preexec'"'"' DEBUG
  fi
fi'

MARKER="# trip shell hook"

resolve_path() {
    # Follow symlinks portably (macOS lacks readlink -f)
    target="$1"
    while [ -L "$target" ]; do
        target="$(readlink "$target")"
    done
    echo "$target"
}

remove_old_hook() {
    file="$(resolve_path "$1")"
    [ -f "$file" ] || return 0
    if grep -qF "$MARKER" "$file"; then
        # -i.suffix (attached) is the one in-place form both GNU and BSD sed accept
        sed -i.trip-bak "/$MARKER/,/^$/d" "$file"
        rm -f "$file.trip-bak"
    fi
}

install_hook() {
    file="$1"
    remove_old_hook "$file"
    printf '\n%s\n' "$HOOK" >> "$file"
    echo "Added shell hook to $file"
}

install_hook "$HOME/.zshrc"
install_hook "$HOME/.bashrc"

echo "Done. Run 'trip enter' to get started."
