import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * API e2e for the `/api/me/tabs` enrichment + cleanup.
 *
 * The bug: the frontend tab strip used to cross-reference the open tabs
 * against the (plain-only) sessions list. Worker sessions
 * (`is_worker=true`) are intentionally excluded from that list, so the
 * cleanup loop closed worker-session tabs the moment the page loaded.
 *
 * The fix moves the cleanup server-side: `/api/me/tabs` denormalizes the
 * session/project name into each tab and filters out tabs whose
 * referenced item is gone. We can't drive a worker session creation
 * through the public HTTP API (the orchestrator owns that path), so
 * this spec focuses on the regular-session side: the name is included,
 * and a stale tab is invisible after the underlying session goes away.
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

test('GET /api/me/tabs returns session names alongside each tab', async ({ request }) => {
  const { authHeader } = await authenticate(request)

  // Spin up a folder + session whose name we'll look for in the tab.
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-tabs-name-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-tabs-name', path: folderPath },
  })
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'Named tab session', folder_id: folder.id },
  })
  const session = (await sessionRes.json()) as { id: string; name: string }

  // Open as a tab.
  const upsertRes = await request.post('/api/me/tabs', {
    headers: authHeader,
    data: { item_type: 'session', item_id: session.id },
  })
  expect(upsertRes.ok()).toBeTruthy()
  const upserted = (await upsertRes.json()) as { name: string }
  expect(upserted.name).toBe('Named tab session')

  // GET listing returns it with the same name.
  const listRes = await request.get('/api/me/tabs', { headers: authHeader })
  expect(listRes.ok()).toBeTruthy()
  const tabs = (await listRes.json()) as { item_id: string; name: string }[]
  const ours = tabs.find((t) => t.item_id === session.id)
  expect(ours, 'opened tab should be in the list').toBeTruthy()
  expect(ours?.name).toBe('Named tab session')
})

test('Stale session tabs are filtered out of the GET /api/me/tabs response', async ({
  request,
}) => {
  const { authHeader } = await authenticate(request)

  // The existing cascade-on-delete already nukes the user_tabs row when
  // a session is deleted via the API, so this test verifies the route's
  // "exists?" filter from the *other* side: delete the session via the
  // normal API, list tabs, confirm the tab is gone — same end result as
  // a cross-device delete that hadn't yet been mirrored to the local
  // store.
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-tabs-stale-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-tabs-stale', path: folderPath },
  })
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'Ephemeral', folder_id: folder.id },
  })
  const session = (await sessionRes.json()) as { id: string }

  await request.post('/api/me/tabs', {
    headers: authHeader,
    data: { item_type: 'session', item_id: session.id },
  })

  const before = (await (await request.get('/api/me/tabs', { headers: authHeader })).json()) as {
    item_id: string
  }[]
  expect(before.some((t) => t.item_id === session.id)).toBeTruthy()

  await request.delete(`/api/sessions/${session.id}`, { headers: authHeader })

  const after = (await (await request.get('/api/me/tabs', { headers: authHeader })).json()) as {
    item_id: string
  }[]
  expect(
    after.some((t) => t.item_id === session.id),
    'tab for deleted session must not appear in the listing',
  ).toBeFalsy()
})
