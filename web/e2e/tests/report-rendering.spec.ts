import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdirSync, writeFileSync } from 'node:fs'
import path from 'node:path'

/**
 * A report is one of the documents the review screen renders, and it has to
 * read the same way in its own viewer: ```mermaid fences as diagrams, fenced
 * code highlighted, images inside the column. The viewer used to render the
 * document with heading anchors and nothing else, so the same report showed a
 * diagram inside a review and its raw source here.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const REPORT_FOLDER = '2026-07-29'

const BODY = [
  '## Pipeline',
  '',
  '![App icon](/icon-192.png)',
  '',
  '```mermaid',
  'flowchart LR',
  '  A[Draft] --> B[Review]',
  '```',
  '',
  '```rust',
  'fn main() {',
  '    println!("hi");',
  '}',
  '```',
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

/** Write a report straight into the server's reports tree — the only HTTP
 *  write endpoint is a PUT that needs the file to exist already. */
function writeReportFile(folder: string, file: string, title: string, body: string) {
  const dataDir = process.env.PECKBOARD_E2E_DATA_DIR
  if (!dataDir) throw new Error('PECKBOARD_E2E_DATA_DIR must be set (see playwright.config.ts)')
  const dir = path.join(dataDir, 'reports', folder)
  mkdirSync(dir, { recursive: true })
  writeFileSync(
    path.join(dir, file),
    `---\ntitle: "${title}"\ndate: "${folder}T09:00:00Z"\n---\n\n${body}\n`,
  )
}

test('the report viewer renders diagrams, highlighted code and fitted images', async ({
  request,
  page,
}) => {
  const { token } = await authenticate(request)
  const file = 'render-report.md'
  writeReportFile(REPORT_FOLDER, file, 'Render Report', BODY)

  await loadAt(page, token, `/reports/${REPORT_FOLDER}/${file}`)

  const doc = page.locator('.report-content')
  await expect(doc).toBeVisible({ timeout: 10_000 })

  // Settle either way first, so a mermaid failure reports as "fell back to
  // the source" rather than as a missing element.
  await expect(
    doc.locator('[data-testid="mermaid-diagram"], [data-testid="mermaid-error"]'),
  ).toBeVisible({ timeout: 15_000 })
  await expect(page.getByTestId('mermaid-error'), 'mermaid fell back to raw source').toHaveCount(0)
  await expect(doc.getByTestId('mermaid-diagram').locator('svg')).toContainText('Draft')
  await expect(doc.locator('pre code.language-mermaid')).toHaveCount(0)

  const code = doc.locator('pre code.language-rust')
  await expect(code).toHaveClass(/hljs/)
  await expect(code.locator('.hljs-keyword').first()).toBeVisible()

  const img = doc.locator('img')
  await expect(img).toHaveJSProperty('complete', true)
  expect(await img.evaluate((el: HTMLImageElement) => el.naturalWidth)).toBe(192)

  // The screen's own affordance still works — the anchored heading survives
  // sharing the component map with the diagram renderer.
  await expect(doc.locator('h2#pipeline .report-heading-anchor')).toHaveCount(1)
})
