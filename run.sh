#!/bin/bash
# Start agygram daemon
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
cd $(dirname $0)
exec ./target/release/agygram daemon
