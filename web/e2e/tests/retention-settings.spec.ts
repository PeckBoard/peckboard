import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * E2E for the data-retention settings surface (`/api/settings/retention` +
 * the Server sub-page's "Data retention" section). Host-wide and
 * destructive in effect (the hourly sweeper reads it), so it's admin-gated
 * like the rest of the Server sub-page.
 */

const ADMIN_USER = 'e2e-user'
const ADMIN_PASS = 'e2e-password-1234'

let cachedAdmin: { token: string; auth: Record<string, string> } | null = null

async function authenticateAdmin(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  if (cachedAdmin) return cachedAdmin
  const res = await request.post('/api/auth/login', {
    data: { username: ADMIN_USER, password: ADMIN_PASS },
  })
  expect(res.ok(), `admin login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  cachedAdmin = { token, auth: { Authorization: `Bearer ${token}` } }
  return cachedAdmin
}

async function createNonAdmin(
  request: APIRequestContext,
  adminAuth: Record<string, string>,
  suffix: string,
): Promise<{ token: string; auth: Record<string, string> }> {
  const username = `retention-test-${suffix}-${Date.now()}`
  const password = 'gate-password-1234'
  const created = await request.post('/api/users', {
    headers: adminAuth,
    data: { username, password, role: 'user' },
  })
  expect(created.ok(), `create user failed: ${await created.text()}`).toBeTruthy()

  const res = await request.post('/api/auth/login', { data: { username, password } })
  expect(res.ok(), `login as ${username} failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function openSettings(page: Page, token: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto('/settings')
  await expect(page.getByTestId('settings-page')).toBeVisible({ timeout: 10_000 })
}

test('a non-admin is refused by the retention settings API', async ({ request }) => {
  const { auth: adminAuth } = await authenticateAdmin(request)
  const { auth } = await createNonAdmin(request, adminAuth, 'api')

  const read = await request.get('/api/settings/retention', { headers: auth })
  expect(read.status()).toBe(403)

  const write = await request.put('/api/settings/retention', {
    headers: auth,
    data: { repeating_session_max_age_days: 30 },
  })
  expect(write.status()).toBe(403)
})

test('an admin can read and persist retention settings via the API', async ({ request }) => {
  const { auth } = await authenticateAdmin(request)

  const put = await request.put('/api/settings/retention', {
    headers: auth,
    data: {
      repeating_session_max_age_days: 30,
      repeating_session_max_per_task: 5,
      event_max_age_days: 14,
      event_max_count_per_session: 500,
      report_max_age_days: 90,
      report_max_count: 200,
    },
  })
  expect(put.ok(), `PUT retention failed: ${await put.text()}`).toBeTruthy()

  const get = await request.get('/api/settings/retention', { headers: auth })
  expect(get.ok()).toBeTruthy()
  const body = await get.json()
  expect(body).toEqual({
    repeating_session_max_age_days: 30,
    repeating_session_max_per_task: 5,
    event_max_age_days: 14,
    event_max_count_per_session: 500,
    report_max_age_days: 90,
    report_max_count: 200,
  })

  // Reset to keep-forever defaults so this test doesn't leak state into
  // the sweeper for the rest of the suite.
  const reset = await request.put('/api/settings/retention', {
    headers: auth,
    data: {
      repeating_session_max_age_days: 0,
      repeating_session_max_per_task: 0,
      event_max_age_days: 0,
      event_max_count_per_session: 0,
      report_max_age_days: 0,
      report_max_count: 0,
    },
  })
  expect(reset.ok()).toBeTruthy()
})

test('an admin can edit and save retention settings from the Server sub-page', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticateAdmin(request)

  // Start from a known baseline so the UI assertion isn't order-dependent.
  await request.put('/api/settings/retention', {
    headers: auth,
    data: {
      repeating_session_max_age_days: 0,
      repeating_session_max_per_task: 0,
      event_max_age_days: 0,
      event_max_count_per_session: 0,
      report_max_age_days: 0,
      report_max_count: 0,
    },
  })

  await openSettings(page, token)
  const settings = page.getByTestId('settings-page')
  await settings.getByTestId('settings-nav-data').click()

  const section = settings.getByTestId('retention-settings-section')
  await expect(section).toBeVisible()

  const reportAgeInput = section.getByTestId('retention-input-report_max_age_days')
  await reportAgeInput.fill('45')
  await section.getByTestId('retention-settings-save').click()
  await expect(section).toContainText('Saved.')

  const get = await request.get('/api/settings/retention', { headers: auth })
  const body = await get.json()
  expect(body.report_max_age_days).toBe(45)

  // Reset so later specs see keep-forever defaults again.
  await request.put('/api/settings/retention', {
    headers: auth,
    data: { ...body, report_max_age_days: 0 },
  })
})
