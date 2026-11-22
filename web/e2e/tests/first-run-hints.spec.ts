import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * First-run nudges:
 *
 * - An empty chat shows starter prompt cards. Clicking one fills and focuses
 *   the composer without sending; the cards go once a message is sent.
 * - Projects, Views and Skills each show a one-time hint that stays
 *   dismissed across reloads.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

test('starter prompts fill the composer and hide once a message is sent', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-starter-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'e2e-starter', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'starter prompts', folder_id: folder.id, model: 'mock:echo' },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  await loadAt(page, token, `/sessions/${session.id}`)

  const cards = page.getByTestId('starter-prompts')
  await expect(cards).toBeVisible({ timeout: 10_000 })
  const buttons = cards.locator('.starter-prompt')
  const count = await buttons.count()
  expect(count).toBeGreaterThanOrEqual(4)
  expect(count).toBeLessThanOrEqual(6)
  // Never a HomeKit prompt, whatever is installed.
  await expect(cards).not.toContainText(/homekit/i)

  await cards.getByTestId('starter-prompt-quick-mockup').click()
  const composer = page.locator('.input-textarea')
  await expect(composer).toBeFocused()
  await expect(composer).toHaveValue(/HTML mockup/)
  // Filling is not sending: still no messages, cards still offered.
  await expect(cards).toBeVisible()

  await composer.fill('hello from the starter test')
  await composer.press('Enter')
  await expect(page.getByTestId('starter-prompts')).toHaveCount(0, { timeout: 10_000 })
})

test('starter prompts fit a phone-width screen', async ({ request, page }) => {
  await page.setViewportSize({ width: 375, height: 740 })
  const { token, auth } = await authenticate(request)
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-starter-m-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'e2e-starter-m', path: folderPath },
  })
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'starter mobile', folder_id: folder.id, model: 'mock:echo' },
  })
  const session = (await sessionRes.json()) as { id: string }

  await loadAt(page, token, `/sessions/${session.id}`)
  const first = page.getByTestId('starter-prompt-explain-project')
  await expect(first).toBeVisible({ timeout: 10_000 })
  const box = await first.boundingBox()
  expect(box).not.toBeNull()
  expect(box!.x).toBeGreaterThanOrEqual(0)
  expect(box!.x + box!.width).toBeLessThanOrEqual(375)
})

for (const { key, route } of [
  { key: 'projects', route: '/projects' },
  { key: 'views', route: '/views' },
  { key: 'skills', route: '/settings/skills' },
]) {
  test(`first-visit hint on ${key} dismisses for good`, async ({ request, page }) => {
    const { token } = await authenticate(request)
    await loadAt(page, token, route)

    const hint = page.getByTestId(`first-visit-hint-${key}`)
    await expect(hint).toBeVisible({ timeout: 10_000 })
    await page.getByTestId(`first-visit-hint-dismiss-${key}`).click()
    await expect(hint).toHaveCount(0)
    // The hint waits for the signed-in user, so wait for /me before
    // asserting it stayed away — otherwise the check passes vacuously.
    const me = page.waitForResponse((r) => r.url().includes('/api/auth/me'))
    await page.reload()
    await me
    await page.waitForTimeout(500)
    await expect(page.getByTestId(`first-visit-hint-${key}`)).toHaveCount(0)
    await expect(page.getByTestId(`first-visit-hint-${key}`)).toHaveCount(0)
  })
}
