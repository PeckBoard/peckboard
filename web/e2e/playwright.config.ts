import { defineConfig } from '@playwright/test'

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
    command: `PECKBOARD_DATA_DIR=$(mktemp -d /tmp/peckboard-e2e-XXXXXX) PECKBOARD_BOOTSTRAP_USERNAME=${E2E_USER} PECKBOARD_BOOTSTRAP_PASSWORD=${E2E_PASS} ../../target/release/peckboard --port ${PORT} --https-port ${HTTPS_PORT} --host 127.0.0.1`,
    url: `http://127.0.0.1:${PORT}/api/health`,
    reuseExistingServer: !process.env.CI,
    timeout: 60_000,
    stdout: 'pipe',
    stderr: 'pipe',
  },
})
