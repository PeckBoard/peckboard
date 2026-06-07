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
  const here = path.dirname(fileURLToPath(import.meta.url))
  const webDir = path.resolve(here, '..')
  const repoRoot = path.resolve(here, '..', '..')

  console.log('[e2e] Building frontend...')
  execSync('npm run build', { cwd: webDir, stdio: 'inherit' })

  console.log('[e2e] Building release binary (this is slow on first run)...')
  execSync('cargo build --release', { cwd: repoRoot, stdio: 'inherit' })
}
