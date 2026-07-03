#!/usr/bin/env bash
# Run the full "Definition of Done" verification cycle from CLAUDE.md:
#
#   1. cargo fmt --check          — format clean
#   2. cargo clippy               — no errors
#   3. cargo test                 — unit + integration tests
#   4. web lint                   — eslint clean
#   5. web format:check           — prettier clean
#   6. cargo build --release      — binary the e2e suite boots
#   7. web e2e                    — Playwright suite
#
# Every step runs even if an earlier one fails, so one invocation reports
# the whole picture; the exit code is non-zero if ANY step failed. Pass
# --fast to skip the release build + Playwright suite (steps 6-7).
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

FAST=0
[[ "${1:-}" == "--fast" ]] && FAST=1

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
  cd "$ROOT"
  # The Playwright webServer boots target/release/peckboard, so the
  # release binary must be rebuilt or the suite tests stale code.
  run_step "cargo build --release" cargo build --release
  cd "$ROOT/web"
  run_step "web e2e" npm run e2e
else
  echo ""
  echo "(--fast: skipping release build + Playwright e2e)"
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
