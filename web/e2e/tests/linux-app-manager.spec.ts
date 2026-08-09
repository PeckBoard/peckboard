import {
  test,
  expect,
  type APIRequestContext,
  type FrameLocator,
  type Page,
} from '@playwright/test'

/**
 * UI e2e for the linux-app-manager plugin's App Manager page (sidebar →
 * App Manager): the page loads inside its sandboxed iframe, the target picker
 * is a real `<select>`, the distro banner reports what was detected on the
 * local host, the app grid shows installed state per app, and the add-remote-
 * target dialog picks an SSH key from a `<select>` (never free text) and
 * reports validation failures as prose.
 *
 * Deliberately does NOT install or remove anything: those run real
 * package-manager commands as root on whatever machine the suite runs on.
 * The install/remove job lifecycle is covered by the plugin's own vitest
 * suite (`peck-plugins/linux-app-manager/test/`).
 *
 * SKIPS when the plugin wasm isn't built — `playwright.config.ts` only stages
 * `peck-plugins/linux-app-manager/dist/plugin.wasm` if it exists (build it
 * with `peck-plugins/linux-app-manager/build.sh`, which needs `extism-js`).
 */

const PLUGIN = 'linux-app-manager'

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

test.describe('linux-app-manager page', () => {
  test('shows the target picker, distro banner and app grid for the local host', async ({
    request,
    page,
  }) => {
    const token = await authenticate(request)
    await loadApp(page, token)
    test.skip(!(await pluginPresent(page)), 'linux-app-manager wasm not built')

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
  })

  test('picks an SSH key from a dropdown and reports save failures as prose', async ({
    request,
    page,
  }) => {
    const token = await authenticate(request)
    await loadApp(page, token)
    test.skip(!(await pluginPresent(page)), 'linux-app-manager wasm not built')

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
})
