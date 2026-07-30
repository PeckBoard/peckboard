import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * A review shows the document, not a transcript of its source: ```mermaid
 * fences render as diagrams, fenced code is highlighted, and an image loads
 * and stays inside the text column.
 *
 * Nothing here needs the AI half of the feature, so no review session is ever
 * created and no provider runs — this is the render pipeline only.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

/** Fixture document. Line numbers are load-bearing: blocks are addressed by
 *  the `data-line-start` DocPane reads off the parsed node, and the mermaid
 *  fence spans 5–8. `/icon-192.png` is served by the app itself, so the image
 *  assertions don't depend on the network. */
const DOC = [
  '# Rich doc', // 1
  '', // 2
  '![App icon](/icon-192.png)', // 3
  '', // 4
  '```mermaid', // 5
  'flowchart LR', // 6
  '  A[Draft] --> B[Review]', // 7
  '```', // 8
  '', // 9
  '```rust', // 10
  'fn main() {', // 11
  '    println!("hi");', // 12
  '}', // 13
  '```', // 14
  '',
].join('\n')

type Auth = { token: string; auth: Record<string, string> }

async function authenticate(request: APIRequestContext): Promise<Auth> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

/** A registered workspace folder holding the fixture at `docs/rich.md`. */
async function seedFolder(
  request: APIRequestContext,
  auth: Record<string, string>,
  suffix: string,
): Promise<string> {
  const dir = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-render-${suffix}-`))
  mkdirSync(path.join(dir, 'docs'), { recursive: true })
  writeFileSync(path.join(dir, 'docs', 'rich.md'), DOC)
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-render-${suffix}-${Date.now()}`, path: dir },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function createReview(
  request: APIRequestContext,
  auth: Record<string, string>,
  folderId: string,
  title: string,
): Promise<string> {
  const res = await request.post('/api/doc-reviews', {
    headers: auth,
    data: { source_kind: 'file', source_ref: `${folderId}:docs/rich.md`, title },
  })
  expect(res.ok(), `create review failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { review: { id: string } }).review.id
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto(route)
}

/** Leave no reviews behind: an open review owns a tab chip, and the tab-strip
 *  specs later in the run count the chips they find. */
async function cleanupReviews(request: APIRequestContext, auth: Record<string, string>) {
  const res = await request.get('/api/doc-reviews', { headers: auth })
  if (!res.ok()) return
  for (const r of ((await res.json()) as { reviews: Array<{ id: string }> }).reviews) {
    await request.delete(`/api/doc-reviews/${r.id}`, { headers: auth })
  }
}

test.describe('document review — markdown rendering', () => {
  test.afterEach(async ({ request }) => {
    const { auth } = await authenticate(request)
    await cleanupReviews(request, auth)
  })

  test('renders diagrams, highlights code and fits images to the column', async ({
    request,
    page,
  }) => {
    const { token, auth } = await authenticate(request)
    const folderId = await seedFolder(request, auth, 'render')
    const reviewId = await createReview(request, auth, folderId, 'Render review')

    await loadAt(page, token, `/review/${reviewId}`)
    const doc = page.getByTestId('review-doc')
    await expect(doc).toBeVisible({ timeout: 15_000 })

    // 1. The mermaid fence is a diagram, not its own source. mermaid loads
    //    lazily, so wait for it to settle either way first — a render failure
    //    then reports as "fell back to the source", which is what actually
    //    went wrong, instead of as a missing element.
    const diagram = doc.getByTestId('mermaid-diagram')
    await expect(
      doc.locator('[data-testid="mermaid-diagram"], [data-testid="mermaid-error"]'),
    ).toBeVisible({ timeout: 15_000 })
    await expect(page.getByTestId('mermaid-error'), 'mermaid fell back to raw source').toHaveCount(
      0,
    )
    await expect(diagram.locator('svg')).toBeVisible()
    await expect(diagram.locator('svg')).toContainText('Draft')
    // And no leftover code block where the diagram now is.
    await expect(doc.locator('pre code.language-mermaid')).toHaveCount(0)

    // 2. The rust fence stays a code block, and rehype-highlight tokenised it.
    const code = doc.locator('pre code.language-rust')
    await expect(code).toHaveClass(/hljs/)
    await expect(code.locator('.hljs-keyword').first()).toBeVisible()

    // 3. The image actually loaded, and the stylesheet caps it at the column
    //    width rather than letting a wide screenshot push the text sideways.
    const img = doc.locator('img')
    await expect(img).toHaveJSProperty('complete', true)
    expect(await img.evaluate((el: HTMLImageElement) => el.naturalWidth)).toBe(192)
    expect(await img.evaluate((el) => getComputedStyle(el).maxWidth)).toBe('100%')
    const docBox = await doc.boundingBox()
    const imgBox = await img.boundingBox()
    expect(imgBox!.width).toBeLessThanOrEqual(docBox!.width)
  })

  test('a diagram is one annotatable block', async ({ request, page }) => {
    const { token, auth } = await authenticate(request)
    const folderId = await seedFolder(request, auth, 'annotate')
    const reviewId = await createReview(request, auth, folderId, 'Diagram review')

    await loadAt(page, token, `/review/${reviewId}`)
    await expect(page.getByTestId('review-doc')).toBeVisible({ timeout: 15_000 })
    await expect(page.getByTestId('mermaid-diagram')).toBeVisible({ timeout: 15_000 })

    // The fence spans lines 5–8; the wrapper that replaced its <pre> carries
    // the anchor, so clicking the diagram offers the same annotation verbs as
    // clicking a paragraph.
    const block = page.locator('[data-testid="review-block"][data-line-start="5"]')
    await expect(block).toHaveCount(1)
    await block.click()
    await expect(page.getByTestId('review-popover')).toBeVisible()
    await page.getByTestId('review-popover-comment').click()
    await page.getByTestId('review-annotation-editor').fill('which stage owns the handoff?')
    await page.getByTestId('review-annotation-submit').click()
    await expect(page.getByTestId('review-popover')).toHaveCount(0)

    // It reads as annotated: the pending tint and the margin pin, on the
    // diagram's own block.
    await expect(page.getByTestId('review-annotation-item')).toHaveCount(1)
    await expect(block).toHaveClass(/review-block--pending/)
    await expect(block.getByTestId('review-block-pin')).toHaveText('1')
  })
})
