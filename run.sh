#!/usr/bin/env bash
# Build (release) and start Opportunity with the settings in .env. Open http://localhost:8080
set -euo pipefail
cd "$(dirname "$0")"
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
cargo build --release -p server
exec ./target/release/opportunity
