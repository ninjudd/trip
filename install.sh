#!/bin/sh
set -e

if [ "$1" = "--dev" ]; then
    echo "Building drip (debug)..."
    cargo build
    echo "Linking to /usr/local/bin/drip..."
    sudo ln -sf "$(pwd)/target/debug/drip" /usr/local/bin/drip
else
    echo "Building drip..."
    cargo build --release
    echo "Installing to /usr/local/bin/drip..."
    sudo cp target/release/drip /usr/local/bin/drip
fi

HOOK='# drip shell hook
if [ -n "$DRIP_SESSION" ]; then
  _drip_preexec() { eval "$(drip init)"; }
  if [ -n "$ZSH_VERSION" ]; then
    preexec_functions+=(_drip_preexec)
  elif [ -n "$BASH_VERSION" ]; then
    trap '"'"'eval "$(drip init)"'"'"' DEBUG
  fi
fi'

MARKER="# drip shell hook"

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
        sed -i '' "/$MARKER/,/^$/d" "$file"
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

echo "Done. Run 'drip enter' to get started."
