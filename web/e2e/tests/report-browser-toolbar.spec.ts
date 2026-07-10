import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdirSync, rmSync, writeFileSync } from 'node:fs'
import path from 'node:path'

/**
 * E2E for the Reports index toolbar: date sorting, text search, and the
 * session / project filters (ReportBrowser + filterAndSortReports).
 *
 * All three seeded reports share one unique `projectName`, so selecting
 * that project isolates them from any reports other specs leave on the
 * shared run data dir — every ordering / count assertion below runs with
 * the project filter engaged.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const PROJECT = 'ReportToolbarE2E'
const SESSION_A = 'sess-aaaa1111-toolbar'
const SESSION_B = 'sess-bbbb2222-toolbar'
const SESSION_B_NAME = 'Beta Toolbar Session'
const SESSION_B_CREATED = '2026-07-09T09:30:00Z'

async function authenticate(request: APIRequestContext): Promise<{ token: string }> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok()).toBeTruthy()
  return (await res.json()) as { token: string }
}

async function loadReports(page: Page, token: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto('/reports')
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })
}

/** Seed a report markdown file directly into the run's reports dir. */
function writeReport(
  folder: string,
  file: string,
  title: string,
  date: string,
  sessionId: string,
  session?: { name?: string; createdAt?: string },
): string {
  const dataDir = process.env.PECKBOARD_E2E_DATA_DIR
  if (!dataDir) throw new Error('PECKBOARD_E2E_DATA_DIR must be set (see playwright.config.ts)')
  const dir = path.join(dataDir, 'reports', folder)
  mkdirSync(dir, { recursive: true })
  const filePath = path.join(dir, file)
  let fm = `title: "${title}"\ndate: "${date}"\nsessionId: "${sessionId}"\nprojectName: "${PROJECT}"`
  if (session?.name) fm += `\nsessionName: "${session.name}"`
  if (session?.createdAt) fm += `\nsessionCreatedAt: "${session.createdAt}"`
  writeFileSync(filePath, `---\n${fm}\n---\n\n# ${title}\n\nbody\n`)
  return filePath
}

test.describe('reports index toolbar', () => {
  test('sorts by date, searches, and filters by session / project', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL).toBeTruthy()
    const { token } = await authenticate(request)

    const seeded = [
      writeReport('2026-07-01', 'older-toolbar.md', 'Older Report', '2026-07-01', SESSION_A),
      writeReport('2026-07-05', 'middle-toolbar.md', 'Middle Report', '2026-07-05', SESSION_A),
      writeReport('2026-07-09', 'newer-toolbar.md', 'Newer Report', '2026-07-09', SESSION_B, {
        name: SESSION_B_NAME,
        createdAt: SESSION_B_CREATED,
      }),
    ]

    try {
      await loadReports(page, token)

      const projectFilter = page.locator('select[aria-label="Filter by project"]')
      const sessionFilter = page.locator('button[aria-label="Filter by session"]')
      const search = page.locator('input.report-search')
      const sortToggle = page.locator('.report-sort-toggle')
      const names = page.locator('.list-view-name')

      // Isolate this spec's reports.
      await projectFilter.selectOption(PROJECT)
      await expect(names).toHaveCount(3)

      // Default order is newest-first.
      await expect(names).toHaveText(['Newer Report', 'Middle Report', 'Older Report'])

      // Sort toggle flips to oldest-first.
      await sortToggle.click()
      await expect(names).toHaveText(['Older Report', 'Middle Report', 'Newer Report'])
      await sortToggle.click()
      await expect(names).toHaveText(['Newer Report', 'Middle Report', 'Older Report'])

      // Session filter: searchable dropdown. SESSION_B carries a name +
      // creation date; SESSION_A (no metadata) falls back to its id prefix.
      const sessionMenu = page.locator('.dropdown-menu')
      await expect(sessionFilter).toContainText('All sessions')
      await sessionFilter.click()
      const betaItem = sessionMenu.locator('.dropdown-item', { hasText: SESSION_B_NAME })
      await expect(betaItem).toContainText('2026') // created date rendered
      await expect(betaItem).toContainText(SESSION_B.slice(0, 8))

      // The filter input matches the full session id via `searchText`.
      await sessionMenu.locator('.model-picker-search').fill(SESSION_A)
      await expect(sessionMenu.locator('.dropdown-item')).toHaveCount(1)
      await sessionMenu.locator('.dropdown-item', { hasText: SESSION_A.slice(0, 8) }).click()
      await expect(names).toHaveText(['Middle Report', 'Older Report'])
      await expect(sessionFilter).toContainText(SESSION_A.slice(0, 8))

      // …and matches the session name.
      await sessionFilter.click()
      await sessionMenu.locator('.model-picker-search').fill('beta toolbar')
      await sessionMenu.locator('.dropdown-item', { hasText: SESSION_B_NAME }).click()
      await expect(names).toHaveText(['Newer Report'])
      await expect(sessionFilter).toContainText(SESSION_B_NAME)

      // "All sessions" resets the filter.
      await sessionFilter.click()
      await sessionMenu.locator('.dropdown-item', { hasText: 'All sessions' }).click()
      await expect(names).toHaveCount(3)

      // Text search matches title substrings (case-insensitive).
      await search.fill('middle')
      await expect(names).toHaveText(['Middle Report'])

      // No match → filtered empty state.
      await search.fill('zzz-no-such-report')
      await expect(page.locator('.list-view-empty')).toContainText('No reports match your filters')
    } finally {
      for (const f of seeded) rmSync(f, { force: true })
    }
  })
})
