#!/usr/bin/env bash
set -euo pipefail

# Start agygram daemon (or forward explicit CLI subcommands/arguments)
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

BIN="${AGYGRAM_BIN:-}"
if [ -z "$BIN" ]; then
    if [ -x "./target/release/agygram" ]; then
        BIN="./target/release/agygram"
    elif [ -x "./agygram" ]; then
        BIN="./agygram"
    elif [ -x "$HOME/agygram" ]; then
        BIN="$HOME/agygram"
    else
        echo "Error: executable not found at ./target/release/agygram, ./agygram, or $HOME/agygram. Run 'cargo build --release' or './update.sh' first." >&2
        exit 1
    fi
fi

if [ "$#" -eq 0 ]; then
    exec "$BIN" daemon
else
    exec "$BIN" "$@"
fi
