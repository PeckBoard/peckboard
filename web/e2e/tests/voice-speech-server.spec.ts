import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { createServer, type Server } from 'node:http'
import type { AddressInfo } from 'node:net'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { openVoicePanel } from './voice-harness'

/**
 * External speech server (VoiceStudio) voices in the voice assistant.
 *
 * A fake VoiceStudio runs inside the test process: `/health`, a
 * bearer-gated `/v1/audio/voices`, and `/v1/audio/speech` answering raw
 * 24 kHz PCM. Peckboard's real server proxies to it, so this covers the
 * Settings page, the saved (write-only) key, the voice picker and a
 * spoken reply end to end. Recognition and the browser voice are stubbed
 * as in voice-kokoro.spec.ts.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const KEY = 'k-e2e-speech'

let fake: Server
let fakeUrl = ''
const speechRequests: { input: string; voice: string; format: string }[] = []

test.beforeAll(async () => {
  fake = createServer((req, res) => {
    const authed = req.headers.authorization === `Bearer ${KEY}`
    const send = (status: number, body: unknown) => {
      res.writeHead(status, { 'Content-Type': 'application/json' })
      res.end(JSON.stringify(body))
    }
    if (req.url === '/health') return send(200, { status: 'ok', version: '0.5.6', device: 'cuda' })
    if (!authed) return send(401, { detail: 'API key required' })
    if (req.url === '/v1/audio/voices') {
      // VoiceStudio-shaped: fixed profiles plus an OpenAI alias that maps to
      // a random speaker per request — the picker must not offer it.
      return send(200, {
        voices: [
          { voice_id: 'alloy', name: 'alloy', type: 'openai_alias' },
          { voice_id: 'v-ana', name: 'Ana', type: 'profile' },
          { voice_id: 'v-ben', name: 'Ben', type: 'profile' },
        ],
      })
    }
    if (req.url === '/v1/audio/speech' && req.method === 'POST') {
      let raw = ''
      req.on('data', (c) => (raw += c))
      req.on('end', () => {
        const b = JSON.parse(raw) as { input: string; voice: string; response_format: string }
        speechRequests.push({ input: b.input, voice: b.voice, format: b.response_format })
        // 0.4 s of a quiet 440 Hz tone, raw s16le mono at 24 kHz.
        const n = 9600
        const pcm = Buffer.alloc(n * 2)
        for (let i = 0; i < n; i++) {
          pcm.writeInt16LE(Math.round(Math.sin((i / 24_000) * 2 * Math.PI * 440) * 2000), i * 2)
        }
        res.writeHead(200, { 'Content-Type': 'audio/pcm' })
        res.end(pcm)
      })
      return
    }
    send(404, { detail: 'not found' })
  })
  await new Promise<void>((resolve) => fake.listen(0, '127.0.0.1', resolve))
  fakeUrl = `http://127.0.0.1:${(fake.address() as AddressInfo).port}`
})

test.afterAll(async () => {
  await new Promise<void>((resolve) => fake.close(() => resolve()))
})

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-speech-'))
  const folderRes = await request.post('/api/folders', {
    headers: { Authorization: `Bearer ${token}` },
    data: { name: `speech-${path.basename(folderPath)}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  return token
}

async function setServer(request: APIRequestContext, token: string, body: Record<string, string>) {
  const res = await request.put('/api/voice/speech-server', {
    headers: { Authorization: `Bearer ${token}` },
    data: body,
  })
  expect(res.ok(), `save speech server failed: ${await res.text()}`).toBeTruthy()
}

type StubWindow = {
  __voiceRec: unknown
  __voiceSay: (t: string) => void
  __voiceInterim: (t: string) => void
  __spoken: string[]
  __audioStarts: number
}

/** Stubs: Web Speech recognition + synthesis, a Web Audio start counter. */
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
      const synth = {
        speaking: false,
        pending: false,
        paused: false,
        getVoices: () => [
          {
            voiceURI: 'stub-alpha',
            name: 'Stub Alpha',
            lang: 'en-US',
            default: true,
            localService: true,
          },
        ],
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
      const proto = AudioBufferSourceNode.prototype
      const start = proto.start
      proto.start = function (...args: Parameters<typeof start>) {
        w.__audioStarts = (w.__audioStarts as number) + 1
        return start.apply(this, args)
      }
    },
    [token, voiceURI ?? ''] as const,
  )
}

test('Settings → Voice saves a speech server, tests it, and lists its voices', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await setServer(request, token, { base_url: '', api_key: '' })
  await primePage(page, token)
  await page.goto('/settings/voice')

  const section = page.getByTestId('voice-speech-server-section')
  await expect(section).toBeVisible({ timeout: 10_000 })
  await expect(page.getByTestId('speech-server-test')).toBeDisabled()

  // A bad address is rejected inline, on the field.
  await page.getByTestId('speech-server-url').fill('gpu-box:3900')
  await page.getByTestId('speech-server-save').click()
  await expect(page.getByTestId('speech-server-url-error')).toContainText('http')

  // A wrong key reaches the server but fails the test with its message.
  await page.getByTestId('speech-server-url').fill(`${fakeUrl}/v1`)
  await page.getByTestId('speech-server-key').fill('wrong')
  await page.getByTestId('speech-server-test').click()
  await expect(page.getByTestId('speech-server-test-result')).toContainText('API key required')
  // Saved without the /v1 suffix; the key is never shown back.
  await expect(page.getByTestId('speech-server-url')).toHaveValue(fakeUrl)
  await expect(page.getByTestId('speech-server-key')).toHaveValue('')
  await expect(page.getByTestId('speech-server-key')).toHaveAttribute('placeholder', /Saved/)

  await page.getByTestId('speech-server-key').fill(KEY)
  await page.getByTestId('speech-server-test').click()
  await expect(page.getByTestId('speech-server-test-result')).toHaveText(
    'Connected — 2 voices, version 0.5.6, on cuda.',
  )
  const view = await request.get('/api/voice/speech-server', {
    headers: { Authorization: `Bearer ${token}` },
  })
  expect(await view.json()).toEqual({ base_url: fakeUrl, model: 'tts-1', has_api_key: true })

  // Its voices join the picker, and the choice persists.
  const select = page.getByTestId('voice-voice-select')
  await expect(select.locator('option', { hasText: 'Speech server — Ana' })).toHaveCount(1, {
    timeout: 10_000,
  })
  await select.selectOption('studio:v-ben')
  // VoiceStudio's random-speaker alias is not offered next to its profiles.
  await expect(select.locator('option[value="studio:alloy"]')).toHaveCount(0)
  await page.reload()
  await expect(page.getByTestId('voice-voice-select')).toHaveValue('studio:v-ben', {
    timeout: 10_000,
  })

  await setServer(request, token, { base_url: '', api_key: '' })
})

test('a reply plays through the speech server voice', async ({ request, page }) => {
  const token = await authenticate(request)
  await setServer(request, token, { base_url: fakeUrl, api_key: KEY })
  const res = await request.post('/api/voice/session', {
    headers: { Authorization: `Bearer ${token}` },
    data: { model: 'mock:echo' },
  })
  expect(res.ok(), `voice session failed: ${await res.text()}`).toBeTruthy()
  await primePage(page, token, 'studio:v-ana')
  speechRequests.length = 0
  await page.goto('/')

  await openVoicePanel(page)
  await expect(page.getByTestId('voice-status')).toHaveText('Listening')
  await page.getByTestId('voice-panel').click({ position: { x: 10, y: 10 } })
  await expect
    .poll(() => page.evaluate(() => (window as unknown as StubWindow).__voiceRec !== null))
    .toBe(true)
  await page.evaluate(() => (window as unknown as StubWindow).__voiceInterim('Studio'))
  await page.evaluate(() => (window as unknown as StubWindow).__voiceSay('Studio voice please.'))

  await expect
    .poll(() => speechRequests.map((r) => r.input).join(' '), { timeout: 15_000 })
    .toContain('Studio voice please.')
  expect(speechRequests.every((r) => r.voice === 'v-ana' && r.format === 'pcm')).toBe(true)
  await expect
    .poll(() => page.evaluate(() => (window as unknown as StubWindow).__audioStarts), {
      timeout: 15_000,
    })
    .toBeGreaterThan(0)
  // Spoken by the speech server, not the browser voice.
  expect(await page.evaluate(() => (window as unknown as StubWindow).__spoken)).toEqual([])

  await setServer(request, token, { base_url: '', api_key: '' })
})
test('a speech-server reply is its first sentence, then the rest as one clip', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await setServer(request, token, { base_url: fakeUrl, api_key: KEY })
  const res = await request.post('/api/voice/session', {
    headers: { Authorization: `Bearer ${token}` },
    data: { model: 'mock:echo' },
  })
  expect(res.ok(), `voice session failed: ${await res.text()}`).toBeTruthy()
  await primePage(page, token, 'studio:v-ana')
  await page.goto('/')

  await openVoicePanel(page)
  await expect(page.getByTestId('voice-status')).toHaveText('Listening')
  await page.getByTestId('voice-panel').click({ position: { x: 10, y: 10 } })
  await expect
    .poll(() => page.evaluate(() => (window as unknown as StubWindow).__voiceRec !== null))
    .toBe(true)
  speechRequests.length = 0
  await page.evaluate(() => (window as unknown as StubWindow).__voiceInterim('First'))
  await page.evaluate(() =>
    (window as unknown as StubWindow).__voiceSay(
      'First sentence here. Then a second one. And finally a third.',
    ),
  )

  // The thinking fillers ("One sec." …) are synthesized in the background;
  // only the reply's own clips matter here.
  const replyClips = () =>
    speechRequests.map((r) => r.input).filter((t) => t.startsWith('First') || t.startsWith('Then'))
  await expect
    .poll(replyClips, { timeout: 15_000 })
    .toEqual(['First sentence here.', 'Then a second one. And finally a third.'])
  // Spoken by the speech server, not the browser voice.
  expect(await page.evaluate(() => (window as unknown as StubWindow).__spoken)).toEqual([])

  await setServer(request, token, { base_url: '', api_key: '' })
})
