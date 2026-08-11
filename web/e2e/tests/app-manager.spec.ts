import {
  test,
  expect,
  type APIRequestContext,
  type FrameLocator,
  type Page,
} from '@playwright/test'

/**
 * UI e2e for the app-manager plugin's App Manager page (sidebar →
 * App Manager): the page loads inside its sandboxed iframe, the target picker
 * is a real `<select>`, the distro banner reports what was detected on the
 * local host, the app grid shows installed state per app, and the add-remote-
 * target dialog picks an SSH key from a `<select>` (never free text) and
 * reports validation failures as prose.
 *
 * Deliberately does NOT install or remove anything: those run real
 * package-manager commands as root on whatever machine the suite runs on.
 * The install/remove job lifecycle is covered by the plugin's own vitest
 * suite (`peck-plugins/app-manager/test/`).
 *
 * SKIPS when the plugin wasm isn't built — `playwright.config.ts` only stages
 * `peck-plugins/app-manager/dist/plugin.wasm` if it exists (build it
 * with `peck-plugins/app-manager/build.sh`, which needs `extism-js`).
 */

const PLUGIN = 'app-manager'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: {
      username: process.env.PECKBOARD_E2E_USER ?? 'e2e-user',
      password: process.env.PECKBOARD_E2E_PASS ?? 'e2e-password-1234',
    },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { token: string }).token
}

async function loadApp(page: Page, token: string) {
  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto('/')
  await expect(page.locator('.rail-brand')).toBeVisible({ timeout: 20_000 })
}
/** Is the plugin's sidebar entry there? False = its wasm was never staged.
 *  The rail renders before `/api/plugins` resolves, so this waits rather than
 *  sampling once (a bare count() races the fetch and skips a working plugin). */
async function pluginPresent(page: Page): Promise<boolean> {
  return page
    .getByTestId(`plugin-sidebar-${PLUGIN}-${PLUGIN}`)
    .waitFor({ state: 'attached', timeout: 15_000 })
    .then(() => true)
    .catch(() => false)
}

async function openAppManager(page: Page): Promise<FrameLocator> {
  await page.getByTestId(`plugin-sidebar-${PLUGIN}-${PLUGIN}`).click()
  const frame = page.frameLocator('[data-testid="plugin-fullpage-frame"]')
  await expect(frame.locator('#targetSel')).toBeVisible({ timeout: 30_000 })
  return frame
}

test.describe('app-manager page', () => {
  test('shows the target picker, distro banner and app grid for the local host', async ({
    request,
    page,
  }) => {
    const token = await authenticate(request)
    await loadApp(page, token)
    test.skip(!(await pluginPresent(page)), 'app-manager wasm not built')

    const f = await openAppManager(page)

    // Target picker is a dropdown with the always-present local target.
    await expect(f.locator('select#targetSel')).toHaveCount(1)
    await expect(f.locator('#targetSel option')).toHaveText(['Local (this host)'])
    await expect(f.locator('#targetDetail')).toHaveText('this Peckboard host')

    // Distro banner: either a detected package manager, or a readable refusal.
    await expect(f.locator('#bannerText')).not.toHaveText('Loading…', { timeout: 60_000 })
    const banner = (await f.locator('#bannerText').textContent()) ?? ''
    expect(banner).not.toContain('{')

    // One row per catalog app, each with a state badge and one action button.
    const rows = f.locator('.approw')
    await expect(rows.first()).toBeVisible({ timeout: 60_000 })
    expect(await rows.count()).toBeGreaterThan(1)
    const badges = await rows.locator('.badge').first().allTextContents()
    expect(badges.join()).toMatch(/Installed|Not installed/)
    await expect(rows.first().locator('.acts button').first()).toHaveText(/Install|Remove/)

    // Provenance card: every listed entry carries a version. Git is
    // certainly installed on the host running this suite (the repo itself is
    // a git checkout), so its row must render the probed version string.
    const gitRow = rows.filter({ hasText: 'Distributed version control' })
    await expect(gitRow).toHaveCount(1)
    await expect(gitRow.locator('.ver')).toContainText(/\d+\.\d+/)
  })

  test('picks an SSH key from a dropdown and reports save failures as prose', async ({
    request,
    page,
  }) => {
    const token = await authenticate(request)
    await loadApp(page, token)
    test.skip(!(await pluginPresent(page)), 'app-manager wasm not built')

    const f = await openAppManager(page)
    await f.locator('#addTargetBtn').click()
    await expect(f.locator('#targetBackdrop')).toHaveClass(/open/)

    // The key picker is a <select> fed from the core vault — never a free-text
    // field, and the page never shows key material.
    await expect(f.locator('select#f_key_id')).toHaveCount(1)
    await expect(f.locator('#keyHint')).toContainText('never sees or stores private key material')
    const dialogText = (await f.locator('.modal').first().textContent()) ?? ''
    expect(dialogText).not.toContain('PRIVATE KEY')

    // A missing required field comes back as a sentence, not a JSON envelope.
    await f.locator('#f_hostname').fill('10.0.0.5')
    await f.locator('#targetSave').click()
    const err = f.locator('#targetErr')
    await expect(err).not.toHaveText('')
    const errText = (await err.textContent()) ?? ''
    expect(errText).not.toContain('{')
    expect(errText).toMatch(/required/i)

    // Escape closes the dialog (keyboard-operable, focus returns to the page).
    await page.keyboard.press('Escape')
    await expect(f.locator('#targetBackdrop')).not.toHaveClass(/open/)
  })

  test('resolves and renders an installed app dependency tree with versions', async ({
    request,
    page,
  }) => {
    const token = await authenticate(request)
    await loadApp(page, token)
    test.skip(!(await pluginPresent(page)), 'app-manager wasm not built')

    const f = await openAppManager(page)

    // git is installed on the suite host (this repo is a git checkout), so
    // its row must be present and marked Installed. Waiting on the row also
    // gates on /apps having resolved, so the banner below has settled past
    // its transient "Checking…" state before the skip guard reads it.
    const gitRow = f.locator('.approw').filter({ hasText: 'Distributed version control' })
    await expect(gitRow).toHaveCount(1)
    await expect(gitRow.locator('.badge').first()).toHaveText('Installed', { timeout: 60_000 })

    const banner = (await f.locator('#bannerText').textContent()) ?? ''
    test.skip(!/apt|dnf|pacman|zypper/i.test(banner), 'no supported package manager on this host')

    // Fresh data dir → nothing cached yet, and the bar says so instead of
    // showing an empty graph.
    await expect(f.locator('#depsInfo')).toContainText('not resolved yet')

    await f.locator('#depsRefreshBtn').click()
    await expect(f.locator('#depsInfo')).toContainText('Dependencies resolved', {
      timeout: 120_000,
    })

    await gitRow.locator('.depstoggle').click()
    // The app's own package opens pre-expanded: real dependency nodes with
    // real package-DB versions underneath, kind chips on every line.
    const firstDep = gitRow.locator('.depnode .depnode').first()
    await expect(firstDep).toBeVisible()
    await expect(firstDep.locator('.depver').first()).toContainText(/\d/)
    await expect(gitRow.locator('.depnode .depkind').first()).not.toBeEmpty()

    // The reverse-view picker filled with the resolved dependency set.
    expect(await f.locator('#libSel option').count()).toBeGreaterThan(1)
  })

  test('installs via a temporary AI session with a picked account + model', async ({
    request,
    page,
  }) => {
    const token = await authenticate(request)
    await loadApp(page, token)
    test.skip(!(await pluginPresent(page)), 'app-manager wasm not built')

    const f = await openAppManager(page)
    const rows = f.locator('.approw')
    await expect(rows.first()).toBeVisible({ timeout: 60_000 })

    // Any app not installed on this host will do; skip only in the unlikely
    // case the whole catalog is already present.
    const row = f.locator('.approw:has(button.primary:enabled)').first()
    test.skip((await row.count()) === 0, 'every catalog app is already installed on this host')
    const appName = ((await row.locator('.name').textContent()) ?? '')
      .replace(/Not installed/, '')
      .trim()

    // The picker is a real <select> over server-filtered thinking models —
    // the non-thinking mock scenarios must never be offered.
    await row.locator('button.primary').click()
    await expect(f.locator('#installBackdrop')).toHaveClass(/open/)
    const modelSel = f.locator('select#f_model')
    await expect(modelSel).toHaveCount(1)
    await expect(modelSel.locator('option[value="mock:plan-review"]')).toHaveCount(1, {
      timeout: 20_000,
    })
    expect(await modelSel.locator('option[value*="happy-path"]').count()).toBe(0)

    await modelSel.selectOption('mock:plan-review')
    await f.locator('#installStart').click()
    await expect(f.locator('#installBackdrop')).not.toHaveClass(/open/)

    // Progress panel: honest tool-level session activity, never a fake log.
    // The mock run can finish faster than the first 2s poll, so accept any
    // session-job status — the terminal state is asserted strictly below.
    await expect(f.locator('#logStatus')).toHaveText(
      /Installing via AI session…|Waiting for your answer|Install session (failed|succeeded)|Installed via AI session/,
      { timeout: 20_000 },
    )
    await expect(f.locator('#sessionBar')).toBeVisible()
    await expect(f.locator('#sessionNote')).toContainText('never command output')
    await expect(f.locator('#openSessionBtn')).toBeEnabled({ timeout: 20_000 })
    await expect(f.locator('#logTail')).toContainText('Agent started', { timeout: 20_000 })

    // The mock run ends without installing anything, so the plugin's detect
    // probe honestly reports failure — proving the settle path end to end.
    await expect(f.locator('#logStatus')).toHaveText('Install session failed', {
      timeout: 30_000,
    })
    await expect(f.locator('#logTail')).toContainText('not detected')

    // The chosen account+model persisted as the default for next time.
    await row.locator('button.primary').click()
    await expect(f.locator('#installBackdrop')).toHaveClass(/open/)
    await expect(modelSel).toHaveValue('mock:plan-review', { timeout: 20_000 })
    await page.keyboard.press('Escape')

    // Deep link: the panel's button opens the real session tab in the app.
    await f.locator('#openSessionBtn').click()
    await expect(page).toHaveURL(/\/sessions\//, { timeout: 20_000 })
    await expect(page.getByText(`Install ${appName}`, { exact: false }).first()).toBeVisible({
      timeout: 20_000,
    })
    // The temp session ran the mock plan-review scenario — its transcript
    // renders, proving the deep link landed on the real conversation.
    await expect(page.getByText('Plan saved via propose_plan.').first()).toBeVisible({
      timeout: 20_000,
    })
  })

  test("fills a hand-added app's blank entries in from a research session", async ({
    request,
    page,
  }) => {
    const token = await authenticate(request)
    await loadApp(page, token)
    test.skip(!(await pluginPresent(page)), 'app-manager wasm not built')

    const f = await openAppManager(page)
    await expect(f.locator('.approw').first()).toBeVisible({ timeout: 60_000 })

    // Add an app with nothing but a name — the case every entry is missing.
    const name = `E2E Blank ${Date.now()}`
    await f.locator('#addAppBtn').click()
    await expect(f.locator('#appBackdrop')).toHaveClass(/open/)
    await f.locator('#f_app_name').fill(name)
    await f.locator('#appSave').click()
    await expect(f.locator('#appBackdrop')).not.toHaveClass(/open/)

    const row = f.locator('.approw').filter({ hasText: name })
    await expect(row).toHaveCount(1, { timeout: 60_000 })
    await expect(row.locator('.badge.manual')).toHaveText('added by hand')
    // The row names what is blank rather than showing empty fields.
    await expect(row.locator('.fill')).toContainText('Still blank:', { timeout: 30_000 })
    await expect(row.locator('.fill')).toContainText('install command')

    // "Fill in details" opens the same account+model picker as an install,
    // in a mode that says plainly it installs nothing.
    await row.locator('.fill button', { hasText: 'Fill in details' }).click()
    await expect(f.locator('#installBackdrop')).toHaveClass(/open/)
    await expect(f.locator('#installModalTitle')).toHaveText(`Fill in details for ${name}`)
    await expect(f.locator('#installIntro')).toContainText('IT INSTALLS NOTHING')
    await expect(f.locator('#installIntro')).toContainText('stored as a suggestion')
    await expect(f.locator('#installStart')).toHaveText('Start research session')

    const modelSel = f.locator('select#f_model')
    await expect(modelSel.locator('option[value="mock:plan-review"]')).toHaveCount(1, {
      timeout: 20_000,
    })
    await modelSel.selectOption('mock:plan-review')
    await f.locator('#installStart').click()
    await expect(f.locator('#installBackdrop')).not.toHaveClass(/open/)

    // While it runs the row says so; the mock scenario never calls
    // app_record_details, so the settled state admits it recorded nothing
    // instead of claiming a result — and the entries stay blank.
    await expect(row.locator('.fill')).toContainText(
      /Filling in details…|without recording any details/,
      { timeout: 30_000 },
    )
    await expect(row.locator('.fill')).toContainText('without recording any details', {
      timeout: 60_000,
    })
    await expect(row.locator('.fill')).toContainText('Still blank:')

    // Nothing was installed by any of this.
    await expect(row.locator('.badge').first()).toHaveText('Not installed')

    // Forget cleans the row up again (and uninstalls nothing).
    await row.locator('.acts button', { hasText: 'Forget' }).click()
    await f.locator('#confirmOk').click()
    await expect(f.locator('.approw').filter({ hasText: name })).toHaveCount(0, {
      timeout: 60_000,
    })
  })
})
