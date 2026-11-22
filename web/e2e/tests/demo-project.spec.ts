import { test, expect, type APIRequestContext, type Page } from '../harness'

/**
 * Demo project: "See a demo project" on the Projects empty state starts a
 * scripted board whose helpers run on the hidden `mock:demo` model, so a
 * card walks to Done with no AI account. Then the demo is deleted again.
 *
 * The e2e server preinstalls every plugin (Mock included), so the
 * "works with Mock off" half is covered by `tests/demo_project.rs`; here the
 * hidden model must still never reach the model picker's catalog.
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
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

/** The Projects page shows its empty state only with zero projects, and
 *  other specs on this shard may have made some — show it empty. */
async function emptyProjectList(page: Page) {
  await page.route('**/api/projects', async (route) => {
    if (route.request().method() !== 'GET') return route.continue()
    await route.fulfill({ status: 200, contentType: 'application/json', body: '[]' })
  })
}

test('start the demo, watch a card reach Done, then delete the demo', async ({ request, page }) => {
  test.setTimeout(150_000)
  const { token, auth } = await authenticate(request)
  await request.delete('/api/demo-project', { headers: auth })

  // The demo model is hidden from every catalog the UI reads.
  const modelsRes = await request.get('/api/models', { headers: auth })
  expect(modelsRes.ok()).toBeTruthy()
  const catalog = (await modelsRes.json()) as {
    models: { id: string }[]
    providers: { models: { id: string }[] }[]
  }
  const ids = [
    ...catalog.models.map((m) => m.id),
    ...catalog.providers.flatMap((p) => p.models.map((m) => m.id)),
  ]
  expect(ids).not.toContain('mock:demo')

  await emptyProjectList(page)
  await loadAt(page, token, '/projects')
  const demoCard = page.getByTestId('projects-empty').getByTestId('demo-project-card')
  await expect(demoCard.getByRole('heading', { name: 'See a demo project' })).toBeVisible()
  await expect(demoCard).toContainText('no AI account needed')

  await page.unroute('**/api/projects')
  await demoCard.getByTestId('demo-project-start').click()

  // Lands on the demo board with its cards.
  await expect(page).toHaveURL(/\/projects\/[^/]+$/, { timeout: 10_000 })
  await expect(page.locator('.kanban-card')).toHaveCount(4, { timeout: 15_000 })
  await expect(page.locator('.kanban-card-title', { hasText: 'List the pages' })).toBeVisible()

  const demoRes = await request.get('/api/demo-project', { headers: auth })
  const demo = (await demoRes.json()) as {
    id: string
    name: string
    model: string
    folder_id: string
  }
  expect(demo.name).toBe('Demo: Plan a small website')
  expect(demo.model).toBe('mock:demo')
  expect(page.url()).toContain(demo.id)

  // Its folder (inside the data dir) is never offered for real work: it is
  // missing from the folder list, and a session there is refused.
  const foldersRes = await request.get('/api/folders', { headers: auth })
  const folders = (await foldersRes.json()) as { id: string }[]
  expect(folders.map((f) => f.id)).not.toContain(demo.folder_id)
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'not here', folder_id: demo.folder_id, model: 'mock:echo' },
  })
  expect(sessionRes.status()).toBe(400)
  expect(await sessionRes.text()).toContain('belongs to the demo project')

  // The scripted helpers move at least one card all the way to Done.
  const doneColumn = page.locator('.kanban-column').filter({
    has: page.locator('.kanban-column-header h3', { hasText: /^Done$/ }),
  })
  await expect(doneColumn.locator('.kanban-card').first()).toBeVisible({ timeout: 120_000 })

  // Delete it from the card on the (empty) Projects page.
  await emptyProjectList(page)
  await page.goto('/projects')
  const again = page.getByTestId('demo-project-card')
  await again.getByTestId('demo-project-delete').click()
  const dialog = page.getByTestId('demo-project-delete-dialog')
  await expect(dialog).toContainText('Your own projects are not touched')
  await page.getByTestId('confirm-dialog-confirm').click()
  await expect(again.getByTestId('demo-project-start')).toBeVisible({ timeout: 10_000 })

  const gone = await request.get('/api/demo-project', { headers: auth })
  expect(await gone.json()).toBeNull()
  const projectRes = await request.get(`/api/projects/${demo.id}`, { headers: auth })
  expect(projectRes.status()).toBe(404)
})
