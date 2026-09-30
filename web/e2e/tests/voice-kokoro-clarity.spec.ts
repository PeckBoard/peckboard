import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Kokoro clarity: the Settings → Voice "Natural voice speed" slider is
 * persisted and sent as `speed` (never as a playback rate), and the first
 * clip of a burst gets the Bluetooth wake pre-roll while the clip right
 * after it does not.
 *
 * The synth routes are stubbed (the e2e server never downloads the model);
 * played buffers are observed by hooking `AudioBufferSourceNode.start`.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const CLIP_SECONDS = 0.3

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-kokoro-clarity-'))
  const folderRes = await request.post('/api/folders', {
    headers: { Authorization: `Bearer ${token}` },
    data: { name: `kokoro-clarity-${path.basename(folderPath)}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const voiceRes = await request.post('/api/voice/session', {
    headers: { Authorization: `Bearer ${token}` },
    data: { model: 'mock:echo' },
  })
  expect(voiceRes.ok(), `voice session failed: ${await voiceRes.text()}`).toBeTruthy()
  return token
}

/** 16-bit mono 24 kHz WAV of a quiet tone. */
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

type StubWindow = {
  __voiceRec: unknown
  __voiceSay: (t: string) => void
  __played: number[]
  __rates: number[]
}

/** Stubs: fake recognition, auth, a Kokoro voice, and a played-buffer log. */
async function primePage(page: Page, token: string) {
  await page.addInitScript((t) => {
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
    }
    const w = window as unknown as Record<string, unknown>
    w.SpeechRecognition = FakeRecognition
    w.webkitSpeechRecognition = FakeRecognition
    w.__voiceRec = null
    w.__voiceSay = (text: string) => {
      const rec = (window as unknown as { __voiceRec: FakeRecognition | null }).__voiceRec
      if (!rec) throw new Error('no live recognition')
      const result = Object.assign([{ transcript: text }], { isFinal: true, length: 1 })
      rec.onresult?.({ resultIndex: 0, results: [result] })
    }
    w.__played = [] as number[]
    w.__rates = [] as number[]
    const proto = AudioBufferSourceNode.prototype
    const start = proto.start
    proto.start = function (...args: Parameters<typeof start>) {
      ;(w.__played as number[]).push(this.buffer?.duration ?? -1)
      ;(w.__rates as number[]).push(this.playbackRate.value)
      return start.apply(this, args)
    }
  }, token)
}

/** Stub the Kokoro routes; returns the parsed `/api/voice/tts` bodies. */
async function stubKokoro(page: Page) {
  const ready = { state: 'ready', progress: 1, error: null }
  await page.route('**/api/voice/tts/prepare', (r) => r.fulfill({ json: ready }))
  await page.route('**/api/voice/tts/status', (r) => r.fulfill({ json: ready }))
  const bodies: { text: string; speed?: number }[] = []
  await page.route('**/api/voice/tts', (r) => {
    bodies.push(r.request().postDataJSON() as { text: string; speed?: number })
    return r.fulfill({ status: 200, contentType: 'audio/wav', body: wav(CLIP_SECONDS) })
  })
  return bodies
}

async function openPanel(page: Page) {
  await page.getByTestId('voice-fab').click()
  await expect(page.getByTestId('voice-status')).toHaveText('Listening')
  // A click inside the panel is the gesture that unlocks audio output.
  await page.getByTestId('voice-panel').click({ position: { x: 10, y: 10 } })
  await expect
    .poll(() => page.evaluate(() => (window as unknown as StubWindow).__voiceRec !== null))
    .toBe(true)
}

test('natural voice speed persists and is sent as the Kokoro speed', async ({ request, page }) => {
  const token = await authenticate(request)
  await primePage(page, token)
  const bodies = await stubKokoro(page)

  await page.goto('/settings/voice')
  const slider = page.getByTestId('voice-kokoro-speed')
  await expect(slider).toHaveValue('0.95', { timeout: 10_000 })
  await slider.fill('1.1')
  await expect(page.getByTestId('voice-kokoro-speed-value')).toHaveText('1.10×')
  await page.reload()
  await expect(page.getByTestId('voice-kokoro-speed')).toHaveValue('1.1', { timeout: 10_000 })

  await page.goto('/')
  await openPanel(page)
  await page.evaluate(() =>
    (window as unknown as StubWindow).__voiceSay('Please read this speed check sentence back.'),
  )
  await expect.poll(() => bodies.length, { timeout: 15_000 }).toBeGreaterThan(0)
  expect(bodies[0].speed).toBe(1.1)
  // Speed goes only to the model: playback is never time-stretched
  // (a playbackRate != 1 resamples the clip and warbles).
  await expect
    .poll(() => page.evaluate(() => (window as unknown as StubWindow).__rates.length), {
      timeout: 15_000,
    })
    .toBeGreaterThan(0)
  const rates = await page.evaluate(() => (window as unknown as StubWindow).__rates)
  expect(rates.every((r) => r === 1)).toBe(true)
})

test('the first clip of a burst gets the wake pre-roll, the next one does not', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await primePage(page, token)
  await stubKokoro(page)
  await page.goto('/')
  await openPanel(page)
  await page.evaluate(() =>
    (window as unknown as StubWindow).__voiceSay('Alpha is first. Bravo is second.'),
  )
  const played = () => page.evaluate(() => (window as unknown as StubWindow).__played)
  await expect.poll(async () => (await played()).length, { timeout: 15_000 }).toBe(2)
  const [first, second] = await played()
  // 200 ms pre-roll on the first clip only.
  expect(first).toBeCloseTo(CLIP_SECONDS + 0.2, 1)
  expect(second).toBeCloseTo(CLIP_SECONDS, 1)
})
