import { test, expect, type APIRequestContext, type Page } from '../harness'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

// Screenshots for a visual check of the section (light, dark, narrow).
// Written next to the repo; not asserted on.
const SHOTS = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  '../../../.tmp-retention-shots',
)

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

// Must stay first: the tests below overwrite the stored limits.
test('a new install starts with the recommended file cleanup limits', async ({ request }) => {
  const { auth } = await authenticateAdmin(request)
  const get = await request.get('/api/settings/retention', { headers: auth })
  expect(get.ok()).toBeTruthy()
  expect(await get.json()).toMatchObject({
    browser_run_max_age_days: 7,
    tool_image_max_age_days: 30,
    shared_file_max_age_days: 30,
    transcript_max_age_days: 30,
    // Limits on sessions, events and reports stay off.
    event_max_age_days: 0,
    report_max_age_days: 0,
  })
})
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
  // toMatchObject: the file-cleanup limits not sent above come back as 0.
  expect(body.transcript_max_age_days).toBe(0)
  expect(body).toMatchObject({
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

test('an admin can preview the file cleanup without saving or deleting anything', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticateAdmin(request)

  // All limits off, like an install that existed before the limits did.
  const off = await request.put('/api/settings/retention', { headers: auth, data: {} })
  expect(off.ok()).toBeTruthy()

  // An out-of-range limit is refused before anything is measured.
  const bad = await request.post('/api/settings/retention/preview', {
    headers: auth,
    data: { browser_run_max_age_days: 99_999 },
  })
  expect(bad.status()).toBe(400)

  await openSettings(page, token)
  const settings = page.getByTestId('settings-page')
  await settings.getByTestId('settings-nav-data').click()
  const section = settings.getByTestId('retention-settings-section')

  await section.getByTestId('retention-input-browser_run_max_age_days').fill('30')
  await section.getByTestId('retention-settings-preview').click()
  const preview = section.getByTestId('retention-preview')
  // A fresh server has nothing 30 days old; a limit left at 0 says so.
  await expect(preview).toContainText('Browser runs: nothing is old enough yet.')
  await expect(preview).toContainText('Tool images: kept forever.')
  await expect(preview).toContainText('Nothing has been deleted.')

  for (const scheme of ['light', 'dark'] as const) {
    await page.emulateMedia({ colorScheme: scheme })
    await section.screenshot({ path: path.join(SHOTS, `preview-${scheme}.png`) })
  }
  await page.setViewportSize({ width: 380, height: 800 })
  await section.screenshot({ path: path.join(SHOTS, 'preview-dark-380.png') })
  await page.emulateMedia({ colorScheme: null })
  await page.setViewportSize({ width: 1280, height: 720 })

  // Preview is not Save: the stored limit is unchanged.
  const get = await request.get('/api/settings/retention', { headers: auth })
  expect((await get.json()).browser_run_max_age_days).toBe(0)

  // Editing a limit drops the now-stale preview.
  await section.getByTestId('retention-input-browser_run_max_age_days').fill('31')
  await expect(preview).toHaveCount(0)

  // The size readout lists every kind of file, and the database.
  const usage = section.getByTestId('retention-usage')
  await expect(usage.getByTestId('retention-usage-shared_files')).toContainText('Shared files:')
  await expect(usage.getByTestId('retention-usage-database')).toContainText('Database')

  // With every file limit off, a notice offers the recommended limits.
  // Taking them fills the fields and previews; it does not save.
  const notice = section.getByTestId('retention-off-notice')
  await expect(notice).toHaveCount(0)
  await section.getByTestId('retention-input-browser_run_max_age_days').fill('0')
  await expect(notice).toContainText('kept forever')
  // Only the file limits are filled in; a limit set elsewhere stays.
  await section.getByTestId('retention-input-report_max_age_days').fill('12')
  await section.getByTestId('retention-use-recommended').click()
  await expect(section.getByTestId('retention-input-report_max_age_days')).toHaveValue('12')
  await expect(section.getByTestId('retention-input-browser_run_max_age_days')).toHaveValue('7')
  await expect(section.getByTestId('retention-input-shared_file_max_age_days')).toHaveValue('30')
  await expect(preview).toContainText('Shared files: nothing is old enough yet.')
  await expect(notice).toHaveCount(0)
  const after = await request.get('/api/settings/retention', { headers: auth })
  expect((await after.json()).shared_file_max_age_days).toBe(0)
})
