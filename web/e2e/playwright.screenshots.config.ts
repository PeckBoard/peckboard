import { defineConfig } from '@playwright/test'
import baseConfig from './playwright.config'

/**
 * Config for the docs screenshot capture run (`npm run screenshots`).
 *
 * Reuses the main e2e harness as-is — same webServer boot of the
 * release binary against a fresh temp data dir, same bootstrap admin —
 * but points testDir at `./screenshots` (which the regular `npm run
 * e2e` never picks up, since its testDir is `./tests`) and fixes the
 * viewport so the captured PNGs are consistent run-to-run.
 */
export default defineConfig({
  ...baseConfig,
  testDir: './screenshots',
  use: {
    ...baseConfig.use,
    viewport: { width: 1280, height: 800 },
    colorScheme: 'light',
  },
})
