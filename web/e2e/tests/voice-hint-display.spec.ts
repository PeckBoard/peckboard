import { test, expect, type APIRequestContext } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * The voice assistant's inline pronunciation hints (`[word](/phonemes/)`)
 * are for TTS only. Opened in the normal chat view, the voice session's
 * assistant bubble must show plain words — while the reply is still
 * streaming in small chunks (a hint split across chunks) and after it
 * finishes — including plain-ASCII phoneme hints like `[be](/bi/)`.
 * `mock:echo-stream` echoes the message back in 5-character chunks.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-hints-'))
  const folderRes = await request.post('/api/folders', {
    headers: { Authorization: `Bearer ${token}` },
    data: { name: `hints-${path.basename(folderPath)}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  return token
}

const HINTED =
  "[Great](/ɡɹˈAt/). [I'll](/ˈIl/) [tell](/tˈɛl/) [the](/ðə/) [team](/tˈim/) " +
  '[to](/tu/) [be](/bi/) [a](/A/) [bit](/bˈɪt/) [faster](/fˈæstɚ/), [Peckboard](/pˈɛkbˌɔːɹd/)!'
const PLAIN = "Great. I'll tell the team to be a bit faster, Peckboard!"

test('fully hinted voice replies show plain words in the chat view, mid-stream and done', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const auth = { Authorization: `Bearer ${token}` }
  const res = await request.post('/api/voice/session', {
    headers: auth,
    data: { model: 'mock:echo-stream' },
  })
  expect(res.ok(), `voice session failed: ${await res.text()}`).toBeTruthy()
  const { session_id: sessionId } = (await res.json()) as { session_id: string }
  // The voice session is global: an earlier spec may have left a turn
  // running (e.g. mock:block) that would hold our message in the queue.
  await request.post(`/api/sessions/${sessionId}/interrupt`, { headers: auth })
  await expect
    .poll(
      async () => {
        const s = await request.get(`/api/sessions/${sessionId}`, { headers: auth })
        return ((await s.json()) as { status?: string }).status
      },
      { timeout: 10_000 },
    )
    .not.toBe('running')

  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto(`/sessions/${sessionId}`)
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })

  const send = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth,
    data: { text: HINTED },
  })
  expect(send.ok(), `send failed: ${await send.text()}`).toBeTruthy()

  // Sample every assistant bubble while the reply streams: no frame may
  // show markup, even if the reply is split across bubbles mid-hint.
  const bubbles = page.locator('.chat-bubble-assistant')
  await expect(bubbles.last()).toBeVisible({ timeout: 15_000 })
  const allText = async () => (await bubbles.allTextContents()).join(' ')
  const seen: string[] = []
  const deadline = Date.now() + 15_000
  while (Date.now() < deadline) {
    const frame = await bubbles.allTextContents()
    seen.push(...frame)
    if (frame.join(' ').includes('Peckboard!')) break
    await page.waitForTimeout(25)
  }
  for (const frame of seen) {
    expect(frame, `streaming frame leaked markup: ${frame}`).not.toContain('](/')
    expect(frame, `streaming frame leaked markup: ${frame}`).not.toMatch(/\(\/[^)]*$|\/\)/)
  }
  // The words arrive whole, whichever bubbles they land in.
  await expect
    .poll(async () => (await allText()).replace(/\s+/g, ' '), { timeout: 15_000 })
    .toContain('to be a bit faster, Peckboard!')
  const text = await allText()
  for (const word of PLAIN.replace(/[.,!]/g, '').split(' ')) expect(text).toContain(word)
  expect(text).not.toContain('](/')
  // Not rendered as links either.
  await expect(page.locator('.chat-bubble-assistant a')).toHaveCount(0)
})
