import { test, expect, type APIRequestContext, type Page } from '../harness'
import { copyFileSync, mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { openVoicePanel, voiceTypeInput } from './voice-harness'

/**
 * The voice assistant's media embed: image, video, or 3D model above the orb.
 *
 * The voice session runs on `mock:mcp`, which runs each ```mcp block in the
 * message against the REAL handlers, so a typed tool call exercises the tool,
 * the `assistant-media` frame, and the live page. Speech recognition is faked
 * here; real speech against real speakers is not exercised.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const FIXTURES = path.resolve(process.cwd(), 'e2e/fixtures')

// A 10 mm cube, printable-side up.
const CUBE_STL = (() => {
  const v = (x: number, y: number, z: number) => `vertex ${x * 10} ${y * 10} ${z * 10}`
  const tri = (a: number[], b: number[], c: number[]) =>
    `facet normal 0 0 0\nouter loop\n${v(a[0], a[1], a[2])}\n${v(b[0], b[1], b[2])}\n${v(c[0], c[1], c[2])}\nendloop\nendfacet`
  const quad = (a: number[], b: number[], c: number[], d: number[]) =>
    `${tri(a, b, c)}\n${tri(a, c, d)}`
  const faces = [
    quad([0, 0, 0], [0, 1, 0], [1, 1, 0], [1, 0, 0]),
    quad([0, 0, 1], [1, 0, 1], [1, 1, 1], [0, 1, 1]),
    quad([0, 0, 0], [1, 0, 0], [1, 0, 1], [0, 0, 1]),
    quad([0, 1, 0], [0, 1, 1], [1, 1, 1], [1, 1, 0]),
    quad([0, 0, 0], [0, 0, 1], [0, 1, 1], [0, 1, 0]),
    quad([1, 0, 0], [1, 1, 0], [1, 1, 1], [1, 0, 1]),
  ]
  return `solid cube\n${faces.join('\n')}\nendsolid cube\n`
})()

type SpeechWindow = {
  __voiceRec: unknown
  __voiceSay: (text: string) => void
  __voiceInterim: (text: string) => void
}

/** A folder holding the test media, registered so absolute paths resolve. */
async function setup(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  const headers = { Authorization: `Bearer ${token}` }
  const dir = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-media-'))
  copyFileSync(path.join(FIXTURES, 'test-clip.webm'), path.join(dir, 'clip.webm'))
  copyFileSync(path.join(FIXTURES, 'test-image.png'), path.join(dir, 'picture.png'))
  writeFileSync(path.join(dir, 'cube.stl'), CUBE_STL)
  const folder = await request.post('/api/folders', {
    headers,
    data: { name: `media-${path.basename(dir)}`, path: dir },
  })
  expect(folder.ok(), `create folder failed: ${await folder.text()}`).toBeTruthy()
  const voice = await request.post('/api/voice/session', {
    headers,
    data: { model: 'mock:mcp' },
  })
  expect(voice.ok(), `voice session failed: ${await voice.text()}`).toBeTruthy()
  return { token, dir }
}

/** Auth, silent speech output, and a recognizer the test can talk through. */
async function primePage(page: Page, token: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
    localStorage.setItem(
      'peckboard_voice_prefs',
      JSON.stringify({ voiceURI: '', rate: 1, pitch: 1, lang: '', autoListen: true }),
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
    class FakeUtterance {
      text: string
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
      speak: (u: FakeUtterance) => setTimeout(() => u.onend?.(), 5),
      cancel: () => {},
      pause: () => {},
      resume: () => {},
      addEventListener: () => {},
      removeEventListener: () => {},
    }
    Object.defineProperty(window, 'speechSynthesis', { value: synth, configurable: true })
  }, token)
}

async function openPage(page: Page) {
  await page.goto('/')
  await openVoicePanel(page)
  // The gesture that lets a video start with sound.
  await page.getByTestId('voice-panel').click({ position: { x: 10, y: 10 } })
}

/** The assistant calls a tool. Single line: the typed input drops newlines. */
async function call(page: Page, tool: string, args: Record<string, unknown>) {
  await voiceTypeInput(page).fill('```mcp ' + JSON.stringify({ tool, args }) + ' ```')
  await page.getByTestId('voice-type-send').click()
}

/** Close the transcript drawer, to see the page as the user does. */
async function closeDrawer(page: Page) {
  const toggle = page.getByTestId('assistant-transcript-toggle')
  if ((await toggle.getAttribute('aria-expanded')) === 'true') await toggle.click()
  await expect(toggle).toHaveAttribute('aria-expanded', 'false')
  // The drawer and the orb both animate for about half a second.
  await page.waitForTimeout(800)
}

async function expectAboveOrb(page: Page) {
  const stage = await page.getByTestId('assistant-media-stage').boundingBox()
  const orb = await page.locator('.assistant-orb').boundingBox()
  expect(stage && orb, 'stage and orb both render').toBeTruthy()
  expect(stage!.y + stage!.height).toBeLessThanOrEqual(orb!.y + 1)
  // Centred on the page, and the orb is whole, not cut off by the page's edge.
  const pageBox = (await page.getByTestId('assistant-page').boundingBox())!
  const middle = (b: { x: number; width: number }) => b.x + b.width / 2
  expect(Math.abs(middle(stage!) - middle(pageBox))).toBeLessThan(2)
  expect(orb!.y + orb!.height).toBeLessThanOrEqual(pageBox.y + pageBox.height + 1)
}

const videoState = (page: Page) =>
  page.getByTestId('assistant-media-video').evaluate((el) => {
    const v = el as HTMLVideoElement
    return { paused: v.paused, time: v.currentTime, volume: v.volume, muted: v.muted }
  })

test('an image opens above the orb, turns and zooms on command, and closes', async ({
  request,
  page,
}, testInfo) => {
  const { token, dir } = await setup(request)
  await primePage(page, token)
  await openPage(page)
  await expect(page.getByTestId('assistant-media-stage')).toHaveCount(0)

  await call(page, 'show_media', { path: path.join(dir, 'picture.png'), title: 'Test picture' })
  const image = page.getByTestId('assistant-media-image')
  await expect(image).toBeVisible({ timeout: 15_000 })
  await expect
    .poll(() => image.evaluate((el) => (el as HTMLImageElement).naturalWidth))
    .toBeGreaterThan(0)
  await expect(page.getByTestId('assistant-page')).toHaveAttribute('data-media-open', 'true')
  await closeDrawer(page)
  await expect.poll(() => expectAboveOrb(page).then(() => true)).toBe(true)
  await page.screenshot({ path: testInfo.outputPath('image-desktop.png') })

  const toggle = page.getByTestId('assistant-transcript-toggle')
  await toggle.click()
  await call(page, 'media_control', { action: 'rotate', degrees: 90 })
  await expect(image).toHaveAttribute('data-rotation', '90')
  await call(page, 'media_control', { action: 'zoom', factor: 2 })
  await expect(image).toHaveAttribute('data-zoom', '2')
  await call(page, 'media_control', { action: 'reset' })
  await expect(image).toHaveAttribute('data-rotation', '0')

  // Done reviewing: the embed goes and the orb takes the page back.
  await call(page, 'hide_media', {})
  await expect(page.getByTestId('assistant-media-stage')).toHaveCount(0)
  await expect(page.getByTestId('assistant-page')).toHaveAttribute('data-media-open', 'false')
})

test('a video autoplays with sound and obeys play, pause, and seek', async ({
  request,
  page,
}, testInfo) => {
  const { token, dir } = await setup(request)
  await primePage(page, token)
  await openPage(page)

  await call(page, 'show_media', { path: path.join(dir, 'clip.webm') })
  const video = page.getByTestId('assistant-media-video')
  await expect(video).toBeVisible({ timeout: 15_000 })
  await expect
    .poll(async () => (await videoState(page)).time, { timeout: 15_000 })
    .toBeGreaterThan(0)
  const started = await videoState(page)
  expect(started.paused).toBe(false)
  expect(started.muted, 'plays with sound after a gesture').toBe(false)
  await closeDrawer(page)
  await page.screenshot({ path: testInfo.outputPath('video-desktop.png') })
  await page.getByTestId('assistant-transcript-toggle').click()

  await call(page, 'media_control', { action: 'pause' })
  await expect.poll(async () => (await videoState(page)).paused).toBe(true)

  await call(page, 'media_control', { action: 'seek', seconds: 30 })
  await expect.poll(async () => Math.round((await videoState(page)).time)).toBe(30)
  await call(page, 'media_control', { action: 'seek', by: -10 })
  await expect.poll(async () => Math.round((await videoState(page)).time)).toBe(20)
  // Past the end clamps instead of erroring.
  await call(page, 'media_control', { action: 'seek', seconds: 500 })
  await expect.poll(async () => Math.round((await videoState(page)).time)).toBe(60)
  await call(page, 'media_control', { action: 'seek', seconds: 5 })
  await expect.poll(async () => Math.round((await videoState(page)).time)).toBe(5)

  await call(page, 'media_control', { action: 'play' })
  await expect.poll(async () => (await videoState(page)).paused).toBe(false)
  await call(page, 'media_control', { action: 'mute' })
  await expect.poll(async () => (await videoState(page)).muted).toBe(true)
  await call(page, 'media_control', { action: 'unmute' })
  await expect.poll(async () => (await videoState(page)).muted).toBe(false)

  await call(page, 'hide_media', {})
  await expect(page.getByTestId('assistant-media-stage')).toHaveCount(0)
})

test('while a video plays the mic still hears the user and the video ducks', async ({
  request,
  page,
}) => {
  const { token, dir } = await setup(request)
  await primePage(page, token)
  await openPage(page)
  await expect
    .poll(() => page.evaluate(() => (window as unknown as SpeechWindow).__voiceRec !== null))
    .toBe(true)

  await call(page, 'show_media', { path: path.join(dir, 'clip.webm') })
  await expect
    .poll(async () => (await videoState(page).catch(() => ({ time: 0 }))).time, { timeout: 15_000 })
    .toBeGreaterThan(0)
  const video = page.getByTestId('assistant-media-video')
  await expect.poll(async () => (await videoState(page)).volume).toBe(1)

  // Lowered as soon as the user is heard, and the speech isn't thrown away.
  await page.evaluate(() => (window as unknown as SpeechWindow).__voiceInterim('pause the'))
  await expect(video).toHaveAttribute('data-ducked', 'true')
  await expect.poll(async () => (await videoState(page)).volume).toBeLessThan(0.3)
  await page.evaluate(() => (window as unknown as SpeechWindow).__voiceSay('pause the video'))
  await expect(
    page.getByTestId('voice-panel').getByTestId('voice-line-user').filter({
      hasText: 'pause the video',
    }),
  ).toHaveCount(1, { timeout: 15_000 })

  // The assistant answering doesn't pause it; the volume comes back after.
  const state = await videoState(page)
  expect(state.paused, 'speech must not pause this player').toBe(false)
  await expect(video).toHaveAttribute('data-ducked', 'false', { timeout: 15_000 })
  await expect.poll(async () => (await videoState(page)).volume, { timeout: 15_000 }).toBe(1)
  expect((await videoState(page)).paused).toBe(false)
})

test('a 3D model rotates, zooms, and resets on command', async ({ request, page }, testInfo) => {
  const { token, dir } = await setup(request)
  await primePage(page, token)
  await openPage(page)

  await call(page, 'show_media', { path: path.join(dir, 'cube.stl') })
  const model = page.getByTestId('assistant-media-model')
  await expect(model).toHaveAttribute('data-yaw', /.+/, { timeout: 30_000 })
  await expect(model.locator('canvas')).toBeVisible({ timeout: 30_000 })
  await closeDrawer(page)
  await expect.poll(() => expectAboveOrb(page).then(() => true)).toBe(true)
  await page.waitForTimeout(300)
  const canvas = model.locator('canvas')
  const before = await canvas.screenshot({ path: testInfo.outputPath('model-before.png') })
  await page.screenshot({ path: testInfo.outputPath('model-desktop.png') })
  await page.getByTestId('assistant-transcript-toggle').click()

  const yaw = async () => Number(await model.getAttribute('data-yaw'))
  const zoom = async () => Number(await model.getAttribute('data-zoom'))
  const polar = async () => Number(await model.getAttribute('data-polar'))

  // Measured from the camera itself, not the command.
  await call(page, 'media_control', { action: 'rotate', degrees: 45 })
  await expect.poll(yaw).toBeCloseTo(45, 0)
  await page.waitForTimeout(300)
  const after = await canvas.screenshot({ path: testInfo.outputPath('model-rotated.png') })
  expect(after.equals(before), 'the picture actually changed').toBe(false)
  await call(page, 'media_control', { action: 'rotate', degrees: 45 })
  await expect.poll(yaw).toBeCloseTo(90, 0)
  await call(page, 'media_control', { action: 'rotate', degrees: -90 })
  await expect.poll(yaw).toBeCloseTo(0, 0)

  const startPolar = await polar()
  await call(page, 'media_control', { action: 'rotate', degrees: 20, axis: 'horizontal' })
  await expect.poll(polar).toBeCloseTo(startPolar - 20, 0)
  // Past the pole it stops instead of flipping over.
  await call(page, 'media_control', { action: 'rotate', degrees: 400, axis: 'horizontal' })
  await expect.poll(polar).toBeLessThan(5)

  await call(page, 'media_control', { action: 'zoom', factor: 2 })
  await expect.poll(zoom).toBeCloseTo(2, 1)
  await call(page, 'media_control', { action: 'reset' })
  await expect.poll(yaw).toBeCloseTo(0, 0)
  await expect.poll(zoom).toBeCloseTo(1, 1)
  await expect.poll(polar).toBeCloseTo(startPolar, 0)

  await call(page, 'hide_media', {})
  await expect(page.getByTestId('assistant-media-stage')).toHaveCount(0)
})

test('on a phone the embed stays above the orb, and the close button works', async ({
  request,
  page,
}, testInfo) => {
  await page.setViewportSize({ width: 390, height: 844 })
  const { token, dir } = await setup(request)
  await primePage(page, token)
  await openPage(page)

  await call(page, 'show_media', { path: path.join(dir, 'picture.png'), title: 'Phone picture' })
  await expect(page.getByTestId('assistant-media-image')).toBeVisible({ timeout: 15_000 })
  await closeDrawer(page)
  await expect.poll(() => expectAboveOrb(page).then(() => true)).toBe(true)
  await page.screenshot({ path: testInfo.outputPath('image-phone.png') })

  await page.getByTestId('assistant-media-close').click()
  await expect(page.getByTestId('assistant-media-stage')).toHaveCount(0)
})
