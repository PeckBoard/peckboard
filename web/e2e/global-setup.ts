import { execSync } from 'node:child_process'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

/**
 * Global setup for Playwright e2e tests.
 *
 * Builds the frontend (so the binary embeds the latest assets) and the
 * release binary that the webServer block will launch. Both builds are
 * incremental — repeated runs are fast.
 */
export default async function globalSetup() {
  const here = path.dirname(fileURLToPath(import.meta.url))
  const webDir = path.resolve(here, '..')
  const repoRoot = path.resolve(here, '..', '..')

  // Copy built WASM plugins into the e2e data dir so the server can load
  // them on startup. This runs regardless of PECKBOARD_E2E_SKIP_BUILD so
  // the plugin tests always have the latest WASM available.
  const dataDir = process.env.PECKBOARD_E2E_DATA_DIR
  if (dataDir) {
    const pluginsDir = path.join(dataDir, 'plugins')
    fs.mkdirSync(pluginsDir, { recursive: true })
    const openaiCompatWasm = path.resolve(
      repoRoot,
      '..',
      'peck-plugins',
      'openai-compat',
      'dist',
      'plugin.wasm',
    )
    if (fs.existsSync(openaiCompatWasm)) {
      fs.copyFileSync(openaiCompatWasm, path.join(pluginsDir, 'openai-compat.wasm'))
      console.log('[e2e] Copied openai-compat.wasm to e2e data dir')
    }
  }

  // Escape hatch for machines where the release re-link is slow: set
  // PECKBOARD_E2E_SKIP_BUILD=1 when you've just built the frontend AND
  // the release binary yourself (in that order — the binary embeds the
  // dist). CI leaves it unset and always rebuilds.
  if (process.env.PECKBOARD_E2E_SKIP_BUILD === '1') {
    console.log('[e2e] PECKBOARD_E2E_SKIP_BUILD=1 — using existing frontend + binary')
    return
  }

  console.log('[e2e] Building frontend...')
  execSync('npm run build', { cwd: webDir, stdio: 'inherit' })

  // rust-embed bakes web/dist into the binary at compile time, but cargo
  // keys recompilation on Rust source — a dist-only change doesn't
  // invalidate the embedding module, so the binary would serve STALE
  // assets (the e2e then fails against UI that "isn't there"). Touch the
  // module that derives RustEmbed so the fresh dist is always re-embedded.
  execSync('touch src/frontend.rs', { cwd: repoRoot, stdio: 'inherit' })

  console.log('[e2e] Building release binary (this is slow on first run)...')
  execSync('cargo build --release', { cwd: repoRoot, stdio: 'inherit' })
}
