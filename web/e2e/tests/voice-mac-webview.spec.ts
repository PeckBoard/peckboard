import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { openVoicePanel } from './voice-harness'

/**
 * The Mac app and `peckboard --desktop` run in a WKWebView, where a microphone
 * failure can be silent: Web Speech may never fire a single event. This fakes
 * that web view's user agent and its failures. It does not run on a real Mac.
 */

const MAC_WEBVIEW =
  'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko)'

test.use({ userAgent: MAC_WEBVIEW })

type MicMode = 'refused' | 'granted'

async function setup(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: 'e2e-user', password: 'e2e-password-1234' },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  const dir = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-macmic-'))
  const folder = await request.post('/api/folders', {
    headers: { Authorization: `Bearer ${token}` },
    data: { name: `macmic-${path.basename(dir)}`, path: dir },
  })
  expect(folder.ok(), `create folder failed: ${await folder.text()}`).toBeTruthy()
  const voice = await request.post('/api/voice/session', {
    headers: { Authorization: `Bearer ${token}` },
    data: { model: 'mock:echo' },
  })
  expect(voice.ok(), `voice session failed: ${await voice.text()}`).toBeTruthy()
  return token
}

/** A recognizer that stays silent, or fails with `recognitionError`. */
async function primePage(page: Page, token: string, mic: MicMode, recognitionError = '') {
  await page.addInitScript(
    ([t, micMode, error]) => {
      localStorage.setItem('peckboard_token', t)
      localStorage.setItem(
        'peckboard_voice_prefs',
        JSON.stringify({ voiceURI: '', rate: 1, pitch: 1, lang: '', autoListen: true }),
      )
      const w = window as unknown as Record<string, unknown>
      w.__micAsks = 0
      navigator.mediaDevices.getUserMedia = async () => {
        ;(w.__micAsks as number)++
        if (micMode === 'refused') throw new DOMException('refused', 'NotAllowedError')
        const ctx = new AudioContext()
        return ctx.createMediaStreamDestination().stream
      }
      class SilentRecognition {
        lang = ''
        continuous = false
        interimResults = false
        maxAlternatives = 1
        onend: (() => void) | null = null
        onerror: ((ev: { error: string }) => void) | null = null
        start() {
          w.__recognitionStarts = ((w.__recognitionStarts as number) ?? 0) + 1
          if (error) setTimeout(() => this.onerror?.({ error }), 10)
        }
        stop() {
          this.onend?.()
        }
        abort() {
          this.onend?.()
        }
      }
      w.SpeechRecognition = SilentRecognition
      w.webkitSpeechRecognition = SilentRecognition
    },
    [token, mic, recognitionError] as const,
  )
}

test('a refused microphone says so instead of doing nothing', async ({ request, page }) => {
  const token = await setup(request)
  await primePage(page, token, 'refused')
  await page.goto('/')
  await openVoicePanel(page)
  await expect(page.getByText('System Settings → Privacy & Security → Microphone')).toBeVisible({
    timeout: 15_000,
  })
  // Asked the OS directly, and didn't go on to a recognizer that can't work.
  expect(await page.evaluate(() => (window as unknown as { __micAsks: number }).__micAsks)).toBe(1)
  expect(
    await page.evaluate(
      () => (window as unknown as { __recognitionStarts?: number }).__recognitionStarts ?? 0,
    ),
  ).toBe(0)
})

test('a recognizer that never answers says so instead of doing nothing', async ({
  request,
  page,
}) => {
  const token = await setup(request)
  await primePage(page, token, 'granted')
  await page.clock.install()
  await page.goto('/')
  await openVoicePanel(page)
  await expect
    .poll(
      async () => {
        await page.clock.runFor(1000)
        return page.evaluate(
          () => (window as unknown as { __recognitionStarts?: number }).__recognitionStarts ?? 0,
        )
      },
      { timeout: 15_000 },
    )
    .toBeGreaterThan(0)
  await page.clock.fastForward('00:40')
  await page.clock.runFor(6000)
  await expect(page.getByText('Speech recognition never responded')).toBeVisible({
    timeout: 15_000,
  })
})

test('the mic test records what happened and reports it to the server', async ({
  request,
  page,
}) => {
  const token = await setup(request)
  await primePage(page, token, 'granted', 'service-not-allowed')
  await page.goto('/')
  await page.locator('.rail-avatar').click()
  await page.locator('.user-menu-dropdown').getByRole('menuitem', { name: 'Settings' }).click()
  const settings = page.getByTestId('settings-page')
  await settings.getByTestId('settings-nav-assistant').click()

  const reported = page.waitForResponse('**/api/voice/mic-report')
  await settings.getByTestId('mic-test').click()
  const output = settings.getByTestId('mic-test-output')
  await expect(output).toContainText('mic level (echo cancellation on)', { timeout: 15_000 })
  await expect(output).toContainText('mic level (echo cancellation off)', { timeout: 15_000 })
  await expect(output).toContainText('ERROR service-not-allowed', { timeout: 20_000 })
  expect((await reported).ok()).toBe(true)
  await expect(settings.getByTestId('mic-test')).toBeEnabled()
})
