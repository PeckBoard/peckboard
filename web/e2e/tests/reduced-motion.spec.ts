import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * `prefers-reduced-motion: reduce` support.
 *
 * The global block in `web/src/styles/reduced-motion.css` collapses every
 * animation and transition to ~0 and caps iteration counts at 1, so nothing
 * loops forever. State that was signalled ONLY by motion (a working agent,
 * a lost connection, a running tab) keeps a static halo instead, so the
 * signal survives the motion being removed.
 */

test.use({ reducedMotion: 'reduce' })

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function loadAt(page: Page, token: string, route: string) {
  // Belt-and-braces with the file-level `test.use`: emulate explicitly on
  // the page too, so the preference is set no matter how the context was
  // created.
  await page.emulateMedia({ reducedMotion: 'reduce' })
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
  expect(
    await page.evaluate(() => matchMedia('(prefers-reduced-motion: reduce)').matches),
    'reduced-motion preference is active',
  ).toBe(true)
}

async function makeFolder(request: APIRequestContext, auth: Record<string, string>, tag: string) {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-rm-${tag}-`))
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-rm-${tag}-${Date.now()}`, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as { id: string }
}

async function seedSession(request: APIRequestContext, auth: Record<string, string>) {
  const folder = await makeFolder(request, auth, 'session')
  const res = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'reduced motion', folder_id: folder.id },
  })
  expect(res.ok(), `create session failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

/** Elements whose computed style still declares a looping animation. */
async function loopingAnimations(page: Page): Promise<string[]> {
  return page.evaluate(() =>
    Array.from(document.querySelectorAll('*'))
      .filter((el) => {
        const s = getComputedStyle(el)
        return s.animationName !== 'none' && s.animationIterationCount !== '1'
      })
      .map((el) => `${el.className || el.tagName}:${getComputedStyle(el).animationName}`),
  )
}

test('modals open and close with reduced motion', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const folder = await makeFolder(request, auth, 'project')
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    // No workers — a quiet board, so the modal isn't racing worker updates.
    data: { name: 'reduced motion', folder_id: folder.id, worker_count: 0, workflow: 'task' },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  await loadAt(page, token, `/projects/${project.id}`)
  await page.locator('.kanban-board-scroll').waitFor({ state: 'visible', timeout: 10_000 })

  const backdrop = page.locator('.modal-backdrop')
  await page.getByRole('button', { name: /Add Card/i }).click()
  // `modalIn` / `fadeIn` are collapsed to 0.01ms, so the modal must be
  // visible immediately rather than stuck mid-animation.
  await expect(backdrop).toBeVisible({ timeout: 5_000 })
  await expect(backdrop.locator('.modal').first()).toBeVisible()

  await page.keyboard.press('Escape')
  await expect(backdrop).toBeHidden({ timeout: 5_000 })

  expect(await loopingAnimations(page), 'no looping animation on the board').toEqual([])
})

test('working agent stays distinguishable without its pulse', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const sessionId = await seedSession(request, auth)

  await loadAt(page, token, `/sessions/${sessionId}`)
  await expect(page.getByTestId('chat-toolbar-status')).toBeVisible({ timeout: 10_000 })

  // A bare `user` event puts the UI in the working state: toolbar dot +
  // thinking dots, both animated by default.
  const injected = await request.post(`/api/sessions/${sessionId}/events`, {
    headers: auth,
    data: { kind: 'user', data: { text: 'hello?' } },
  })
  expect(injected.ok(), `inject user failed: ${await injected.text()}`).toBeTruthy()

  const dot = page.locator('.status-dot-working')
  await expect(dot).toBeVisible({ timeout: 10_000 })
  await expect(page.locator('.chat-thinking-dots')).toBeVisible()

  const style = await dot.evaluate((el) => {
    const s = getComputedStyle(el)
    return {
      iterations: s.animationIterationCount,
      duration: s.animationDuration,
      boxShadow: s.boxShadow,
      background: s.backgroundColor,
    }
  })
  // The pulse is gone…
  expect(style.iterations).toBe('1')
  // Computed as `1e-05s` — anything under a millisecond is imperceptible.
  expect(parseFloat(style.duration)).toBeLessThan(0.001)
  // …and a static halo carries the "working" signal in its place.
  expect(style.boxShadow).not.toBe('none')
  expect(style.background).not.toBe('rgba(0, 0, 0, 0)')

  // The status text is still there too, so the state is never colour-only.
  await expect(page.getByTestId('chat-toolbar-status')).toHaveText('Working...')

  expect(await loopingAnimations(page), 'no looping animation in chat').toEqual([])
  const running = await page.evaluate(
    () => document.getAnimations().filter((a) => a.playState === 'running').length,
  )
  expect(running, 'no animation left running').toBe(0)
})
