#!/usr/bin/env bash
# Run only the e2e specs the current change can plausibly affect.
#
#   scripts/e2e-impacted.sh              # working tree vs HEAD
#   scripts/e2e-impacted.sh origin/main  # vs a base ref
#
# INNER LOOP ONLY. `scripts/verify.sh` always runs the whole suite — the
# impact map is an accelerator for iterating, never a gate for merging.
# It falls back to the full suite whenever it cannot prove a narrower set
# is safe (unknown file, shared primitive, no map on disk).
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# stderr (the rationale) passes through to the terminal; stdout is the list.
SELECTION="$(node "$ROOT/scripts/e2e-impacted.mjs" "${1:-}")" || exit 1

if [[ -z "$SELECTION" ]]; then
  echo "▶ nothing impacted — skipping e2e"
  exit 0
fi

if [[ "$SELECTION" == "ALL" ]]; then
  exec "$ROOT/scripts/e2e-shards.sh" 4
fi

# Playwright takes spec paths as positional filters, relative to testDir's
# parent (web/e2e) — which is exactly how the map stores them.
mapfile -t SPECS <<<"$SELECTION"
echo "▶ running ${#SPECS[@]} impacted spec files"
# Two shards is plenty for a narrowed set; more would spend longer booting
# servers than running tests. Never more shards than specs — Playwright
# treats an empty shard as "no tests", which is an error exit.
SHARDS=2
[[ ${#SPECS[@]} -lt "$SHARDS" ]] && SHARDS=${#SPECS[@]}
exec "$ROOT/scripts/e2e-shards.sh" "$SHARDS" "${SPECS[@]}"
