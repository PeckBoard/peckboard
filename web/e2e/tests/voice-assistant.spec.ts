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
/** Prefix the client puts on an utterance that talked over the assistant. */
const INTERRUPT_MARKER =
  '[user interrupted; the rest of your previous reply was not heard, do not repeat it] '

/** 16-bit mono 24 kHz WAV of a quiet tone, `seconds` long. */
function wav(seconds: number): Buffer {
  const rate = 24_000
  const n = Math.round(rate * seconds)
  const buf = Buffer.alloc(44 + n * 2)
  buf.write('RIFF', 0)
  buf.writeUInt32LE(36 + n * 2, 4)
  buf.write('WAVEfmt ', 8)
  buf.writeUInt32LE(16, 16)
  buf.writeUInt16LE(1, 20)
  buf.writeUInt16LE(1, 22)
  buf.writeUInt32LE(rate, 24)
  buf.writeUInt32LE(rate * 2, 28)
  buf.writeUInt16LE(2, 32)
  buf.writeUInt16LE(16, 34)
  buf.write('data', 36)
  buf.writeUInt32LE(n * 2, 40)
  for (let i = 0; i < n; i++) {
    buf.writeInt16LE(Math.round(Math.sin((i / rate) * 2 * Math.PI * 440) * 2000), 44 + i * 2)
  }
  return buf
}

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
  __ttsFinish: () => void
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
    // Ends the utterances `__ttsHold` kept "playing", as if read to the end.
    w.__ttsFinish = () => {
      const held = current
      current = []
      for (const u of held) u.onend?.()
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
          (e) =>
            e.kind === 'user' &&
            e.data.text === INTERRUPT_MARKER + 'no wait actually do the short one',
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

test('an interrupted reply is never spoken again, and the model is told it was cut off', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const voice = await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token)
  await page.goto('/')

  await page.getByTestId('voice-fab').click()
  await expect(page.getByTestId('voice-status')).toHaveText('Listening')
  await expect
    .poll(() => page.evaluate(() => (window as unknown as StubWindow).__voiceRec !== null))
    .toBe(true)
  await page.evaluate(() => {
    ;(window as unknown as StubWindow).__ttsHold = true
  })

  // `mock:echo` replies with the utterance itself: four sentences, read
  // one at a time.
  const reply = [
    'Alpha is the first point.',
    'Bravo is the second point.',
    'Charlie is the third point.',
    'Delta is the fourth point.',
  ]
  await page.evaluate((t) => (window as unknown as StubWindow).__voiceSay(t), reply.join(' '))
  await expect.poll(async () => spoken(page), { timeout: 15_000 }).toContain(reply[0])
  await expect(page.getByTestId('voice-status')).toHaveText('Speaking')

  // The user starts talking over it — too few words to cut in yet — and
  // the sentence being read finishes: the rest waits while they talk.
  await page.evaluate(() => (window as unknown as StubWindow).__voiceInterim('hold on'))
  await page.evaluate(() => {
    const w = window as unknown as StubWindow
    w.__ttsHold = false
    w.__ttsFinish()
  })
  await page.waitForTimeout(300)
  expect(await spoken(page)).not.toContain(reply[1])

  // They finish their sentence. It talked over the reply, so the held rest
  // is dropped, not read out once they stop.
  const said = 'hold on tell me about zebras'
  await page.evaluate((t) => (window as unknown as StubWindow).__voiceSay(t), said)
  await expect(page.getByTestId('voice-line-user').last()).toContainText(said, {
    timeout: 10_000,
  })
  // The transcript shows what they said, not the marker for the model…
  await expect(
    page.getByTestId('voice-line-user').filter({ hasText: '[user interrupted' }),
  ).toHaveCount(0)
  // …which the session did receive.
  await expect
    .poll(
      async () =>
        (await sessionEvents(request, token, voice.session_id)).some(
          (e) => e.kind === 'user' && e.data.text === INTERRUPT_MARKER + said,
        ),
      { timeout: 10_000 },
    )
    .toBe(true)
  await expect
    .poll(async () => (await spoken(page)).some((s) => s.includes('zebras')), {
      timeout: 15_000,
    })
    .toBe(true)

  // A later turn (a relay reply) is spoken — and still nothing of the
  // interrupted reply.
  const res = await request.post(`/api/sessions/${voice.session_id}/message`, {
    headers: { Authorization: `Bearer ${token}` },
    data: { text: '[relay] update from "zoo": PANGOLIN count finished.' },
  })
  expect(res.ok(), `relay send failed: ${await res.text()}`).toBeTruthy()
  await expect
    .poll(async () => (await spoken(page)).some((s) => s.includes('PANGOLIN')), {
      timeout: 15_000,
    })
    .toBe(true)
  await page.waitForTimeout(500)
  const all = await spoken(page)
  for (const unheard of reply.slice(1)) {
    expect(all.some((s) => s.includes(unheard.split(' ')[0]))).toBe(false)
  }
})

test('Kokoro audio fetched before a barge-in never plays after it', async ({ request, page }) => {
  const token = await authenticate(request)
  await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token)
  await page.addInitScript(() => {
    const w = window as unknown as Record<string, number>
    w.__audioStarts = 0
    const proto = AudioBufferSourceNode.prototype
    const start = proto.start
    proto.start = function (...args: Parameters<typeof start>) {
      w.__audioStarts += 1
      return start.apply(this, args)
    }
  })
  // Kokoro (the default voice) is ready; the first sentence plays for a
  // while, and every later sentence's audio arrives only after a delay.
  const ready = { state: 'ready', progress: 1, error: null }
  await page.route('**/api/voice/tts/prepare', (r) => r.fulfill({ json: ready }))
  await page.route('**/api/voice/tts/status', (r) => r.fulfill({ json: ready }))
  const requested: { text: string; at: number }[] = []
  await page.route('**/api/voice/tts', async (r) => {
    const text = String((r.request().postDataJSON() as { text?: string }).text ?? '')
    requested.push({ text, at: Date.now() })
    if (!text.startsWith('Kilo')) await new Promise((res) => setTimeout(res, 1500))
    await r
      .fulfill({
        status: 200,
        contentType: 'audio/wav',
        body: wav(text.startsWith('Kilo') ? 6 : 0.2),
      })
      .catch(() => undefined)
  })
  await page.goto('/')
  const audioStarts = () =>
    page.evaluate(() => (window as unknown as { __audioStarts: number }).__audioStarts)

  await page.getByTestId('voice-fab').click()
  await expect(page.getByTestId('voice-status')).toHaveText('Listening')
  // A click inside the panel is the gesture that unlocks audio output.
  await page.getByTestId('voice-panel').click({ position: { x: 10, y: 10 } })
  await expect
    .poll(() => page.evaluate(() => (window as unknown as StubWindow).__voiceRec !== null))
    .toBe(true)

  const reply = [
    'Kilo is the first point.',
    'Lima is the second point.',
    'Mike is the third point.',
  ]
  await page.evaluate((t) => (window as unknown as StubWindow).__voiceSay(t), reply.join(' '))
  await expect.poll(audioStarts, { timeout: 15_000 }).toBe(1)
  // The next sentence is being prefetched while the first plays.
  await expect.poll(() => requested.some((q) => q.text.startsWith('Lima'))).toBe(true)

  await page.evaluate(() =>
    (window as unknown as StubWindow).__voiceInterim('stop right there please'),
  )
  const bargedAt = Date.now()
  await expect(page.getByTestId('voice-status')).not.toHaveText('Speaking')
  // The prefetched audio lands after the barge-in: it must not play.
  await page.waitForTimeout(2500)
  expect(await audioStarts()).toBe(1)
  expect(
    requested.some((q) => q.at > bargedAt && /^(Lima|Mike)/.test(q.text)),
    'no unheard sentence is fetched after the barge-in',
  ).toBe(false)

  await page.evaluate(() =>
    (window as unknown as StubWindow).__voiceSay('stop right there please and count zebras'),
  )
  await expect
    .poll(() => requested.some((q) => q.text.includes('zebras')), {
      timeout: 15_000,
    })
    .toBe(true)
  await page.waitForTimeout(500)
  expect(requested.some((q) => q.at > bargedAt && /^(Lima|Mike)/.test(q.text))).toBe(false)
})

test('a relay arriving mid-utterance is not spoken over the user and does not split the utterance', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const voice = await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token)
  await page.goto('/')

  await page.getByTestId('voice-fab').click()
  await expect(page.getByTestId('voice-status')).toHaveText('Listening')
  await expect
    .poll(() => page.evaluate(() => (window as unknown as StubWindow).__voiceRec !== null))
    .toBe(true)

  // The user starts talking…
  await page.evaluate(() => (window as unknown as StubWindow).__voiceInterim('please tell the'))
  await expect(page.getByTestId('voice-interim')).toContainText('please tell the')

  // …and a relay lands on the voice session. `mock:echo` answers it by
  // echoing it back, so its token would be spoken if the client talked
  // over the user.
  const relay = '[relay] update from "stashify dev": QUOKKA build finished.'
  const res = await request.post(`/api/sessions/${voice.session_id}/message`, {
    headers: { Authorization: `Bearer ${token}` },
    data: { text: relay },
  })
  expect(res.ok(), `relay send failed: ${await res.text()}`).toBeTruthy()
  await expect
    .poll(
      async () =>
        (await sessionEvents(request, token, voice.session_id)).some(
          (e) => e.kind === 'agent-text' && String(e.data.text ?? '').includes('QUOKKA'),
        ),
      { timeout: 15_000 },
    )
    .toBe(true)
  // The reply to the relay is in, but the user is still mid-sentence.
  await page.waitForTimeout(500)
  expect((await spoken(page)).some((s) => s.includes('QUOKKA'))).toBe(false)

  // The user finishes the sentence in two phrases with a short pause: one
  // utterance, not two.
  await page.evaluate(() =>
    (window as unknown as StubWindow).__voiceSay('please tell the stashify session'),
  )
  await page.waitForTimeout(300)
  expect((await spoken(page)).some((s) => s.includes('QUOKKA'))).toBe(false)
  await page.evaluate(() => (window as unknown as StubWindow).__voiceInterim('to add'))
  await page.evaluate(() => (window as unknown as StubWindow).__voiceSay('to add tests'))

  const full = 'please tell the stashify session to add tests'
  await expect(page.getByTestId('voice-line-user').last()).toContainText(full, {
    timeout: 10_000,
  })
  const userTexts = (await sessionEvents(request, token, voice.session_id))
    .filter((e) => e.kind === 'user')
    .map((e) => String(e.data.text ?? ''))
  expect(userTexts).toContain(full)
  expect(userTexts).not.toContain('please tell the stashify session')

  // Once the user's turn is sent, the held reply is spoken, then the answer.
  await expect.poll(async () => (await spoken(page)).some((s) => s.includes('QUOKKA'))).toBe(true)
  await expect
    .poll(async () => (await spoken(page)).some((s) => s.includes('to add tests')), {
      timeout: 15_000,
    })
    .toBe(true)
  const all = await spoken(page)
  expect(all.findIndex((s) => s.includes('QUOKKA'))).toBeLessThan(
    all.findIndex((s) => s.includes('to add tests')),
  )
})

/** Open the panel and wait for the always-on mic to be live. */
async function openListening(page: Page) {
  await page.getByTestId('voice-fab').click()
  await expect(page.getByTestId('voice-status')).toHaveText('Listening')
  await expect
    .poll(() => page.evaluate(() => (window as unknown as StubWindow).__voiceRec !== null))
    .toBe(true)
}

function say(page: Page, text: string) {
  return page.evaluate((t) => (window as unknown as StubWindow).__voiceSay(t), text)
}

async function userTexts(
  request: APIRequestContext,
  token: string,
  sessionId: string,
): Promise<string[]> {
  return (await sessionEvents(request, token, sessionId))
    .filter((e) => e.kind === 'user')
    .map((e) => String(e.data.text ?? ''))
}

test('a mid-sentence pause waits for the rest; the finished sentence goes as one message', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const voice = await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token)
  await page.goto('/')
  await openListening(page)

  const fragment = 'OK I think you might be affected by the'
  await say(page, fragment)
  // It ends on "the": the line stays up, marked as waiting for the rest.
  const heard = page.getByTestId('voice-interim')
  await expect(heard).toContainText(fragment)
  await expect(heard).toHaveAttribute('data-unfinished', 'true')
  // Well past the short end-of-turn gap: still not sent.
  await page.waitForTimeout(2000)
  expect(await userTexts(request, token, voice.session_id)).not.toContain(fragment)
  await expect(heard).toContainText(fragment)

  const resumedAt = Date.now()
  await say(page, 'latest update')
  const full = `${fragment} latest update`
  await expect(page.getByTestId('voice-line-user').filter({ hasText: full })).toHaveCount(1, {
    timeout: 5_000,
  })
  // Complete-looking, it goes after the short gap, not the long wait.
  expect(Date.now() - resumedAt).toBeLessThan(3_500)
  await expect.poll(() => userTexts(request, token, voice.session_id)).toContain(full)
  expect(await userTexts(request, token, voice.session_id)).not.toContain(fragment)
})

test('a finished sentence is sent after a short silence', async ({ request, page }) => {
  const token = await authenticate(request)
  const voice = await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token)
  await page.goto('/')
  await openListening(page)

  const sentence = "What's the status of Stashify?"
  const line = page.getByTestId('voice-line-user').filter({ hasText: sentence })
  const saidAt = Date.now()
  await say(page, sentence)
  await page.waitForTimeout(800)
  await expect(line).toHaveCount(0)
  await expect(page.getByTestId('voice-interim')).not.toHaveAttribute('data-unfinished', 'true')
  await expect(line).toHaveCount(1, { timeout: 5_000 })
  const elapsed = Date.now() - saidAt
  expect(elapsed).toBeGreaterThanOrEqual(1_200)
  expect(elapsed).toBeLessThan(3_000)
  await expect.poll(() => userTexts(request, token, voice.session_id)).toContain(sentence)
})

test('a recognition restart mid-utterance neither sends nor drops what was heard', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const voice = await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token)
  await page.goto('/')
  await openListening(page)
  const recStarts = () => page.evaluate(() => (window as unknown as StubWindow).__recStarts)

  await say(page, 'please remind me to')
  await page.evaluate(() => (window as unknown as StubWindow).__voiceInterim('check the'))
  const startsBefore = await recStarts()
  // Chrome ends recognition sessions on silence, even mid-hypothesis.
  await page.evaluate(() => (window as unknown as StubWindow).__voiceRec?.finish())
  await expect.poll(recStarts).toBeGreaterThan(startsBefore)
  await expect(page.getByTestId('voice-interim')).toContainText('please remind me to check the')
  await page.waitForTimeout(1_500)
  const early = await userTexts(request, token, voice.session_id)
  expect(early.some((t) => t.startsWith('please remind me'))).toBe(false)

  await say(page, 'deploy logs')
  const full = 'please remind me to check the deploy logs'
  await expect
    .poll(() => userTexts(request, token, voice.session_id), { timeout: 10_000 })
    .toContain(full)
  const all = await userTexts(request, token, voice.session_id)
  expect(all.filter((t) => t.startsWith('please remind me'))).toEqual([full])
})

test('a relay arriving during a mid-sentence pause is not spoken; the user still holds the floor', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const voice = await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token)
  // The longest pause, so the relay's round trip can't outlast it.
  await page.addInitScript(() =>
    localStorage.setItem('peckboard_voice_prefs', JSON.stringify({ maxPauseMs: 10_000 })),
  )
  const activity: { state: string; at: number }[] = []
  page.on('request', (r) => {
    if (!r.url().endsWith('/api/voice/activity')) return
    const body = r.postDataJSON() as { state?: string }
    activity.push({ state: String(body.state), at: Date.now() })
  })
  await page.goto('/')
  await openListening(page)

  const saidAt = Date.now()
  await say(page, 'can you ask the stashify session about')
  const relay = '[relay] update from "stashify dev": WOMBAT deploy finished.'
  const res = await request.post(`/api/sessions/${voice.session_id}/message`, {
    headers: { Authorization: `Bearer ${token}` },
    data: { text: relay },
  })
  expect(res.ok(), `relay send failed: ${await res.text()}`).toBeTruthy()
  await expect
    .poll(
      async () =>
        (await sessionEvents(request, token, voice.session_id)).some(
          (e) => e.kind === 'agent-text' && String(e.data.text ?? '').includes('WOMBAT'),
        ),
      { timeout: 15_000 },
    )
    .toBe(true)
  // Past the server's 6s "speaking" hold minus slack, the client re-reported it.
  await page.waitForTimeout(Math.max(500, saidAt + 4_000 - Date.now()))
  expect((await spoken(page)).some((s) => s.includes('WOMBAT'))).toBe(false)
  await expect(page.getByTestId('voice-status')).not.toHaveText('Speaking')
  expect(activity.some((a) => a.state === 'speaking' && a.at > saidAt + 2_500)).toBe(true)
  expect(activity.some((a) => a.state === 'idle' && a.at > saidAt)).toBe(false)

  await say(page, 'the release notes')
  const full = 'can you ask the stashify session about the release notes'
  await expect
    .poll(() => userTexts(request, token, voice.session_id), { timeout: 10_000 })
    .toContain(full)
  // Once the turn is sent, the held reply is spoken.
  await expect
    .poll(async () => (await spoken(page)).some((s) => s.includes('WOMBAT')), { timeout: 15_000 })
    .toBe(true)
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
  await page.getByTestId('voice-max-pause').fill('3000')
  await expect(page.getByTestId('voice-max-pause-value')).toHaveText('3.0s')

  // Test voice speaks a sample right away.
  await page.getByTestId('voice-test').click()
  await expect.poll(() => spoken(page)).toHaveLength(1)

  await page.reload()
  await expect(page.getByTestId('voice-voice-select')).toHaveValue('stub-beta', {
    timeout: 10_000,
  })
  await expect(page.getByTestId('voice-rate')).toHaveValue('1.5')
  await expect(page.getByTestId('voice-auto-listen')).not.toBeChecked()
  await expect(page.getByTestId('voice-max-pause')).toHaveValue('3000')

  // The model section reads the voice session's model from the backend.
  await expect(page.getByTestId('voice-model')).toBeVisible()
  await expect(page.getByTestId('voice-model')).not.toContainText('Loading…', {
    timeout: 10_000,
  })
})

test('pronunciation hints show as plain words but reach Kokoro intact, even split across chunks', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  // `mock:echo-stream` echoes the message in 5-char chunks, so each hint
  // below arrives split across several `agent-text` events.
  await setVoiceModel(request, token, 'mock:echo-stream')
  await primePage(page, token)
  const ready = { state: 'ready', progress: 1, error: null }
  await page.route('**/api/voice/tts/prepare', (r) => r.fulfill({ json: ready }))
  await page.route('**/api/voice/tts/status', (r) => r.fulfill({ json: ready }))
  const requested: string[] = []
  await page.route('**/api/voice/tts', async (r) => {
    requested.push(String((r.request().postDataJSON() as { text?: string }).text ?? ''))
    await r
      .fulfill({ status: 200, contentType: 'audio/wav', body: wav(0.1) })
      .catch(() => undefined)
  })
  await page.goto('/')
  await page.getByTestId('voice-fab').click()
  await expect(page.getByTestId('voice-status')).toHaveText('Listening')

  const sentences = ['Open [Peckboard](/pˈɛkbˌɔɹd/) now.', 'Then say [Kokoro](/kOkˈOɹO/) twice.']
  await page.getByTestId('voice-type-input').fill(sentences.join(' '))
  await page.getByTestId('voice-type-send').click()

  const line = page.getByTestId('voice-line-assistant').last()
  await expect(line).toContainText('Open Peckboard now. Then say Kokoro twice.', {
    timeout: 15_000,
  })
  await expect(line).not.toContainText('](/')
  // Each sentence is sent whole, hint markup included, for the server to read.
  await expect.poll(() => [...new Set(requested)], { timeout: 15_000 }).toEqual(sentences)
})

test('the browser voice reads a hinted word without the markup', async ({ request, page }) => {
  const token = await authenticate(request)
  await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token)
  await page.goto('/')
  await page.getByTestId('voice-fab').click()
  await page.getByTestId('voice-type-input').fill('Ask [Grok](/ɡɹˈɑk/) about it.')
  await page.getByTestId('voice-type-send').click()
  await expect(page.getByTestId('voice-line-assistant').last()).toHaveText(/Ask Grok about it\./, {
    timeout: 15_000,
  })
  await expect.poll(() => spoken(page), { timeout: 15_000 }).toContain('Ask Grok about it.')
})
