import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Voice assistant e2e.
 *
 * The browser's Web Speech API is stubbed via `addInitScript`:
 * `SpeechRecognition` records the live instance so a test can inject what
 * the user says (`__voiceInterim(text)` for a partial hypothesis,
 * `__voiceSay(text)` for a final one), and `speechSynthesis.speak` appends
 * every utterance's text to `window.__spoken` so we can assert what was
 * (and was not) read aloud. Utterances finish on their own after 5ms unless
 * `__ttsHold` is set, which keeps the assistant "speaking" until
 * `cancel()` — that is how barge-in is exercised. The assistant runs on
 * `mock:*` models so replies are deterministic.
 *
 * Backend contract exercised: `POST /api/voice/session` (get-or-create the
 * user's voice session, optionally switching its model), the normal
 * `/api/sessions/:id/message` send route, `/interrupt`, and the WS event
 * stream.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

/** Log in and make sure at least one folder exists: the voice session is
 *  created inside the user's most recent folder, so a bare install (as the
 *  e2e server starts) has to register one first. */
async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-voice-'))
  const folderRes = await request.post('/api/folders', {
    headers: { Authorization: `Bearer ${token}` },
    data: { name: `voice-${path.basename(folderPath)}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  return token
}

type StubWindow = {
  __voiceRec: { finish: () => void } | null
  __recStarts: number
  __voiceSay: (t: string) => void
  __voiceInterim: (t: string) => void
  __spoken: string[]
  __cancels: number
  __ttsHold: boolean
}

/** Install the Web Speech stubs + the auth token before any app script runs. */
async function primePage(page: Page, token: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)

    type Handler<T> = ((ev: T) => void) | null
    class FakeRecognition {
      lang = ''
      continuous = false
      interimResults = false
      maxAlternatives = 1
      onresult: Handler<unknown> = null
      onend: Handler<void> = null
      onerror: Handler<{ error: string }> = null
      start() {
        const w = window as unknown as { __voiceRec: FakeRecognition | null; __recStarts: number }
        w.__voiceRec = this
        w.__recStarts = (w.__recStarts ?? 0) + 1
      }
      stop() {
        this.finish()
      }
      abort() {
        this.finish()
      }
      finish() {
        const w = window as unknown as { __voiceRec: FakeRecognition | null }
        if (w.__voiceRec === this) w.__voiceRec = null
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
    w.__voiceRec = null
    w.__recStarts = 0
    const live = () => {
      const rec = (window as unknown as { __voiceRec: FakeRecognition | null }).__voiceRec
      if (!rec) throw new Error('no live recognition')
      return rec
    }
    // A final transcript. Continuous recognition keeps the session open;
    // a non-continuous one ends after its result, like the real engine.
    w.__voiceSay = (text: string) => {
      const rec = live()
      rec.deliver(text, true)
      if (!rec.continuous) rec.finish()
    }
    w.__voiceInterim = (text: string) => live().deliver(text, false)

    w.__spoken = [] as string[]
    w.__cancels = 0
    w.__ttsHold = false
    class FakeUtterance {
      text: string
      voice: unknown = null
      lang = ''
      rate = 1
      pitch = 1
      volume = 1
      onend: (() => void) | null = null
      onerror: ((ev: { error: string }) => void) | null = null
      constructor(text: string) {
        this.text = text
      }
    }
    w.SpeechSynthesisUtterance = FakeUtterance
    const voices = [
      {
        voiceURI: 'stub-alpha',
        name: 'Stub Alpha',
        lang: 'en-US',
        default: true,
        localService: true,
      },
      {
        voiceURI: 'stub-beta',
        name: 'Stub Beta',
        lang: 'en-GB',
        default: false,
        localService: true,
      },
    ]
    let current: FakeUtterance[] = []
    const synth = {
      speaking: false,
      pending: false,
      paused: false,
      getVoices: () => voices,
      speak: (u: FakeUtterance) => {
        // The silent unlock utterance isn't speech worth asserting on.
        if (!u.text.trim()) return
        ;(w.__spoken as string[]).push(u.text)
        if (w.__ttsHold) current.push(u)
        else setTimeout(() => u.onend?.(), 5)
      },
      cancel: () => {
        w.__cancels = (w.__cancels as number) + 1
        const held = current
        current = []
        for (const u of held) u.onerror?.({ error: 'interrupted' })
      },
      pause: () => {},
      resume: () => {},
      addEventListener: () => {},
      removeEventListener: () => {},
    }
    Object.defineProperty(window, 'speechSynthesis', { value: synth, configurable: true })
  }, token)
}

async function spoken(page: Page): Promise<string[]> {
  return page.evaluate(() => (window as unknown as StubWindow).__spoken)
}

async function setVoiceModel(request: APIRequestContext, token: string, model: string) {
  const res = await request.post('/api/voice/session', {
    headers: { Authorization: `Bearer ${token}` },
    data: { model },
  })
  expect(res.ok(), `voice session failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as { session_id: string; model: string }
}

async function sessionEvents(
  request: APIRequestContext,
  token: string,
  sessionId: string,
): Promise<{ seq: number; kind: string; data: Record<string, unknown> }[]> {
  const res = await request.get(`/api/sessions/${sessionId}/events?limit=500`, {
    headers: { Authorization: `Bearer ${token}` },
  })
  expect(res.ok()).toBeTruthy()
  return res.json()
}

test('Listen opens an always-on mic; an utterance is answered, spoken, and the mic stays live', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token)
  await page.goto('/')

  const fab = page.getByTestId('voice-fab')
  await expect(fab).toBeVisible({ timeout: 10_000 })
  await fab.click()

  const panel = page.getByTestId('voice-panel')
  await expect(panel).toBeVisible()
  await expect(panel).toContainText('Voice Assistant')
  // No mic press needed: opening the panel starts continuous listening.
  await expect(page.getByTestId('voice-status')).toHaveText('Listening')
  await expect(page.getByTestId('voice-mic')).toHaveAttribute('aria-pressed', 'true')
  await expect
    .poll(() => page.evaluate(() => (window as unknown as StubWindow).__recStarts))
    .toBeGreaterThan(0)

  await page.evaluate(() =>
    (window as unknown as StubWindow).__voiceSay('Hello voice assistant. How are you?'),
  )

  // The utterance shows as a user line; the echo reply lands and is spoken
  // sentence by sentence.
  await expect(page.getByTestId('voice-line-user').first()).toContainText('Hello voice assistant.')
  await expect(page.getByTestId('voice-line-assistant').first()).toContainText(
    'Hello voice assistant.',
    { timeout: 15_000 },
  )
  await expect.poll(async () => spoken(page)).toContain('Hello voice assistant.')
  await expect.poll(async () => spoken(page)).toContain('How are you?')

  // Back to listening — and recognition is still (or again) live, with no
  // button press: a second utterance goes straight through.
  await expect(page.getByTestId('voice-status')).toHaveText('Listening', { timeout: 10_000 })
  await expect
    .poll(() => page.evaluate(() => (window as unknown as StubWindow).__voiceRec !== null))
    .toBe(true)
  await page.evaluate(() => (window as unknown as StubWindow).__voiceSay('second turn please'))
  await expect.poll(async () => spoken(page), { timeout: 15_000 }).toContain('second turn please')

  // The mic button mutes the always-on mic.
  await page.getByTestId('voice-mic').click()
  await expect(page.getByTestId('voice-status')).toHaveText('Idle')
  await expect(page.getByTestId('voice-mic')).toHaveAttribute('aria-pressed', 'false')

  // Close stops everything and brings the button back.
  await page.getByTestId('voice-close').click()
  await expect(panel).toBeHidden()
  await expect(fab).toBeVisible()
})

test('typed fallback sends without the microphone', async ({ request, page }) => {
  const token = await authenticate(request)
  await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token)
  await page.goto('/')

  await page.getByTestId('voice-fab').click()
  await page.getByTestId('voice-type-input').fill('typed hello')
  await page.getByTestId('voice-type-send').click()
  await expect(page.getByTestId('voice-line-user').last()).toContainText('typed hello')
  await expect(page.getByTestId('voice-line-assistant').last()).toContainText('typed hello', {
    timeout: 15_000,
  })
  await expect
    .poll(async () => (await spoken(page)).some((s) => s.includes('typed hello')))
    .toBe(true)
})

test('relayed updates render muted and are not spoken; the reply to them is', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const voice = await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token)
  await page.goto('/')

  await page.getByTestId('voice-fab').click()
  await expect(page.getByTestId('voice-panel')).toBeVisible()

  // Simulate the backend injecting a relay line: a user-role message with
  // the `[relay] ` prefix. `mock:happy-path` ignores its input and replies
  // "Working on it...", runs a tool, then "Done.", so the only way
  // "[relay]" could be spoken is if the UI read the relay line itself.
  const relay = '[relay] Worker session "api-refactor" asks: should I also update the docs?'
  const res = await request.post(`/api/sessions/${voice.session_id}/message`, {
    headers: { Authorization: `Bearer ${token}` },
    data: { text: relay, model: 'mock:happy-path' },
  })
  expect(res.ok(), `relay send failed: ${await res.text()}`).toBeTruthy()

  const relayLine = page.getByTestId('voice-line-relay').first()
  await expect(relayLine).toBeVisible({ timeout: 15_000 })
  await expect(relayLine).toContainText('Worker session "api-refactor" asks')
  await expect(relayLine).not.toContainText('[relay]')
  // Not a user bubble. (The voice session is shared across tests, so
  // earlier utterances may still be in the transcript — only the relay
  // text must be absent from user bubbles.)
  await expect(page.getByTestId('voice-line-user').filter({ hasText: 'api-refactor' })).toHaveCount(
    0,
  )

  await expect(page.getByTestId('voice-line-assistant').last()).toContainText('Done.', {
    timeout: 15_000,
  })
  await expect.poll(async () => spoken(page)).toContain('Done.')
  const all = await spoken(page)
  // Text before the tool call was spoken as its own chunk, ahead of "Done.".
  expect(all.indexOf('Working on it...')).toBeGreaterThanOrEqual(0)
  expect(all.indexOf('Working on it...')).toBeLessThan(all.indexOf('Done.'))
  expect(all.some((s) => s.includes('[relay]') || s.includes('api-refactor'))).toBe(false)
})

test('speech streams while the turn runs; talking over it ignores echo, then barges in', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  // `mock:block` says "working…" and then holds the turn open until it is
  // interrupted — a reply that is still streaming.
  const voice = await setVoiceModel(request, token, 'mock:block')
  await primePage(page, token)
  await page.goto('/')
  const cancels = () => page.evaluate(() => (window as unknown as StubWindow).__cancels)

  await page.getByTestId('voice-fab').click()
  await expect(page.getByTestId('voice-status')).toHaveText('Listening')
  await page.evaluate(() => {
    ;(window as unknown as StubWindow).__ttsHold = true
  })

  await page.evaluate(() => (window as unknown as StubWindow).__voiceSay('start the long job'))
  await expect(page.getByTestId('voice-line-user').last()).toContainText('start the long job')

  // The streamed text is spoken while the turn is still running — no
  // waiting for the end of the turn.
  await expect.poll(async () => spoken(page), { timeout: 15_000 }).toContain('working…')
  await expect(page.getByTestId('voice-status')).toHaveText('Speaking')
  expect((await sessionEvents(request, token, voice.session_id)).at(-1)?.kind).not.toBe('agent-end')
  const cancelsBefore = await cancels()

  // The mic hearing the assistant's own voice is not the user talking.
  await page.evaluate(() => (window as unknown as StubWindow).__voiceInterim('working'))
  await page.waitForTimeout(300)
  await expect(page.getByTestId('voice-status')).toHaveText('Speaking')
  expect(await cancels()).toBe(cancelsBefore)
  await expect(page.getByTestId('voice-interim')).toHaveCount(0)

  // The user talks over it: speech stops at once and the turn is interrupted.
  await page.evaluate(() => (window as unknown as StubWindow).__voiceInterim('no wait actually'))
  await expect.poll(cancels).toBeGreaterThan(cancelsBefore)
  await expect(page.getByTestId('voice-status')).not.toHaveText('Speaking')
  await expect(page.getByTestId('voice-interim')).toContainText('no wait actually')

  await page.evaluate(() =>
    (window as unknown as StubWindow).__voiceSay('no wait actually do the short one'),
  )
  await expect(page.getByTestId('voice-line-user').last()).toContainText(
    'no wait actually do the short one',
    { timeout: 10_000 },
  )
  // The interrupted turn ended before the new utterance reached the session.
  await expect
    .poll(
      async () => {
        const events = await sessionEvents(request, token, voice.session_id)
        const firstTurn = events.findIndex(
          (e) => e.kind === 'user' && e.data.text === 'start the long job',
        )
        const endIdx = events.findIndex((e, i) => i > firstTurn && e.kind === 'agent-end')
        const nextUser = events.findIndex(
          (e) => e.kind === 'user' && e.data.text === 'no wait actually do the short one',
        )
        return firstTurn >= 0 && endIdx > firstTurn && nextUser > endIdx
      },
      { timeout: 15_000 },
    )
    .toBe(true)

  // Leave the shared voice session idle for whatever runs next.
  await request.post(`/api/sessions/${voice.session_id}/interrupt`, {
    headers: { Authorization: `Bearer ${token}` },
  })
})

test('Settings → Voice persists the chosen voice across reloads', async ({ request, page }) => {
  const token = await authenticate(request)
  await primePage(page, token)
  await page.goto('/settings/voice')

  const settings = page.getByTestId('settings-page')
  await expect(settings).toBeVisible({ timeout: 10_000 })
  await expect(settings).toHaveAttribute('data-sub', 'voice')
  await expect(page.getByTestId('voice-speech-section')).toBeVisible()

  const select = page.getByTestId('voice-voice-select')
  await expect(select.locator('option', { hasText: 'Stub Beta' })).toHaveCount(1)
  await select.selectOption('stub-beta')
  await page.getByTestId('voice-rate').fill('1.5')
  await expect(page.getByTestId('voice-rate-value')).toHaveText('1.5×')
  await page.getByTestId('voice-auto-listen').uncheck()

  // Test voice speaks a sample right away.
  await page.getByTestId('voice-test').click()
  await expect.poll(() => spoken(page)).toHaveLength(1)

  await page.reload()
  await expect(page.getByTestId('voice-voice-select')).toHaveValue('stub-beta', {
    timeout: 10_000,
  })
  await expect(page.getByTestId('voice-rate')).toHaveValue('1.5')
  await expect(page.getByTestId('voice-auto-listen')).not.toBeChecked()

  // The model section reads the voice session's model from the backend.
  await expect(page.getByTestId('voice-model')).toBeVisible()
  await expect(page.getByTestId('voice-model')).not.toContainText('Loading…', {
    timeout: 10_000,
  })
})
