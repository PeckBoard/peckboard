import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Timed unlock of encrypted environment variables.
 *
 * Settings → Environment Variables grows an unlock panel: password + a
 * window (15m … 24h, or "Until I lock"). While the window is open the
 * server keeps the decrypted values cached, so session dispatch finds them
 * warm and the `EnvUnlockDialog` never appears. Locking ends the window and
 * the next session prompts again — and answering THAT prompt opens a window
 * of its own.
 *
 * Cleanup matters here: a leftover encrypted var makes every later spec's
 * interactive dispatch block on an unanswered prompt, so each test wipes the
 * env vars and locks on the way out.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const VAR_NAME = 'E2E_UNLOCK_PAT'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

async function wipeEnvVars(request: APIRequestContext, auth: Record<string, string>) {
  const res = await request.get('/api/env-vars', { headers: auth })
  expect(res.ok()).toBeTruthy()
  const { vars } = (await res.json()) as { vars: Array<{ id: string }> }
  for (const v of vars) {
    const del = await request.delete(`/api/env-vars/${encodeURIComponent(v.id)}`, { headers: auth })
    expect(del.ok(), `wipe env var ${v.id} failed`).toBeTruthy()
  }
}

async function lockNow(request: APIRequestContext, auth: Record<string, string>) {
  const res = await request.post('/api/env-vars/lock', { headers: auth, data: {} })
  expect(res.ok(), `lock failed: ${await res.text()}`).toBeTruthy()
}

/** A global var sealed with the caller's login password. */
async function createEncryptedVar(request: APIRequestContext, auth: Record<string, string>) {
  const res = await request.post('/api/env-vars', {
    headers: auth,
    data: {
      name: VAR_NAME,
      value: 'ghp-e2e-token',
      encrypt: true,
      password: E2E_PASS,
      folder_id: null,
    },
  })
  expect(res.ok(), `create encrypted var failed: ${await res.text()}`).toBeTruthy()
}

/** A folder + session to dispatch a `mock:*` turn into. */
async function createSession(
  request: APIRequestContext,
  auth: Record<string, string>,
  name: string,
): Promise<string> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-unlock-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: path.basename(folderPath), path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name, folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  return ((await sessionRes.json()) as { id: string }).id
}

async function openSettings(page: Page, token: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/settings')
  await page.getByTestId('settings-nav-variables').click()
}

test.afterEach(async ({ request }) => {
  const token = await authenticate(request)
  const auth = { Authorization: `Bearer ${token}` }
  await wipeEnvVars(request, auth)
  await lockNow(request, auth)
})

test('unlocking for a window stops sessions prompting for the password', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const auth = { Authorization: `Bearer ${token}` }
  await wipeEnvVars(request, auth)
  await lockNow(request, auth)
  await createEncryptedVar(request, auth)

  await openSettings(page, token)
  const panel = page.getByTestId('env-unlock-panel')
  await expect(panel).toBeVisible({ timeout: 10_000 })
  const status = page.getByTestId('env-unlock-status')
  await expect(status).toContainText('Locked')

  // A wrong password is rejected and leaves the window shut.
  await page.getByTestId('env-unlock-password').fill('not-my-password')
  await page.getByTestId('env-unlock-btn').click()
  await expect(panel.getByText('Wrong password')).toBeVisible()
  await expect(status).toContainText('Locked')

  // Unlock for 15 minutes.
  await page.getByTestId('env-unlock-password').fill(E2E_PASS)
  await page.getByTestId('env-unlock-duration-select').selectOption('15m')
  await page.getByTestId('env-unlock-btn').click()
  await expect(status).toContainText(/Unlocked — 1[45]m left/)

  // A session dispatched inside the window uses the unlocked values: the
  // turn runs to completion and no dialog is ever raised.
  const sessionId = await createSession(request, auth, 'unlock-window-warm')
  const send = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth,
    data: { text: 'go', model: 'mock:happy-path' },
  })
  expect(send.ok(), `send message failed: ${await send.text()}`).toBeTruthy()
  await expect(page.getByTestId('env-unlock-dialog')).toHaveCount(0)
  await expect(status).toContainText('Unlocked')
})

test('locking brings the prompt back, and answering it opens a new window', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const auth = { Authorization: `Bearer ${token}` }
  await wipeEnvVars(request, auth)
  await createEncryptedVar(request, auth)

  await openSettings(page, token)
  const status = page.getByTestId('env-unlock-status')
  await expect(status).toBeVisible({ timeout: 10_000 })

  // Unlock, then lock again from the panel: the window closes early.
  await page.getByTestId('env-unlock-password').fill(E2E_PASS)
  await page.getByTestId('env-unlock-duration-select').selectOption('1h')
  await page.getByTestId('env-unlock-btn').click()
  await expect(status).toContainText('Unlocked')
  await page.getByTestId('env-vars-lock-btn').click()
  await expect(status).toContainText('Locked')

  // With the cache cold, dispatch blocks on the prompt — don't await the
  // send until the dialog has been answered.
  const sessionId = await createSession(request, auth, 'unlock-window-cold')
  const sendPromise = request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth,
    data: { text: 'go', model: 'mock:happy-path' },
    timeout: 60_000,
  })

  const dialog = page.getByTestId('env-unlock-dialog')
  await expect(dialog).toBeVisible({ timeout: 20_000 })
  await expect(dialog).toContainText(VAR_NAME)
  await dialog.getByTestId('env-unlock-duration').selectOption('4h')
  await dialog.getByTestId('env-unlock-input').fill(E2E_PASS)
  await dialog.getByTestId('env-unlock-submit').click()
  await expect(dialog).toHaveCount(0)

  const send = await sendPromise
  expect(send.ok(), `send message failed: ${await send.text()}`).toBeTruthy()

  // Answering the prompt opened the window the dialog asked for.
  await expect(status).toContainText(/Unlocked — [34]h/)
})
