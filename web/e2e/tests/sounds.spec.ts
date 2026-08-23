import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
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
    ;(window as unknown as { __pbSounds?: string[] }).__pbSounds = []
    window.addEventListener('peckboard:sound', ((e: CustomEvent<{ kind: string }>) => {
      const w = window as unknown as { __pbSounds?: string[] }
      w.__pbSounds = w.__pbSounds ?? []
      w.__pbSounds.push(e.detail.kind)
    }) as EventListener)
  }, token)
  await page.goto(route)
}

async function played(page: Page): Promise<string[]> {
  return page.evaluate(() => (window as unknown as { __pbSounds?: string[] }).__pbSounds ?? [])
}

test('Settings Sounds page: attention on, frequent off; persist; preview fires', async ({
  request,
  page,
}) => {
  const { token } = await authenticate(request)
  await loadAt(page, token, '/settings/sounds')
  await expect(page.getByTestId('settings-page')).toBeVisible({ timeout: 10_000 })
  await expect(page.getByTestId('sounds-master-section')).toBeVisible()
  await expect(page.getByTestId('sounds-interface-section')).toBeVisible()
  await expect(page.getByTestId('sounds-events-section')).toBeVisible()

  await expect(page.getByTestId('sounds-master')).toBeChecked()
  await expect(page.getByTestId('sounds-toggle-uiClick')).toBeChecked()
  await expect(page.getByTestId('sounds-toggle-error')).toBeChecked()
  await expect(page.getByTestId('sounds-toggle-question')).toBeChecked()
  await expect(page.getByTestId('sounds-toggle-runComplete')).toBeChecked()
  await expect(page.getByTestId('sounds-toggle-accountLimit')).toBeChecked()
  await expect(page.getByTestId('sounds-toggle-runStart')).not.toBeChecked()
  await expect(page.getByTestId('sounds-toggle-toolUsed')).not.toBeChecked()
  await expect(page.getByTestId('sounds-toggle-messageSent')).not.toBeChecked()
  await expect(page.getByTestId('sounds-toggle-queueProcessed')).not.toBeChecked()

  await page.getByTestId('sounds-toggle-messageSent').check()
  await page.getByTestId('sounds-preview-question').click()
  await expect.poll(async () => played(page)).toContain('question')

  await page.reload()
  await expect(page.getByTestId('sounds-toggle-messageSent')).toBeChecked({ timeout: 10_000 })
  await expect(page.getByTestId('sounds-toggle-toolUsed')).not.toBeChecked()
})

test('a finished mock run plays the run-complete chime', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-sounds-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-sounds-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), await folderRes.text()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'sounds run', folder_id: folder.id, model: 'mock:happy-path' },
  })
  expect(sessionRes.ok(), await sessionRes.text()).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  await loadAt(page, token, `/sessions/${session.id}`)
  await expect(page.locator('.send-btn')).toBeVisible({ timeout: 10_000 })

  await page.locator('.input-textarea').fill('go')
  await page.locator('.send-btn').click()

  await expect.poll(async () => played(page), { timeout: 15_000 }).toContain('runComplete')
  const kinds = await played(page)
  expect(kinds).not.toContain('toolUsed')
  expect(kinds).not.toContain('runStart')
})

test('button and menu clicks play uiClick; a form error plays error', async ({ request, page }) => {
  const { token } = await authenticate(request)
  await loadAt(page, token, '/')
  await expect(page.locator('.tab-new')).toBeVisible({ timeout: 10_000 })

  await page.locator('.tab-new').click()
  await expect.poll(async () => played(page)).toContain('uiClick')

  await expect(page.getByTestId('new-session-model')).toBeVisible()
  await page.getByTestId('new-session-model').click()
  const item = page.locator('.dropdown-item:not([disabled])').first()
  await expect(item).toBeVisible({ timeout: 10_000 })
  await item.click()
  await expect.poll(async () => played(page)).toContain('uiClick')

  await page.goto('/users')
  await expect(page.getByRole('button', { name: 'Create User' })).toBeVisible({ timeout: 10_000 })
  await page.getByRole('button', { name: 'Create User' }).click()
  await page.locator('#new-user-username').fill('tester-sounds')
  await page.locator('#new-user-password').fill('abc')
  await expect(page.getByTestId('new-user-password-error')).toBeVisible()
  await expect.poll(async () => played(page)).toContain('error')
})

test('a crashed mock run plays the error chime', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-sounds-crash-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-sounds-crash-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), await folderRes.text()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'sounds crash', folder_id: folder.id, model: 'mock:crash' },
  })
  expect(sessionRes.ok(), await sessionRes.text()).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  await loadAt(page, token, `/sessions/${session.id}`)
  await expect(page.locator('.send-btn')).toBeVisible({ timeout: 10_000 })

  await page.locator('.input-textarea').fill('crash please')
  await page.locator('.send-btn').click()

  await expect.poll(async () => played(page), { timeout: 15_000 }).toContain('error')
  const kinds = await played(page)
  expect(kinds).not.toContain('runComplete')
})
