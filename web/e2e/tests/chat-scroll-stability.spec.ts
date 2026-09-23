import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Chat history loading must never move what the user is reading:
 *
 *  - "Load older" splices the previous page in above the viewport while the
 *    row on screen stays put, frame by frame (no post-paint snap).
 *  - Once history is exhausted the top bar keeps its height, so nothing
 *    shifts when the button retires.
 *  - Revisiting a session that moved on by more than a page must not leave
 *    a hole between the stale cached scrollback and the fresh newest page.
 *
 * Rows are seeded via the events-injection backdoor, alternating
 * user/agent-text so every event folds to its own row.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

async function seedSession(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  name: string,
) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-scroll-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-${name}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name, folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  return ((await sessionRes.json()) as { id: string }).id
}

const pad = (i: number) => String(i).padStart(4, '0')

async function seedPairs(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  sessionId: string,
  from: number,
  to: number,
) {
  for (let i = from; i <= to; i++) {
    for (const [kind, text] of [
      ['user', `msg-${pad(i)}`],
      ['agent-text', `ans-${pad(i)}`],
    ]) {
      const res = await request.post(`/api/sessions/${sessionId}/events`, {
        headers: authHeader,
        data: { kind, data: { text } },
      })
      expect(res.ok(), `inject ${kind} failed: ${await res.text()}`).toBeTruthy()
    }
  }
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

async function switchTo(page: Page, sessionId: string) {
  await page.evaluate((id) => {
    history.pushState(null, '', `/sessions/${id}`)
    window.dispatchEvent(new PopStateEvent('popstate'))
  }, sessionId)
}

test('loading older history keeps the row being read fixed on screen', async ({
  request,
  page,
}) => {
  test.slow() // seeds 240 events over HTTP
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader, 'scroll-anchor')
  await seedPairs(request, authHeader, sessionId, 1, 120)

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.getByText('ans-0120')).toBeVisible({ timeout: 15_000 })
  await expect(page.getByTestId('chat-load-older')).toBeVisible()

  // Scroll up to just outside the prefetch zone, let measurement settle,
  // then load the older page and sample the anchor row's on-screen top
  // on every frame until the prepend has landed.
  const result = await page.locator('.chat-messages').evaluate(async (el) => {
    const frame = () => new Promise<void>((r) => requestAnimationFrame(() => r()))
    const zone = Math.max(800, el.clientHeight * 2)
    el.scrollTop = zone + 600
    for (let i = 0; i < 15; i++) await frame()
    const boxTop = el.getBoundingClientRect().top
    const rows = Array.from(el.querySelectorAll<HTMLElement>('.chat-vrow'))
    const anchor = rows.find((r) => r.getBoundingClientRect().top >= boxTop + 20)
    if (!anchor) return { error: 'no anchor row' }
    const label = (anchor.textContent ?? '').match(/(msg|ans)-\d{4}/)?.[0]
    if (!label) return { error: 'anchor row has no label' }
    const topOf = () => {
      const row = Array.from(el.querySelectorAll<HTMLElement>('.chat-vrow')).find((r) =>
        (r.textContent ?? '').includes(label),
      )
      return row ? row.getBoundingClientRect().top : null
    }
    const initial = topOf()
    const startHeight = el.scrollHeight
    const btn = document.querySelector<HTMLButtonElement>('[data-testid="chat-load-older"]')
    btn?.click()
    const samples: (number | null)[] = []
    let settledFrames = 0
    for (let i = 0; i < 600 && settledFrames < 30; i++) {
      await frame()
      samples.push(topOf())
      if (el.scrollHeight > startHeight + 500) settledFrames++
    }
    return { label, initial, samples, grew: el.scrollHeight > startHeight + 500 }
  })

  expect(result.error, result.error).toBeUndefined()
  expect(result.grew, 'the older page never landed').toBe(true)
  const drift = (result.samples ?? []).map((t) =>
    t === null ? Infinity : Math.abs(t - (result.initial as number)),
  )
  expect(
    Math.max(...drift),
    `row ${result.label} moved during load-older: ${JSON.stringify(result.samples)}`,
  ).toBeLessThanOrEqual(2)

  // History is exhausted after that short page: the button retires into
  // the start marker and the reading position still does not move.
  await expect(page.getByTestId('chat-load-older')).toHaveCount(0)
  await expect(page.getByTestId('chat-history-start')).toBeAttached()
})

test('revisiting a session that moved on by more than a page leaves no gap', async ({
  request,
  page,
}) => {
  test.slow() // seeds 360 events over HTTP
  const { token, authHeader } = await authenticate(request)
  const idA = await seedSession(request, authHeader, 'gap-a')
  const idB = await seedSession(request, authHeader, 'gap-b')
  await seedPairs(request, authHeader, idA, 1, 30)

  // First visit caches A's whole (short) history.
  await loadAppAt(page, token, `/sessions/${idA}`)
  await expect(page.getByText('ans-0030')).toBeVisible({ timeout: 15_000 })

  // Away on B while A grows by 300 events — past one 200-event page, so
  // pairs 31..80 fall between the cache and the fresh newest page.
  await switchTo(page, idB)
  await expect(page.getByText('ans-0030')).toHaveCount(0, { timeout: 10_000 })
  await seedPairs(request, authHeader, idA, 31, 180)

  await switchTo(page, idA)
  await expect(page.getByText('ans-0180')).toBeVisible({ timeout: 15_000 })

  // Load the full history, then an event from inside the old gap must be
  // Page through whatever history is missing, then an event from inside
  // the old gap must be present exactly once. (A pre-fix build merged the
  // stale cache with the newest page and kept "history exhausted", so the
  // gap could never be loaded and this stayed at "No matches".)
  await page.getByTestId('chat-search-toggle').click()
  await page.getByTestId('chat-search-input').fill('msg-0050')
  const loadAll = page.getByTestId('chat-search-load-all')
  await expect
    .poll(
      async () => {
        if (await loadAll.isVisible()) await loadAll.click()
        return page.getByTestId('chat-search-count').textContent()
      },
      { timeout: 20_000 },
    )
    .toBe('1/1')
  await expect(page.getByTestId('chat-search-scope')).not.toBeVisible({ timeout: 15_000 })
  await expect(page.getByTestId('chat-search-count')).toHaveText('1/1')
  await page.getByTestId('chat-search-input').fill('msg-0001')
  await expect(page.getByTestId('chat-search-count')).toHaveText('1/1')
})
