import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Interactive mockups in the right-hand pane:
 *
 *  - an ```html block in a plan opens beside the plan, and its scripts run;
 *  - the mockup cannot reach the user's login or the Peckboard API;
 *  - Close removes the pane and gives the view its width back;
 *  - a shared .html file opens the same way.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

/** Clicking the button proves inline scripts run; the other two lines report
 *  what the sandbox lets the mockup reach. */
const MOCKUP = `<!doctype html>
<html><head><title>Counter mockup</title></head><body>
<button id="inc">Add one</button><output id="count">0</output>
<p id="storage">pending</p><p id="api">pending</p>
<script>
  document.getElementById('inc').addEventListener('click', () => {
    const el = document.getElementById('count')
    el.textContent = String(Number(el.textContent) + 1)
  })
  try {
    document.getElementById('storage').textContent = 'readable:' + localStorage.length
  } catch (e) {
    document.getElementById('storage').textContent = 'blocked'
  }
  fetch('PARENT_ORIGIN/api/sessions', { credentials: 'include' })
    .then((r) => { document.getElementById('api').textContent = r.ok ? 'readable' : 'blocked' })
    .catch(() => { document.getElementById('api').textContent = 'blocked' })
</script>
</body></html>`

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

/** A session in a fresh folder with a live `mock:block` agent, so its
 *  per-session MCP token stays valid for the tool calls below. */
async function seedSession(request: APIRequestContext, auth: Record<string, string>) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-mockup-'))
  writeFileSync(path.join(folderPath, 'shared-mockup.html'), MOCKUP.replace('PARENT_ORIGIN', ''))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'e2e-mockup-pane', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'mockup pane', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
    headers: auth,
    data: { text: 'go', model: 'mock:block' },
  })
  expect(sendRes.ok(), `send message failed: ${await sendRes.text()}`).toBeTruthy()
  const cfgPath = path.join(process.env.PECKBOARD_E2E_DATA_DIR!, 'worker-mcp', `${session.id}.json`)
  await expect
    .poll(() => {
      try {
        readFileSync(cfgPath, 'utf8')
        return true
      } catch {
        return false
      }
    })
    .toBe(true)
  const cfg = JSON.parse(readFileSync(cfgPath, 'utf8')) as {
    mcpServers: { peckboard: { headers: { Authorization: string } } }
  }
  return {
    sessionId: session.id,
    mcpToken: cfg.mcpServers.peckboard.headers.Authorization.replace(/^Bearer /, ''),
  }
}

async function mcpCall(
  request: APIRequestContext,
  mcpToken: string,
  tool: string,
  args: Record<string, unknown>,
) {
  const res = await request.post('/mcp', {
    headers: { Authorization: `Bearer ${mcpToken}` },
    data: { jsonrpc: '2.0', id: 1, method: 'tools/call', params: { name: tool, arguments: args } },
  })
  expect(res.ok()).toBeTruthy()
  const body = (await res.json()) as { error?: unknown; result?: { isError?: boolean } }
  expect(body.error, `${tool} failed: ${JSON.stringify(body)}`).toBeFalsy()
  expect(body.result?.isError, `${tool} failed: ${JSON.stringify(body)}`).toBeFalsy()
  return body
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

test('an html block in a plan runs in a sandboxed pane the user can close', async ({
  request,
  page,
  baseURL,
}) => {
  const { token, auth } = await authenticate(request)
  const { sessionId, mcpToken } = await seedSession(request, auth)
  const html = MOCKUP.replace('PARENT_ORIGIN', baseURL!)
  await mcpCall(request, mcpToken, 'propose_plan', {
    title: 'Counter proposal',
    markdown: `# Counter proposal\n\nTry it:\n\n\`\`\`html\n${html}\n\`\`\`\n`,
  })
  const planRes = await request.get(`/api/plans?session_id=${sessionId}`, { headers: auth })
  expect(planRes.ok()).toBeTruthy()
  const { plan } = (await planRes.json()) as { plan: { id: string } }

  await loadAppAt(page, token, `/plan/${plan.id}`)
  const planView = page.getByTestId('plan-view')
  await expect(planView).toBeVisible({ timeout: 15_000 })
  await expect(page.getByTestId('mockup-pane')).toHaveCount(0)
  const fullWidth = (await planView.boundingBox())!.width

  await page.getByTestId('html-fence-open').click()
  const pane = page.getByTestId('mockup-pane')
  await expect(pane).toBeVisible()
  await expect(page.getByTestId('mockup-pane-title')).toHaveText('Counter mockup')

  // Beside the view, on its right, with the view shrunk to make room.
  const paneBox = (await pane.boundingBox())!
  const shrunk = (await planView.boundingBox())!
  expect(shrunk.width).toBeLessThan(fullWidth)
  expect(paneBox.x).toBeGreaterThanOrEqual(shrunk.x + shrunk.width - 1)

  // Interactive: the mockup's inline script handles the click.
  const frame = page.frameLocator('[data-testid="mockup-pane-frame"]')
  await expect(frame.locator('#count')).toHaveText('0', { timeout: 10_000 })
  await frame.locator('#inc').click()
  await frame.locator('#inc').click()
  await expect(frame.locator('#count')).toHaveText('2')

  // Sandboxed: no login token, and the API refuses it even though the app's
  // own session is live in the same browser.
  await expect(frame.locator('#storage')).toHaveText('blocked')
  await expect(frame.locator('#api')).toHaveText('blocked')

  // Reload restarts the mockup from clean.
  // Sandboxed: no login token, and no reading the API from inside the frame.
  await expect(frame.locator('#storage')).toHaveText('blocked')
  await expect(frame.locator('#api')).toHaveText('blocked')
  // The frame's check above stops at its CSP on an http:// server, so also
  // pin the server-side refusal an https:// install relies on: requests from
  // the frame carry `Origin: null`, even with a valid token attached.
  const fromFrame = await request.get('/api/sessions', {
    headers: { ...auth, Origin: 'null' },
  })
  expect(fromFrame.status()).toBe(403)
  await page.getByTestId('mockup-pane-close').click()
  await expect(pane).toHaveCount(0)
  await expect.poll(async () => (await planView.boundingBox())!.width).toBe(fullWidth)
})

test('a shared html file opens in the pane, and Esc closes it', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const { sessionId, mcpToken } = await seedSession(request, auth)
  await mcpCall(request, mcpToken, 'share_file', { path: 'shared-mockup.html' })

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await page.getByTestId('shared-file-open-mockup').click({ timeout: 15_000 })

  await expect(page.getByTestId('mockup-pane-title')).toHaveText('shared-mockup.html')
  const frame = page.frameLocator('[data-testid="mockup-pane-frame"]')
  await frame.locator('#inc').click({ timeout: 10_000 })
  await expect(frame.locator('#count')).toHaveText('1')

  // Focus lands on Close when the pane opens, but the click above moved it
  // into the frame, so bring it back the way a keyboard user would.
  await page.getByTestId('mockup-pane-close').focus()
  await page.keyboard.press('Escape')
  await expect(page.getByTestId('mockup-pane')).toHaveCount(0)
})
