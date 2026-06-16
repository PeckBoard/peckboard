#!/usr/bin/env bash
# Persistent local DEV INSTANCE of Peckboard.
#
# Unlike scratch/ad-hoc runs (fresh tmp dir + random port), this is a
# named, persistent instance on FIXED ports with a PERSISTENT data dir, so
# its users/sessions/cards survive restarts. It is NOT your real install
# (default 3344/3345) — it has its own data dir.
#
#   HTTP   : 3399
#   HTTPS  : 4499
#   data   : <repo>/../.peckboard-dev   (persists; outside the git repo)
#   registry: serves the sibling PeckBoard/plugins checkout over a tiny
#             local static server so "Available Plugins" works offline.
#
# Usage: scripts/dev-instance.sh [start|stop|status|restart]   (default: start)
#
# Runs in the background (nohup), so it survives this shell closing. It
# does NOT survive a reboot — re-run `start` after boot (or wire a systemd
# user unit if you want that).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BINARY="$ROOT/target/release/peckboard"
DATA_DIR="$(cd "$ROOT/.." && pwd)/.peckboard-dev"
PLUGINS_REPO="$(cd "$ROOT/.." && pwd)/plugins"

HTTP_PORT=3399
HTTPS_PORT=4499
REGISTRY_PORT=3398
REGISTRY_URL="http://127.0.0.1:${REGISTRY_PORT}/registry.json"

APP_PID="$DATA_DIR/dev-instance.pid"
APP_LOG="$DATA_DIR/dev-instance.log"
REG_PID="$DATA_DIR/registry-server.pid"
REG_LOG="$DATA_DIR/registry-server.log"

# Kill a process whose PID is in $1, only if it is actually running. We
# only ever kill PIDs we recorded ourselves — never a broad `pkill
# peckboard`, which would take down the user's real instance.
kill_pidfile() {
  local f="$1"
  if [ -f "$f" ]; then
    local pid
    pid="$(cat "$f" 2>/dev/null || true)"
    if [ -n "${pid:-}" ] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      sleep 0.5
      kill -9 "$pid" 2>/dev/null || true
    fi
    rm -f "$f"
  fi
}

stop() {
  echo "Stopping dev instance..."
  kill_pidfile "$APP_PID"
  kill_pidfile "$REG_PID"
  echo "Stopped."
}

status() {
  local running=0
  for f in "$APP_PID:peckboard" "$REG_PID:registry-server"; do
    local pidfile="${f%%:*}" name="${f##*:}"
    if [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
      echo "  $name: running (pid $(cat "$pidfile"))"
      running=1
    else
      echo "  $name: not running"
    fi
  done
  [ "$running" = 1 ] && echo "  http://localhost:${HTTP_PORT}  /  https://localhost:${HTTPS_PORT}"
}

start() {
  if [ ! -x "$BINARY" ]; then
    echo "Error: $BINARY not found. Build it first: cargo build --release" >&2
    exit 1
  fi
  # Idempotent: clear any prior dev processes we started.
  kill_pidfile "$APP_PID"
  kill_pidfile "$REG_PID"
  mkdir -p "$DATA_DIR"

  # Local registry static server (optional — only if the plugins checkout
  # is present). Peckboard fetches the index server-side, so no CORS needed.
  if [ -f "$PLUGINS_REPO/registry.json" ]; then
    nohup python3 -m http.server "$REGISTRY_PORT" --bind 127.0.0.1 \
      --directory "$PLUGINS_REPO" >"$REG_LOG" 2>&1 &
    echo $! >"$REG_PID"
    echo "Registry server: $REGISTRY_URL (serving $PLUGINS_REPO)"
  else
    echo "Note: $PLUGINS_REPO/registry.json not found — registry will use the default URL."
  fi

  nohup env PECKBOARD_PLUGIN_REGISTRY_URL="$REGISTRY_URL" \
    "$BINARY" --data-dir "$DATA_DIR" --port "$HTTP_PORT" --https-port "$HTTPS_PORT" \
    >"$APP_LOG" 2>&1 &
  echo $! >"$APP_PID"

  # Wait for it to answer.
  for _ in $(seq 1 30); do
    if curl -fsS "http://127.0.0.1:${HTTP_PORT}/api/health" >/dev/null 2>&1 ||
      curl -fsS "http://127.0.0.1:${HTTP_PORT}/" >/dev/null 2>&1; then
      break
    fi
    sleep 0.3
  done

  echo "Dev instance up:"
  echo "  HTTP  : http://localhost:${HTTP_PORT}"
  echo "  HTTPS : https://localhost:${HTTPS_PORT}"
  echo "  data  : $DATA_DIR"
  echo "  logs  : $APP_LOG"
  # On a fresh data dir, first-run bootstrap prints the generated admin
  # password once — surface it.
  if grep -qiE "admin|password" "$APP_LOG" 2>/dev/null; then
    echo "  --- bootstrap (first run only) ---"
    grep -iE "admin|password|bootstrap" "$APP_LOG" | head -4 | sed 's/^/  /'
  fi
}

case "${1:-start}" in
  start) start ;;
  stop) stop ;;
  restart)
    stop
    start
    ;;
  status) status ;;
  *)
    echo "Usage: $0 [start|stop|status|restart]" >&2
    exit 1
    ;;
esac
