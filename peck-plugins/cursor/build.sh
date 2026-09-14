#!/usr/bin/env bash
# Build the Peckboard cursor provider plugin to a WASM module.
#
# Output: target/wasm32-unknown-unknown/release/peckboard_cursor_plugin.wasm
# Drop that file into <dataDir>/plugins/ (rename to cursor.wasm — the
# plugin's config key is its file stem) and (re)start Peckboard, or install it
# via the plugin registry.
set -euo pipefail

cd "$(dirname "$0")"

# The plugin targets wasm32-unknown-unknown. Ensure the target is installed.
rustup target add wasm32-unknown-unknown >/dev/null 2>&1 || true

cargo build --target wasm32-unknown-unknown --release

WASM="target/wasm32-unknown-unknown/release/peckboard_cursor_plugin.wasm"
echo "Built: $WASM"
ls -lh "$WASM"
