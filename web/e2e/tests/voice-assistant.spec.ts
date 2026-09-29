import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Voice assistant e2e.
 *
 * The browser's Web Speech API is stubbed via `addInitScript`:
 * `SpeechRecognition` records the live instance so a test can inject an
 * utterance (`window.__voiceSay(text)`), and `speechSynthesis.speak`
 * appends every utterance's text to `window.__spoken` so we can assert
 * what was (and was not) read aloud. The assistant runs on `mock:*`
 * models so replies are deterministic.
 *
 * Backend contract exercised: `POST /api/voice/session` (get-or-create the
 * user's voice session, optionally switching its model), the normal
 * `/api/sessions/:id/message` send route, and the WS event stream.
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
    }
    const w = window as unknown as Record<string, unknown>
    w.SpeechRecognition = FakeRecognition
    w.webkitSpeechRecognition = FakeRecognition
    w.__voiceRec = null
    w.__recStarts = 0
    // Deliver a final transcript to whichever recognition is live.
    w.__voiceSay = (text: string) => {
      const rec = (window as unknown as { __voiceRec: FakeRecognition | null }).__voiceRec
      if (!rec) throw new Error('no live recognition')
      const result = Object.assign([{ transcript: text }], { isFinal: true, length: 1 })
      rec.onresult?.({ resultIndex: 0, results: [result] })
      rec.finish()
    }

    w.__spoken = [] as string[]
    class FakeUtterance {
      text: string
      voice: unknown = null
      rate = 1
      pitch = 1
      onend: (() => void) | null = null
      onerror: (() => void) | null = null
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
    const synth = {
      speaking: false,
      pending: false,
      paused: false,
      getVoices: () => voices,
      speak: (u: FakeUtterance) => {
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
  }, token)
}

async function spoken(page: Page): Promise<string[]> {
  return page.evaluate(() => (window as unknown as { __spoken: string[] }).__spoken)
}

async function setVoiceModel(request: APIRequestContext, token: string, model: string) {
  const res = await request.post('/api/voice/session', {
    headers: { Authorization: `Bearer ${token}` },
    data: { model },
  })
  expect(res.ok(), `voice session failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as { session_id: string; model: string }
}

test('Listen button opens the panel; an utterance is sent, answered, and spoken', async ({
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
  await expect(page.getByTestId('voice-status')).toHaveText('Idle')

  // Mic on → status flips to Listening and the stub recognition is live.
  await page.getByTestId('voice-mic').click()
  await expect(page.getByTestId('voice-status')).toHaveText('Listening')
  await expect
    .poll(() => page.evaluate(() => (window as unknown as { __recStarts: number }).__recStarts))
    .toBeGreaterThan(0)

  await page.evaluate(() =>
    (window as unknown as { __voiceSay: (t: string) => void }).__voiceSay('Hello voice assistant.'),
  )

  // The utterance shows as a user line; the echo reply lands and is spoken.
  await expect(page.getByTestId('voice-line-user').first()).toContainText('Hello voice assistant.')
  await expect(page.getByTestId('voice-line-assistant').first()).toContainText(
    'Hello voice assistant.',
    { timeout: 15_000 },
  )
  await expect
    .poll(async () => (await spoken(page)).some((s) => s.includes('Hello voice assistant.')))
    .toBe(true)

  // Auto-listen (default on) resumes the mic once the reply is spoken.
  await expect(page.getByTestId('voice-status')).toHaveText('Listening', { timeout: 10_000 })

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
  // "Working on it..." / "Done.", so the only way "[relay]" could be
  // spoken is if the UI read the relay line itself.
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
  await expect.poll(async () => (await spoken(page)).some((s) => s.includes('Done.'))).toBe(true)
  const all = await spoken(page)
  expect(all.some((s) => s.includes('[relay]') || s.includes('api-refactor'))).toBe(false)
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
