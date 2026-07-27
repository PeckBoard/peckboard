import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Browser Back/Forward between two plan routes.
 *
 * Regression: App's `popstate` handler had no `plan` branch and `PlanView`
 * read `parseRoute().activeId` during render, so `/plan/a` -> `/plan/b` ->
 * Back changed the URL while `setViewRaw('plan')` was a no-op — React bailed
 * out and the user kept looking at plan B.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type Auth = { token: string; auth: Record<string, string> }

async function authenticate(request: APIRequestContext): Promise<Auth> {
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

async function poll<T>(fn: () => Promise<T | null>, timeoutMs: number, what: string): Promise<T> {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    const v = await fn()
    if (v) return v
    await new Promise((r) => setTimeout(r, 400))
  }
  throw new Error(`timed out waiting for ${what}`)
}

test('back/forward between two plan routes renders the right plan', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)

  // Two cards on the deterministic plan mock => two persisted plans. Both
  // plans carry the same title, so the assertions key off the plan id the
  // view actually rendered (`data-plan-id`).
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-plannav-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-plannav-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: 'plan back nav',
      folder_id: folder.id,
      worker_count: 2,
      workflow: 'task',
      model: 'mock:plan-review',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  const cardIds: string[] = []
  for (const title of ['Widget one', 'Widget two']) {
    const res = await request.post(`/api/projects/${project.id}/cards`, {
      headers: auth,
      data: { title, description: 'do it', step: 'backlog', priority: 1 },
    })
    expect(res.ok(), `create card failed: ${await res.text()}`).toBeTruthy()
    cardIds.push(((await res.json()) as { id: string }).id)
  }

  const planIds: string[] = []
  for (const cardId of cardIds) {
    const plan = await poll(
      async () => {
        const res = await request.get(`/api/plans?card_id=${cardId}`, { headers: auth })
        if (res.status() === 204 || !res.ok()) return null
        return (await res.json()).plan as { id: string }
      },
      60_000,
      `a plan for card ${cardId}`,
    )
    planIds.push(plan.id)
  }
  const [planA, planB] = planIds
  expect(planA).not.toBe(planB)

  const planView = page.locator('[data-testid="plan-view"]')

  // Plan A, then plan B pushed on top of it — exactly what `openPlan()` in
  // web/src/lib/plan.ts does (pushState + a synthetic popstate).
  await loadAt(page, token, `/plan/${planA}`)
  await expect(planView).toHaveAttribute('data-plan-id', planA, { timeout: 15_000 })

  await page.evaluate((id) => {
    window.history.pushState(null, '', `/plan/${id}`)
    window.dispatchEvent(new PopStateEvent('popstate'))
  }, planB)
  await expect(planView).toHaveAttribute('data-plan-id', planB, { timeout: 10_000 })

  // Back must actually re-render plan A, not just rewrite the URL.
  await page.goBack()
  await expect(page).toHaveURL(new RegExp(`/plan/${planA}$`))
  await expect(planView).toHaveAttribute('data-plan-id', planA, { timeout: 10_000 })

  // Forward returns to plan B.
  await page.goForward()
  await expect(page).toHaveURL(new RegExp(`/plan/${planB}$`))
  await expect(planView).toHaveAttribute('data-plan-id', planB, { timeout: 10_000 })

  const del = await request.delete(`/api/projects/${project.id}`, { headers: auth })
  expect(del.ok(), `delete project failed: ${await del.text()}`).toBeTruthy()
})
