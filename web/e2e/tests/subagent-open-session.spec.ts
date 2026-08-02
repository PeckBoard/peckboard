import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e test: the "Open session ↗" link on a spawn_subagent tool card
 * navigates in-app (pushState + synthetic popstate + openTab) instead of
 * triggering a full page reload, while modified clicks keep the browser's
 * native open-in-new-tab behaviour.
 *
 * Setup uses the `mock:subagent` scenario, which emits a spawn_subagent
 * ToolStart/ToolEnd pair whose `subagent_session_id` is taken from the user
 * message — so we point it at a real child session created up front.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  // The server auto-bootstraps the admin from PECKBOARD_BOOTSTRAP_*
  // env vars at first start (see playwright.config.ts); we just log in.
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

test('subagent "Open session" link navigates in-app without a reload', async ({
  request,
  page,
  context,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, auth } = await authenticate(request)

  // Folder must exist on disk and have a unique path (UNIQUE constraint).
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-subagent-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'e2e-subagent', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  // The child session the tool card's link points at.
  const childRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'subagent child', folder_id: folder.id },
  })
  expect(childRes.ok(), `create child session failed: ${await childRes.text()}`).toBeTruthy()
  const child = (await childRes.json()) as { id: string }

  // The parent session whose transcript shows the spawn_subagent card.
  const parentRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'subagent parent', folder_id: folder.id },
  })
  expect(parentRes.ok(), `create parent session failed: ${await parentRes.text()}`).toBeTruthy()
  const parent = (await parentRes.json()) as { id: string }

  // Drive the scripted scenario; the message text becomes the child id in
  // the tool card's output.
  const sendRes = await request.post(`/api/sessions/${parent.id}/message`, {
    headers: auth,
    data: { text: child.id, model: 'mock:subagent' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  await loadAt(page, token, `/sessions/${parent.id}`)

  const openLink = page.locator('.subagent-open')
  await expect(openLink).toBeVisible({ timeout: 15_000 })
  await expect(openLink).toHaveAttribute('href', `/sessions/${child.id}`)

  // Sentinel: survives pushState navigation, lost on a full reload.
  await page.evaluate(() => {
    ;(window as unknown as Record<string, unknown>).__peckboardNoReload = true
  })

  await openLink.click()

  await expect(page).toHaveURL(`/sessions/${child.id}`)
  await expect(page.locator('.tab.tab-active .tab-label')).toHaveText('subagent child', {
    timeout: 10_000,
  })
  expect(
    await page.evaluate(() => (window as unknown as Record<string, unknown>).__peckboardNoReload),
    'full page reload detected — SPA navigation regressed',
  ).toBe(true)

  // Modified click still opens a real browser tab via the untouched href.
  await page.goBack()
  await expect(page.locator('.subagent-open')).toBeVisible({ timeout: 10_000 })
  const [popup] = await Promise.all([
    context.waitForEvent('page'),
    page.locator('.subagent-open').click({ modifiers: ['ControlOrMeta'] }),
  ])
  await popup.waitForURL(`**/sessions/${child.id}`, { timeout: 10_000 })
  await popup.close()
})
