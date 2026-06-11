import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Card descriptions render as markdown (so headings, bold, lists, and
 * inline code show up styled), and raw HTML in the description is
 * escaped rather than executed.
 *
 * The escape assertion is the security-critical one: a `<script>`,
 * `<img onerror>`, or `<iframe>` in the description must not turn into
 * a live DOM node. We rely on react-markdown's default behaviour
 * (raw HTML in source is escaped — no `rehype-raw` plugin in
 * SafeMarkdown), so the test pins that contract.
 *
 * Covers both rendering surfaces: the card itself on the board, and
 * the detail modal opened by clicking the card.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const DESCRIPTION = `# Card Heading

This card has **bold text**, _italic_, and \`inline code\`.

- bullet one
- bullet two

<script>window.__pwned = 'card'</script>
<img src="x" onerror="window.__pwned_img = 'card'" data-testid="should-not-exist" />
`

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

async function setupProjectWithCard(
  request: APIRequestContext,
  auth: Record<string, string>,
  suffix: string,
): Promise<{ projectId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-md-${suffix}-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-md-${suffix}-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: `markdown card ${suffix}`,
      folder_id: folder.id,
      // No workers — keep the card parked in backlog so the test isn't
      // racing the orchestrator.
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: auth,
    data: {
      title: 'Markdown smoke',
      description: DESCRIPTION,
      step: 'backlog',
      priority: 2,
    },
  })
  expect(cardRes.ok(), `create card failed: ${await cardRes.text()}`).toBeTruthy()
  return { projectId: project.id }
}

test('card description renders markdown and escapes embedded raw HTML on the board', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, auth } = await authenticate(request)
  const { projectId } = await setupProjectWithCard(request, auth, 'board')

  await loadAt(page, token, `/projects/${projectId}`)

  const card = page.locator('.kanban-card', { hasText: 'Markdown smoke' })
  await expect(card.locator('.kanban-card-title', { hasText: 'Markdown smoke' })).toBeVisible({
    timeout: 10_000,
  })

  // Cards are collapsed by default — header only — so the description
  // markdown only renders after the user expands the card. Tap the
  // card header to expand.
  await card.locator('.kanban-card-title').click()

  // Markdown was rendered to real DOM elements (not as raw `#`/`**` text).
  const desc = card.locator('.kanban-card-desc-markdown')
  await expect(desc.locator('h1')).toHaveText('Card Heading')
  await expect(desc.locator('strong')).toHaveText('bold text')
  await expect(desc.locator('em')).toHaveText('italic')
  await expect(desc.locator('code')).toHaveText('inline code')
  await expect(desc.locator('ul li')).toHaveCount(2)

  // The embedded `<script>` and `<img onerror>` were escaped, not
  // executed: their text content shows up but no live DOM nodes appear.
  await expect(desc.locator('script')).toHaveCount(0)
  await expect(desc.locator('img[data-testid="should-not-exist"]')).toHaveCount(0)
  const pwned = await page.evaluate(
    () =>
      (window as unknown as { __pwned?: string; __pwned_img?: string }).__pwned ??
      (window as unknown as { __pwned?: string; __pwned_img?: string }).__pwned_img,
  )
  expect(pwned, 'no script or onerror handler from the description ran').toBeUndefined()
  // The escaped raw-HTML source still shows up as literal text inside the
  // description, so users see exactly what they typed.
  await expect(desc).toContainText('<script>')
})

test('card detail modal renders markdown and escapes embedded raw HTML', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, auth } = await authenticate(request)
  const { projectId } = await setupProjectWithCard(request, auth, 'modal')

  await loadAt(page, token, `/projects/${projectId}`)

  const card = page.locator('.kanban-card', { hasText: 'Markdown smoke' })
  await expect(card.locator('.kanban-card-title', { hasText: 'Markdown smoke' })).toBeVisible({
    timeout: 10_000,
  })

  // Cards are collapsed by default. Tap the card to expand its body,
  // then click the View action button to open the detail modal.
  await card.locator('.kanban-card-title').click()
  await card.locator('[data-testid="card-quick-view"]').click()

  const modal = page.locator('.modal')
  await expect(modal.locator('h2', { hasText: 'Markdown smoke' })).toBeVisible({ timeout: 5_000 })

  const desc = modal.locator('.card-detail-description')
  await expect(desc.locator('h1')).toHaveText('Card Heading')
  await expect(desc.locator('strong')).toHaveText('bold text')
  await expect(desc.locator('code')).toHaveText('inline code')

  await expect(desc.locator('script')).toHaveCount(0)
  await expect(desc.locator('img[data-testid="should-not-exist"]')).toHaveCount(0)
  const pwned = await page.evaluate(
    () =>
      (window as unknown as { __pwned?: string; __pwned_img?: string }).__pwned ??
      (window as unknown as { __pwned?: string; __pwned_img?: string }).__pwned_img,
  )
  expect(pwned, 'no script or onerror handler from the description ran').toBeUndefined()
})
