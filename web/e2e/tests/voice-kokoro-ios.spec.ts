import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Kokoro on iPhone/iPad. Web Audio on iOS shares the audio session with the
 * always-on recognizer and plays as static there, so iOS plays the WAV
 * through an `<audio>` element instead — or, if the user opts in, through
 * the browser voice.
 *
 * This runs Chromium with an iPhone user agent (no WebKit build on the CI
 * host): it proves the iOS routing, the gesture unlock, barge-in, and that
 * the encoder's WAV layout decodes faithfully at the context's default
 * rate. It can't reproduce iOS's audio-session behaviour itself.
 */

const IPHONE_UA =
  'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 ' +
  '(KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1'

test.use({
  userAgent: IPHONE_UA,
  viewport: { width: 393, height: 852 },
  isMobile: true,
  hasTouch: true,
})

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-kokoro-ios-'))
  const folderRes = await request.post('/api/folders', {
    headers: { Authorization: `Bearer ${token}` },
    data: { name: `kokoro-ios-${path.basename(folderPath)}`, path: folderPath },
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

type MediaPlay = { src: string; seconds: number }
type StubWindow = {
  __voiceRec: unknown
  __voiceSay: (t: string) => void
  __voiceInterim: (t: string) => void
  __spoken: string[]
  __audioStarts: number
  __mediaPlays: MediaPlay[]
  __mediaPauses: number
}

const RATE = 24_000
const TONE_HZ = 440
const TONE_AMP = 0.5

/** The server encoder's layout (`kokoro::encode_wav`): 24 kHz mono
 *  16-bit PCM LE, samples scaled by 32767. A 440 Hz tone. */
function wav(seconds: number): Buffer {
  const n = Math.round(RATE * seconds)
  const buf = Buffer.alloc(44 + n * 2)
  buf.write('RIFF', 0)
  buf.writeUInt32LE(36 + n * 2, 4)
  buf.write('WAVEfmt ', 8)
  buf.writeUInt32LE(16, 16)
  buf.writeUInt16LE(1, 20)
  buf.writeUInt16LE(1, 22)
  buf.writeUInt32LE(RATE, 24)
  buf.writeUInt32LE(RATE * 2, 28)
  buf.writeUInt16LE(2, 32)
  buf.writeUInt16LE(16, 34)
  buf.write('data', 36)
  buf.writeUInt32LE(n * 2, 40)
  for (let i = 0; i < n; i++) {
    const s = TONE_AMP * Math.sin((i / RATE) * 2 * Math.PI * TONE_HZ)
    buf.writeInt16LE(Math.trunc(s * 32767), 44 + i * 2)
  }
  return buf
}

/** Stubs: Web Speech (recognition + synthesis), Web Audio and media
 *  element hooks, auth. */
async function primePage(page: Page, token: string, iosBrowserVoice = false) {
  await page.addInitScript(
    ([t, browserVoice]) => {
      localStorage.setItem('peckboard_token', t)
      localStorage.setItem(
        'peckboard_voice_prefs',
        JSON.stringify({
          voiceURI: 'kokoro:af_heart',
          rate: 1,
          pitch: 1,
          lang: '',
          autoListen: true,
        }),
      )
      if (browserVoice) localStorage.setItem('peckboard.voice.iosBrowserVoice', '1')
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
      const synth = {
        speaking: false,
        pending: false,
        paused: false,
        getVoices: () => [],
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
      const start = AudioBufferSourceNode.prototype.start
      AudioBufferSourceNode.prototype.start = function (...args: Parameters<typeof start>) {
        w.__audioStarts = (w.__audioStarts as number) + 1
        return start.apply(this, args)
      }
      // Record media-element playback: source + length, and pauses (barge-in).
      w.__mediaPlays = [] as MediaPlay[]
      w.__mediaPauses = 0
      const play = HTMLMediaElement.prototype.play
      const pause = HTMLMediaElement.prototype.pause
      HTMLMediaElement.prototype.play = function () {
        const p = play.call(this)
        const src = this.src
        p.then(
          () => (w.__mediaPlays as MediaPlay[]).push({ src, seconds: this.duration }),
          () => {},
        )
        return p
      }
      HTMLMediaElement.prototype.pause = function () {
        w.__mediaPauses = (w.__mediaPauses as number) + 1
        return pause.call(this)
      }
    },
    [token, iosBrowserVoice] as const,
  )
}

async function stubKokoro(page: Page, body: Buffer) {
  const ready = { state: 'ready', progress: 1, error: null }
  await page.route('**/api/voice/tts/prepare', (r) => r.fulfill({ json: ready }))
  await page.route('**/api/voice/tts/status', (r) => r.fulfill({ json: ready }))
  await page.route('**/api/voice/tts', (r) =>
    r.fulfill({ status: 200, contentType: 'audio/wav', body }),
  )
}

const recognizing = (page: Page) =>
  page.evaluate(() => (window as unknown as StubWindow).__voiceRec !== null)
const state = (page: Page) =>
  page.evaluate(() => {
    const s = window as unknown as StubWindow
    return {
      audioStarts: s.__audioStarts,
      // Speech, not the 0.05 s silent WAV that unlocks the element.
      speechPlays: s.__mediaPlays.filter((p) => p.src.startsWith('blob:') && p.seconds > 1).length,
      pauses: s.__mediaPauses,
      spoken: s.__spoken,
    }
  })

test('the encoder WAV decodes faithfully at the context default rate', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await primePage(page, token)
  await page.goto('/settings/voice')
  const bytes = [...wav(0.5)]
  const r = await page.evaluate(
    async ({ bytes, rate, hz, amp }) => {
      // Default rate, as the app creates it — decodeAudioData resamples.
      const ctx = new AudioContext()
      const audio = await ctx.decodeAudioData(new Uint8Array(bytes).buffer)
      const d = audio.getChannelData(0)
      let sumSq = 0
      let peak = 0
      let dot = 0
      let refSq = 0
      // Skip the resampler's edges.
      const from = Math.floor(d.length * 0.1)
      const to = Math.floor(d.length * 0.9)
      for (let i = from; i < to; i++) {
        const ref = amp * Math.sin((i / audio.sampleRate) * 2 * Math.PI * hz)
        sumSq += d[i] * d[i]
        peak = Math.max(peak, Math.abs(d[i]))
        dot += d[i] * ref
        refSq += ref * ref
      }
      const rms = Math.sqrt(sumSq / (to - from))
      const out = {
        ctxRate: ctx.sampleRate,
        bufRate: audio.sampleRate,
        channels: audio.numberOfChannels,
        duration: audio.duration,
        rms,
        peak,
        corr: dot / Math.sqrt(sumSq * refSq),
        rate,
      }
      await ctx.close()
      return out
    },
    { bytes, rate: RATE, hz: TONE_HZ, amp: TONE_AMP },
  )
  expect(r.channels).toBe(1)
  expect(r.bufRate).toBe(r.ctxRate)
  expect(r.duration).toBeCloseTo(0.5, 2)
  expect(r.peak).toBeGreaterThan(TONE_AMP * 0.95)
  expect(r.peak).toBeLessThan(TONE_AMP * 1.05)
  expect(r.rms).toBeCloseTo(TONE_AMP / Math.SQRT2, 2)
  // Static would decorrelate from the source tone.
  expect(r.corr).toBeGreaterThan(0.99)
})

test('on iPhone a reply plays through an <audio> element, and barge-in stops it', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  // `mock:echo` ends its turn, so the reply is spoken; the 6 s WAV keeps
  // it playing long enough to barge in.
  await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token)
  await stubKokoro(page, wav(6))
  await page.goto('/')

  await page.getByTestId('voice-fab').click()
  await expect(page.getByTestId('voice-status')).toHaveText('Listening')
  // A click inside the panel is the gesture that unlocks the element.
  await page.getByTestId('voice-panel').click({ position: { x: 10, y: 10 } })
  await expect.poll(() => recognizing(page)).toBe(true)

  await page.evaluate(() => (window as unknown as StubWindow).__voiceSay('Read this back.'))
  await expect.poll(async () => (await state(page)).speechPlays, { timeout: 15_000 }).toBe(1)
  await expect(page.getByTestId('voice-status')).toHaveText('Speaking')
  const s = await state(page)
  // Not Web Audio, and not the browser voice.
  expect(s.audioStarts).toBe(0)
  expect(s.spoken).toEqual([])

  await page.evaluate(() => (window as unknown as StubWindow).__voiceInterim('no wait actually'))
  await expect.poll(async () => (await state(page)).pauses).toBeGreaterThan(s.pauses)
  await expect(page.getByTestId('voice-status')).not.toHaveText('Speaking')
})

test('the iPhone browser-voice option persists and routes replies to it', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await setVoiceModel(request, token, 'mock:echo')
  await primePage(page, token)
  await stubKokoro(page, wav(0.3))
  await page.goto('/settings/voice')

  const toggle = page.getByTestId('voice-ios-browser-voice')
  await expect(toggle).not.toBeChecked()
  await toggle.check()
  await page.reload()
  await expect(page.getByTestId('voice-ios-browser-voice')).toBeChecked()

  await page.goto('/')
  await page.getByTestId('voice-fab').click()
  await expect(page.getByTestId('voice-status')).toHaveText('Listening')
  await expect.poll(() => recognizing(page)).toBe(true)
  await page.evaluate(() => (window as unknown as StubWindow).__voiceSay('Browser voice please.'))

  await expect
    .poll(async () => (await state(page)).spoken, { timeout: 15_000 })
    .toContain('Browser voice please.')
  const s = await state(page)
  expect(s.speechPlays).toBe(0)
  expect(s.audioStarts).toBe(0)
})
