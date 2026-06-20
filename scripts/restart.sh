#!/usr/bin/env bash
# Restart Peckboard with the latest build.
#
# 1. Builds the frontend + release binary (embeds web/dist via rust-embed).
# 2. Stops ONLY the Peckboard instance bound to $PORT — never blanket-kills
#    `peckboard` processes, so scratch runs on other ports are left alone
#    (see AGENTS.md "Never blanket-kill peckboard processes").
# 3. Installs the fresh binary to $BINARY and relaunches it detached against
#    the same data dir and ports, logging to $LOG_FILE.
#
# NOTE: this restarts your real/live instance (default ports 3344/3345,
# data dir ~/.peckboard) — including the server that may be hosting your
# current session.
#
# Usage:
#   scripts/restart.sh                      # build + restart on 3344/3345
#   SKIP_BUILD=1 scripts/restart.sh         # restart current build, no rebuild
#   PORT=3344 HTTPS_PORT=3345 DATA_DIR=~/.peckboard scripts/restart.sh
#   BINARY=~/peckboard-linux-x86_64 scripts/restart.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

PORT="${PORT:-3344}"
HTTPS_PORT="${HTTPS_PORT:-3345}"
DATA_DIR="${DATA_DIR:-$HOME/.peckboard}"
# Where to install and run the built binary from. Defaults to the location
# the instance already runs from so the normal `./peckboard-linux-x86_64`
# launch stays in sync with the latest build.
BINARY="${BINARY:-$HOME/peckboard-linux-x86_64}"
LOG_FILE="${LOG_FILE:-$DATA_DIR/peckboard.log}"

# cargo lives in ~/.cargo/bin, which isn't always on a non-interactive PATH.
if ! command -v cargo >/dev/null 2>&1 && [ -f "$HOME/.cargo/env" ]; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi

# ── 1. Build the latest release ──────────────────────────────────────
if [ -z "${SKIP_BUILD:-}" ]; then
  echo "==> Building frontend..."
  (cd "$ROOT/web" && npm run build)
  echo "==> Building backend (release)..."
  (cd "$ROOT" && cargo build --release)
else
  echo "==> SKIP_BUILD set — using existing target/release/peckboard"
fi

if [ ! -x "$ROOT/target/release/peckboard" ]; then
  echo "Error: $ROOT/target/release/peckboard not found — build failed?" >&2
  exit 1
fi

# ── 2. Stop only the instance bound to our ports ─────────────────────
pids_on_port() {
  # PIDs of processes LISTENing on the given source port. Empty if none.
  ss -ltnpH "sport = :$1" 2>/dev/null | grep -oP 'pid=\K[0-9]+' | sort -u || true
}

stop_on_port() {
  local p="$1" pid
  for pid in $(pids_on_port "$p"); do
    echo "==> Stopping peckboard (pid $pid) on port $p"
    kill "$pid" 2>/dev/null || true
  done
}

stop_on_port "$PORT"
stop_on_port "$HTTPS_PORT"

# Wait for the HTTP port to free (graceful), then escalate to SIGKILL.
for i in $(seq 1 20); do
  [ -z "$(pids_on_port "$PORT")" ] && break
  sleep 0.5
  if [ "$i" -eq 20 ]; then
    echo "==> Port $PORT still busy after 10s — sending SIGKILL"
    for pid in $(pids_on_port "$PORT"); do kill -9 "$pid" 2>/dev/null || true; done
    sleep 1
  fi
done

# ── 3. Install the fresh binary and relaunch detached ────────────────
mkdir -p "$DATA_DIR"
install -m 0755 "$ROOT/target/release/peckboard" "$BINARY"

echo "==> Starting latest build"
echo "    $BINARY --port $PORT --https-port $HTTPS_PORT --data-dir $DATA_DIR"
setsid "$BINARY" \
  --port "$PORT" \
  --https-port "$HTTPS_PORT" \
  --data-dir "$DATA_DIR" \
  >>"$LOG_FILE" 2>&1 </dev/null &
NEW_PID=$!

# Give it a moment to bind, then confirm it actually came up.
sleep 1.5
if [ -n "$(pids_on_port "$PORT")" ]; then
  echo "==> Peckboard is up (pid $NEW_PID)"
  echo "    http://localhost:$PORT   https://localhost:$HTTPS_PORT"
  echo "    logs: $LOG_FILE"
else
  echo "Error: peckboard did not bind to port $PORT — check $LOG_FILE" >&2
  exit 1
fi
