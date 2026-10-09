/**
 * Global setup for Playwright e2e tests.
 *
 * Runs against the already-booted webServer: approves the staged wasm
 * plugins and completes the first-run wizard. The frontend + release
 * binary are built earlier, at config-eval time in playwright.config.ts.
 */
export default async function globalSetup() {
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

  // The health endpoint Playwright waits on can answer a beat before the
  // bootstrap admin row lands, and a login that loses that race used to
  // fail silently — leaving the plugin unapproved, its prompt overlaying
  // every page, and every click in the suite timing out against a
  // `modal-backdrop`. Retry briefly, and say so if it never works.
  const tokenFor = async (): Promise<string | null> => {
    for (let attempt = 0; attempt < 20; attempt++) {
      try {
        const login = await fetch(`${baseURL}/api/auth/login`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            username: process.env.PECKBOARD_E2E_USER ?? 'e2e-user',
            password: process.env.PECKBOARD_E2E_PASS ?? 'e2e-password-1234',
          }),
        })
        if (login.ok) return ((await login.json()) as { token: string }).token
      } catch {
        // Server not up yet — fall through to the wait below.
      }
      await new Promise((resolve) => setTimeout(resolve, 250))
    }
    return null
  }

  const token = await tokenFor()
  if (token) {
    for (const plugin of [
      'openai-compat',
      'chicken-coop',
      'app-manager',
      'project-planner',
      'session-control',
      'ui-gauge',
      'ssh-fleet',
    ]) {
      await fetch(`${baseURL}/api/plugins/${plugin}/approval`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
        body: JSON.stringify({ decision: 'approve' }),
      })
    }
    console.log('[e2e] Approved staged wasm plugins (if present)')

    // The fresh data dir is a "fresh install", so the server seeds the
    // first-run setup wizard as incomplete — which would overlay every
    // page and break unrelated specs. Complete it here; the wizard's own
    // spec stubs GET /api/settings/setup to exercise the UI.
    await fetch(`${baseURL}/api/settings/setup/complete`, {
      method: 'POST',
      headers: { Authorization: `Bearer ${token}` },
    })
    console.log('[e2e] Marked first-run setup complete')
  } else {
    console.warn(
      '[e2e] Could not log in to approve staged plugins — their approval prompt will block clicks',
    )
  }

  // No build here: Playwright boots the webServer BEFORE globalSetup, so a
  // build at this point only ever refreshed the binary for the NEXT run.
  // playwright.config.ts builds at config-eval time instead (via
  // scripts/build-local-release.sh), before the server launches.
}
