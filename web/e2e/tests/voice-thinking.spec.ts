import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * "Thinking" feedback in the voice assistant: a soft Web Audio cue from the
 * send until the first reply audio, and one spoken filler ("One sec.") when
 * the reply is slow to start.
 *
 * Kokoro's routes are stubbed (`/api/voice/tts` answers a tone WAV). A
 * filler's WAV length encodes which filler it is, so the played
 * `AudioBuffer`'s duration tells filler from reply and one filler from
 * another. `OscillatorNode` / `AudioBufferSourceNode` start/stop are hooked
 * with timestamps. A slow reply is simulated by holding the message POST.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

/** Must match `FILLERS` in web/src/voice/thinking.ts. */
const FILLER_TEXTS = [
  'One sec.',
  'Let me think about that.',
  'Give me a second.',
  'One moment.',
  'Let me check.',
]
/** Kokoro's wake pre-roll in front of every filler clip. */
const PREROLL_S = 0.2
const REPLY_S = 0.3

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-thinking-'))
  const folderRes = await request.post('/api/folders', {
    headers: { Authorization: `Bearer ${token}` },
    data: { name: `thinking-${path.basename(folderPath)}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  return token
}

async function setVoiceModel(request: APIRequestContext, token: string, model: string) {
  const res = await request.post('/api/voice/session', {
    headers: { Authorization: `Bearer ${token}` },
    data: { model },
  })
  expect(res.ok(), `voice session failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as { session_id: string; model: string }
}

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

interface Clip {
  dur: number
  start: number
  end: number | null
}
interface Osc {
  start: number
  stop: number | null
}
type StubWindow = {
  __voiceRec: unknown
  __voiceSay: (t: string) => void
  __voiceInterim: (t: string) => void
  __clips: Clip[]
  __oscs: Osc[]
}

async function primePage(page: Page, token: string, prefs: Record<string, unknown> = {}) {
  await page.addInitScript(
    ([t, p]) => {
      localStorage.setItem('peckboard_token', t)
      localStorage.setItem(
        'peckboard_voice_prefs',
        JSON.stringify({
          voiceURI: 'kokoro:af_heart',
          rate: 1,
          pitch: 1,
          lang: '',
          autoListen: true,
          ...p,
        }),
      )
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
          ;(window as unknown as { __voiceRec: FakeRecognition | null }).__voiceRec = this
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
      const live = () => {
        const rec = (window as unknown as { __voiceRec: FakeRecognition | null }).__voiceRec
        if (!rec) throw new Error('no live recognition')
        return rec
      }
      w.__voiceSay = (text: string) => live().deliver(text, true)
      w.__voiceInterim = (text: string) => live().deliver(text, false)

      class FakeUtterance {
        text: string
        onend: (() => void) | null = null
        onerror: ((ev: { error: string }) => void) | null = null
        constructor(text: string) {
          this.text = text
        }
      }
      w.SpeechSynthesisUtterance = FakeUtterance
      const synth = {
        speaking: false,
        pending: false,
        paused: false,
        getVoices: () => [],
        speak: (u: FakeUtterance) => setTimeout(() => u.onend?.(), 5),
        cancel: () => {},
        pause: () => {},
        resume: () => {},
        addEventListener: () => {},
        removeEventListener: () => {},
      }
      Object.defineProperty(window, 'speechSynthesis', { value: synth, configurable: true })

      const clips: Clip[] = []
      const oscs: Osc[] = []
      w.__clips = clips
      w.__oscs = oscs
      type Tagged = { __clip?: Clip; __osc?: Osc }
      const src = AudioBufferSourceNode.prototype
      const srcStart = src.start
      const srcStop = src.stop
      src.start = function (...args: Parameters<typeof srcStart>) {
        const clip: Clip = { dur: this.buffer?.duration ?? 0, start: performance.now(), end: null }
        ;(this as unknown as Tagged).__clip = clip
        clips.push(clip)
        this.addEventListener('ended', () => {
          clip.end ??= performance.now()
        })
        return srcStart.apply(this, args)
      }
      src.stop = function (...args: Parameters<typeof srcStop>) {
        const clip = (this as unknown as Tagged).__clip
        if (clip) clip.end ??= performance.now()
        return srcStop.apply(this, args)
      }
      const osc = OscillatorNode.prototype
      const oscStart = osc.start
      const oscStop = osc.stop
      // Only the thinking cue (196 Hz sine) — not the app's notification sounds.
      osc.start = function (...args: Parameters<typeof oscStart>) {
        if (this.type === 'sine' && Math.round(this.frequency.value) === 196) {
          const rec: Osc = { start: performance.now(), stop: null }
          ;(this as unknown as Tagged).__osc = rec
          oscs.push(rec)
        }
        return oscStart.apply(this, args)
      }
      osc.stop = function (...args: Parameters<typeof oscStop>) {
        const rec = (this as unknown as Tagged).__osc
        if (rec) rec.stop ??= performance.now()
        return oscStop.apply(this, args)
      }
    },
    [token, prefs] as const,
  )
}

/** Kokoro ready; filler `i` synthesizes to `fillerBase + 0.1·i` seconds. */
async function stubKokoro(page: Page, fillerBase = 0.6) {
  const ready = { state: 'ready', progress: 1, error: null }
  await page.route('**/api/voice/tts/prepare', (r) => r.fulfill({ json: ready }))
  await page.route('**/api/voice/tts/status', (r) => r.fulfill({ json: ready }))
  await page.route('**/api/voice/tts', (r) => {
    const text = String((r.request().postDataJSON() as { text?: string }).text ?? '')
    const plain = text.replace(/\[([^\]]*)\]\(\/[^/]*\/\)/g, '$1')
    const i = FILLER_TEXTS.indexOf(plain)
    const seconds = i >= 0 ? fillerBase + 0.1 * i : REPLY_S
    return r.fulfill({ status: 200, contentType: 'audio/wav', body: wav(seconds) })
  })
}

/** The reply is slow: hold every message POST for `ms`. */
async function delayReplies(page: Page, ms: number) {
  await page.route('**/api/sessions/*/message', async (r) => {
    await new Promise((res) => setTimeout(res, ms))
    await r.continue()
  })
}

const clips = (page: Page) => page.evaluate(() => (window as unknown as StubWindow).__clips)
const oscs = (page: Page) => page.evaluate(() => (window as unknown as StubWindow).__oscs)

/** Which filler a clip is (by length), or -1 for a reply clip. */
function fillerIndex(c: Clip, fillerBase = 0.6): number {
  if (c.dur < fillerBase + PREROLL_S - 0.05) return -1
  return Math.round((c.dur - PREROLL_S - fillerBase) / 0.1)
}

async function openPanel(page: Page) {
  await page.goto('/')
  await page.getByTestId('voice-fab').click()
  await expect(page.getByTestId('voice-status')).toHaveText('Listening')
  // The gesture that unlocks audio output.
  await page.getByTestId('voice-panel').click({ position: { x: 10, y: 10 } })
  await expect
    .poll(() => page.evaluate(() => (window as unknown as StubWindow).__voiceRec !== null))
    .toBe(true)
}

async function say(page: Page, text: string) {
  await page.evaluate((t) => (window as unknown as StubWindow).__voiceInterim(t), text)
  await page.evaluate((t) => (window as unknown as StubWindow).__voiceSay(t), text)
}

test('a quick reply: the cue plays from the send and stops as the reply starts; no filler', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token)
  await stubKokoro(page)
  await openPanel(page)

  await say(page, 'Quick question.')
  await expect.poll(async () => (await oscs(page)).length, { timeout: 10_000 }).toBe(1)
  await expect.poll(async () => (await clips(page)).length, { timeout: 15_000 }).toBeGreaterThan(0)
  await page.waitForTimeout(2500)

  const [cue] = await oscs(page)
  const all = await clips(page)
  expect(
    all.every((c) => fillerIndex(c) === -1),
    JSON.stringify(all),
  ).toBe(true)
  expect(cue.stop, 'cue stopped').not.toBeNull()
  // Stopped the moment the first reply audio started.
  expect(cue.stop!).toBeLessThanOrEqual(all[0].start + 50)
  expect(await oscs(page)).toHaveLength(1)
})

test('a slow reply gets one filler, never overlapping the reply or repeating next turn', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const voice = await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token)
  await stubKokoro(page)
  await delayReplies(page, 3500)
  await openPanel(page)

  const turn = async (text: string, before: number) => {
    await say(page, text)
    // Filler, then the reply.
    await expect
      .poll(async () => (await clips(page)).length, { timeout: 20_000 })
      .toBeGreaterThanOrEqual(before + 2)
    await page.waitForTimeout(800)
    const mine = (await clips(page)).slice(before)
    const fillers = mine.filter((c) => fillerIndex(c) >= 0)
    expect(fillers, JSON.stringify(mine)).toHaveLength(1)
    const filler = fillers[0]
    const reply = mine.find((c) => fillerIndex(c) === -1)!
    expect(mine.indexOf(filler)).toBe(0)
    expect(filler.end, 'filler ended').not.toBeNull()
    expect(filler.end!).toBeLessThanOrEqual(reply.start + 20)
    return { filler: fillerIndex(filler), fillerStart: filler.start, count: mine.length }
  }

  const first = await turn('Slow question number one.', 0)
  const [cue] = await oscs(page)
  // The cue stopped as the filler started, and did not come back.
  expect(cue.stop!).toBeLessThanOrEqual(first.fillerStart + 50)
  expect(await oscs(page)).toHaveLength(1)

  await expect(page.getByTestId('voice-status')).toHaveText('Listening', { timeout: 10_000 })
  // Different words, so it isn't taken for the echo of the first reply.
  const second = await turn('Now tell me about zebras.', first.count)
  expect(second.filler).not.toBe(first.filler)

  // Fillers never reach the transcript or the session.
  const panelText = await page.getByTestId('voice-panel').innerText()
  const res = await request.get(`/api/sessions/${voice.session_id}/events?limit=500`, {
    headers: { Authorization: `Bearer ${token}` },
  })
  const events = JSON.stringify(await res.json())
  for (const f of FILLER_TEXTS) {
    expect(panelText).not.toContain(f)
    expect(events).not.toContain(f)
  }
})

test('talking over the filler silences it and the cue at once', async ({ request, page }) => {
  const token = await authenticate(request)
  await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token)
  // Long fillers, so the user can talk over one.
  await stubKokoro(page, 3)
  await delayReplies(page, 6000)
  await openPanel(page)

  await say(page, 'Tell me a long story.')
  await expect
    .poll(async () => (await clips(page)).length, { timeout: 15_000 })
    .toBeGreaterThanOrEqual(1)
  const [filler] = await clips(page)
  expect(fillerIndex(filler, 3)).toBeGreaterThanOrEqual(0)
  expect(filler.end).toBeNull()

  await page.evaluate(() =>
    (window as unknown as StubWindow).__voiceInterim('hold on please stop talking'),
  )
  await expect.poll(async () => (await clips(page))[0].end).not.toBeNull()
  const [stopped] = await clips(page)
  // Cut off well before its ~3 s end.
  expect(stopped.end! - stopped.start).toBeLessThan(2500)
  const [cue] = await oscs(page)
  expect(cue.stop).not.toBeNull()
})

test('with both toggles off, no cue and no filler play', async ({ request, page }) => {
  const token = await authenticate(request)
  await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token, { thinkingCue: false, thinkingFiller: false })
  await stubKokoro(page)
  await delayReplies(page, 3000)
  await openPanel(page)

  await say(page, 'Anything there?')
  await expect.poll(async () => (await clips(page)).length, { timeout: 15_000 }).toBeGreaterThan(0)
  await page.waitForTimeout(500)
  const all = await clips(page)
  expect(
    all.every((c) => fillerIndex(c) === -1),
    JSON.stringify(all),
  ).toBe(true)
  expect(await oscs(page)).toHaveLength(0)

  // The toggles live in Settings → Voice.
  await page.goto('/settings/voice')
  await expect(page.getByTestId('voice-thinking-cue')).not.toBeChecked({ timeout: 10_000 })
  await expect(page.getByTestId('voice-thinking-filler')).not.toBeChecked()
})
