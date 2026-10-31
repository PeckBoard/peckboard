import { test, expect, type APIRequestContext } from '../harness'
import { mkdtempSync, readFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Full-page Assistant (`/assistant`) and its topic ring.
 *
 * The assistant model curates the ring itself through its `assistant_topics`
 * MCP tool; the spec plays that model by calling the tool with the voice
 * session's own MCP token (the voice session runs `mock:block`, so its agent
 * — and token — stay live). Covered:
 *  - the voice FAB opens the page; tool-added topics render with their kinds;
 *  - clicking a topic plays the absorb animation, makes it current (shown in
 *    the orb), and sends the voice session "Let's talk about <title>.";
 *  - a focus the assistant makes over the tool reaches the page over WS.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  const authHeader = { Authorization: `Bearer ${token}` }
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-assistant-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `assistant-${path.basename(folderPath)}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  return { token, authHeader, folderId: folder.id }
}

/** The voice session's MCP token, from the config its agent was spawned with. */
async function mcpTokenOf(sessionId: string): Promise<string> {
  const cfgPath = path.join(process.env.PECKBOARD_E2E_DATA_DIR!, 'worker-mcp', `${sessionId}.json`)
  let raw = ''
  await expect
    .poll(() => {
      try {
        raw = readFileSync(cfgPath, 'utf8')
        return true
      } catch {
        return false
      }
    })
    .toBe(true)
  const cfg = JSON.parse(raw) as {
    mcpServers: { peckboard: { headers: { Authorization: string } } }
  }
  return cfg.mcpServers.peckboard.headers.Authorization.replace(/^Bearer /, '')
}

async function topicsTool(
  request: APIRequestContext,
  mcpToken: string,
  args: Record<string, unknown>,
): Promise<{ topics: { id: string; kind: string; title: string; current?: boolean }[] }> {
  const res = await request.post('/mcp', {
    headers: { Authorization: `Bearer ${mcpToken}` },
    data: {
      jsonrpc: '2.0',
      id: 1,
      method: 'tools/call',
      params: { name: 'assistant_topics', arguments: args },
    },
  })
  expect(res.ok()).toBeTruthy()
  const body = (await res.json()) as {
    result?: { content: { text?: string }[]; isError?: boolean }
  }
  expect(body.result?.isError, JSON.stringify(body)).toBeFalsy()
  return JSON.parse(body.result!.content[0].text!)
}

async function mcpTool(
  request: APIRequestContext,
  mcpToken: string,
  name: string,
  args: Record<string, unknown>,
): Promise<{ isError: boolean; text: string }> {
  const res = await request.post('/mcp', {
    headers: { Authorization: `Bearer ${mcpToken}` },
    data: { jsonrpc: '2.0', id: 1, method: 'tools/call', params: { name, arguments: args } },
  })
  expect(res.ok()).toBeTruthy()
  const body = (await res.json()) as {
    result?: { content: { text?: string }[]; isError?: boolean }
    error?: { message: string }
  }
  if (body.error) return { isError: true, text: body.error.message }
  return { isError: !!body.result?.isError, text: body.result?.content[0]?.text ?? '' }
}

test('topics the assistant curates render on the Assistant page and focus both ways', async ({
  request,
  page,
}) => {
  const { token, authHeader, folderId } = await authenticate(request)
  const voiceRes = await request.post('/api/voice/session', {
    headers: authHeader,
    data: { model: 'mock:block' },
  })
  expect(voiceRes.ok(), await voiceRes.text()).toBeTruthy()
  const voice = (await voiceRes.json()) as { session_id: string }
  const work = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'stashify dev', folder_id: folderId },
  })
  expect(work.ok()).toBeTruthy()
  const workId = ((await work.json()) as { id: string }).id

  // A live (blocked) voice turn, so its MCP token stays valid.
  const go = await request.post(`/api/sessions/${voice.session_id}/message`, {
    headers: authHeader,
    data: { text: 'hello' },
  })
  expect(go.ok(), await go.text()).toBeTruthy()
  const mcpToken = await mcpTokenOf(voice.session_id)

  try {
    await topicsTool(request, mcpToken, {
      action: 'add',
      kind: 'conversation',
      title: 'Dinner plans',
    })
    const ring = await topicsTool(request, mcpToken, {
      action: 'add',
      kind: 'session',
      title: 'Stashify build',
      ref_id: workId,
    })
    const dinner = ring.topics.find((t) => t.title === 'Dinner plans')!
    const stashify = ring.topics.find((t) => t.title === 'Stashify build')!

    await page.addInitScript((t) => {
      localStorage.setItem('peckboard_token', t)
      // A silent recognizer (the voice button needs one) and no real speech
      // output in the test browser.
      class FakeRecognition {
        lang = ''
        continuous = false
        interimResults = false
        maxAlternatives = 1
        onresult: unknown = null
        onerror: unknown = null
        onend: (() => void) | null = null
        start() {}
        stop() {
          this.onend?.()
        }
        abort() {
          this.onend?.()
        }
      }
      const w = window as unknown as Record<string, unknown>
      w.SpeechRecognition = FakeRecognition
      w.webkitSpeechRecognition = FakeRecognition
      Object.defineProperty(window, 'speechSynthesis', {
        value: {
          speak: () => {},
          cancel: () => {},
          pause: () => {},
          resume: () => {},
          getVoices: () => [],
          addEventListener: () => {},
          removeEventListener: () => {},
          speaking: false,
          pending: false,
          paused: false,
        },
        configurable: true,
      })
    }, token)
    await page.goto('/')

    // The voice button opens the full page.
    await page.getByTestId('voice-fab').click()
    await expect(page).toHaveURL(/\/assistant$/)
    await expect(page.getByTestId('assistant-page')).toBeVisible()

    const topic = page.getByTestId('assistant-topic')
    await expect(topic).toHaveCount(2)
    await expect(page.locator(`[data-topic-id="${dinner.id}"]`)).toHaveAttribute(
      'data-kind',
      'conversation',
    )
    await expect(page.locator(`[data-topic-id="${stashify.id}"]`)).toHaveAttribute(
      'data-kind',
      'session',
    )
    await expect(page.getByTestId('assistant-current-topic')).toHaveCount(0)

    // Click → absorb animation, the orb shows it, the assistant is told.
    await page.locator(`[data-topic-id="${dinner.id}"]`).click()
    await expect(page.getByTestId('assistant-absorbing')).toBeAttached()
    await expect(page.getByTestId('assistant-current-topic')).toContainText('Dinner plans')
    await expect(page.locator(`[data-topic-id="${dinner.id}"]`)).toHaveCount(0)
    await expect
      .poll(async () => {
        const res = await request.get(`/api/sessions/${voice.session_id}/events?limit=500`, {
          headers: authHeader,
        })
        const events = (await res.json()) as { kind: string; data: { text?: string } }[]
        return events.filter(
          (e) => e.kind === 'user' && e.data.text === "Let's talk about Dinner plans.",
        ).length
      })
      .toBe(1)

    // The assistant switches topic itself: the page follows over WS.
    await topicsTool(request, mcpToken, { action: 'focus', id: stashify.id })
    await expect(page.getByTestId('assistant-current-topic')).toContainText('Stashify build')
    await expect(page.locator(`[data-topic-id="${dinner.id}"]`)).toHaveCount(1)
    // Focus alone never moves the screen (auto_show_on_topic is off by default).
    await expect(page).toHaveURL(/\/assistant$/)

    // The user clicks the session topic on the ring: still no navigation, and
    // a show_view in that focus turn is refused — the ring and the screen are
    // separate concerns.
    await topicsTool(request, mcpToken, { action: 'focus', id: dinner.id })
    await expect(page.getByTestId('assistant-current-topic')).toContainText('Dinner plans')
    await page.locator(`[data-topic-id="${stashify.id}"]`).click()
    await expect(page.getByTestId('assistant-current-topic')).toContainText('Stashify build')
    await expect(page).toHaveURL(/\/assistant$/)
    await expect
      .poll(async () => {
        const res = await request.get(`/api/sessions/${voice.session_id}/events?limit=500`, {
          headers: authHeader,
        })
        const events = (await res.json()) as { kind: string; data: { text?: string } }[]
        return events.filter((e) => e.kind === 'user').at(-1)?.data.text
      })
      .toBe("Let's talk about Stashify build.")
    const refused = await mcpTool(request, mcpToken, 'show_view', {
      target: 'session',
      id: workId,
    })
    expect(refused.isError, refused.text).toBe(true)
    expect(refused.text).toContain('topic changes')
    await expect(page).toHaveURL(/\/assistant$/)

    // An explicit show_view outside a focus turn still navigates.
    await topicsTool(request, mcpToken, { action: 'focus', id: dinner.id })
    const shown = await mcpTool(request, mcpToken, 'show_view', {
      target: 'session',
      id: workId,
    })
    expect(shown.isError, shown.text).toBeFalsy()
    await expect(page).not.toHaveURL(/\/assistant$/)
  } finally {
    for (const t of (await topicsTool(request, mcpToken, { action: 'list' })).topics) {
      await topicsTool(request, mcpToken, { action: 'close', id: t.id })
    }
    await request.post(`/api/sessions/${voice.session_id}/cancel`, { headers: authHeader })
    // The voice session is per-user and shared with the other voice specs:
    // leave its transcript empty so their "first user line" checks hold.
    await request.post(`/api/sessions/${voice.session_id}/clear`, { headers: authHeader })
  }
})
