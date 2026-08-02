import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Sessions sidebar search box: types into `[data-testid="session-filter"]`
 * and asserts the list narrows to server-matched results, including
 * sessions the client never fetched via the initial paginated page (the
 * whole point of filtering server-side rather than client-side).
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

async function uniqueFolder(request: APIRequestContext, auth: Record<string, string>) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-session-search-'))
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name: path.basename(folderPath), path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as { id: string }
}

test('sessions sidebar search narrows the list and matches sessions outside the loaded page', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const folder = await uniqueFolder(request, auth)

  // Seed one uniquely-named session, then pad with enough others that
  // a plain client-side filter over "sessions currently in the store"
  // would miss it if the server-side page size were small. The e2e
  // fixture keeps the default page size (100), so we don't need to
  // seed past that to prove the point — the API round trip itself is
  // what's under test.
  const uniqueName = `zzz-unique-target-${Date.now()}`
  const target = await request.post('/api/sessions', {
    headers: auth,
    data: { name: uniqueName, folder_id: folder.id },
  })
  expect(target.ok()).toBeTruthy()

  for (let i = 0; i < 5; i++) {
    const res = await request.post('/api/sessions', {
      headers: auth,
      data: { name: `other-session-${i}`, folder_id: folder.id },
    })
    expect(res.ok()).toBeTruthy()
  }

  await loadAt(page, token, '/')
  await expect(page.getByTestId('session-filter')).toBeVisible({ timeout: 10_000 })

  await expect(page.locator('.list-view-name', { hasText: 'other-session-0' })).toBeVisible()

  await page.getByTestId('session-filter').fill('zzz-unique-target')

  // Debounce is 250ms; wait for the match to land and the non-matching
  // rows to disappear.
  await expect(page.locator('.list-view-name', { hasText: uniqueName })).toBeVisible({
    timeout: 5_000,
  })
  await expect(page.locator('.list-view-name', { hasText: 'other-session-0' })).toHaveCount(0)

  // Clearing the box restores the unfiltered paginated list.
  await page.getByTestId('session-filter').fill('')
  await expect(page.locator('.list-view-name', { hasText: 'other-session-0' })).toBeVisible({
    timeout: 5_000,
  })
})

test('sessions sidebar search shows an empty state for no matches', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const folder = await uniqueFolder(request, auth)
  const res = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'has-a-name', folder_id: folder.id },
  })
  expect(res.ok()).toBeTruthy()

  await loadAt(page, token, '/')
  await expect(page.getByTestId('session-filter')).toBeVisible({ timeout: 10_000 })
  await page.getByTestId('session-filter').fill('no-such-session-exists-xyz')

  await expect(page.getByTestId('session-filter-empty')).toBeVisible({ timeout: 5_000 })
})
