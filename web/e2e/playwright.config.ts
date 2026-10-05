import { defineConfig } from '@playwright/test'
import { execFileSync } from 'node:child_process'
import { copyFileSync, existsSync, mkdirSync, mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

/**
 * Playwright config for peckboard end-to-end tests.
 *
 * The webServer block boots the release binary with a fresh temp data dir
 * per run, on a fixed test port. The MockProvider is available out of the
 * box (it is registered alongside the Claude provider), so tests that
 * need a deterministic agent can create sessions with model id
 * `mock:echo`, `mock:happy-path`, etc.
 *
 * `ignoreHTTPSErrors` is set because peckboard self-signs its TLS cert.
 *
 * No `.spec.ts` files exist yet — this is the scaffolding only.
 */
const PORT = process.env.PECKBOARD_E2E_PORT ?? '4444'
const HTTPS_PORT = process.env.PECKBOARD_E2E_HTTPS_PORT ?? '4445'
// Where `doc-review-pr.spec.ts` stands up its fake GitHub API. Fixed rather
// than random because the server reads the base URL from its environment at
// boot, long before any spec runs.
const GITHUB_STUB_PORT = process.env.PECKBOARD_E2E_GITHUB_PORT ?? '4446'
process.env.PECKBOARD_E2E_GITHUB_PORT = GITHUB_STUB_PORT
// Impact-map capture. Set only by scripts/e2e-impact-map.sh; when unset,
// every branch below is inert and this config behaves exactly as before.
// The server appends its per-request route log next to the coverage and
// timing records, one file per shard so the wall-clock join stays within
// a single server's timeline.
const IMPACT_DIR = process.env.PECKBOARD_E2E_IMPACT_DIR
const SHARD = process.env.PECKBOARD_E2E_SHARD ?? '1'
const ROUTE_LOG = IMPACT_DIR ? path.join(IMPACT_DIR, `routes-${SHARD}.jsonl`) : ''

// Self-service registration was removed; the server now bootstraps a
// single admin from the bootstrap env vars on first start. We pre-set
// known credentials here so the tests can log in directly. The
// credentials are also exported via `process.env` so the spec helpers
// can read them.
const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
process.env.PECKBOARD_E2E_USER = E2E_USER
process.env.PECKBOARD_E2E_PASS = E2E_PASS

// The server's data dir is created here (instead of inline in the
// webServer shell command) so its path is known to the spec processes
// via `process.env.PECKBOARD_E2E_DATA_DIR`. A few specs need to read
// server-written files under it — e.g. the per-session MCP token at
// `worker-mcp/<session_id>.json`, which is the only way to drive MCP
// tools (like `spin_up_experts`) over the loopback `/mcp` endpoint.
// One dir per run, same isolation as the previous inline `mktemp -d`.
const DATA_DIR =
  process.env.PECKBOARD_E2E_DATA_DIR ?? mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-'))
process.env.PECKBOARD_E2E_DATA_DIR = DATA_DIR

// Copy built WASM plugins into the fresh data dir NOW, at config-eval time:
// Playwright launches the webServer BEFORE globalSetup runs, so a copy made
// there lands after the server's plugin load_all and is never loaded.
// Config evaluation happens first (it defines the webServer), making this
// the only reliable pre-boot hook. Idempotent — worker processes re-eval
// this file against the same DATA_DIR.
// A plugin may live in-tree (`<repo>/peck-plugins/<id>`) or in the older
// sibling checkout next to the repo — same two candidates the Rust plugin
// tests probe. First one with a built wasm wins.
const e2eDir = path.dirname(fileURLToPath(import.meta.url))

// Pairing-v2 enrollment (`remote-access-enroll.spec.ts`) runs a real local
// relay (`peckboard-relay --dev-self-signed`) and a real `peckboard-connect`
// against the box. Both binaries come from scripts/build-local-release.sh
// (target/verify-tools). The relay writes its throwaway cert into
// RELAY_STATE_DIR; the box pins that file on every relay connection
// (hidden dev knob PECKBOARD_DEV_RELAY_CERT, read lazily), so the spec can
// start the relay long after the server booted. PECKBOARD_DEV_LINK_TTL_SECS
// keeps the default hour but unlocks a per-link `ttl_secs` so one spec can
// watch a link expire within seconds.
const RELAY_STATE_DIR = path.join(DATA_DIR, 'e2e-relay')
process.env.PECKBOARD_E2E_RELAY_STATE_DIR = RELAY_STATE_DIR
const TOOLS_DIR =
  process.env.PECKBOARD_E2E_TOOLS_DIR ??
  path.resolve(e2eDir, '..', '..', 'target', 'verify-tools', 'release')
process.env.PECKBOARD_E2E_TOOLS_DIR = TOOLS_DIR

// The binary scripts/build-local-release.sh produces (`--profile verify`).
// PECKBOARD_E2E_BIN points the suite at another, already-built binary, e.g.
// ../../target/release/peckboard to test exactly what CI compiles.
const SERVER_BIN = process.env.PECKBOARD_E2E_BIN ?? '../../target/verify/peckboard'

// Build the frontend + that binary NOW, for the same reason as the plugin
// copy below: this is the last hook before the webServer boots (globalSetup
// runs after, so a build there only ever refreshed the NEXT run). The
// helper skips the web build when web/dist is current and builds
// incrementally. Setting the skip flag afterwards stops worker processes,
// which re-eval this file, from building again. scripts/e2e-shards.sh and
// verify.sh build once up front and pass PECKBOARD_E2E_SKIP_BUILD=1.
if (process.env.PECKBOARD_E2E_SKIP_BUILD !== '1' && !process.env.PECKBOARD_E2E_BIN) {
  execFileSync(path.resolve(e2eDir, '..', '..', 'scripts', 'build-local-release.sh'), {
    stdio: 'inherit',
  })
  process.env.PECKBOARD_E2E_SKIP_BUILD = '1'
}

const pluginsSrcRoots = [
  path.resolve(e2eDir, '..', '..', 'peck-plugins'),
  path.resolve(e2eDir, '..', '..', '..', 'peck-plugins'),
]
// A JS-toolchain plugin ships `dist/plugin.wasm`; a Rust plugin's artifact
// is `target/wasm32-unknown-unknown/release/peckboard_<id>_plugin.wasm`.
const artifactCandidates = (plugin: string) => [
  path.join(plugin, 'dist', 'plugin.wasm'),
  path.join(
    plugin,
    'target',
    'wasm32-unknown-unknown',
    'release',
    `peckboard_${plugin.replace(/-/g, '_')}_plugin.wasm`,
  ),
]
for (const plugin of [
  'openai-compat',
  'chicken-coop',
  'app-manager',
  'project-planner',
  'session-control',
  'ui-gauge',
]) {
  const wasm = pluginsSrcRoots
    .flatMap((root) => artifactCandidates(plugin).map((rel) => path.join(root, rel)))
    .find((candidate) => existsSync(candidate))
  if (wasm) {
    const pluginsDir = path.join(DATA_DIR, 'plugins')
    mkdirSync(pluginsDir, { recursive: true })
    copyFileSync(wasm, path.join(pluginsDir, `${plugin}.wasm`))
  }
}
export default defineConfig({
  testDir: './tests',
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 2 : 0,
  workers: 1,
  // The impact reporter records each test's wall-clock window; it writes
  // nothing unless PECKBOARD_E2E_IMPACT_DIR is set.
  reporter: IMPACT_DIR ? [['list'], ['./impact/reporter.ts']] : process.env.CI ? 'github' : 'list',
  globalSetup: './global-setup.ts',
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    ignoreHTTPSErrors: true,
    trace: 'on-first-retry',
  },
  webServer: {
    // Fresh data dir each run so prior state can't bleed in.
    // The binary embeds the frontend, so we only run the binary here —
    // both builds happen in global-setup before webServer launches.
    // PECKBOARD_CLAUDE_MODEL_DISCOVERY=0 pins the Claude catalog to the
    // static seed: specs assert exact model labels, which must not depend
    // on whatever `claude` binary the host machine has installed.
    // PECKBOARD_GITHUB_TOKEN + PECKBOARD_GITHUB_API_BASE point the PR
    // features at the stub `doc-review-pr.spec.ts` runs on
    // GITHUB_STUB_PORT, so "both directions" is exercised for real without
    // ever leaving the machine.
    // PECKBOARD_E2E_ROUTE_LOG is empty (inert) outside the impact-map run.
    // PECKBOARD_PREINSTALL_PLUGINS=all seeds every bundled crate plugin
    // installed — fresh installs default to none, but specs assume mock
    // and friends are active; the install/uninstall flow is covered by
    // crate-plugin-install.spec.ts which round-trips from this state.
    // PECKBOARD_MIRROR_TEST_ENDPOINTS=1 lets the Assistant mirror post to
    // the http://127.0.0.1 receiver `assistant-mirror.spec.ts` runs.
    // PECKBOARD_DEV_RELAY_CERT + PECKBOARD_DEV_LINK_TTL_SECS: see the
    // RELAY_STATE_DIR comment above (remote-access-enroll.spec.ts).
    command: `PECKBOARD_DATA_DIR=${DATA_DIR} PECKBOARD_BOOTSTRAP_USERNAME=${E2E_USER} PECKBOARD_BOOTSTRAP_PASSWORD=${E2E_PASS} PECKBOARD_PREINSTALL_PLUGINS=all PECKBOARD_CLAUDE_MODEL_DISCOVERY=0 PECKBOARD_TTS_DOWNLOAD=0 PECKBOARD_GITHUB_TOKEN=e2e-stub-token PECKBOARD_GITHUB_API_BASE=http://127.0.0.1:${GITHUB_STUB_PORT} PECKBOARD_MIRROR_TEST_ENDPOINTS=1 PECKBOARD_DEV_RELAY_CERT=${RELAY_STATE_DIR}/dev-cert.der PECKBOARD_DEV_LINK_TTL_SECS=3600 PECKBOARD_E2E_ROUTE_LOG=${ROUTE_LOG} ${SERVER_BIN} --port ${PORT} --https-port ${HTTPS_PORT} --host 127.0.0.1`,
    reuseExistingServer: !process.env.CI,
    timeout: 60_000,
    stdout: 'pipe',
    stderr: 'pipe',
  },
})
