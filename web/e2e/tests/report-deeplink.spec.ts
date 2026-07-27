import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Report chips deep-link to the report they name.
 *
 * Two surfaces show a *specific* report and used to dump the user on the
 * generic `/reports` index instead:
 *
 *   1. the chat report chip (rendered from a `system` event carrying
 *      `reportFolder` / `reportFile`), and
 *   2. the per-card report rows in the kanban card detail modal, which
 *      did a full `window.location.assign('/reports')` page load.
 *
 * Both now route through `openReport()` to `/reports/<folder>/<file>` —
 * the already-existing single-report route. The tests pin both the click
 * (the exact report opens, in-SPA) and the shareability of the resulting
 * URL (a reload of that URL reopens the same report).
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const REPORT_FOLDER = '2026-07-27'
const REPORT_BODY_HEADING = 'Deep Link Body Heading'

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

async function seedFolder(
  request: APIRequestContext,
  auth: Record<string, string>,
  suffix: string,
): Promise<string> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-rdl-${suffix}-`))
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-rdl-${suffix}-${Date.now()}`, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  const folder = (await res.json()) as { id: string }
  return folder.id
}

/** Write a markdown report straight into the server's `reports/<folder>`
 *  directory (same trick as tabs-report-and-repeating-task.spec.ts — the
 *  only HTTP write endpoint is a PUT that needs the file to exist). */
function writeReportFile(folder: string, file: string, frontmatter: string, body: string) {
  const dataDir = process.env.PECKBOARD_E2E_DATA_DIR
  if (!dataDir) {
    throw new Error('PECKBOARD_E2E_DATA_DIR must be set (see playwright.config.ts)')
  }
  const dir = path.join(dataDir, 'reports', folder)
  mkdirSync(dir, { recursive: true })
  writeFileSync(path.join(dir, file), `---\n${frontmatter}\n---\n\n${body}\n`)
}

/** Assert the single-report view for `file` is on screen and the URL is
 *  its deep link. */
async function expectReportOpen(page: Page, file: string, title: string) {
  await expect(page).toHaveURL(new RegExp(`/reports/${REPORT_FOLDER}/${file}$`))
  await expect(page.locator('.report-viewer-title')).toHaveText(title, { timeout: 10_000 })
  await expect(page.locator('.report-content h1')).toHaveText(REPORT_BODY_HEADING)
}

test('chat report chip opens that exact report and the URL is shareable', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, auth } = await authenticate(request)
  const file = 'chat-chip-report.md'
  const title = 'Chat Chip Report'

  writeReportFile(
    REPORT_FOLDER,
    file,
    `title: "${title}"\ndate: "${REPORT_FOLDER}T10:00:00Z"`,
    `# ${REPORT_BODY_HEADING}\n\nBody written by the chat worker.`,
  )

  const folderId = await seedFolder(request, auth, 'chat')
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'report chip session', folder_id: folderId },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  // The chip is driven by the `system` event `write_report` appends —
  // seed the identical shape rather than driving a whole worker run.
  const eventRes = await request.post(`/api/sessions/${session.id}/events`, {
    headers: auth,
    data: {
      kind: 'system',
      data: {
        text: `Report written: ${title}`,
        reportFolder: REPORT_FOLDER,
        reportFile: file,
      },
    },
  })
  expect(eventRes.ok(), `append event failed: ${await eventRes.text()}`).toBeTruthy()

  await loadAt(page, token, `/sessions/${session.id}`)

  const chip = page.locator('[data-testid="chat-report-chip"]')
  await expect(chip).toBeVisible({ timeout: 10_000 })
  await expect(chip.locator('.chat-report-chip-title')).toHaveText(`Report written: ${title}`)

  await chip.click()
  await expectReportOpen(page, file, title)

  // Shareable: the same URL in a fresh load reopens the same report.
  await page.reload()
  await expectReportOpen(page, file, title)
})

test('kanban card report row opens that exact report and the URL is shareable', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, auth } = await authenticate(request)
  const file = 'kanban-card-report.md'
  const title = 'Kanban Card Report'

  const folderId = await seedFolder(request, auth, 'kanban')
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: `report deeplink ${Date.now()}`,
      folder_id: folderId,
      // No workers — the card stays parked in backlog so the test isn't
      // racing the orchestrator.
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: auth,
    data: { title: 'Deep link card', description: '', step: 'backlog', priority: 2 },
  })
  expect(cardRes.ok(), `create card failed: ${await cardRes.text()}`).toBeTruthy()
  const card = (await cardRes.json()) as { id: string }

  // `cardId` in the frontmatter is what links the report to the card
  // (see list_card_reports in src/routes/projects/cards.rs).
  writeReportFile(
    REPORT_FOLDER,
    file,
    `title: "${title}"\ndate: "${REPORT_FOLDER}T11:00:00Z"\ncardId: "${card.id}"`,
    `# ${REPORT_BODY_HEADING}\n\nBody written by the card's worker.`,
  )

  await loadAt(page, token, `/projects/${project.id}`)

  const boardCard = page.locator('.kanban-card', { hasText: 'Deep link card' })
  await expect(boardCard.locator('.kanban-card-title')).toBeVisible({ timeout: 10_000 })
  await boardCard.locator('.kanban-card-title').click()
  await boardCard.locator('[data-testid="card-quick-view"]').click()

  const reportLink = page.locator('.modal [data-testid="card-report-link"]')
  await expect(reportLink).toBeVisible({ timeout: 10_000 })
  await expect(reportLink.locator('.card-report-title')).toHaveText(title)

  await reportLink.click()
  await expectReportOpen(page, file, title)

  await page.reload()
  await expectReportOpen(page, file, title)
})
