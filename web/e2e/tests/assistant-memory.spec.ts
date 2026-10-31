import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync, readFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Assistant temporary memory + automatic context clear.
 *
 * The spec plays the assistant model by calling its MCP tools with the voice
 * session's own token (the voice session runs `mock:block`, so its agent —
 * and token — stay live while a turn is open). Covered:
 *  - closing a topic with memory lists the entry, with its expiry, in
 *    Settings → Assistant → Memory, and shows the pending clear;
 *  - with clear_delay_minutes = 0 the clear fires once the turn ends: the
 *    transcript shows "Context cleared …", and the stored prompt carries
 *    the memory in its Recent Memory snapshot;
 *  - deleting an entry from the list;
 *  - auto_clear = off (set in the UI): closing another topic schedules no
 *    clear and the conversation is kept.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const DEFAULTS = {
  memory_enabled: true,
  retention_hours: 36,
  max_entries: 40,
  auto_clear: 'after_topic',
  clear_delay_minutes: 2,
  idle_topic_end_minutes: 30,
}

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  const authHeader = { Authorization: `Bearer ${token}` }
  // The Assistant session needs a folder to live in.
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-memory-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `memory-${path.basename(folderPath)}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  return { token, authHeader }
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
  if (!res.ok()) return { isError: true, text: `HTTP ${res.status()}` }
  const body = (await res.json()) as {
    result?: { content: { text?: string }[]; isError?: boolean }
    error?: { message: string }
  }
  if (body.error) return { isError: true, text: body.error.message }
  return { isError: !!body.result?.isError, text: body.result?.content[0]?.text ?? '' }
}

/** The voice session's live MCP token, from the config its current agent
 *  was spawned with (polled until the token works: a respawn rewrites it). */
async function liveMcpToken(request: APIRequestContext, sessionId: string): Promise<string> {
  const cfgPath = path.join(process.env.PECKBOARD_E2E_DATA_DIR!, 'worker-mcp', `${sessionId}.json`)
  let token = ''
  await expect
    .poll(async () => {
      try {
        const cfg = JSON.parse(readFileSync(cfgPath, 'utf8')) as {
          mcpServers: { peckboard: { headers: { Authorization: string } } }
        }
        token = cfg.mcpServers.peckboard.headers.Authorization.replace(/^Bearer /, '')
      } catch {
        return false
      }
      return !(await mcpTool(request, token, 'assistant_memory', { action: 'list' })).isError
    })
    .toBe(true)
  return token
}

async function topics(
  request: APIRequestContext,
  mcpToken: string,
  args: Record<string, unknown>,
): Promise<{ topics: { id: string; title: string }[] }> {
  const r = await mcpTool(request, mcpToken, 'assistant_topics', args)
  expect(r.isError, r.text).toBeFalsy()
  return JSON.parse(r.text)
}

async function openWithFakeSpeech(page: Page, token: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
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
}

test('assistant memory survives the automatic context clear and is managed in Settings', async ({
  request,
  page,
}) => {
  test.setTimeout(120_000)
  const { token, authHeader } = await authenticate(request)
  const put = (data: Record<string, unknown>) =>
    request.put('/api/voice/settings', { headers: authHeader, data })

  const voiceRes = await request.post('/api/voice/session', {
    headers: authHeader,
    data: { model: 'mock:block' },
  })
  expect(voiceRes.ok(), await voiceRes.text()).toBeTruthy()
  const voiceId = ((await voiceRes.json()) as { session_id: string }).session_id
  const send = async (text: string) => {
    const r = await request.post(`/api/sessions/${voiceId}/message`, {
      headers: authHeader,
      data: { text },
    })
    expect(r.ok(), await r.text()).toBeTruthy()
  }
  const events = async () => {
    const res = await request.get(`/api/sessions/${voiceId}/events?limit=500`, {
      headers: authHeader,
    })
    return (await res.json()) as { kind: string; data: { text?: string; kind?: string } }[]
  }

  let mcpToken = ''
  try {
    expect((await put({ ...DEFAULTS, clear_delay_minutes: 0 })).ok()).toBeTruthy()
    await request.delete('/api/voice/memory', { headers: authHeader })

    // A live (blocked) turn: the token works, and the clear waits for it.
    await send('hello')
    mcpToken = await liveMcpToken(request, voiceId)
    const ring = await topics(request, mcpToken, { action: 'add', title: 'Dinner plans' })
    const dinner = ring.topics.find((t) => t.title === 'Dinner plans')!
    await topics(request, mcpToken, {
      action: 'close',
      id: dinner.id,
      memory: [{ kind: 'did', summary: 'Booked the table for eight.' }],
    })

    // Settings → Assistant → Memory lists it with an expiry, and the
    // pending clear (held back by the running turn).
    await openWithFakeSpeech(page, token)
    await page.goto('/settings/assistant')
    const section = page.getByTestId('voice-memory-section')
    await expect(section).toBeVisible({ timeout: 10_000 })
    await expect(section).toContainText('Booked the table for eight.')
    await expect(section).toContainText('Dinner plans')
    const listed = (await (
      await request.get('/api/voice/memory', { headers: authHeader })
    ).json()) as { entries: { id: string; expires_at: string }[] }
    expect(listed.entries).toHaveLength(1)
    const entryId = listed.entries[0].id
    await expect(page.getByTestId(`voice-memory-expiry-${entryId}`)).toContainText(
      /expires in 3\dh/,
    )
    await expect(page.getByTestId('voice-memory-pending')).toContainText('Dinner plans')

    // The Assistant page is open when the turn ends: the clear fires, and
    // the transcript shows it as a status line.
    await page.goto('/assistant')
    await expect(page.getByTestId('assistant-page')).toBeVisible()
    await request.post(`/api/sessions/${voiceId}/cancel`, { headers: authHeader })
    await expect(page.getByTestId('voice-line-status')).toContainText(
      'Context cleared after "Dinner plans" · 1 memory kept',
      { timeout: 20_000 },
    )
    const after = await events()
    expect(after.filter((e) => e.kind === 'user')).toHaveLength(0)
    expect(
      after.some((e) => e.kind === 'system' && e.data.kind === 'assistant-context-cleared'),
    ).toBe(true)
    const session = (await (
      await request.get(`/api/sessions/${voiceId}`, { headers: authHeader })
    ).json()) as { system_prompt: string }
    expect(session.system_prompt).toContain('## Recent Memory')
    expect(session.system_prompt).toContain('Booked the table for eight.')

    // Delete the entry from the list.
    await page.goto('/settings/assistant')
    await expect(page.getByTestId(`voice-memory-entry-${entryId}`)).toBeVisible({
      timeout: 10_000,
    })
    await expect(page.getByTestId('voice-memory-last-clear')).toContainText('Dinner plans')
    await page.locator(`[data-row-id="${entryId}"] .list-view-menu`).click()
    await page.getByTestId(`voice-memory-delete-${entryId}`).click()
    await expect(page.getByTestId(`voice-memory-entry-${entryId}`)).toHaveCount(0)
    await expect(page.getByTestId('voice-memory-empty')).toBeVisible()
    await expect
      .poll(async () => {
        const res = await request.get('/api/voice/memory', { headers: authHeader })
        return ((await res.json()) as { entries: unknown[] }).entries.length
      })
      .toBe(0)

    // A bad number is caught inline and never saved.
    await page.getByTestId('voice-memory-retention').fill('500')
    await expect(page.getByTestId('voice-memory-retention-error')).toContainText('1 and 168')

    // auto_clear off: closing another topic schedules nothing.
    await page.getByTestId('voice-memory-auto-clear').selectOption('off')
    await expect
      .poll(async () => {
        const res = await request.get('/api/voice/settings', { headers: authHeader })
        return ((await res.json()) as { auto_clear: string; retention_hours: number }).auto_clear
      })
      .toBe('off')
    await send('next topic')
    mcpToken = await liveMcpToken(request, voiceId)
    const ring2 = await topics(request, mcpToken, { action: 'add', title: 'Garden' })
    const garden = ring2.topics.find((t) => t.title === 'Garden')!
    await topics(request, mcpToken, { action: 'close', id: garden.id })
    const status = (await (
      await request.get('/api/voice/memory/status', { headers: authHeader })
    ).json()) as { pending: unknown }
    expect(status.pending).toBeNull()
    await request.post(`/api/sessions/${voiceId}/cancel`, { headers: authHeader })
    // Give the 5s auto-clear ticker a chance to (wrongly) fire.
    await page.waitForTimeout(7_000)
    const kept = await events()
    expect(kept.some((e) => e.kind === 'user' && e.data.text === 'next topic')).toBe(true)
    // The closed topic still got its fallback memory (title + ring summary).
    const mem = (await (
      await request.get('/api/voice/memory', { headers: authHeader })
    ).json()) as {
      entries: { topic_title: string; source: string }[]
    }
    expect(mem.entries.some((m) => m.topic_title === 'Garden' && m.source === 'fallback')).toBe(
      true,
    )
  } finally {
    // Shared voice session state: no topics, no memory, default settings,
    // empty transcript (other voice specs check the first user line).
    await put({ ...DEFAULTS, auto_clear: 'off' })
    if (mcpToken) {
      const listed = await mcpTool(request, mcpToken, 'assistant_topics', { action: 'list' })
      if (!listed.isError) {
        for (const t of (JSON.parse(listed.text) as { topics: { id: string }[] }).topics) {
          await mcpTool(request, mcpToken, 'assistant_topics', { action: 'close', id: t.id })
        }
      }
    }
    await request.post(`/api/sessions/${voiceId}/cancel`, { headers: authHeader })
    await request.delete('/api/voice/memory', { headers: authHeader })
    await put(DEFAULTS)
    await request.post(`/api/sessions/${voiceId}/clear`, { headers: authHeader })
  }
})
