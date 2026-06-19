#!/usr/bin/env bash
# Run the compiled Peckboard binary.
# Assumes you have already run scripts/build.sh.
# All arguments are forwarded (e.g. --port 8080 --host 127.0.0.1).
set -e

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BINARY="$ROOT/target/release/peckboard"

if [ ! -f "$BINARY" ]; then
  echo "Error: $BINARY not found. Run scripts/build.sh first."
  exit 1
fi

exec "$BINARY" "$@"
