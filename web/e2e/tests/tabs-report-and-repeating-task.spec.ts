import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * E2E for the report + repeating-task tab kinds.
 *
 * The cross-device tab strip is now kind-agnostic — sessions, projects,
 * reports and repeating tasks all flow through one persistent backend
 * (`/api/me/tabs`) and one frontend `TabKindRegistry`. These tests pin
 * the new kinds end-to-end:
 *
 *   1. Opening a repeating task from the list view writes a tab,
 *      sticks the chip on the strip, marks it active, and persists
 *      across a reload.
 *   2. Deleting that task removes the chip — both locally and on the
 *      server — so a cross-device sync (another browser refresh)
 *      can't resurrect it.
 *   3. Opening a report from the index navigates to the report view,
 *      writes a tab, and the chip is labelled with the report's
 *      frontmatter title (not the file name).
 *   4. Reload restores the report tab and keeps it active.
 *   5. The tab strip persists a mixed set: a session tab beside a
 *      repeating-task tab and a report tab, all on the strip in MRU
 *      order with the active one highlighted.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

let cachedAuth: { token: string; auth: Record<string, string> } | null = null

async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  if (cachedAuth) return cachedAuth
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok()).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  cachedAuth = { token, auth: { Authorization: `Bearer ${token}` } }
  return cachedAuth
}

async function clearTabs(request: APIRequestContext, auth: Record<string, string>) {
  const res = await request.get('/api/me/tabs', { headers: auth })
  if (!res.ok()) return
  const tabs = (await res.json()) as Array<{ item_type: string; item_id: string }>
  for (const t of tabs) {
    await request.delete(`/api/me/tabs/${t.item_type}/${encodeURIComponent(t.item_id)}`, {
      headers: auth,
    })
  }
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })
}

async function seedFolder(
  request: APIRequestContext,
  auth: Record<string, string>,
  prefix: string,
): Promise<string> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-${prefix}-`))
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-${prefix}`, path: folderPath },
  })
  expect(res.ok()).toBeTruthy()
  const folder = (await res.json()) as { id: string }
  return folder.id
}

async function seedRepeatingTask(
  request: APIRequestContext,
  auth: Record<string, string>,
  name: string,
): Promise<string> {
  const folderId = await seedFolder(request, auth, 'rt-tabs')
  const res = await request.post('/api/repeating-tasks', {
    headers: { ...auth, 'Content-Type': 'application/json' },
    data: {
      name,
      description: '',
      folder_id: folderId,
      prompt: 'do the thing',
      schedule_kind: 'interval',
      schedule_value: { minutes: 60 },
    },
  })
  expect(res.ok()).toBeTruthy()
  const task = (await res.json()) as { id: string }
  return task.id
}

/** Seed a session for the cross-kind mixing test. Repeats the helper
 *  in tabs.spec.ts so this file stays self-contained. */
async function seedSession(
  request: APIRequestContext,
  auth: Record<string, string>,
  name: string,
): Promise<string> {
  const folderId = await seedFolder(request, auth, 'session-tabs')
  const res = await request.post('/api/sessions', {
    headers: auth,
    data: { name, folder_id: folderId },
  })
  expect(res.ok()).toBeTruthy()
  const session = (await res.json()) as { id: string }
  return session.id
}

/** Write a markdown report directly into the server's reports/<folder>
 *  directory. The PECKBOARD_E2E_DATA_DIR env var is set by
 *  playwright.config.ts (one dir per run, exported into the spec
 *  process), and the binary serves whatever it finds on disk under
 *  `/api/reports`. Using the filesystem directly is the simplest way to
 *  seed report content from a test — the only HTTP endpoint is a PUT
 *  that requires the file to already exist. */
function writeReportFile(folder: string, file: string, frontmatter: string, body: string): string {
  const dataDir = process.env.PECKBOARD_E2E_DATA_DIR
  if (!dataDir) {
    throw new Error('PECKBOARD_E2E_DATA_DIR must be set (see playwright.config.ts)')
  }
  const dir = path.join(dataDir, 'reports', folder)
  mkdirSync(dir, { recursive: true })
  const filePath = path.join(dir, file)
  writeFileSync(filePath, `---\n${frontmatter}\n---\n\n${body}\n`)
  return filePath
}

test.describe('tabs for reports and repeating tasks', () => {
  test('opening a repeating task adds a tab and persists across reload', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)
    const taskId = await seedRepeatingTask(request, auth, 'tab-task-alpha')

    // Land on the task detail view; the openTab effect fires when the
    // active id is set, regardless of how the user got here.
    await loadAt(page, token, `/repeating-tasks/${taskId}`)

    const tab = page.locator('.tab-opened', { hasText: 'tab-task-alpha' })
    await expect(tab).toBeVisible({ timeout: 5_000 })
    await expect(tab).toHaveClass(/tab-active/)

    // The kind-specific icon distinguishes the chip from a session.
    await expect(tab.locator('.tab-icon-repeating-task')).toBeVisible()

    // Survives a hard reload.
    await page.reload()
    await expect(page.locator('.tab-opened', { hasText: 'tab-task-alpha' })).toBeVisible({
      timeout: 5_000,
    })

    // Server agrees: GET /api/me/tabs lists the same row.
    const listed = await request.get('/api/me/tabs', { headers: auth })
    expect(listed.ok()).toBeTruthy()
    const tabs = (await listed.json()) as Array<{
      item_type: string
      item_id: string
      name: string
    }>
    expect(tabs.find((t) => t.item_type === 'repeating_task' && t.item_id === taskId)?.name).toBe(
      'tab-task-alpha',
    )
  })

  test('deleting a repeating task removes the tab (no orphan chip)', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)
    const taskId = await seedRepeatingTask(request, auth, 'tab-task-doomed')

    await loadAt(page, token, `/repeating-tasks/${taskId}`)
    const tab = page.locator('.tab-opened', { hasText: 'tab-task-doomed' })
    await expect(tab).toBeVisible()

    // Right-click the tab → Delete task → confirm. Same affordance as
    // the session/project tabs use.
    await tab.click({ button: 'right' })
    const deleteBtn = page.locator('.context-menu button', { hasText: 'Delete task' })
    await expect(deleteBtn).toBeVisible()
    await deleteBtn.click()

    const confirmBtn = page.locator('.confirm-dialog-danger', { hasText: /^Delete$/ })
    await expect(confirmBtn).toBeVisible()
    await confirmBtn.click()

    // Chip is gone — both the labelled one and a generic "Task"
    // placeholder (the fallback if the server name lookup misses).
    await expect(page.locator('.tab-opened', { hasText: 'tab-task-doomed' })).toHaveCount(0)
    await expect(page.locator('.tab-opened', { hasText: /^Task$/ })).toHaveCount(0)

    // Server-side: both the task row and its user_tabs row must be
    // gone. This is the cross-device guarantee — refetching from
    // another browser must not resurrect either.
    await page.waitForTimeout(200)
    const tabsRes = await request.get('/api/me/tabs', { headers: auth })
    expect(tabsRes.ok()).toBeTruthy()
    const tabs = (await tabsRes.json()) as Array<{ item_id: string }>
    expect(tabs.find((t) => t.item_id === taskId)).toBeUndefined()

    const taskRes = await request.get(`/api/repeating-tasks/${taskId}`, { headers: auth })
    expect(taskRes.status()).toBe(404)
  })

  test('opening a report adds a tab labelled with the frontmatter title', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)

    // Seed a report file directly on the server's disk. The
    // frontmatter title is what should label the tab chip — the file
    // name is unfriendly.
    const folder = '2026-06-11'
    const file = 'tab-report-alpha.md'
    const reportPath = writeReportFile(
      folder,
      file,
      'title: "Friendly Report Title"\ndate: "2026-06-11"',
      '# Hello\n\nbody text',
    )

    try {
      await loadAt(page, token, `/reports/${folder}/${file}`)

      // The viewer renders with the seeded title in the H2, AND the
      // tab chip is labelled with the same frontmatter title.
      await expect(page.locator('.report-viewer-title')).toHaveText('Friendly Report Title')

      const tab = page.locator('.tab-opened', { hasText: 'Friendly Report Title' })
      await expect(tab).toBeVisible({ timeout: 5_000 })
      await expect(tab).toHaveClass(/tab-active/)
      await expect(tab.locator('.tab-icon-report')).toBeVisible()

      // Reload restores the tab — same cross-device guarantee as
      // session/project tabs.
      await page.reload()
      await expect(page.locator('.tab-opened', { hasText: 'Friendly Report Title' })).toBeVisible({
        timeout: 5_000,
      })

      // Server-side row uses the `<folder>/<file>` encoding so both
      // surfaces (URL and tab) share one identifier.
      const listed = await request.get('/api/me/tabs', { headers: auth })
      const tabs = (await listed.json()) as Array<{
        item_type: string
        item_id: string
        name: string
      }>
      const row = tabs.find((t) => t.item_type === 'report' && t.item_id === `${folder}/${file}`)
      expect(row?.name).toBe('Friendly Report Title')
    } finally {
      rmSync(reportPath, { force: true })
    }
  })

  test('closing a report tab sticks — the close DELETE reaches the server row', async ({
    request,
    page,
    baseURL,
  }) => {
    // Regression test: the close DELETE used to send the report id
    // (`<folder>/<file>`) unencoded, so the extra `/` added a path
    // segment, `/api/me/tabs/{item_type}/{item_id}` never matched, the
    // server kept the row, and the "closed" chip resurrected on the
    // next refetch or reload. Closing must stick until the user opens
    // the report again — and opening it again must store a fresh tab.
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)

    const folder = '2026-06-11'
    const file = 'tab-report-closeable.md'
    const reportPath = writeReportFile(folder, file, 'title: "Closeable Report"', '# close me\n')

    try {
      await loadAt(page, token, `/reports/${folder}/${file}`)
      const tab = page.locator('.tab-opened', { hasText: 'Closeable Report' })
      await expect(tab).toBeVisible({ timeout: 5_000 })

      await tab.click({ button: 'right' })
      const closeBtn = page.locator('.context-menu button', { hasText: 'Close tab' })
      await expect(closeBtn).toBeVisible()
      await closeBtn.click()
      await expect(tab).toHaveCount(0)

      // Server-side the row must be gone — this is what used to fail.
      await page.waitForTimeout(200)
      const listed = await request.get('/api/me/tabs', { headers: auth })
      expect(listed.ok()).toBeTruthy()
      const tabs = (await listed.json()) as Array<{ item_type: string; item_id: string }>
      expect(tabs.find((t) => t.item_type === 'report')).toBeUndefined()

      // A reload must not resurrect the chip…
      await page.reload()
      await expect(page.locator('.rail')).toBeVisible({ timeout: 10_000 })
      await expect(page.locator('.tab-opened', { hasText: 'Closeable Report' })).toHaveCount(0)

      // …but deliberately opening the report again stores it again.
      await page.evaluate(
        ([f, fl]) => {
          history.pushState(null, '', `/reports/${f}/${fl}`)
          window.dispatchEvent(new PopStateEvent('popstate'))
        },
        [folder, file],
      )
      await expect(page.locator('.tab-opened', { hasText: 'Closeable Report' })).toBeVisible({
        timeout: 5_000,
      })
    } finally {
      rmSync(reportPath, { force: true })
    }
  })

  test('mixed-kind tab strip: session + repeating task + report all coexist', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token, auth } = await authenticate(request)
    await clearTabs(request, auth)

    const sessionId = await seedSession(request, auth, 'mixed-session')
    const taskId = await seedRepeatingTask(request, auth, 'mixed-task')
    const folder = '2026-06-11'
    const file = 'mixed-report.md'
    const reportPath = writeReportFile(folder, file, 'title: "Mixed Report"', '# mixed\n')

    try {
      await loadAt(page, token, `/sessions/${sessionId}`)
      await expect(page.locator('.tab-opened', { hasText: 'mixed-session' })).toHaveClass(
        /tab-active/,
      )

      // Open the task — same SPA push trick the tabs.spec.ts uses to
      // avoid a full reload between navigations.
      await page.evaluate((id) => {
        history.pushState(null, '', `/repeating-tasks/${id}`)
        window.dispatchEvent(new PopStateEvent('popstate'))
      }, taskId)
      await expect(page.locator('.tab-opened', { hasText: 'mixed-task' })).toBeVisible({
        timeout: 5_000,
      })

      // Open the report.
      await page.evaluate(
        ([f, fl]) => {
          history.pushState(null, '', `/reports/${f}/${fl}`)
          window.dispatchEvent(new PopStateEvent('popstate'))
        },
        [folder, file],
      )
      await expect(page.locator('.tab-opened', { hasText: 'Mixed Report' })).toBeVisible({
        timeout: 5_000,
      })

      // All three chips visible; the most-recent (report) is active.
      const allTabs = page.locator('.tab-opened')
      await expect(allTabs).toHaveCount(3)
      await expect(allTabs.first()).toContainText('Mixed Report')
      await expect(allTabs.first()).toHaveClass(/tab-active/)
      await expect(page.locator('.tab-opened', { hasText: 'mixed-task' })).not.toHaveClass(
        /tab-active/,
      )
      await expect(page.locator('.tab-opened', { hasText: 'mixed-session' })).not.toHaveClass(
        /tab-active/,
      )
    } finally {
      rmSync(reportPath, { force: true })
    }
  })

  test('backend refuses upserting a tab for a non-existent report or task', async ({ request }) => {
    // Defense-in-depth: the existence check at upsert time stops a stale
    // URL (bookmark / cross-device race) from writing an orphan row.
    const { auth } = await authenticate(request)

    const taskRes = await request.post('/api/me/tabs', {
      headers: auth,
      data: { item_type: 'repeating_task', item_id: 'does-not-exist' },
    })
    expect(taskRes.status()).toBe(404)

    const reportRes = await request.post('/api/me/tabs', {
      headers: auth,
      data: { item_type: 'report', item_id: '1999-01-01/never-written.md' },
    })
    expect(reportRes.status()).toBe(404)
  })

  test('backend rejects unknown item_type', async ({ request }) => {
    const { auth } = await authenticate(request)
    const res = await request.post('/api/me/tabs', {
      headers: auth,
      data: { item_type: 'mystery', item_id: 'whatever' },
    })
    expect(res.status()).toBe(400)
  })
})
