#!/usr/bin/env bash
# Builds readme and installs it as ~/.local/bin/readme.
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release
# Replaced rather than overwritten, so a readme that is running keeps working
install -Dm755 target/release/readme "$HOME/.local/bin/readme.new"
mv -f "$HOME/.local/bin/readme.new" "$HOME/.local/bin/readme"
echo "Installed $HOME/.local/bin/readme"
