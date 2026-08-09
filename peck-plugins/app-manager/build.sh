#!/usr/bin/env bash
# Build the Peckboard app-manager plugin to a WASM module via the
# Extism js-pdk. esbuild bundles src/index.ts -> dist/index.js, then
# extism-js compiles it to dist/plugin.wasm.
#
# Output: dist/plugin.wasm
#
# Requires `extism-js` on PATH (e.g. ~/bin) and Node/npm.
set -euo pipefail
cd "$(dirname "$0")"

# Install deps on first run (or when node_modules is missing).
if [ ! -d node_modules ]; then
  echo "Installing npm dependencies..."
  npm install
fi

npm run build

WASM="dist/plugin.wasm"
echo "Built: $WASM"
ls -lh "$WASM"
