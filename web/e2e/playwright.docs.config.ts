import { defineConfig } from '@playwright/test'

/**
 * Playwright config for the public docs site (docs/ → Jekyll → docs/_site).
 *
 * Unlike the app e2e config this does not boot the peckboard binary; it
 * serves the prebuilt static site under the production /peckboard baseurl
 * via e2e/docs/serve.mjs. Build the site first (see that file's header),
 * then: npm run e2e:docs
 */
const PORT = process.env.DOCS_E2E_PORT ?? '4448'

export default defineConfig({
  testDir: './docs',
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: 0,
  reporter: process.env.CI ? 'github' : 'list',
  use: {
    baseURL: `http://127.0.0.1:${PORT}/peckboard/`,
    trace: 'on-first-retry',
  },
  webServer: {
    command: 'node docs/serve.mjs',
    url: `http://127.0.0.1:${PORT}/peckboard/`,
    reuseExistingServer: !process.env.CI,
    timeout: 15_000,
    stdout: 'pipe',
    stderr: 'pipe',
  },
})
