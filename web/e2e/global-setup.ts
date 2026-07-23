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

  // NOTE: the openai-compat wasm copy lives in playwright.config.ts, not
  // here — Playwright launches the webServer BEFORE globalSetup, so any
  // copy made here lands after the server's plugin load_all and is never
  // loaded. Config evaluation is the only pre-boot hook.
  //
  // The flip side: the server is already up NOW, so approve the copied
  // plugin immediately. Left pending, its approval prompt overlays every
  // page and times out unrelated UI tests. Approval without settings
  // registers no provider (the plugin skips — no base_url yet); the
  // openai-compat spec re-approves after configuring settings, which
  // re-dispatches provider.register with the stub config.
  const port = process.env.PECKBOARD_E2E_PORT ?? '4444'
  const baseURL = `http://127.0.0.1:${port}`
  try {
    const login = await fetch(`${baseURL}/api/auth/login`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        username: process.env.PECKBOARD_E2E_USER ?? 'e2e-user',
        password: process.env.PECKBOARD_E2E_PASS ?? 'e2e-password-1234',
      }),
    })
    if (login.ok) {
      const { token } = (await login.json()) as { token: string }
      for (const plugin of ['openai-compat', 'chicken-coop']) {
        await fetch(`${baseURL}/api/plugins/${plugin}/approval`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
          body: JSON.stringify({ decision: 'approve' }),
        })
      }
      console.log('[e2e] Approved staged wasm plugins (if present)')
    }
  } catch {
    // Server not reachable yet — plugin tests will self-skip.
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
