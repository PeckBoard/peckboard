import { test, expect, type APIRequestContext } from '../harness'
import { mkdtempSync } from 'node:fs'
import { createServer, type Server } from 'node:http'
import type { AddressInfo } from 'node:net'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Settings → Assistant → Conversation Mirror, end to end.
 *
 * A node HTTP server on 127.0.0.1 stands in for both the Slack and the
 * Discord webhook (the webServer runs with PECKBOARD_MIRROR_TEST_ENDPOINTS=1,
 * which lets the mirror post to loopback). The spec configures both through
 * the UI, checks the per-field errors, sends a test to each, then runs one
 * Assistant turn on `mock:echo` and expects the receiver to get both the
 * utterance and the reply. The email digest is covered by the Rust
 * integration test (`tests/assistant_mirror.rs`).
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

interface Hit {
  path: string
  body: { text?: string; content?: string }
}

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  // The Assistant session lives in the most recent folder.
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-mirror-'))
  const folderRes = await request.post('/api/folders', {
    headers: { Authorization: `Bearer ${token}` },
    data: { name: `mirror-${path.basename(folderPath)}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  return token
}

/** A webhook receiver that records every POST and answers 200. */
async function startReceiver(): Promise<{ server: Server; port: number; hits: Hit[] }> {
  const hits: Hit[] = []
  const server = createServer((req, res) => {
    let raw = ''
    req.on('data', (chunk) => (raw += chunk))
    req.on('end', () => {
      try {
        hits.push({ path: req.url ?? '', body: JSON.parse(raw) })
      } catch {
        hits.push({ path: req.url ?? '', body: {} })
      }
      res.writeHead(200, { 'Content-Type': 'text/plain' })
      res.end('ok')
    })
  })
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve))
  return { server, port: (server.address() as AddressInfo).port, hits }
}

/** Mirror settings are instance-global: start and end with both webhooks off. */
async function resetMirror(request: APIRequestContext, token: string) {
  const res = await request.put('/api/assistant/mirror', {
    headers: { Authorization: `Bearer ${token}` },
    data: {
      slack: { enabled: false, webhook_url: '' },
      discord: { enabled: false, webhook_url: '' },
      email: { enabled: false },
    },
  })
  expect(res.ok(), `mirror reset failed: ${await res.text()}`).toBeTruthy()
}

const texts = (hits: Hit[], prefix: string) =>
  hits.filter((h) => h.path.startsWith(prefix)).map((h) => h.body.text ?? h.body.content ?? '')

test('Conversation Mirror: configure Slack + Discord in Settings, test them, mirror a turn', async ({
  request,
  page,
}) => {
  test.setTimeout(60_000)
  const token = await authenticate(request)
  const auth = { Authorization: `Bearer ${token}` }
  const { server, port, hits } = await startReceiver()
  let sessionId = ''
  try {
    await resetMirror(request, token)

    await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
    await page.goto('/settings/assistant')
    const section = page.getByTestId('assistant-mirror-section')
    await expect(section).toBeVisible({ timeout: 10_000 })
    const save = page.getByTestId('mirror-save')
    await expect(save).toBeDisabled()
    await expect(page.getByTestId('mirror-save-reason')).toHaveText('No changes to save.')

    // Turning a channel on without a webhook is caught before saving.
    await page.getByTestId('mirror-discord-enabled').check()
    await expect(page.getByTestId('mirror-discord-url-error')).toHaveText(
      'Add a webhook URL to turn Discord on',
    )
    await expect(save).toBeDisabled()
    await expect(page.getByTestId('mirror-save-reason')).toHaveText('Fix the fields above to save.')

    // A URL that isn't a Slack webhook comes back as a field error.
    await page.getByTestId('mirror-slack-enabled').check()
    await page.getByTestId('mirror-slack-url').fill('https://example.com/not-a-webhook')
    await page.getByTestId('mirror-discord-url').fill(`http://127.0.0.1:${port}/discord/hook`)
    await expect(save).toBeEnabled()
    await save.click()
    await expect(page.getByTestId('mirror-slack-url-error')).toContainText(
      'Must be a Slack incoming webhook',
    )
    await expect(page.getByTestId('mirror-discord-url-error')).toHaveCount(0)

    await page.getByTestId('mirror-slack-url').fill(`http://127.0.0.1:${port}/slack/hook`)
    await expect(page.getByTestId('mirror-slack-url-error')).toHaveCount(0)
    await save.click()
    // Saved secrets are never echoed back into the inputs.
    for (const ch of ['slack', 'discord']) {
      const input = page.getByTestId(`mirror-${ch}-url`)
      await expect(input).toHaveValue('')
      await expect(input).toHaveAttribute('placeholder', '•••• saved')
    }
    await expect(save).toBeDisabled()

    for (const ch of ['slack', 'discord']) {
      await page.getByTestId(`mirror-${ch}-test`).click()
      await expect(page.getByTestId(`mirror-${ch}-status`)).toHaveAttribute('data-state', 'ok', {
        timeout: 10_000,
      })
      await expect(page.getByTestId(`mirror-${ch}-status`)).toContainText('Delivered')
      expect(texts(hits, `/${ch}/`).join('\n')).toContain('Peckboard Assistant mirror test')
    }

    // One Assistant turn, mirrored to both channels.
    const sessionRes = await request.post('/api/voice/session', {
      headers: auth,
      data: { model: 'mock:echo' },
    })
    expect(sessionRes.ok(), `voice session failed: ${await sessionRes.text()}`).toBeTruthy()
    sessionId = ((await sessionRes.json()) as { session_id: string }).session_id
    // The Assistant session is global: an earlier spec may have left a turn running.
    await request.post(`/api/sessions/${sessionId}/interrupt`, { headers: auth })
    await expect
      .poll(
        async () => {
          const s = await request.get(`/api/sessions/${sessionId}`, { headers: auth })
          return ((await s.json()) as { status?: string }).status
        },
        { timeout: 10_000 },
      )
      .not.toBe('running')

    const said = 'mirror check wombat seven'
    const send = await request.post(`/api/sessions/${sessionId}/message`, {
      headers: auth,
      data: { text: said },
    })
    expect(send.ok(), `send failed: ${await send.text()}`).toBeTruthy()

    for (const ch of ['slack', 'discord']) {
      await expect
        .poll(() => texts(hits, `/${ch}/`).some((t) => t.startsWith('You: ') && t.includes(said)), {
          timeout: 15_000,
        })
        .toBe(true)
      await expect
        .poll(
          () =>
            texts(hits, `/${ch}/`).some((t) => t.startsWith('Assistant: ') && t.includes('wombat')),
          { timeout: 15_000 },
        )
        .toBe(true)
    }
  } finally {
    await resetMirror(request, token)
    // Leave the global Assistant transcript empty for the voice specs.
    if (sessionId) {
      await request.post(`/api/sessions/${sessionId}/interrupt`, { headers: auth })
      await request.post(`/api/sessions/${sessionId}/clear`, { headers: auth })
    }
    // The server's HTTP client keeps its connections alive; drop them or
    // close() never resolves.
    server.closeAllConnections()
    await new Promise<void>((resolve) => server.close(() => resolve()))
  }
})
