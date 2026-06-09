import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * End-to-end tests for the two pagination surfaces:
 *
 * 1. `GET /api/sessions` returns `{items, next_cursor}` and the cursor
 *    round-trips for the next page.
 * 2. `GET /api/sessions/:id/events` honours `before_seq` and the chat
 *    view exposes a "Load older messages" button that pulls another
 *    page in front of the existing buffer without shifting the user's
 *    viewport.
 *
 * Both are wired by the same persistence change ("never close tabs"
 * scaling) so they're tested in the same file — sharing the auth +
 * folder setup keeps the file short.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type AuthBundle = {
  token: string
  authHeader: { Authorization: string }
}

async function authenticate(request: APIRequestContext): Promise<AuthBundle> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

async function uniqueFolder(request: APIRequestContext, authHeader: { Authorization: string }) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-pagination-'))
  const res = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: path.basename(folderPath), path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as { id: string }
}

test('sessions endpoint returns paginated shape and cursor walks to the next page', async ({
  request,
}) => {
  const { authHeader } = await authenticate(request)
  const folder = await uniqueFolder(request, authHeader)

  // Seed 5 sessions one second apart so last_activity timestamps are
  // distinct (no need to test the tie-break path here — DB unit tests
  // cover that).
  const ids: string[] = []
  for (let i = 0; i < 5; i++) {
    const res = await request.post('/api/sessions', {
      headers: authHeader,
      data: { name: `paged-${i}`, folder_id: folder.id },
    })
    expect(res.ok()).toBeTruthy()
    const s = (await res.json()) as { id: string }
    ids.push(s.id)
    await new Promise((r) => setTimeout(r, 25))
  }

  // Page 1: ask for 2 of our 5. The folder filter scopes the response
  // so other tests' sessions can't leak in.
  const p1Res = await request.get(`/api/sessions?folder_id=${folder.id}&limit=2`, {
    headers: authHeader,
  })
  expect(p1Res.ok()).toBeTruthy()
  const p1 = (await p1Res.json()) as {
    items: { id: string; last_activity: string }[]
    next_cursor: { last_activity: string; id: string } | null
  }
  expect(p1.items.length).toBe(2)
  // Newest-first: last inserted comes back first.
  expect(p1.items[0].id).toBe(ids[ids.length - 1])
  expect(p1.next_cursor, 'full page must yield a real next_cursor').not.toBeNull()

  // Page 2: walk the cursor. Must not repeat any id from page 1 — that's
  // the keyset-pagination correctness guarantee.
  const cursor = p1.next_cursor!
  const params = new URLSearchParams({
    folder_id: folder.id,
    limit: '2',
    cursor_la: cursor.last_activity,
    cursor_id: cursor.id,
  })
  const p2Res = await request.get(`/api/sessions?${params.toString()}`, {
    headers: authHeader,
  })
  expect(p2Res.ok()).toBeTruthy()
  const p2 = (await p2Res.json()) as {
    items: { id: string }[]
    next_cursor: { last_activity: string; id: string } | null
  }
  expect(p2.items.length).toBe(2)
  const p1Ids = new Set(p1.items.map((i) => i.id))
  for (const it of p2.items) {
    expect(p1Ids.has(it.id), `page 2 row ${it.id} also on page 1`).toBeFalsy()
  }

  // Page 3: the final partial page. `next_cursor` is null — this is
  // what the frontend uses to stop the infinite-scroll loop.
  const c2 = p2.next_cursor!
  const params3 = new URLSearchParams({
    folder_id: folder.id,
    limit: '2',
    cursor_la: c2.last_activity,
    cursor_id: c2.id,
  })
  const p3Res = await request.get(`/api/sessions?${params3.toString()}`, {
    headers: authHeader,
  })
  const p3 = (await p3Res.json()) as {
    items: { id: string }[]
    next_cursor: unknown
  }
  expect(p3.items.length).toBe(1)
  expect(p3.next_cursor, 'short page must yield null cursor (end of list)').toBeNull()
})

test('events endpoint walks backward via before_seq with oldest-first pages', async ({
  request,
}) => {
  const { authHeader } = await authenticate(request)
  const folder = await uniqueFolder(request, authHeader)
  const sRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'events-paged', folder_id: folder.id },
  })
  const session = (await sRes.json()) as { id: string }

  // Append 8 events by hitting the public events endpoint. `kind` is
  // arbitrary — we only care about seq numbers here.
  for (let i = 0; i < 8; i++) {
    const r = await request.post(`/api/sessions/${session.id}/events`, {
      headers: authHeader,
      data: { kind: 'note', data: { text: `m${i}` } },
    })
    expect(r.ok()).toBeTruthy()
  }

  // Default fetch with `?limit=3` returns the latest 3 events (seqs
  // 6, 7, 8), oldest-first within the page.
  const latestRes = await request.get(`/api/sessions/${session.id}/events?limit=3`, {
    headers: authHeader,
  })
  const latest = (await latestRes.json()) as { seq: number }[]
  expect(latest.map((e) => e.seq)).toEqual([6, 7, 8])

  // Page upward: events strictly before seq 6.
  const olderRes = await request.get(`/api/sessions/${session.id}/events?before_seq=6&limit=3`, {
    headers: authHeader,
  })
  const older = (await olderRes.json()) as { seq: number }[]
  expect(older.map((e) => e.seq)).toEqual([3, 4, 5])

  // Last page: only 2 events left (1, 2), so a 3-limit returns 2 rows.
  // Frontend infers "no more history" from a short page.
  const oldestRes = await request.get(`/api/sessions/${session.id}/events?before_seq=3&limit=3`, {
    headers: authHeader,
  })
  const oldest = (await oldestRes.json()) as { seq: number }[]
  expect(oldest.map((e) => e.seq)).toEqual([1, 2])
})

async function loginInBrowser(page: Page) {
  await page.goto('/')
  await page.getByLabel('Username').fill(E2E_USER)
  await page.getByLabel('Password').fill(E2E_PASS)
  await page.getByRole('button', { name: /sign in/i }).click()
  // Wait for the sidebar to render — proves we made it past auth.
  await expect(page.locator('.list-view, .sidebar, .app-shell')).toBeVisible({ timeout: 10_000 })
}

test('chat view shows "Load older messages" button and prepends a page on click', async ({
  page,
  request,
}) => {
  // Seed a session with > DEFAULT_EVENTS_PAGE_SIZE (200) events via the
  // API so the initial frontend fetch comes back as a full page — only
  // then does the "Load older" button render.
  const { authHeader } = await authenticate(request)
  const folder = await uniqueFolder(request, authHeader)
  const sRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'chat-pagination', folder_id: folder.id },
  })
  const session = (await sRes.json()) as { id: string }

  // 220 events is enough to fill the first page (200) and leave 20 for
  // the "older" page. Using `note` kind keeps the events unrendered as
  // chat bubbles but the server-side seq math is what we're testing.
  // We intersperse a few `user` events at the top so the chat view
  // actually has something to render and a non-zero scroll-height.
  for (let i = 0; i < 220; i++) {
    await request.post(`/api/sessions/${session.id}/events`, {
      headers: authHeader,
      data: {
        kind: i < 3 || i > 216 ? 'user' : 'note',
        data: { text: `msg-${i}` },
      },
    })
  }

  await loginInBrowser(page)

  // Navigate directly to the session by URL — the frontend parses
  // `/sessions/:id` on mount.
  await page.goto(`/sessions/${session.id}`)

  // Wait for the chat to render at least one of our newer `user`
  // messages so we know the default-latest page has loaded.
  await expect(page.getByText('msg-219')).toBeVisible({ timeout: 10_000 })

  // The button must be visible because we just loaded a full page.
  const loadOlder = page.getByTestId('chat-load-older')
  await expect(loadOlder).toBeVisible()

  // Older `user` messages are NOT yet loaded — sanity check the
  // window is actually bounded.
  await expect(page.getByText('msg-0')).toHaveCount(0)

  await loadOlder.click()

  // After the click, the older page splices in at the top — one of
  // the original `user` events (msg-0/1/2) must now be in the DOM.
  await expect(page.getByText('msg-0')).toBeVisible({ timeout: 5_000 })
})
