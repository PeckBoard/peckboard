import { test, expect, type APIRequestContext, type Locator, type Page } from '../harness'
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Inline playback of shared audio / video (`SharedFileCard`):
 *
 *  - a shared .wav renders the inline audio player with a decoded
 *    waveform, and plays;
 *  - shared .webm and .mp4 play inline in a native `<video>`;
 *  - one at a time: starting any player pauses the one already playing;
 *  - the preview modal continues the video from the inline position;
 *  - a file the browser can't decode says so instead of a dead player;
 *  - .mid stays download-only.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

// 3 s, 32×32, no audio track: VP8 in WebM and VP9 in MP4 (open codecs, so
// Playwright's Chromium decodes both).
const WEBM_CLIP = Buffer.from(
  'GkXfo59ChoEBQveBAULygQRC84EIQoKEd2VibUKHgQJChYECGFOAZwEAAAAAAANDEU2bdLpNu4tTq4QVSalmU6yBoU27i1OrhBZUrmtTrIHWTbuMU6uEElTDZ1OsggEjTbuMU6uEHFO7a1OsggMt7AEAAAAAAABZAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAVSalmsCrXsYMPQkBNgIxMYXZmNjIuMy4xMDBXQYxMYXZmNjIuMy4xMDBEiYhAp3AAAAAAABZUrmvIrgEAAAAAAAA/14EBc8WIpX+tYkI793WcgQAitZyDdW5kiIEAhoVWX1ZQOIOBASPjg4QL68IA4JCwgSC6gSCagQJVsIRVuYEBElTDZ/tzc59jwIBnyJlFo4dFTkNPREVSRIeMTGF2ZjYyLjMuMTAwc3PWY8CLY8WIpX+tYkI793VnyKFFo4dFTkNPREVSRIeUTGF2YzYyLjExLjEwMCBsaWJ2cHhnyKFFo4hEVVJBVElPTkSHkzAwOjAwOjAzLjAwMDAwMDAwMAAfQ7Z1QYTngQCjvYEAAIDQAgCdASogACAAAEcIhYWIhYSIAgICdaoD+AIIIQg9AP7/TRL//FhX8WFfxYV/8WFf/PzO7cX85gCjlYEAyACxAQABEBAAGAAYWC/0AAhwAKOVgQGQALEBAAEQEAAYABhYL/QACHAAo5WBAlgAsQEAARAQABgAGFgv9AAIcACjlYEDIACxAQABEBAAGAAYWC/0AAhwAKOVgQPoALEBAAEQEAAYABhYL/QACHAAo5WBBLAAsQEAARAQABgAGFgv9AAIcACjlYEFeACxAQABEBAUYABhYL/QACHAAKOVgQZAALEBAAEQEAAYABhYL/QACHAAo5WBBwgAsQEAARAQABgAGFgv9AAIcACjlYEH0ACxAQABEBAAGAAYWC/0AAhwAKOVgQiYALEBAAEQEAAYABhYL/QACHAAo5WBCWAAsQEAARAQABgAGFgv9AAIcACjlYEKKACxAQABEBAAGAAYWC/0AAhwAKOVgQrwALEBAAEQEAAYABhYL/QACHAAHFO7a5G7j7OBALeK94EB8YIBo/CBAw==',
  'base64',
)
const MP4_CLIP = Buffer.from(
  'AAAAHGZ0eXBpc29tAAACAGlzb21pc28ybXA0MQAAA05tb292AAAAbG12aGQAAAAAAAAAAAAAAAAAAAPoAAALuAABAAABAAAAAAAAAAAAAAAAAQAAAAAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACAAACeXRyYWsAAABcdGtoZAAAAAMAAAAAAAAAAAAAAAEAAAAAAAALuAAAAAAAAAAAAAAAAAAAAAAAAQAAAAAAAAAAAAAAAAAAAAEAAAAAAAAAAAAAAAAAAEAAAAAAIAAAACAAAAAAACRlZHRzAAAAHGVsc3QAAAAAAAAAAQAAC7gAAAAAAAEAAAAAAfFtZGlhAAAAIG1kaGQAAAAAAAAAAAAAAAAAACgAAAB4AFXEAAAAAAAtaGRscgAAAAAAAAAAdmlkZQAAAAAAAAAAAAAAAFZpZGVvSGFuZGxlcgAAAAGcbWluZgAAABR2bWhkAAAAAQAAAAAAAAAAAAAAJGRpbmYAAAAcZHJlZgAAAAAAAAABAAAADHVybCAAAAABAAABXHN0YmwAAACoc3RzZAAAAAAAAAABAAAAmHZwMDkAAAAAAAAAAQAAAAAAAAAAAAAAAAAAAAAAIAAgAEgAAABIAAAAAAAAAAEYTGF2YzYyLjExLjEwMCBsaWJ2cHgtdnA5AAAAAAAAAAAY//8AAAAUdnBjQwEAAAAACoICAgIAAAAAAApmaWVsAQAAAAAQcGFzcAAAAAEAAAABAAAAFGJ0cnQAAAAAAABOIAAAAAAAAAAYc3R0cwAAAAAAAAABAAAADwAACAAAAAAUc3RzcwAAAAAAAAABAAAAAQAAABxzdHNjAAAAAAAAAAEAAAABAAAADwAAAAEAAABQc3RzegAAAAAAAAAAAAAADwAAACgAAAAPAAAADwAAAA8AAAAPAAAADwAAAA8AAAAPAAAADwAAAA8AAAAPAAAADwAAAA8AAAAPAAAADwAAABRzdGNvAAAAAAAAAAEAAAN6AAAAYXVkdGEAAABZbWV0YQAAAAAAAAAhaGRscgAAAAAAAAAAbWRpcmFwcGwAAAAAAAAAAAAAAAAsaWxzdAAAACSpdG9vAAAAHGRhdGEAAAABAAAAAExhdmY2Mi4zLjEwMAAAAAhmcmVlAAABAm1kYXSCSYNCAAHwAfYAOCQcGEIAADBgAAAQ3//5plf////6Za/////9imAAhgBAkpwASUAAAyAAAENAhgBAkpwASsAAAyAAAENAhgBAkpwAScAAAyAAAENAhgBAkpwASKAAAyAAAENAhgBAkpwAR4AAAyAAAENAhgBAkpwARuAAAyAAAENAhgBAkpwARkAAAyAAAENAhgBAkpwARcAAAyAAAENAhgBAkpwARUAAAyAAAENAhgDAkpwAQQAAAyAAAENAhgBAkpwAROAAAyAAAENAhgBAkpwARGAAAyAAAENAhgBAkpwARAAAAyAAAENAhgBAkpwAQ6AAAyAAAENA',
  'base64',
)

/** A 4 s mono 16-bit PCM tone with a swelling envelope (a visible shape). */
function toneWav(seconds = 4, rate = 8000): Buffer {
  const n = seconds * rate
  const buf = Buffer.alloc(44 + n * 2)
  buf.write('RIFF', 0)
  buf.writeUInt32LE(36 + n * 2, 4)
  buf.write('WAVE', 8)
  buf.write('fmt ', 12)
  buf.writeUInt32LE(16, 16)
  buf.writeUInt16LE(1, 20) // PCM
  buf.writeUInt16LE(1, 22) // mono
  buf.writeUInt32LE(rate, 24)
  buf.writeUInt32LE(rate * 2, 28)
  buf.writeUInt16LE(2, 32)
  buf.writeUInt16LE(16, 34)
  buf.write('data', 36)
  buf.writeUInt32LE(n * 2, 40)
  for (let i = 0; i < n; i++) {
    const env = 0.2 + 0.8 * Math.abs(Math.sin((i / rate) * 2))
    buf.writeInt16LE(Math.round(Math.sin((2 * Math.PI * 440 * i) / rate) * 12000 * env), 44 + i * 2)
  }
  return buf
}

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

/** A session in a fresh folder holding the media, with a live `mock:block`
 *  agent so the per-session MCP token stays valid. */
async function seedSession(request: APIRequestContext, authHeader: Record<string, string>) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-media-'))
  writeFileSync(path.join(folderPath, 'tone.wav'), toneWav())
  writeFileSync(path.join(folderPath, 'clip.webm'), WEBM_CLIP)
  writeFileSync(path.join(folderPath, 'clip.mp4'), MP4_CLIP)
  writeFileSync(path.join(folderPath, 'broken.flac'), Buffer.from('not really flac '.repeat(64)))
  writeFileSync(path.join(folderPath, 'song.mid'), Buffer.from('MThd\0\0\0\u0006\0\0\0\u0001\0`'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-shared-media', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'shared media', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
    headers: authHeader,
    data: { text: 'go', model: 'mock:block' },
  })
  expect(sendRes.ok(), `send message failed: ${await sendRes.text()}`).toBeTruthy()
  const cfgPath = path.join(process.env.PECKBOARD_E2E_DATA_DIR!, 'worker-mcp', `${session.id}.json`)
  await expect
    .poll(() => {
      try {
        readFileSync(cfgPath, 'utf8')
        return true
      } catch {
        return false
      }
    })
    .toBe(true)
  const cfg = JSON.parse(readFileSync(cfgPath, 'utf8')) as {
    mcpServers: { peckboard: { headers: { Authorization: string } } }
  }
  const mcpToken = cfg.mcpServers.peckboard.headers.Authorization.replace(/^Bearer /, '')
  return { sessionId: session.id, mcpToken }
}

async function shareFile(request: APIRequestContext, mcpToken: string, p: string) {
  const res = await request.post('/mcp', {
    headers: { Authorization: `Bearer ${mcpToken}` },
    data: {
      jsonrpc: '2.0',
      id: 1,
      method: 'tools/call',
      params: { name: 'share_file', arguments: { path: p } },
    },
  })
  expect(res.ok()).toBeTruthy()
  const body = (await res.json()) as { error?: unknown; result?: { isError?: boolean } }
  expect(body.error, `share_file ${p} failed: ${JSON.stringify(body)}`).toBeFalsy()
  expect(body.result?.isError).toBeFalsy()
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

const currentTime = (media: Locator) => media.evaluate((el) => (el as HTMLMediaElement).currentTime)
const isPaused = (media: Locator) => media.evaluate((el) => (el as HTMLMediaElement).paused)
const play = (media: Locator) => media.evaluate((el) => (el as HTMLMediaElement).play())

test('shared audio and video play inline, one at a time', async ({ request, page }) => {
  const { token, authHeader } = await authenticate(request)
  const { sessionId, mcpToken } = await seedSession(request, authHeader)
  for (const p of ['tone.wav', 'clip.webm', 'clip.mp4', 'broken.flac', 'song.mid']) {
    await shareFile(request, mcpToken, p)
  }

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  const cards = page.getByTestId('shared-file-card')
  await expect(cards).toHaveCount(5, { timeout: 10_000 })
  const wavCard = cards.filter({ hasText: 'tone.wav' })
  const webmCard = cards.filter({ hasText: 'clip.webm' })
  const mp4Card = cards.filter({ hasText: 'clip.mp4' })
  const brokenCard = cards.filter({ hasText: 'broken.flac' })
  const midCard = cards.filter({ hasText: 'song.mid' })

  // Audio: inline player with a decoded waveform (decoded once the card
  // scrolls into view); Play starts it.
  await expect(wavCard).toHaveAttribute('data-kind', 'audio')
  await wavCard.scrollIntoViewIfNeeded()
  const player = wavCard.getByTestId('audio-player')
  const audio = wavCard.getByTestId('audio-element')
  await expect(wavCard.getByTestId('audio-waveform')).toHaveAttribute('data-state', 'ready')
  await expect(wavCard.getByLabel('Playback speed')).toBeVisible()
  await expect(wavCard.getByRole('button', { name: 'Loop' })).toBeVisible()
  await expect(wavCard.getByTestId('audio-time')).toContainText('0:04')
  await wavCard.getByRole('button', { name: 'Play tone.wav' }).click()
  await expect(player).toHaveAttribute('data-playing', 'true')
  await expect.poll(() => currentTime(audio)).toBeGreaterThan(0.2)

  // WebM video plays inline and pauses the audio.
  const webm = webmCard.getByTestId('shared-file-video')
  await expect(webm).toBeVisible()
  await play(webm)
  await expect.poll(() => currentTime(webm)).toBeGreaterThan(0.2)
  await expect(player).toHaveAttribute('data-playing', 'false')
  expect(await isPaused(audio)).toBe(true)

  // MP4 video plays inline and pauses the WebM.
  const mp4 = mp4Card.getByTestId('shared-file-video')
  await play(mp4)
  await expect.poll(() => currentTime(mp4)).toBeGreaterThan(0.2)
  expect(await isPaused(webm)).toBe(true)

  // Audio again (keyboard: Space on the scrubber) pauses the video.
  await wavCard.getByRole('slider', { name: 'Seek tone.wav' }).focus()
  await page.keyboard.press(' ')
  await expect(player).toHaveAttribute('data-playing', 'true')
  expect(await isPaused(mp4)).toBe(true)

  // The preview modal picks the WebM up where the inline player stopped.
  const inlineAt = await currentTime(webm)
  await webmCard.getByTestId('shared-file-open').click()
  const modal = page.getByTestId('shared-file-modal')
  const modalVideo = modal.getByTestId('shared-file-preview-video')
  await expect(modalVideo).toBeVisible()
  await expect.poll(() => currentTime(modalVideo)).toBeGreaterThanOrEqual(inlineAt)
  await expect(player).toHaveAttribute('data-playing', 'false')
  await page.keyboard.press('Escape')
  await expect(modal).toBeHidden()

  // Undecodable: a clear notice, never a dead player; Download stays.
  await expect(brokenCard.getByTestId('shared-file-unsupported')).toContainText(
    "Your browser can't play this format",
  )
  await expect(brokenCard.getByTestId('audio-player')).toHaveCount(0)
  await expect(brokenCard.getByTestId('shared-file-download')).toBeVisible()

  // MIDI is download-only.
  await expect(midCard).toHaveAttribute('data-kind', 'other')
  await expect(midCard.getByTestId('audio-player')).toHaveCount(0)
})
