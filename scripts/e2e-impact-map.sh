#!/usr/bin/env bash
# Regenerate web/e2e/impact-map.json — "which spec files exercise this
# source file" — from one instrumented full run of the e2e suite.
#
#   scripts/e2e-impact-map.sh [shard-count]
#
# Run this after adding or substantially reworking specs, and commit the
# result. `scripts/e2e-impacted.sh` reads it to pick a subset for the
# inner loop; a stale map only ever means the inner loop runs the wrong
# subset, never that a merge skips tests — the full suite is what gates
# `scripts/verify.sh`.
#
# The run is instrumented three ways (see scripts/e2e-impact-map.mjs):
#   - the bundle is built WITH sourcemaps so V8 coverage traces back to
#     web/src/** files,
#   - web/e2e/harness.ts records that coverage per spec,
#   - the server logs every matched route, joined to tests by wall clock.
#
# It is slower than a normal run (coverage + sourcemap decoding), which is
# exactly why it is a separate, occasional command.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
SHARDS="${1:-4}"

IMPACT_DIR="$(mktemp -d)"
echo "▶ impact capture dir: $IMPACT_DIR"

echo "▶ building frontend WITH sourcemaps + release binary"
(cd "$ROOT/web" && PECKBOARD_E2E_COVERAGE=1 npm run build) || exit 1
touch "$ROOT/src/frontend.rs"
(cd "$ROOT" && cargo build --release) || exit 1

# The shard script would rebuild without the coverage flag and throw the
# sourcemaps away, so tell it the build is already done.
echo "▶ running the full suite instrumented ($SHARDS shards)"
PECKBOARD_E2E_SKIP_BUILD=1 \
  PECKBOARD_E2E_IMPACT_DIR="$IMPACT_DIR" \
  "$ROOT/scripts/e2e-shards.sh" "$SHARDS"
run_status=$?
if [[ "$run_status" -ne 0 ]]; then
  # A failing test still executed code, so the capture is still usable —
  # but say so, because a spec that crashed early under-reports what it
  # touches.
  echo "⚠ some specs failed; the map they contribute may under-report"
fi

echo "▶ building the map"
node "$ROOT/scripts/e2e-impact-map.mjs" "$IMPACT_DIR" || exit 1
# Prettier-format the generated file: the pre-commit hook and web
# format:check both run `prettier --check` over committed JSON, and
# JSON.stringify's layout is not prettier's.
(cd "$ROOT/web" && npx prettier --write e2e/impact-map.json) >/dev/null || exit 1

# Leave web/dist as a normal (sourcemap-free) build so nobody accidentally
# embeds sourcemaps into a release binary. rust-embed keys on Rust source,
# not on web/dist, so the touch is what makes the NEXT release build pick
# the clean bundle up instead of re-embedding the instrumented one.
echo "▶ rebuilding frontend without sourcemaps"
(cd "$ROOT/web" && npm run build) >/dev/null || exit 1
touch "$ROOT/src/frontend.rs"

# The capture is kept: rebuilding the map after tweaking the generator is
# then `node scripts/e2e-impact-map.mjs <dir>`, not another 8-minute run.
echo "▶ done — review and commit web/e2e/impact-map.json"
echo "  capture kept at $IMPACT_DIR (delete when you're done with it)"
