import { defineConfig } from '@playwright/test'
import { copyFileSync, existsSync, mkdirSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import baseConfig from './playwright.config'

/**
 * Config for the docs screenshot capture run (`npm run screenshots`).
 *
 * Reuses the main e2e harness as-is — same webServer boot of the
 * release binary against a fresh temp data dir, same bootstrap admin —
 * but points testDir at `./screenshots` (which the regular `npm run
 * e2e` never picks up, since its testDir is `./tests`) and fixes the
 * viewport so the captured PNGs are consistent run-to-run.
 *
 * On top of the openai-compat wasm the base config stages, this run also
 * copies the experts plugin wasm into the data dir (config-eval is the
 * only pre-boot hook — see the base config's note): experts.png needs
 * `spin_up_experts`, which ships in the experts plugin since it moved
 * out of core. The capture spec approves it after boot.
 */
const expertsWasm = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  '..',
  '..',
  '..',
  'peck-plugins',
  'experts',
  'dist',
  'plugin.wasm',
)
if (existsSync(expertsWasm)) {
  const pluginsDir = path.join(process.env.PECKBOARD_E2E_DATA_DIR!, 'plugins')
  mkdirSync(pluginsDir, { recursive: true })
  copyFileSync(expertsWasm, path.join(pluginsDir, 'experts.wasm'))
}
export default defineConfig({
  ...baseConfig,
  testDir: './screenshots',
  use: {
    ...baseConfig.use,
    viewport: { width: 1280, height: 800 },
    colorScheme: 'light',
  },
})
