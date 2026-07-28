import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Validation feedback has to name the thing that's actually wrong.
 *
 * Three defects motivated this spec:
 *  - Create User answered a short password with "username required,
 *    password min 12 chars", accusing a field the user filled in.
 *  - The repeating-task interval field silently clamped 0 up to 1, so a
 *    user who typed 0 meaning "don't repeat" got a task running every minute.
 *  - New Project disabled Create with no stated reason for a negative
 *    worker count.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

let cachedAuth: { token: string; auth: Record<string, string> } | null = null

/** Authenticate once per spec file — the per-IP login limiter sees the
 *  whole suite as one client. */
async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  if (cachedAuth) return cachedAuth
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  cachedAuth = { token, auth: { Authorization: `Bearer ${token}` } }
  return cachedAuth
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

test('Create User blames the password alone when only the password is short', async ({
  request,
  page,
}) => {
  const { token } = await authenticate(request)
  await loadAt(page, token, '/users')

  await page.getByRole('button', { name: 'Create User' }).click()
  await page.locator('#new-user-username').fill('tester2')
  await page.locator('#new-user-password').fill('abc')

  // Live, before submit: the password is called out, the username is not.
  const passwordError = page.getByTestId('new-user-password-error')
  await expect(passwordError).toHaveText('Password must be at least 12 characters')
  await expect(page.getByTestId('new-user-username-error')).toHaveCount(0)

  await page.getByRole('button', { name: 'Create', exact: true }).click()

  // Still only the password, and no page-wide banner naming the username.
  await expect(passwordError).toBeVisible()
  await expect(page.getByTestId('new-user-username-error')).toHaveCount(0)
  await expect(page.locator('.form-error')).toHaveCount(0)

  // A long enough password clears the message and the form submits.
  await page.locator('#new-user-password').fill('correct-horse-battery')
  await expect(page.getByTestId('new-user-password-error')).toHaveCount(0)
})

test('repeating task keeps a typed interval of 0 and refuses to submit it', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-interval-zero-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-interval-zero-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()

  await loadAt(page, token, '/repeating-tasks')
  await page.getByRole('button', { name: /new task/i }).click()
  await expect(page.getByRole('heading', { name: /new repeating task/i })).toBeVisible()

  await page.getByPlaceholder(/daily project sweep/i).fill('interval zero task')
  await page.getByPlaceholder(/message sent to the new session/i).fill('go')

  const minutes = page.locator('#schedule-minutes')
  await minutes.fill('0')

  // The typed value survives — the old editor rewrote it to 1 on the spot.
  await expect(minutes).toHaveValue('0')
  await expect(page.getByTestId('schedule-minutes-error')).toHaveText(
    'Every (minutes) must be at least 1',
  )

  const submit = page.getByRole('button', { name: /create task/i })
  await expect(submit).toBeDisabled()
  await expect(page.getByTestId('repeating-task-disabled-reason')).toHaveText(
    'Every (minutes) must be at least 1',
  )

  // A legal interval unblocks it.
  await minutes.fill('5')
  await expect(page.getByTestId('schedule-minutes-error')).toHaveCount(0)
  await expect(submit).toBeEnabled()
})

test('New Project states why Create is blocked', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-blocked-reason-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-blocked-reason-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()

  await loadAt(page, token, '/projects')
  await page.getByRole('button', { name: '+ New project' }).click()
  await expect(page.getByRole('heading', { name: 'New Project' })).toBeVisible()

  const submit = page.getByRole('button', { name: 'Create Project' })
  const reason = page.getByTestId('new-project-disabled-reason')

  await expect(reason).toHaveText('Enter a name')
  await page.getByPlaceholder('My project').fill('e2e blocked reason')

  // Name + folder filled, workflow missing: the form looks complete, so the
  // reason has to say what's left.
  await expect(submit).toBeDisabled()
  await expect(reason).toHaveText('Pick a workflow')

  await page.locator('.workflow-select-trigger').click()
  await page.getByRole('menuitem', { name: /Fast Develop Software/ }).click()
  await expect(submit).toBeEnabled()
  await expect(reason).toHaveCount(0)

  // A negative worker count blocks Create *and* says so, inline and next
  // to the button.
  await page.getByRole('button', { name: /advanced settings/i }).click()
  await page.locator('#new-project-worker-count').fill('-5')
  await expect(submit).toBeDisabled()
  await expect(reason).toHaveText('Worker count must be between 1 and 10')
  await expect(page.getByTestId('new-project-worker-count-error')).toHaveText(
    'Worker count must be between 1 and 10',
  )

  await page.locator('#new-project-worker-count').fill('2')
  await expect(page.getByTestId('new-project-worker-count-error')).toHaveCount(0)
  await expect(submit).toBeEnabled()
})
