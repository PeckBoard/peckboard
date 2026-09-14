#!/usr/bin/env bash
# Run the Playwright e2e suite split across N parallel shards.
#
# The suite is ~430 tests at `workers: 1` (specs mutate shared server
# state, so they cannot share one server). Serialising them on an
# 8-core box costs ~14 minutes. This script keeps `workers: 1` — and
# therefore every isolation assumption the specs already make — but
# runs N Playwright processes side by side, each against its OWN
# server on its OWN port block with its OWN fresh data dir.
#
#   scripts/e2e-shards.sh            # 4 shards
#   scripts/e2e-shards.sh 6          # 6 shards
#   scripts/e2e-shards.sh 4 --grep "kanban"   # extra args go to playwright
#
# Playwright splits by spec FILE, deterministically, so a given spec
# runs in exactly one shard — specs that write fixed-path artifacts
# (screenshots under web/e2e/test-results/) cannot collide.
#
# Builds the frontend and the release binary ONCE up front, then sets
# PECKBOARD_E2E_SKIP_BUILD=1 for the shards. Without that, N shards
# each fire `cargo build --release` at the same time and contend on
# the same target dir.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

SHARDS="${1:-4}"
if [[ ! "$SHARDS" =~ ^[0-9]+$ ]] || [[ "$SHARDS" -lt 1 ]]; then
  echo "usage: $0 [shard-count] [extra playwright args...]" >&2
  exit 2
fi
shift || true

# Each shard needs three free ports (http, https, github stub). Step by
# 10 from a base well clear of the 3344/3345 default install and the
# 4444-4447 single-run / screenshots configs.
PORT_BASE="${PECKBOARD_E2E_PORT_BASE:-4500}"

# ── build once ────────────────────────────────────────────────────────
if [[ "${PECKBOARD_E2E_SKIP_BUILD:-}" != "1" ]]; then
  echo "▶ building frontend + release binary once for all $SHARDS shards"
  (cd "$ROOT/web" && npm run build) || exit 1
  # rust-embed keys on Rust source, not on web/dist — without this the
  # binary re-serves the PREVIOUS bundle. Same reason global-setup.ts
  # does it.
  touch "$ROOT/src/frontend.rs"
  (cd "$ROOT" && cargo build --release) || exit 1
fi
export PECKBOARD_E2E_SKIP_BUILD=1

LOG_DIR="$(mktemp -d)"
declare -a PIDS=()
declare -a LOGS=()

# Kill only the shards WE started. Never pkill/killall peckboard — the
# user's own instance is very likely running alongside this.
cleanup() {
  for pid in "${PIDS[@]:-}"; do
    [[ -n "$pid" ]] && kill "$pid" 2>/dev/null
  done
}
trap cleanup INT TERM

echo "▶ running $SHARDS shards (logs: $LOG_DIR)"
for ((i = 1; i <= SHARDS; i++)); do
  port=$((PORT_BASE + (i - 1) * 10))
  log="$LOG_DIR/shard-$i.log"
  LOGS+=("$log")
  (
    cd "$ROOT/web" || exit 1
    PECKBOARD_E2E_PORT="$port" \
      PECKBOARD_E2E_HTTPS_PORT="$((port + 1))" \
      PECKBOARD_E2E_GITHUB_PORT="$((port + 2))" \
      PECKBOARD_E2E_SHARD="$i" \
      PECKBOARD_E2E_DATA_DIR="$(mktemp -d)" \
      npx playwright test \
      --config=e2e/playwright.config.ts \
      --shard="$i/$SHARDS" \
      --output="e2e/test-results/shard-$i" \
      "$@"
  ) >"$log" 2>&1 &
  PIDS+=($!)
done

# Stream progress so a multi-minute run isn't a silent wait. Tail every
# shard log with a shard prefix; the tails die when the wait below
# returns and the trap fires.
tail -q -F --pid=$$ "${LOGS[@]}" 2>/dev/null | while IFS= read -r line; do
  printf '  %s\n' "$line"
done &
TAIL_PID=$!

failed=0
declare -a RESULTS=()
for ((i = 1; i <= SHARDS; i++)); do
  if wait "${PIDS[$((i - 1))]}"; then
    RESULTS+=("ok")
  else
    RESULTS+=("FAILED")
    failed=1
  fi
done

kill "$TAIL_PID" 2>/dev/null
wait "$TAIL_PID" 2>/dev/null

echo ""
echo "══════════════════════════════════════════════════"
echo "e2e shard summary"
echo "══════════════════════════════════════════════════"
for ((i = 1; i <= SHARDS; i++)); do
  # Playwright's closing counts line, e.g. "  429 passed (13.8m)".
  counts="$(grep -E '^\s+[0-9]+ (passed|failed|skipped|flaky)' "$LOG_DIR/shard-$i.log" | tr '\n' ' ' | tr -s ' ')"
  printf '  shard %d/%d  %-8s %s\n' "$i" "$SHARDS" "${RESULTS[$((i - 1))]}" "$counts"
done

if [[ "$failed" -eq 1 ]]; then
  # Name the failing tests here. The live tail above is killed the moment
  # the last shard exits, which is exactly when Playwright prints its
  # failure list — so without this replay the only record of WHAT failed
  # is a temp file nobody looks at.
  echo ""
  echo "failed tests"
  echo "──────────────────────────────────────────────────"
  for ((i = 1; i <= SHARDS; i++)); do
    [[ "${RESULTS[$((i - 1))]}" == "FAILED" ]] || continue
    # Playwright's per-failure header lines, e.g.
    #   "  3) e2e/tests/tabs.spec.ts:101:3 › tabs › …".
    grep -E '^\s+[0-9]+\) e2e/' "$LOG_DIR/shard-$i.log" |
      sed "s|^|  shard $i/$SHARDS |"
  done
  echo ""
  echo "Failing shard logs kept at: $LOG_DIR"
else
  rm -rf "$LOG_DIR"
fi
exit "$failed"
