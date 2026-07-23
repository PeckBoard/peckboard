import { defineConfig } from '@playwright/test'
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
const pluginsSrcRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  '..',
  '..',
  '..',
  'peck-plugins',
)
for (const plugin of ['openai-compat', 'chicken-coop']) {
  const wasm = path.join(pluginsSrcRoot, plugin, 'dist', 'plugin.wasm')
  if (existsSync(wasm)) {
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
  reporter: process.env.CI ? 'github' : 'list',
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
    command: `PECKBOARD_DATA_DIR=${DATA_DIR} PECKBOARD_BOOTSTRAP_USERNAME=${E2E_USER} PECKBOARD_BOOTSTRAP_PASSWORD=${E2E_PASS} ../../target/release/peckboard --port ${PORT} --https-port ${HTTPS_PORT} --host 127.0.0.1`,
    url: `http://127.0.0.1:${PORT}/api/health`,
    reuseExistingServer: !process.env.CI,
    timeout: 60_000,
    stdout: 'pipe',
    stderr: 'pipe',
  },
})
