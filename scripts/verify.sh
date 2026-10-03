#!/usr/bin/env bash
# Run the "Definition of Done" verification cycle from AGENTS.md:
#
#   1. cargo fmt --check          — format clean
#   2. cargo clippy               — no errors
#   3. cargo test                 — unit + integration tests
#   4. web lint                   — eslint clean
#   5. web format:check           — prettier clean
#   6. local release build        — web bundle (tsc + vite) + the binary the
#                                   e2e suite boots (scripts/build-local-release.sh)
#   7. web e2e                    — Playwright suite, sharded
#
# Every step runs even if an earlier one fails, so one invocation reports
# the whole picture; the exit code is non-zero if ANY step failed.
#
#   (no flag)        the full suite — the gate for larger releases / nightly
#   --fast           skip the release build + Playwright suite (steps 6-7)
#   --impacted       all checks in full, e2e narrowed to the specs the change
#                    can reach (falls back to the whole suite when that can't
#                    be proven safe)
#   --changed [ref]  proportional: classify what changed vs <ref> (default
#                    origin/main, plus working tree + untracked build inputs)
#                    and run only the checks that change can break — see
#                    scripts/verify-changed.mjs and "Proportional Verify" in
#                    AGENTS.md. Schema / build / dependency changes escalate
#                    to the full suite automatically.
#
# Step 6 builds the web bundle only when its inputs changed and builds the
# release binary incrementally — a LOCAL build. The shipped binary is CI's
# plain `cargo build --release`; see scripts/build-local-release.sh.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="$HOME/.cargo/bin:$PATH"

# Memory guard: cap parallel rustc jobs so a full build can't OOM the box.
# Each rustc job can take ~1 GiB (the release build + rust-lld link spike
# higher), and this machine also runs the live Peckboard server. On
# 2026-09-21 an uncapped `cargo build --release` (64 jobs) + Playwright
# tripped the kernel OOM killer three times: it shot the provider CLI
# children (reported as "grok exited without a successful result") and
# finally peckboard.service itself (29.2G peak). journalctl -k has the
# oom-kill records. Preset CARGO_BUILD_JOBS wins; floor of 2.
if [[ -z "${CARGO_BUILD_JOBS:-}" ]]; then
  avail_kb=$(awk '/MemAvailable/ {print $2}' /proc/meminfo 2>/dev/null || echo 0)
  cpu_jobs=$(nproc 2>/dev/null || echo 4)
  mem_jobs=$((avail_kb / 1048576)) # ~1 GiB per job
  jobs=$((mem_jobs < cpu_jobs ? mem_jobs : cpu_jobs))
  ((jobs < 2)) && jobs=2
  export CARGO_BUILD_JOBS="$jobs"
  echo "(memory guard: CARGO_BUILD_JOBS=$jobs — MemAvailable ${avail_kb} kB, ${cpu_jobs} CPUs)"
fi

# Test-binary prune: every full `cargo test` links one ~300 MB executable
# per tests/*.rs (66 of them, ~20 GB), and they pile up across runs. Before
# any build, drop test executables not relinked for
# PECKBOARD_VERIFY_TEST_BIN_MAX_AGE_H hours (default 24), and — when free
# space is under PECKBOARD_VERIFY_PRUNE_BELOW_GB (default 40) — all of them.
# Cargo relinks whatever a run needs; rlibs are never touched.
prune_test_bins() {
  local deps="${CARGO_TARGET_DIR:-$ROOT/target}/debug/deps"
  [[ -d "$deps" ]] || return 0
  local below_gb="${PECKBOARD_VERIFY_PRUNE_BELOW_GB:-40}"
  local max_age_min=$((${PECKBOARD_VERIFY_TEST_BIN_MAX_AGE_H:-24} * 60))
  local free_kb age=() why="older than $((max_age_min / 60))h"
  free_kb=$(df -Pk "$ROOT" 2>/dev/null | awk 'NR==2 {print $4}')
  if [[ -n "$free_kb" ]] && ((free_kb < below_gb * 1048576)); then
    why="all: under ${below_gb} GB free"
  else
    age=(-mmin "+$max_age_min")
  fi
  local n
  n=$(find "$deps" -maxdepth 1 -type f -perm -u+x ! -name '*.*' "${age[@]}" -print -delete | wc -l)
  if ((n > 0)); then
    echo "(test-binary prune: removed $n from $deps — $why)"
  fi
  return 0
}
prune_test_bins

# Disk guard: this box also runs the live Peckboard, whose SQLite DB shares
# the root filesystem. On 2026-09-30 a cold `target/verify` build plus four
# e2e shards filled the disk and the live service crash-looped (~60
# restarts, "migration failed: disk I/O error") for six minutes. Refuse to
# start below PECKBOARD_VERIFY_MIN_FREE_GB (default 20) free on the volume
# holding target/ — free space first, or override deliberately.
min_free_gb="${PECKBOARD_VERIFY_MIN_FREE_GB:-20}"
free_kb=$(df -Pk "$ROOT" 2>/dev/null | awk 'NR==2 {print $4}')
if [[ -n "$free_kb" ]] && ((free_kb < min_free_gb * 1048576)); then
  echo "verify.sh: only $((free_kb / 1048576)) GB free on the volume holding $ROOT" >&2
  echo "  (need ${min_free_gb} GB; builds could fill the disk and crash the live Peckboard)." >&2
  echo "  Free space (e.g. old target/ dirs) or set PECKBOARD_VERIFY_MIN_FREE_GB to override." >&2
  exit 2
fi
FAST=0
IMPACTED=0
CHANGED=0
BASE="origin/main"
case "${1:-}" in
--fast) FAST=1 ;;
--impacted) IMPACTED=1 ;;
--changed)
  CHANGED=1
  [[ -n "${2:-}" ]] && BASE="$2"
  ;;
esac

# --changed: turn the diff into a plan. SCOPE=full falls through to the
# normal full run below; everything else runs a narrowed step list.
SCOPE=full
RUST_CHANGED=1
WEB_CHANGED=1
INTEGRATION_TESTS=""
WEB_FILES=""
if [[ "$CHANGED" -eq 1 ]]; then
  plan="$(node "$ROOT/scripts/verify-changed.mjs" "$BASE")" || exit 1
  eval "$plan"
  if [[ "$SCOPE" == "none" ]]; then
    echo "▶ no build inputs changed vs $BASE — nothing to verify"
    exit 0
  fi
  [[ "$SCOPE" == "full" ]] && CHANGED=0
fi

declare -a NAMES=()
declare -a RESULTS=()

run_step() {
  local name="$1"
  shift
  echo ""
  echo "══════════════════════════════════════════════════"
  echo "▶ $name"
  echo "══════════════════════════════════════════════════"
  local start=$SECONDS
  if "$@"; then
    RESULTS+=("ok ($((SECONDS - start))s)")
  else
    RESULTS+=("FAILED ($((SECONDS - start))s)")
  fi
  NAMES+=("$name")
}

cd "$ROOT"
if [[ "$CHANGED" -eq 1 ]]; then
  echo "▶ proportional verify: scope=$SCOPE (rust=$RUST_CHANGED web=$WEB_CHANGED) vs $BASE"
  if [[ "$RUST_CHANGED" -eq 1 ]]; then
    run_step "cargo fmt --check" cargo fmt --check
    run_step "cargo clippy" cargo clippy --all-targets --no-deps
    run_step "cargo test --lib" cargo test --lib
    if [[ -n "$INTEGRATION_TESTS" ]]; then
      read -ra its <<<"$INTEGRATION_TESTS"
      run_step "cargo test (integration: $INTEGRATION_TESTS)" \
        cargo test $(printf -- '--test %s ' "${its[@]}")
    fi
  fi
  run_step "plugin blobs current" "$ROOT/scripts/check-plugin-blobs.sh"
  # The browser sidecar is embedded JS outside the web lint scope and no e2e
  # spec runs it: syntax-check it here so a broken edit can't ship.
  run_step "embedded JS syntax" node --check "$ROOT/src/service/browser_sidecar.mjs"
  if [[ "$WEB_CHANGED" -eq 1 && -n "$WEB_FILES" ]]; then
    read -ra wf <<<"$WEB_FILES"
    cd "$ROOT/web"
    lintable=()
    for f in "${wf[@]}"; do [[ "$f" =~ \.(tsx?|jsx?|mjs|cjs)$ ]] && lintable+=("$f"); done
    if ((${#lintable[@]})); then
      run_step "web lint (changed files)" npx eslint --no-warn-ignored "${lintable[@]}"
    fi
    run_step "web format:check (changed files)" npx prettier --check --ignore-unknown "${wf[@]}"
    cd "$ROOT"
  fi
  # tsc -b runs inside the web build (skipped only when web/dist is newer
  # than every input, i.e. it already passed on this exact tree).
  run_step "local release build" "$ROOT/scripts/build-local-release.sh"
  export PECKBOARD_E2E_SKIP_BUILD=1
  run_step "web e2e (impacted vs $BASE)" "$ROOT/scripts/e2e-impacted.sh" "$BASE"
else
  run_step "cargo fmt --check" cargo fmt --check
  run_step "cargo clippy" cargo clippy --all-targets --no-deps
  run_step "cargo test" cargo test
  run_step "plugin blobs current" "$ROOT/scripts/check-plugin-blobs.sh"

  run_step "embedded JS syntax" node --check "$ROOT/src/service/browser_sidecar.mjs"
  cd "$ROOT/web"
  run_step "web lint" npm run lint
  run_step "web format:check" npm run format:check

  if [[ "$FAST" -eq 0 ]]; then
    # rust-embed bakes web/dist into the binary at compile time, so a stale
    # bundle means the e2e suite drives the PREVIOUS UI; the helper builds
    # the web assets (when stale) before the binary that embeds them.
    run_step "local release build" "$ROOT/scripts/build-local-release.sh"
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
    echo "(--fast: skipping release build + Playwright e2e)"
  fi
fi

echo ""
echo "══════════════════════════════════════════════════"
echo "Summary"
echo "══════════════════════════════════════════════════"
failed=0
for i in "${!NAMES[@]}"; do
  printf '  %-34s %s\n' "${NAMES[$i]}" "${RESULTS[$i]}"
  [[ "${RESULTS[$i]}" == FAILED* ]] && failed=1
done
exit "$failed"
