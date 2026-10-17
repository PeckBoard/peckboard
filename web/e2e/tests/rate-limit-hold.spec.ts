import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * A worker that crashes on a usage limit is held, not crash-blocked: the
 * orchestrator records the account's reset time and waits for it instead
 * of respawning every tick, and the limit never counts toward the
 * two-crash block. The mock `rate-limit` scenario crashes with
 * "usage limit … try again in 12 seconds" on every spawn.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

type CardRow = {
  id: string
  blocked: boolean
  worker_session_id: string | null
  last_worker_session_id: string | null
}

async function getCard(
  request: APIRequestContext,
  auth: Record<string, string>,
  projectId: string,
  cardId: string,
): Promise<CardRow> {
  const res = await request.get(`/api/projects/${projectId}`, { headers: auth })
  expect(res.ok(), `get project failed: ${await res.text()}`).toBeTruthy()
  const body = (await res.json()) as { cards: CardRow[] }
  const card = body.cards.find((c) => c.id === cardId)
  expect(card, 'card present').toBeTruthy()
  return card!
}

/** Usage-limit crashes recorded on the card's latest worker session. */
async function limitCrashes(
  request: APIRequestContext,
  auth: Record<string, string>,
  sessionId: string | null,
): Promise<number> {
  if (!sessionId) return 0
  const res = await request.get(`/api/sessions/${sessionId}/events`, { headers: auth })
  if (!res.ok()) return 0
  const events = (await res.json()) as {
    kind: string
    data: { status?: string; reason?: string }
  }[]
  return events.filter(
    (e) =>
      e.kind === 'agent-end' &&
      e.data.status === 'crashed' &&
      /usage limit/.test(String(e.data.reason ?? '')),
  ).length
}

test('usage-limit crash holds the card until the reset instead of blocking it', async ({
  request,
  page,
}) => {
  test.setTimeout(90_000)
  const { token, auth } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-rate-limit-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-rate-limit-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: 'rate limit hold',
      folder_id: folder.id,
      worker_count: 1,
      workflow: 'task',
      model: 'mock:rate-limit',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  try {
    const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
      headers: auth,
      data: { title: 'Quota task', description: '', step: 'backlog', priority: 1 },
    })
    expect(cardRes.ok(), `create card failed: ${await cardRes.text()}`).toBeTruthy()
    const cardId = ((await cardRes.json()) as { id: string }).id

    // First spawn crashes on the usage limit.
    let sessionId: string | null = null
    await expect
      .poll(
        async () => {
          sessionId = (await getCard(request, auth, project.id, cardId)).last_worker_session_id
          return limitCrashes(request, auth, sessionId)
        },
        { timeout: 30_000 },
      )
      .toBeGreaterThanOrEqual(1)

    // Inside the 12s hold the orchestrator (5s ticks) does not respawn.
    await page.waitForTimeout(7_000)
    const held = await getCard(request, auth, project.id, cardId)
    expect(held.blocked).toBe(false)
    expect(held.worker_session_id).toBeNull()
    expect(await limitCrashes(request, auth, held.last_worker_session_id)).toBe(1)

    // After the reset it retries — and a second limit crash still does not
    // trip the two-crash block.
    await expect
      .poll(
        async () => {
          const c = await getCard(request, auth, project.id, cardId)
          if (c.last_worker_session_id !== sessionId) return 2
          return limitCrashes(request, auth, c.last_worker_session_id)
        },
        { timeout: 40_000 },
      )
      .toBeGreaterThanOrEqual(2)
    expect((await getCard(request, auth, project.id, cardId)).blocked).toBe(false)

    await loadAt(page, token, `/projects/${project.id}`)
    const card = page.locator('.kanban-card').filter({ hasText: 'Quota task' })
    await expect(card).toBeVisible()
    await expect(card.getByTestId('card-blocked-chip')).toHaveCount(0)
  } finally {
    await request.delete(`/api/projects/${project.id}`, { headers: auth })
  }
})
