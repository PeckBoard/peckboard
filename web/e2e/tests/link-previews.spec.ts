import { test, expect, type APIRequestContext, type Page } from '../harness'
import { createServer, type Server } from 'node:http'
import type { AddressInfo } from 'node:net'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Link previews under chat messages (`LinkPreview`). `mock:echo` replies
 * with the user's text, so a message carrying links becomes an assistant
 * bubble carrying the same links.
 *
 * Nothing reaches the real network: the site card is fetched by the server
 * from a fixture HTTP server on 127.0.0.1 (trusted through the admin link
 * preview settings, since loopback is otherwise refused), the YouTube
 * preview is fulfilled in the browser, and the YouTube frame request is
 * aborted — the test asserts the sandboxed iframe's attributes, not the
 * player.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

// 1×1 transparent PNG.
const PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==',
  'base64',
)

let cachedAuth: { token: string; authHeader: Record<string, string> } | null = null
let fixture: Server
let fixtureBase = ''

async function authenticate(request: APIRequestContext) {
  if (cachedAuth) return cachedAuth
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  cachedAuth = { token, authHeader: { Authorization: `Bearer ${token}` } }
  return cachedAuth
}

test.beforeAll(async () => {
  fixture = createServer((req, res) => {
    if (req.url === '/article') {
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' })
      res.end(`<!doctype html><html><head>
        <title>Fallback title</title>
        <meta property="og:title" content="Fixture Article">
        <meta property="og:description" content="A page served by the e2e fixture.">
        <meta property="og:site_name" content="Fixture Site">
        <meta property="og:image" content="/hero.png">
        <meta name="theme-color" content="#336699">
      </head><body>hi</body></html>`)
    } else if (req.url === '/hero.png') {
      res.writeHead(200, { 'Content-Type': 'image/png' })
      res.end(PNG)
    } else {
      res.writeHead(404, { 'Content-Type': 'text/plain' })
      res.end('nope')
    }
  })
  await new Promise<void>((resolve) => fixture.listen(0, '127.0.0.1', resolve))
  fixtureBase = `http://127.0.0.1:${(fixture.address() as AddressInfo).port}`
})

test.afterAll(async () => {
  await new Promise<void>((resolve) => fixture.close(() => resolve()))
})

async function trustLoopback(request: APIRequestContext, authHeader: Record<string, string>) {
  const res = await request.put('/api/settings/link-preview', {
    headers: authHeader,
    data: { trusted_internal_hosts: ['127.0.0.1'], screenshot_fallback: false },
  })
  expect(res.status(), await res.text()).toBe(204)
}

async function seedSession(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  name: string,
): Promise<string> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-links-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-links-${name}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const res = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name, folder_id: folder.id },
  })
  expect(res.ok(), `create session failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-bubble').first())).toBeVisible({
    timeout: 10_000,
  })
}

async function echo(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  sessionId: string,
  text: string,
) {
  const res = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text, model: 'mock:echo' },
  })
  expect(res.ok(), `send failed: ${await res.text()}`).toBeTruthy()
}

const assistant = (page: Page) => page.locator('.chat-bubble-assistant').last()

test('a site link renders an Open Graph card with its proxied image', async ({ request, page }) => {
  const { token, authHeader } = await authenticate(request)
  await trustLoopback(request, authHeader)
  const sessionId = await seedSession(request, authHeader, 'og-card')
  await loadAppAt(page, token, `/sessions/${sessionId}`)

  // A link inside code never becomes a preview.
  await echo(
    request,
    authHeader,
    sessionId,
    `Read ${fixtureBase}/article please. \`${fixtureBase}/in-code\``,
  )

  const card = assistant(page).getByTestId('link-card')
  await expect(card).toHaveCount(1, { timeout: 15_000 })
  await expect(card.getByTestId('link-card-title')).toHaveText('Fixture Article')
  await expect(card).toContainText('A page served by the e2e fixture.')
  await expect(card).toContainText('Fixture Site')
  await expect(card).toHaveAttribute('href', `${fixtureBase}/article`)
  // The image goes through the signed same-origin proxy and actually loads.
  const img = card.getByTestId('link-card-image')
  await expect(img).toHaveAttribute('src', /^\/api\/link-preview\/image\?/)
  await expect
    .poll(() => img.evaluate((el) => (el as HTMLImageElement).naturalWidth))
    .toBeGreaterThan(0)
})

test('a YouTube link shows a facade, then a sandboxed iframe on click', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader, 'youtube')
  const watch = 'https://www.youtube.com/watch?v=dQw4w9WgXcQ'
  await page.route(
    (url) =>
      url.pathname === '/api/link-preview' &&
      (url.searchParams.get('url') ?? '').startsWith('https://www.youtube.com/'),
    (route) =>
      route.fulfill({
        json: {
          url: watch,
          final_url: watch,
          kind: 'embed',
          provider: 'youtube',
          site_name: 'YouTube',
          title: 'Fixture Video',
          embed: {
            iframe_src: 'https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ',
            sandbox_mode: 'origin',
            aspect: '16/9',
          },
        },
      }),
  )
  await page.route('https://www.youtube-nocookie.com/**', (route) => route.abort())
  await loadAppAt(page, token, `/sessions/${sessionId}`)

  await echo(request, authHeader, sessionId, `Watch this: ${watch}`)

  const embed = assistant(page).getByTestId('link-embed')
  await expect(embed).toBeVisible({ timeout: 15_000 })
  await expect(embed).toHaveAttribute('data-live', 'false')
  await expect(embed.getByTestId('link-embed-title')).toHaveText('Fixture Video')
  await expect(embed.locator('iframe')).toHaveCount(0)

  await embed.getByTestId('link-embed-facade').click()
  const frame = embed.getByTestId('link-embed-frame')
  await expect(frame).toBeVisible()
  await expect(embed).toHaveAttribute('data-live', 'true')
  await expect(frame).toHaveAttribute(
    'sandbox',
    'allow-scripts allow-same-origin allow-popups allow-presentation',
  )
  await expect(frame).toHaveAttribute(
    'allow',
    'autoplay; encrypted-media; picture-in-picture; fullscreen',
  )
  await expect(frame).toHaveAttribute('referrerpolicy', 'strict-origin-when-cross-origin')
  const src = new URL((await frame.getAttribute('src')) ?? '')
  expect(src.origin).toBe('https://www.youtube-nocookie.com')
  expect(src.searchParams.get('enablejsapi')).toBe('1')
  expect(src.searchParams.get('autoplay')).toBe('1')

  // Unloading returns to the facade.
  await frame.hover()
  await embed.getByTestId('link-embed-close').click()
  await expect(embed).toHaveAttribute('data-live', 'false')
})

test('a link to a session renders a live card that follows a rename', async ({
  request,
  page,
  baseURL,
}) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader, 'linked-before')
  await loadAppAt(page, token, `/sessions/${sessionId}`)

  await echo(request, authHeader, sessionId, `See ${baseURL}/sessions/${sessionId} for context`)

  const card = assistant(page).getByTestId('link-internal')
  await expect(card).toBeVisible({ timeout: 15_000 })
  await expect(card).toHaveAttribute('data-entity', 'session')
  await expect(card.getByTestId('link-internal-title')).toHaveText('linked-before')
  await expect(card).toHaveAttribute('href', `/sessions/${sessionId}`)

  await page.locator('.chat-toolbar-menu').click()
  await page.getByTestId('chat-menu-rename').click()
  const input = page.getByTestId('rename-input')
  await input.fill('linked-after')
  await input.press('Enter')

  await expect(card.getByTestId('link-internal-title')).toHaveText('linked-after')
})

test('a link whose preview fails renders nothing but the link', async ({ request, page }) => {
  const { token, authHeader } = await authenticate(request)
  await trustLoopback(request, authHeader)
  const sessionId = await seedSession(request, authHeader, 'broken-link')
  await loadAppAt(page, token, `/sessions/${sessionId}`)

  const missing = `${fixtureBase}/missing`
  const previewDone = page.waitForResponse(
    (r) =>
      new URL(r.url()).pathname === '/api/link-preview' &&
      new URL(r.url()).searchParams.get('url') === missing,
    { timeout: 15_000 },
  )
  await echo(request, authHeader, sessionId, `Broken: ${missing}`)
  const res = await previewDone
  expect(res.ok()).toBeFalsy()

  const bubble = assistant(page)
  await expect(bubble.locator(`a[href="${missing}"]`)).toBeVisible()
  await expect(bubble.getByTestId('link-previews').locator(':scope > *')).toHaveCount(0)
})
