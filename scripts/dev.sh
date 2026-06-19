#!/usr/bin/env bash
# Start Peckboard in development mode.
# - Rust backend with cargo run (port 3333)
# - Vite dev server with HMR (port 5173, proxies API to backend)
set -e

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

cleanup() {
  echo "Shutting down..."
  kill $BACKEND_PID $FRONTEND_PID 2>/dev/null
  wait $BACKEND_PID $FRONTEND_PID 2>/dev/null
}
trap cleanup EXIT INT TERM

echo "Starting backend (cargo run)..."
cargo run --manifest-path "$ROOT/Cargo.toml" "$@" &
BACKEND_PID=$!

echo "Starting frontend (vite dev)..."
cd "$ROOT/web"
npm run dev &
FRONTEND_PID=$!

echo ""
echo "Peckboard dev servers running:"
echo "  Frontend: http://localhost:5173"
echo "  Backend:  http://localhost:3333"
echo ""

wait
