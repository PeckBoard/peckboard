import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Speech recognition through the speech server (Settings → Speech Server →
 * Speech recognition: Speech server) in a browser with NO
 * webkitSpeechRecognition — the Android WebView case.
 *
 * The microphone is a stubbed `getUserMedia` (an oscillator stream), so the
 * real capture path — AudioContext, the `/pcm16-worklet.js` AudioWorklet,
 * 16 kHz PCM16 frames — runs. The `/api/voice/stt` socket is replaced in
 * the page by a fake that records what the client sends (`__sttSent`,
 * `__sttAudioBytes`) and lets the test push server frames (`__sttEmit`).
 * The relay itself is covered by `ws::voice_stt` tests in Rust.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-stt-'))
  const folderRes = await request.post('/api/folders', {
    headers: { Authorization: `Bearer ${token}` },
    data: { name: `stt-${path.basename(folderPath)}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  return token
}

async function setSpeechServer(request: APIRequestContext, token: string, baseUrl: string) {
  const res = await request.put('/api/voice/speech-server', {
    headers: { Authorization: `Bearer ${token}` },
    data: { base_url: baseUrl },
  })
  expect(res.ok(), `save speech server failed: ${await res.text()}`).toBeTruthy()
}

type StubWindow = {
  __sttSent: { type?: string; token?: string }[]
  __sttAudioBytes: number
  __sttSockets: number
  __sttEmit: (frame: Record<string, unknown>) => void
  __spoken: string[]
}

/** Stubs: no Web Speech recognition, a recording speechSynthesis, an
 *  oscillator microphone and a fake `/api/voice/stt` socket. */
async function primePage(page: Page, token: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
    localStorage.setItem(
      'peckboard_voice_prefs',
      JSON.stringify({ voiceURI: 'stub-alpha', rate: 1, pitch: 1, lang: '', autoListen: true }),
    )
    localStorage.setItem('peckboard.voice.recognition', 'server')
    const w = window as unknown as Record<string, unknown>
    delete w.SpeechRecognition
    for (const name of ['SpeechRecognition', 'webkitSpeechRecognition']) {
      Object.defineProperty(window, name, { value: undefined, configurable: true })
    }
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
    Object.defineProperty(window, 'speechSynthesis', {
      configurable: true,
      value: {
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
      },
    })

    // The microphone: a quiet tone from a separate audio graph.
    navigator.mediaDevices.getUserMedia = async () => {
      const ctx = new AudioContext()
      if (ctx.state !== 'running') await ctx.resume()
      const osc = ctx.createOscillator()
      const gain = ctx.createGain()
      gain.gain.value = 0.1
      const dest = ctx.createMediaStreamDestination()
      osc.connect(gain)
      gain.connect(dest)
      osc.start()
      return dest.stream
    }

    w.__sttSent = []
    w.__sttAudioBytes = 0
    w.__sttSockets = 0
    const sockets: FakeSttSocket[] = []
    class FakeSttSocket {
      readyState = 0
      binaryType = 'blob'
      bufferedAmount = 0
      onopen: ((ev: Event) => void) | null = null
      onmessage: ((ev: MessageEvent) => void) | null = null
      onclose: ((ev: CloseEvent) => void) | null = null
      onerror: ((ev: Event) => void) | null = null
      url: string
      constructor(url: string) {
        this.url = url
        w.__sttSockets = (w.__sttSockets as number) + 1
        sockets.push(this)
        setTimeout(() => {
          this.readyState = 1
          this.onopen?.(new Event('open'))
        }, 0)
      }
      send(data: string | ArrayBuffer) {
        if (typeof data === 'string') {
          const msg = JSON.parse(data) as { type?: string }
          ;(w.__sttSent as unknown[]).push(msg)
          if (msg.type === 'auth') setTimeout(() => this.emit({ type: 'ready' }), 0)
        } else {
          w.__sttAudioBytes = (w.__sttAudioBytes as number) + data.byteLength
        }
      }
      emit(frame: Record<string, unknown>) {
        this.onmessage?.(new MessageEvent('message', { data: JSON.stringify(frame) }))
      }
      close() {
        if (this.readyState === 3) return
        this.readyState = 3
        setTimeout(() => this.onclose?.(new CloseEvent('close')), 0)
      }
    }
    w.__sttEmit = (frame: Record<string, unknown>) => {
      const live = sockets.filter((s) => s.readyState === 1).pop()
      if (!live) throw new Error('no live speech-server socket')
      live.emit(frame)
    }
    const RealWebSocket = window.WebSocket
    function PatchedWebSocket(url: string | URL, protocols?: string | string[]) {
      if (String(url).endsWith('/api/voice/stt')) return new FakeSttSocket(String(url))
      return new RealWebSocket(url, protocols)
    }
    PatchedWebSocket.prototype = RealWebSocket.prototype
    Object.assign(PatchedWebSocket, { CONNECTING: 0, OPEN: 1, CLOSING: 2, CLOSED: 3 })
    w.WebSocket = PatchedWebSocket
  }, token)
}

const stub = <K extends keyof StubWindow>(page: Page, key: K) =>
  page.evaluate((k) => (window as unknown as StubWindow)[k], key) as Promise<StubWindow[K]>

test('speech-server recognition streams the mic and sends a recognized utterance', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const voice = await request.post('/api/voice/session', {
    headers: { Authorization: `Bearer ${token}` },
    data: { model: 'mock:echo' },
  })
  expect(voice.ok(), `voice session failed: ${await voice.text()}`).toBeTruthy()
  // Nothing listens there: only "a server is saved" matters to the page.
  await setSpeechServer(request, token, 'http://127.0.0.1:9')
  try {
    await primePage(page, token)

    // Settings: no browser recognition, but the speech server can listen.
    await page.goto('/settings/voice')
    const select = page.getByTestId('speech-recognition-select')
    await expect(select).toBeEnabled({ timeout: 10_000 })
    await expect(select).toHaveValue('server')
    await expect(page.getByTestId('voice-unsupported')).toHaveCount(0)
    await select.selectOption('browser')
    await expect(page.getByTestId('voice-unsupported')).toBeVisible()
    await select.selectOption('server')
    await expect(page.getByTestId('voice-unsupported')).toHaveCount(0)

    await page.goto('/')
    await page.getByTestId('voice-fab').click()
    await expect(page.getByTestId('voice-status')).toHaveText('Listening')
    await expect
      .poll(() => stub(page, '__sttSent'))
      .toContainEqual(expect.objectContaining({ type: 'auth', token }))
    // 100 ms PCM16 frames from the AudioWorklet reach the socket.
    await expect
      .poll(() => stub(page, '__sttAudioBytes'), { timeout: 10_000 })
      .toBeGreaterThan(3200)

    const said = 'Hello from the speech server.'
    await page.evaluate(() => {
      const w = window as unknown as StubWindow
      w.__sttEmit({ type: 'partial', text: 'Hello from the' })
    })
    await expect(page.getByTestId('voice-interim')).toContainText('Hello from the')
    await page.evaluate((t) => {
      const w = window as unknown as StubWindow
      w.__sttEmit({ type: 'final', final_kind: 'utterance', text: t })
      // The whole-session summary repeats it: not a second utterance.
      w.__sttEmit({ type: 'final', final_kind: 'summary', text: t })
    }, said)

    await expect(page.getByTestId('voice-line-user').last()).toContainText(said, {
      timeout: 15_000,
    })
    await expect
      .poll(() => stub(page, '__spoken'), { timeout: 15_000 })
      .toContainEqual(expect.stringContaining('Hello from the speech server'))
    await expect(page.getByTestId('voice-line-user').filter({ hasText: said })).toHaveCount(1)
  } finally {
    await setSpeechServer(request, token, '')
  }
})
