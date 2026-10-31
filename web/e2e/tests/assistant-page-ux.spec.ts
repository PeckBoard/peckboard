import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync, readFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Full-page Assistant (`/assistant`) surroundings: live captions under the
 * orb, the held-relay queue badge (`GET /api/voice/queue`, tap = `POST
 * /api/voice/queue/next`), the Upcoming panel of Assistant-target tasks, and
 * the page's model picker.
 *
 * Speech is stubbed like voice-assistant.spec: `__voiceInterim` / `__voiceSay`
 * feed the recognizer; `speechSynthesis.speak` fires the utterance's
 * `onstart` (captions follow audio start) and, while `__ttsHold` is set,
 * keeps it "playing" until `__ttsFinish()`. Models are `mock:*`.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type StubWindow = {
  __voiceSay: (t: string) => void
  __voiceInterim: (t: string) => void
  __spoken: string[]
  __ttsHold: boolean
  __ttsFinish: () => void
}

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  const authHeader = { Authorization: `Bearer ${token}` }
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-assistant-ux-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `assistant-ux-${path.basename(folderPath)}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  return { token, authHeader, folderId: folder.id }
}

async function setVoiceModel(request: APIRequestContext, token: string, model?: string) {
  const res = await request.post('/api/voice/session', {
    headers: { Authorization: `Bearer ${token}` },
    data: model ? { model } : {},
  })
  expect(res.ok(), `voice session failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as { session_id: string; model: string }
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

/** Speech stubs + auth token, before any app script runs. */
async function primePage(page: Page, token: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
    type Handler<T> = ((ev: T) => void) | null
    /** The live recognition session (at most one). */
    const live: FakeRecognition[] = []
    class FakeRecognition {
      lang = ''
      continuous = false
      interimResults = false
      maxAlternatives = 1
      onresult: Handler<unknown> = null
      onend: Handler<void> = null
      onerror: Handler<{ error: string }> = null
      start() {
        live.splice(0, live.length, this)
      }
      stop() {
        this.finish()
      }
      abort() {
        this.finish()
      }
      finish() {
        if (live[0] === this) live.length = 0
        this.onend?.()
      }
      deliver(text: string, isFinal: boolean) {
        const result = Object.assign([{ transcript: text }], { isFinal, length: 1 })
        this.onresult?.({ resultIndex: 0, results: [result] })
      }
    }
    const w = window as unknown as Record<string, unknown>
    w.SpeechRecognition = FakeRecognition
    w.webkitSpeechRecognition = FakeRecognition
    const rec = () => {
      if (!live[0]) throw new Error('no live recognition')
      return live[0]
    }
    w.__voiceInterim = (text: string) => rec().deliver(text, false)
    w.__voiceSay = (text: string) => rec().deliver(text, true)

    w.__spoken = [] as string[]
    w.__ttsHold = false
    class FakeUtterance {
      text: string
      voice: unknown = null
      lang = ''
      rate = 1
      pitch = 1
      volume = 1
      onstart: (() => void) | null = null
      onend: (() => void) | null = null
      onerror: ((ev: { error: string }) => void) | null = null
      constructor(text: string) {
        this.text = text
      }
    }
    w.SpeechSynthesisUtterance = FakeUtterance
    let current: FakeUtterance[] = []
    const synth = {
      speaking: false,
      pending: false,
      paused: false,
      getVoices: () => [
        { voiceURI: 'stub', name: 'Stub', lang: 'en-US', default: true, localService: true },
      ],
      speak: (u: FakeUtterance) => {
        if (!u.text.trim()) return
        ;(w.__spoken as string[]).push(u.text)
        setTimeout(() => u.onstart?.(), 1)
        if (w.__ttsHold) current.push(u)
        else setTimeout(() => u.onend?.(), 5)
      },
      cancel: () => {
        const held = current
        current = []
        for (const u of held) u.onerror?.({ error: 'interrupted' })
      },
      pause: () => {},
      resume: () => {},
      addEventListener: () => {},
      removeEventListener: () => {},
    }
    w.__ttsFinish = () => {
      const held = current
      current = []
      for (const u of held) u.onend?.()
    }
    Object.defineProperty(window, 'speechSynthesis', { value: synth, configurable: true })
    // Browser voice: the stub above, not the speech server.
    localStorage.setItem(
      'peckboard_voice_prefs',
      JSON.stringify({ voiceURI: 'stub', captions: true, thinkingFiller: false }),
    )
  }, token)
}

async function openAssistant(page: Page) {
  await page.goto('/')
  await page.getByTestId('voice-fab').click()
  await expect(page).toHaveURL(/\/assistant$/)
  await expect(page.getByTestId('assistant-page')).toBeVisible()
}

async function cleanVoice(request: APIRequestContext, authHeader: Record<string, string>) {
  // On a model that answers, so anything queued behind a cancelled turn
  // drains instead of starting another blocked one.
  const voice = await request.post('/api/voice/session', {
    headers: authHeader,
    data: { model: 'mock:echo' },
  })
  const { session_id } = (await voice.json()) as { session_id: string }
  await request.post(`/api/sessions/${session_id}/cancel`, { headers: authHeader })
  // Shared with the other voice specs: leave the transcript empty.
  await request.post(`/api/sessions/${session_id}/clear`, { headers: authHeader })
}

test('live captions show interim speech, then the sentence being read aloud', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token)
  try {
    await openAssistant(page)
    const captions = page.getByTestId('assistant-captions')
    await expect(captions).toHaveAttribute('role', 'status')

    await page.evaluate(() =>
      (window as unknown as StubWindow).__voiceInterim('caption check please'),
    )
    await expect(captions).toHaveAttribute('data-visible', 'true')
    await expect(captions).toHaveAttribute('data-speaker', 'user')
    await expect(captions).toContainText('caption check please')

    // The echo reply is read aloud: the caption is the sentence playing.
    await page.evaluate(() => {
      const w = window as unknown as StubWindow
      w.__ttsHold = true
      w.__voiceSay('Caption check please.')
    })
    await expect(captions).toHaveAttribute('data-speaker', 'assistant', { timeout: 15_000 })
    await expect(captions).toContainText('Caption check please.')

    // Done speaking: the caption lingers, then fades out.
    await page.evaluate(() => {
      const w = window as unknown as StubWindow
      w.__ttsHold = false
      w.__ttsFinish()
    })
    await expect(captions).toHaveAttribute('data-visible', 'false', { timeout: 10_000 })
  } finally {
    await cleanVoice(request, authHeader)
  }
})

test('a held relay shows as a queue badge, and tapping it hands it to the Assistant', async ({
  request,
  page,
}) => {
  const { token, authHeader, folderId } = await authenticate(request)
  const voice = await setVoiceModel(request, token, 'mock:block')
  // A live (blocked) voice turn keeps its MCP token valid.
  const go = await request.post(`/api/sessions/${voice.session_id}/message`, {
    headers: authHeader,
    data: { text: 'hello' },
  })
  expect(go.ok(), await go.text()).toBeTruthy()
  const mcpToken = await mcpTokenOf(voice.session_id)
  try {
    // The assistant owes a reply: the gate holds every relay.
    const sent = await request.post('/api/voice/activity', {
      headers: authHeader,
      data: { session_id: voice.session_id, state: 'sent' },
    })
    expect(sent.status()).toBe(204)
    const work = await request.post('/api/sessions', {
      headers: authHeader,
      data: { name: 'stashify dev', folder_id: folderId, model: 'mock:echo' },
    })
    expect(work.ok(), await work.text()).toBeTruthy()
    const workId = ((await work.json()) as { id: string }).id
    // The assistant hands it work: linked, so its turn end is relayed.
    const call = await request.post('/mcp', {
      headers: { Authorization: `Bearer ${mcpToken}` },
      data: {
        jsonrpc: '2.0',
        id: 1,
        method: 'tools/call',
        params: { name: 'send_message', arguments: { session_id: workId, text: 'build it' } },
      },
    })
    expect(call.ok(), await call.text()).toBeTruthy()
    const callBody = (await call.json()) as { result?: { isError?: boolean } }
    expect(callBody.result?.isError, JSON.stringify(callBody)).toBeFalsy()
    await expect
      .poll(
        async () => {
          const res = await request.get('/api/voice/queue', { headers: authHeader })
          return ((await res.json()) as { count: number }).count
        },
        { timeout: 20_000 },
      )
      .toBe(1)

    await primePage(page, token)
    await openAssistant(page)
    const badge = page.getByTestId('assistant-queue-badge')
    await expect(badge).toHaveText('1 update waiting')
    await badge.click()
    await expect(badge).toHaveCount(0)
    const after = await request.get('/api/voice/queue', { headers: authHeader })
    expect(((await after.json()) as { count: number }).count).toBe(0)

    // Delivered: once the blocked turn is stopped (on a model that answers),
    // the relay is the Assistant's next turn.
    await setVoiceModel(request, token, 'mock:echo')
    await request.post(`/api/sessions/${voice.session_id}/cancel`, { headers: authHeader })
    await expect
      .poll(
        async () => {
          const res = await request.get(`/api/sessions/${voice.session_id}/events?limit=500`, {
            headers: authHeader,
          })
          const events = (await res.json()) as { kind: string; data: { text?: string } }[]
          return events.some(
            (e) =>
              e.kind === 'user' &&
              (e.data.text ?? '').startsWith('[relay] update from "stashify dev"'),
          )
        },
        { timeout: 20_000 },
      )
      .toBe(true)
  } finally {
    await cleanVoice(request, authHeader)
  }
})

test('Upcoming lists the Assistant’s scheduled tasks and Delete removes one', async ({
  request,
  page,
}) => {
  const { token, authHeader, folderId } = await authenticate(request)
  await setVoiceModel(request, token, 'mock:echo')
  const at = new Date(Date.now() + 2 * 3600_000)
  const pad = (n: number) => String(n).padStart(2, '0')
  const local = `${at.getUTCFullYear()}-${pad(at.getUTCMonth() + 1)}-${pad(at.getUTCDate())}T${pad(at.getUTCHours())}:${pad(at.getUTCMinutes())}`
  const created = await request.post('/api/repeating-tasks', {
    headers: authHeader,
    data: {
      name: 'Remind me to call Sam',
      folder_id: folderId,
      prompt: 'Remind the user to call Sam.',
      schedule_kind: 'once',
      schedule_value: { at: local },
      timezone: null,
      target: 'assistant',
    },
  })
  expect(created.ok(), await created.text()).toBeTruthy()
  const task = (await created.json()) as { id: string }

  await primePage(page, token)
  try {
    await openAssistant(page)
    await page.getByTestId('assistant-upcoming-toggle').click()
    const panel = page.getByTestId('assistant-upcoming')
    await expect(panel).toHaveAttribute('data-open', 'true')
    const row = panel.locator(`[data-row-id="${task.id}"]`)
    await expect(row).toContainText('Remind me to call Sam')
    await expect(row).toContainText(/in (2 hours|120 minutes)/)

    await row.getByRole('button', { name: 'Row menu' }).click()
    await page.getByTestId('assistant-upcoming-delete').click()
    await page.getByTestId('confirm-dialog-confirm').click()
    await expect(row).toHaveCount(0)
    const list = await request.get('/api/repeating-tasks', { headers: authHeader })
    const ids = ((await list.json()) as { id: string }[]).map((t) => t.id)
    expect(ids).not.toContain(task.id)
  } finally {
    await request.delete(`/api/repeating-tasks/${task.id}`, { headers: authHeader })
    await cleanVoice(request, authHeader)
  }
})

test('the page’s model picker switches the Assistant’s model', async ({ request, page }) => {
  const { token, authHeader } = await authenticate(request)
  await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token)
  try {
    await openAssistant(page)
    const picker = page.getByTestId('assistant-model')
    await expect(picker).toBeVisible()
    await picker.click()
    await page.getByTestId('assistant-model-search').fill('happy')
    await page.getByTestId('assistant-model-option-mock:happy-path').click()
    await expect
      .poll(async () => (await setVoiceModel(request, token)).model)
      .toBe('mock:happy-path')
  } finally {
    await setVoiceModel(request, token, 'mock:echo')
    await cleanVoice(request, authHeader)
  }
})
