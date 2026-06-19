import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e test for assistant-message markdown rendering.
 *
 * Drives the real React app: logs in via the API to grab a token,
 * injects it into the page's localStorage so the app boots
 * authenticated, navigates to a freshly-created session, sends a
 * message backed by the `mock:markdown` scenario, and asserts that
 * ReactMarkdown actually produced HTML elements (heading, bold, list,
 * inline code, fenced code block with rehype-highlight classes) inside
 * the chat bubble.
 *
 * The mock provider eliminates the only non-deterministic input here
 * (an LLM); everything else is real code on the real stack.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type AuthBundle = {
  token: string
  authHeader: { Authorization: string }
}

async function authenticate(request: APIRequestContext): Promise<AuthBundle> {
  // The server auto-bootstraps the admin from PECKBOARD_BOOTSTRAP_*
  // env vars at first start (see playwright.config.ts); we just log in.
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

async function seedAuthedSession(
  request: APIRequestContext,
  authHeader: Record<string, string>,
): Promise<{ sessionId: string }> {
  // Folder path must be unique per test (peckboard enforces UNIQUE on
  // folders.path) and must exist on disk (the backend validates it).
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-md-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-markdown', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'markdown smoke', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  return { sessionId: session.id }
}

async function loadAppAt(page: Page, token: string, route: string) {
  // Inject the auth token before any app script runs so the React app
  // boots authenticated instead of redirecting to login.
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

test('mock:markdown renders rich HTML in the assistant bubble', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedAuthedSession(request, authHeader)

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  // Confirm the app booted authenticated and the chat surface rendered
  // — either the empty-state placeholder, or (after a fast race) a bubble.
  await expect(page.locator('.chat-empty').or(page.locator('.chat-bubble').first())).toBeVisible({
    timeout: 10_000,
  })

  // Trigger the scripted markdown reply through the real route. We do
  // this from the test rather than the UI because we already covered
  // the UI input path in other tests; what we're verifying here is
  // *rendering*.
  const sendRes = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'render markdown please', model: 'mock:markdown' },
  })
  expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

  const assistantBubble = page.locator('.chat-bubble-assistant').last()

  // Every flow that matters for "markdown rendering works": the
  // heading, bold inline emphasis, the list, inline code, and the
  // fenced code block with syntax highlighting applied. If any of
  // these locators fails the markdown pipeline is broken.
  await expect(assistantBubble.locator('.chat-markdown h1')).toHaveText(/Hello from mock/, {
    timeout: 10_000,
  })
  await expect(assistantBubble.locator('.chat-markdown strong')).toHaveText('bold text')
  await expect(assistantBubble.locator('.chat-markdown ul li')).toHaveCount(3)
  await expect(assistantBubble.locator('.chat-markdown :not(pre) > code')).toHaveText(
    'mock:markdown',
  )

  // The fenced ```rust block should become a <pre><code class="hljs language-rust">
  // and rehype-highlight should annotate tokens (e.g. .hljs-keyword for `fn`).
  const codeBlock = assistantBubble.locator('.chat-markdown pre code')
  await expect(codeBlock).toHaveClass(/language-rust/)
  await expect(codeBlock).toHaveClass(/hljs/)
  await expect(codeBlock.locator('.hljs-keyword').first()).toBeVisible()
})
