import { execSync } from 'node:child_process'
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
  // Escape hatch for machines where the release re-link is slow: set
  // PECKBOARD_E2E_SKIP_BUILD=1 when you've just built the frontend AND
  // the release binary yourself (in that order — the binary embeds the
  // dist). CI leaves it unset and always rebuilds.
  if (process.env.PECKBOARD_E2E_SKIP_BUILD === '1') {
    console.log('[e2e] PECKBOARD_E2E_SKIP_BUILD=1 — using existing frontend + binary')
    return
  }
  const here = path.dirname(fileURLToPath(import.meta.url))
  const webDir = path.resolve(here, '..')
  const repoRoot = path.resolve(here, '..', '..')

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
