#!/usr/bin/env bash
# Build the web bundle + the local release-grade binary the e2e suite boots
# (target/verify/peckboard), doing as little work as possible. Shared by
# verify.sh, e2e-shards.sh, e2e-impact-map.sh and web/e2e/playwright.config.ts
# so every local path builds the same way.
#
#   scripts/build-local-release.sh
#
# Local-only accelerators — CI and shipped binaries still come from a plain
# `cargo build --release`, which this never touches:
#
#   1. The web bundle is rebuilt only when an input is newer than
#      web/dist/index.html. vite empties web/dist on every build, which
#      bumps every mtime, and build.rs watches web/dist (rerun-if-changed),
#      so an unconditional `npm run build` forced a recompile of the
#      peckboard crate even when nothing in the UI changed. A current dist
#      also means `tsc -b` already passed on this exact tree.
#      PECKBOARD_FORCE_WEB_BUILD=1 rebuilds anyway.
#
#   2. The binary is built with `--profile verify` (Cargo.toml): release
#      opt-level, but incremental, in its own target/verify dir.
#      ~/.cargo/config.toml on the dev box sets `build.incremental = false`,
#      which overrides every profile, so CARGO_INCREMENTAL=1 is set here —
#      without it each rebuild re-optimises the whole peckboard crate from
#      scratch (~2m20s). The separate dir matters on a box where several
#      agents share target/: incremental and non-incremental builds of the
#      workspace crates have different fingerprints, so mixing them in
#      target/release made each side recompile the other's output.
#      Incremental also raises rustc's default codegen-units 16 → 256 for
#      the workspace crates, so the local binary is optimised slightly
#      differently from CI's (same opt-level, no LTO either way).
#
# Sourcemap builds (PECKBOARD_E2E_COVERAGE=1, see e2e-impact-map.sh) always
# rebuild the web bundle, and a dist that still holds sourcemaps is treated
# as stale, so an instrumented bundle can't leak into a normal run.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"
WEB="$ROOT/web"
STAMP="$WEB/dist/index.html"

web_stale() {
  [[ "${PECKBOARD_FORCE_WEB_BUILD:-}" == "1" ]] && return 0
  [[ "${PECKBOARD_E2E_COVERAGE:-}" == "1" ]] && return 0
  [[ -f "$STAMP" ]] || return 0
  # Leftover instrumented bundle from an impact-map run.
  [[ -n "$(find "$WEB/dist" -name '*.map' -print -quit)" ]] && return 0
  local newer
  newer="$(find "$WEB/src" "$WEB/public" "$WEB/index.html" \
    "$WEB/package.json" "$WEB/package-lock.json" "$WEB/vite.config.ts" \
    "$WEB"/tsconfig*.json -newer "$STAMP" -print -quit 2>/dev/null)"
  [[ -n "$newer" ]]
}

if web_stale; then
  echo "▶ web build (inputs changed)"
  (cd "$WEB" && npm run build) || exit 1
else
  echo "▶ web build skipped — web/dist is newer than every input"
fi
echo "▶ cargo build --profile verify (incremental)"
cd "$ROOT" || exit 1
CARGO_INCREMENTAL=1 cargo build --profile verify || exit 1

# The pairing-v2 enrollment spec (web/e2e/tests/remote-access-enroll.spec.ts)
# drives a real local relay and a real peckboard-connect against the box.
# Both are standalone crates (own Cargo.lock), built into one shared
# target dir of their own so they never trade fingerprints with the
# peckboard crate's. Skipped with PECKBOARD_E2E_SKIP_TOOLS=1.
if [[ "${PECKBOARD_E2E_SKIP_TOOLS:-}" != "1" ]]; then
  TOOLS="$ROOT/target/verify-tools"
  echo "▶ cargo build --release peckboard-relay + peckboard-connect → $TOOLS"
  cargo build --release --manifest-path "$ROOT/peckboard-relay/Cargo.toml" \
    --features server --bin peckboard-relay --target-dir "$TOOLS" || exit 1
  cargo build --release --manifest-path "$ROOT/peckboard-connect/Cargo.toml" \
    --target-dir "$TOOLS" || exit 1
fi
