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

echo "Done. Run 'drip enter' to get started."
