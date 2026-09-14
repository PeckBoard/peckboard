#!/usr/bin/env bash
# Run the full "Definition of Done" verification cycle from CLAUDE.md:
#
#   1. cargo fmt --check          — format clean
#   2. cargo clippy               — no errors
#   3. cargo test                 — unit + integration tests
#   4. web lint                   — eslint clean
#   5. web format:check           — prettier clean
#   6. web build                  — the bundle rust-embed compiles in
#   7. cargo build --release      — binary the e2e suite boots
#   8. web e2e                    — Playwright suite, sharded
#
# Every step runs even if an earlier one fails, so one invocation reports
# the whole picture; the exit code is non-zero if ANY step failed.
#
#   --fast       skip the web/release builds + Playwright suite (steps 6-8)
#   --impacted   run only the specs the current change can affect (step 8),
#                falling back to the whole suite whenever that cannot be
#                proven safe. INNER LOOP ONLY — the full suite is the gate.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

FAST=0
IMPACTED=0
case "${1:-}" in
--fast) FAST=1 ;;
--impacted) IMPACTED=1 ;;
esac

declare -a NAMES=()
declare -a RESULTS=()

run_step() {
  local name="$1"
  shift
  echo ""
  echo "══════════════════════════════════════════════════"
  echo "▶ $name"
  echo "══════════════════════════════════════════════════"
  if "$@"; then
    RESULTS+=("ok")
  else
    RESULTS+=("FAILED")
  fi
  NAMES+=("$name")
}

cd "$ROOT"
run_step "cargo fmt --check" cargo fmt --check
run_step "cargo clippy" cargo clippy --all-targets --no-deps
run_step "cargo test" cargo test

cd "$ROOT/web"
run_step "web lint" npm run lint
run_step "web format:check" npm run format:check

if [[ "$FAST" -eq 0 ]]; then
  # rust-embed bakes web/dist into the release binary at compile time, so a
  # stale bundle means the e2e suite drives the PREVIOUS UI. Build the web
  # assets before the binary that embeds them.
  run_step "web build" npm run build
  cd "$ROOT"
  # The Playwright webServer boots target/release/peckboard, so the
  # release binary must be rebuilt or the suite tests stale code.
  run_step "cargo build --release" cargo build --release
  cd "$ROOT/web"
  # Both builds just ran; don't let the shard runner redo them.
  export PECKBOARD_E2E_SKIP_BUILD=1
  if [[ "$IMPACTED" -eq 1 ]]; then
    run_step "web e2e (impacted)" "$ROOT/scripts/e2e-impacted.sh"
  else
    # Sharded: one server + data dir per shard, still workers:1 inside each,
    # so every isolation assumption the specs make still holds.
    run_step "web e2e (4 shards)" "$ROOT/scripts/e2e-shards.sh" 4
  fi
else
  echo ""
  echo "(--fast: skipping web/release builds + Playwright e2e)"
fi

echo ""
echo "══════════════════════════════════════════════════"
echo "Summary"
echo "══════════════════════════════════════════════════"
failed=0
for i in "${!NAMES[@]}"; do
  printf '  %-22s %s\n' "${NAMES[$i]}" "${RESULTS[$i]}"
  [[ "${RESULTS[$i]}" == "FAILED" ]] && failed=1
done
exit "$failed"
