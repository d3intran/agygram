#!/usr/bin/env bash
set -euo pipefail

# Start agygram daemon (or forward explicit CLI subcommands/arguments)
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

BIN="./target/release/agygram"
if [ ! -x "$BIN" ]; then
    echo "Error: executable not found at $SCRIPT_DIR/target/release/agygram. Run 'cargo build --release' first." >&2
    exit 1
fi

if [ "$#" -eq 0 ]; then
    exec "$BIN" daemon
else
    exec "$BIN" "$@"
fi
