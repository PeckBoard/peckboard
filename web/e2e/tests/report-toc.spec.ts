import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdirSync, writeFileSync } from 'node:fs'
import path from 'node:path'

/**
 * A long agent report has to be navigable.
 *
 * The viewer used to hand the whole document to `SafeMarkdown` with no
 * table of contents, no heading anchors and no way to get the raw
 * markdown back out. These tests pin the three affordances that
 * replaced that: the TOC lists the h2/h3 headings and scrolls to them,
 * the resulting `#slug` URL reopens at that section after a reload, and
 * "Copy markdown" puts the source on the clipboard.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const REPORT_FOLDER = '2026-07-27'

/** Enough body text under each heading that the report actually scrolls. */
function filler(marker: string): string {
  return Array.from(
    { length: 25 },
    (_, i) => `${marker} paragraph ${i + 1}. Lorem ipsum dolor sit amet, consectetur adipiscing.`,
  ).join('\n\n')
}

const REPORT_BODY = [
  '# Long Agent Report',
  '',
  filler('Intro'),
  '',
  '## Executive Summary',
  '',
  filler('Summary'),
  '',
  '## Findings',
  '',
  filler('Findings'),
  '',
  '### Finding One',
  '',
  filler('One'),
  '',
  '### Finding Two',
  '',
  filler('Two'),
  '',
  '## Next Steps',
  '',
  '```sh',
  '# Not A Heading',
  'echo hi',
  '```',
  '',
  filler('Steps'),
].join('\n')

async function authenticate(request: APIRequestContext): Promise<{ token: string }> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token }
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

/** Write a markdown report straight into the server's `reports/<folder>`
 *  directory (same trick as report-deeplink.spec.ts — the only HTTP
 *  write endpoint is a PUT that needs the file to exist). */
function writeReportFile(folder: string, file: string, title: string, body: string) {
  const dataDir = process.env.PECKBOARD_E2E_DATA_DIR
  if (!dataDir) {
    throw new Error('PECKBOARD_E2E_DATA_DIR must be set (see playwright.config.ts)')
  }
  const dir = path.join(dataDir, 'reports', folder)
  mkdirSync(dir, { recursive: true })
  writeFileSync(
    path.join(dir, file),
    `---\ntitle: "${title}"\ndate: "${folder}T09:00:00Z"\n---\n\n${body}\n`,
  )
}

const TOC_ENTRIES = ['Executive Summary', 'Findings', 'Finding One', 'Finding Two', 'Next Steps']

test.describe('report table of contents — desktop', () => {
  test.use({ permissions: ['clipboard-read', 'clipboard-write'] })

  test('TOC lists the headings, navigates, deep-links and copies the markdown', async ({
    request,
    page,
  }) => {
    const { token } = await authenticate(request)
    const file = 'toc-desktop-report.md'
    writeReportFile(REPORT_FOLDER, file, 'TOC Desktop Report', REPORT_BODY)

    await loadAt(page, token, `/reports/${REPORT_FOLDER}/${file}`)

    const toc = page.locator('[data-testid="report-toc"]')
    await expect(toc).toBeVisible({ timeout: 10_000 })
    // h2 + h3 only — the h1 title and the `# Not A Heading` line inside
    // the fenced code block must not appear.
    await expect(toc.locator('.report-toc-link')).toHaveText(TOC_ENTRIES)

    const scroller = page.locator('.report-scroll')
    expect(await scroller.evaluate((el) => el.scrollTop)).toBe(0)

    await toc.getByRole('link', { name: 'Finding Two', exact: true }).click()

    await expect(page).toHaveURL(new RegExp(`/reports/${REPORT_FOLDER}/${file}#finding-two$`))
    await expect(page.locator('h3#finding-two')).toBeInViewport()
    expect(await scroller.evaluate((el) => el.scrollTop)).toBeGreaterThan(0)

    // The anchor survives a reload — the section is shareable.
    await page.reload()
    await expect(page.locator('h3#finding-two')).toBeInViewport({ timeout: 10_000 })
    expect(await scroller.evaluate((el) => el.scrollTop)).toBeGreaterThan(0)

    // Heading anchors expose the same fragment for copying a link.
    await expect(page.locator('h2#executive-summary .report-heading-anchor')).toHaveAttribute(
      'href',
      '#executive-summary',
    )

    const copy = page.locator('[data-testid="report-copy-markdown"]')
    await copy.click()
    await expect(copy).toHaveText('Copied')
    const clipboard = await page.evaluate(() => navigator.clipboard.readText())
    expect(clipboard).toContain('## Findings')
    expect(clipboard).toContain('### Finding Two')
  })
})

test.describe('report table of contents — mobile', () => {
  test.use({ viewport: { width: 390, height: 844 } })

  test('TOC collapses behind a disclosure and still navigates', async ({ request, page }) => {
    const { token } = await authenticate(request)
    const file = 'toc-mobile-report.md'
    writeReportFile(REPORT_FOLDER, file, 'TOC Mobile Report', REPORT_BODY)

    await loadAt(page, token, `/reports/${REPORT_FOLDER}/${file}`)

    const toggle = page.locator('[data-testid="report-toc-toggle"]')
    await expect(toggle).toBeVisible({ timeout: 10_000 })
    await expect(toggle).toHaveAttribute('aria-expanded', 'false')
    await expect(page.locator('.report-toc-link')).toHaveCount(0)

    await toggle.click()
    await expect(toggle).toHaveAttribute('aria-expanded', 'true')
    await expect(page.locator('.report-toc-link')).toHaveText(TOC_ENTRIES)

    await page.getByRole('link', { name: 'Next Steps', exact: true }).click()

    // Picking a section closes the disclosure so the report is readable.
    await expect(toggle).toHaveAttribute('aria-expanded', 'false')
    await expect(page).toHaveURL(new RegExp(`/reports/${REPORT_FOLDER}/${file}#next-steps$`))
    await expect(page.locator('h2#next-steps')).toBeInViewport()
  })
})
