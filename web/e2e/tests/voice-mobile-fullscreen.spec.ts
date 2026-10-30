import { test, expect } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { openVoicePanel, voiceTypeInput } from './voice-harness'

/**
 * On a phone-sized viewport the Assistant page's transcript drawer — the
 * voice panel — covers the whole screen (above the rail and tab strip),
 * with the typed-input row pinned to the bottom edge. Desktop keeps the
 * side drawer — covered by `voice-assistant.spec.ts`.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

test.use({ viewport: { width: 390, height: 844 } })

test('mobile: the voice transcript fills the viewport and closes', async ({ request, page }) => {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  // The voice session lives in the user's most recent folder.
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-voice-mobile-'))
  const folderRes = await request.post('/api/folders', {
    headers: { Authorization: `Bearer ${token}` },
    data: { name: `voice-mobile-${path.basename(folderPath)}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()

  // Speech recognition must look supported or the Listen button is inert.
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
    class FakeRecognition {
      start() {}
      stop() {}
      abort() {}
    }
    const w = window as unknown as Record<string, unknown>
    w.SpeechRecognition = FakeRecognition
    w.webkitSpeechRecognition = FakeRecognition
  }, token)
  await page.goto('/')

  const panel = await openVoicePanel(page)
  // Measured once the drawer has finished sliding in.
  await expect.poll(() => panel.boundingBox()).toEqual({ x: 0, y: 0, width: 390, height: 844 })

  // Nothing from the app chrome pokes through: the top-left corner and the
  // bottom edge both hit the panel.
  const hits = await page.evaluate(() => {
    const p = document.querySelector('[data-testid="voice-panel"]')
    return [
      [5, 5],
      [195, 840],
    ].map(([x, y]) => !!p && p.contains(document.elementFromPoint(x, y)))
  })
  expect(hits).toEqual([true, true])

  // Typed-input row is pinned to the bottom of the screen.
  const input = await voiceTypeInput(page).boundingBox()

  expect(input!.y + input!.height).toBeGreaterThan(844 - 80)
  // Closing the transcript returns to the Assistant page (inside the app
  // chrome, so navigation stays reachable) with the conversation running.
  await page.getByTestId('assistant-transcript-toggle').click()
  await expect(page.getByTestId('assistant-transcript')).toHaveAttribute('data-open', 'false')
  await expect(page.getByTestId('assistant-orb')).toBeVisible()
  await expect(page.getByTestId('voice-fab')).toHaveAttribute('aria-pressed', 'true')
})
