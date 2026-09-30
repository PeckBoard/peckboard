import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Kokoro (server-side) TTS in the voice assistant.
 *
 * The e2e server never downloads the model (`PECKBOARD_TTS_DOWNLOAD=0`),
 * so the synth routes are stubbed with `page.route`: `/api/voice/tts`
 * returns a short WAV (or fails), and prepare/status report `ready`.
 * Web Audio playback is observed by hooking
 * `AudioBufferSourceNode.prototype.start/stop`; the browser fallback by a
 * stubbed `speechSynthesis` that records every utterance in `__spoken`.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-kokoro-'))
  const folderRes = await request.post('/api/folders', {
    headers: { Authorization: `Bearer ${token}` },
    data: { name: `kokoro-${path.basename(folderPath)}`, path: folderPath },
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

type StubWindow = {
  __voiceRec: unknown
  __voiceSay: (t: string) => void
  __voiceInterim: (t: string) => void
  __spoken: string[]
  __audioStarts: number
  __audioStops: number
  __audioLive: number
  __audioStall: boolean
}

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

/** Stubs: Web Speech (recognition + synthesis), Web Audio hooks, auth. */
async function primePage(page: Page, token: string, voiceURI?: string) {
  await page.addInitScript(
    ([t, v]) => {
      localStorage.setItem('peckboard_token', t)
      if (v) {
        localStorage.setItem(
          'peckboard_voice_prefs',
          JSON.stringify({ voiceURI: v, rate: 1, pitch: 1, lang: '', autoListen: true }),
        )
      }
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

      w.__spoken = [] as string[]
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
      ]
      const synth = {
        speaking: false,
        pending: false,
        paused: false,
        getVoices: () => voices,
        speak: (u: FakeUtterance) => {
          if (!u.text.trim()) return
          ;(w.__spoken as string[]).push(u.text)
          setTimeout(() => u.onend?.(), 5)
        },
        cancel: () => {},
        pause: () => {},
        resume: () => {},
        addEventListener: () => {},
        removeEventListener: () => {},
      }
      Object.defineProperty(window, 'speechSynthesis', { value: synth, configurable: true })

      w.__audioStarts = 0
      w.__audioStops = 0
      // Sources started and neither stopped nor ended: still able to sound.
      w.__audioLive = 0
      // `__audioStall`: the output device stalls (a Bluetooth route change
      // freezes the context's clock) — a clip starts but never plays out.
      w.__audioStall = false
      const sounding = new Set<AudioBufferSourceNode>()
      const proto = AudioBufferSourceNode.prototype
      const start = proto.start
      const stop = proto.stop
      proto.start = function (...args: Parameters<typeof start>) {
        w.__audioStarts = (w.__audioStarts as number) + 1
        sounding.add(this)
        w.__audioLive = sounding.size
        this.addEventListener('ended', () => {
          sounding.delete(this)
          w.__audioLive = sounding.size
        })
        if (w.__audioStall) return start.call(this, this.context.currentTime + 1000)
        return start.apply(this, args)
      }
      proto.stop = function (...args: Parameters<typeof stop>) {
        w.__audioStops = (w.__audioStops as number) + 1
        sounding.delete(this)
        w.__audioLive = sounding.size
        return stop.apply(this, args)
      }
    },
    [token, voiceURI ?? ''] as const,
  )
}

/** Stub the Kokoro routes: ready, and `/api/voice/tts` answering `tts`. */
async function stubKokoro(page: Page, tts: { status: number; body?: Buffer }) {
  const ready = { state: 'ready', progress: 1, error: null }
  await page.route('**/api/voice/tts/prepare', (r) => r.fulfill({ json: ready }))
  await page.route('**/api/voice/tts/status', (r) => r.fulfill({ json: ready }))
  await page.route('**/api/voice/tts', (r) =>
    tts.status === 200
      ? r.fulfill({ status: 200, contentType: 'audio/wav', body: tts.body })
      : r.fulfill({ status: tts.status, json: { error: 'boom' } }),
  )
}

const recognizing = (page: Page) =>
  page.evaluate(() => (window as unknown as StubWindow).__voiceRec !== null)
const audioStarts = (page: Page) =>
  page.evaluate(() => (window as unknown as StubWindow).__audioStarts)
const audioStops = (page: Page) =>
  page.evaluate(() => (window as unknown as StubWindow).__audioStops)
const spoken = (page: Page) => page.evaluate(() => (window as unknown as StubWindow).__spoken)

test('Settings → Voice lists Kokoro and browser voices and persists the choice', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await primePage(page, token)
  await page.goto('/settings/voice')

  const select = page.getByTestId('voice-voice-select')
  await expect(select.locator('option', { hasText: 'Kokoro — Heart (US)' })).toHaveCount(1, {
    timeout: 10_000,
  })
  await expect(select.locator('option', { hasText: 'Stub Alpha' })).toHaveCount(1)
  // Kokoro Heart is the default voice.
  await expect(select).toHaveValue('kokoro:af_heart')

  await select.selectOption('kokoro:am_michael')
  await page.reload()
  await expect(page.getByTestId('voice-voice-select')).toHaveValue('kokoro:am_michael', {
    timeout: 10_000,
  })
  await page.getByTestId('voice-voice-select').selectOption('stub-alpha')
  await page.reload()
  await expect(page.getByTestId('voice-voice-select')).toHaveValue('stub-alpha', {
    timeout: 10_000,
  })
})

test('a reply plays through Kokoro audio, and barge-in stops it', async ({ request, page }) => {
  const token = await authenticate(request)
  // `mock:block` says "working…" then holds the turn open.
  const voice = await setVoiceModel(request, token, 'mock:block')
  await primePage(page, token, 'kokoro:af_heart')
  await stubKokoro(page, { status: 200, body: wav(6) })
  await page.goto('/')

  await page.getByTestId('voice-fab').click()
  await expect(page.getByTestId('voice-status')).toHaveText('Listening')
  // A click inside the panel is the gesture that unlocks audio output.
  await page.getByTestId('voice-panel').click({ position: { x: 10, y: 10 } })
  await expect.poll(() => recognizing(page)).toBe(true)

  await page.evaluate(() => (window as unknown as StubWindow).__voiceSay('start the long job'))
  await expect.poll(() => audioStarts(page), { timeout: 15_000 }).toBeGreaterThan(0)
  await expect(page.getByTestId('voice-status')).toHaveText('Speaking')
  // Played by Kokoro, not the browser voice.
  expect(await spoken(page)).toEqual([])

  const stopsBefore = await audioStops(page)
  await page.evaluate(() => (window as unknown as StubWindow).__voiceInterim('no wait actually'))
  await expect.poll(() => audioStops(page)).toBeGreaterThan(stopsBefore)
  await expect(page.getByTestId('voice-status')).not.toHaveText('Speaking')

  await request.post(`/api/sessions/${voice.session_id}/interrupt`, {
    headers: { Authorization: `Bearer ${token}` },
  })
})

test('a failing Kokoro request falls back to the browser voice', async ({ request, page }) => {
  const token = await authenticate(request)
  await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token, 'kokoro:af_heart')
  await stubKokoro(page, { status: 500 })
  await page.goto('/')

  await page.getByTestId('voice-fab').click()
  await expect(page.getByTestId('voice-status')).toHaveText('Listening')
  await expect.poll(() => recognizing(page)).toBe(true)
  // Real speech builds up through a partial result before the final (a
  // bare final right after the mic starts reads as a recognizer phantom).
  await page.evaluate(() => (window as unknown as StubWindow).__voiceInterim('Fallback'))
  await page.evaluate(() => (window as unknown as StubWindow).__voiceSay('Fallback please.'))

  await expect.poll(() => spoken(page), { timeout: 15_000 }).toContain('Fallback please.')
  expect(await audioStarts(page)).toBe(0)
})

test('a sentence skipped on a stalled output never plays after a barge-in', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const voice = await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token, 'kokoro:af_heart')
  const ready = { state: 'ready', progress: 1, error: null }
  await page.route('**/api/voice/tts/prepare', (r) => r.fulfill({ json: ready }))
  await page.route('**/api/voice/tts/status', (r) => r.fulfill({ json: ready }))
  const requested: string[] = []
  await page.route('**/api/voice/tts', (r) => {
    requested.push(String((r.request().postDataJSON() as { text?: string }).text ?? ''))
    return r.fulfill({ status: 200, contentType: 'audio/wav', body: wav(0.2) })
  })
  await page.goto('/')
  const audioLive = () => page.evaluate(() => (window as unknown as StubWindow).__audioLive)

  await page.getByTestId('voice-fab').click()
  await expect(page.getByTestId('voice-status')).toHaveText('Listening')
  await page.getByTestId('voice-panel').click({ position: { x: 10, y: 10 } })
  await expect.poll(() => recognizing(page)).toBe(true)

  // The output stalls: "Fair enough." starts but never plays out, so the
  // playback watchdog moves on to the next sentence.
  await page.evaluate(() => {
    ;(window as unknown as StubWindow).__audioStall = true
  })
  await page.evaluate(
    (t) => (window as unknown as StubWindow).__voiceSay(t),
    'Fair enough. Golf is the second point. Hotel is the third point.',
  )
  await expect.poll(() => audioStarts(page), { timeout: 15_000 }).toBeGreaterThanOrEqual(2)

  // The user talks over it: nothing already started may still sound once
  // the output recovers — least of all the first sentence.
  await page.evaluate(() =>
    (window as unknown as StubWindow).__voiceInterim('stop right there please'),
  )
  await expect(page.getByTestId('voice-status')).not.toHaveText('Speaking')
  await expect.poll(audioLive).toBe(0)

  await page.evaluate(() => {
    const w = window as unknown as StubWindow
    w.__audioStall = false
    w.__voiceSay('stop right there please and count zebras')
  })
  await expect
    .poll(() => requested.some((t) => t.includes('zebras')), { timeout: 15_000 })
    .toBe(true)
  const res = await request.post(`/api/sessions/${voice.session_id}/message`, {
    headers: { Authorization: `Bearer ${token}` },
    data: { text: '[relay] update from "zoo": PANGOLIN count finished.' },
  })
  expect(res.ok(), `relay send failed: ${await res.text()}`).toBeTruthy()
  await expect
    .poll(() => requested.some((t) => t.includes('PANGOLIN')), { timeout: 15_000 })
    .toBe(true)
  await page.waitForTimeout(500)
  expect(
    requested.filter((t) => t.startsWith('Fair enough')).length,
    JSON.stringify(requested),
  ).toBe(1)
  expect(await spoken(page)).toEqual([])
})
