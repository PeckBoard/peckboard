import { test, expect, type APIRequestContext, type Page } from '../harness'

/**
 * The Welcome page (`/welcome`): the getting-started walkthrough (ticked from
 * real data, each step explained in a panel), and the "What PeckBoard can do"
 * gallery — ordered by the user's use cases (`/api/me/use-cases`, set in the
 * setup wizard) — whose "Try it" buttons start the matching flows. Reached
 * from the Sessions-page summary's "Open Welcome" link.
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

test('the Welcome page walks through the steps and its gallery follows the use cases', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  try {
    const put = await request.put('/api/me/use-cases', {
      headers: auth,
      data: { use_cases: ['printing'] },
    })
    expect(put.status()).toBe(204)
    await loadAt(page, token, '/welcome')

    const welcome = page.getByTestId('welcome-page')
    await expect(welcome).toBeVisible({ timeout: 10_000 })
    await expect(welcome.getByRole('heading', { name: 'Welcome', exact: true })).toBeVisible()

    // Walkthrough: every step is there; picking one explains it.
    for (const key of ['ai', 'folder', 'session', 'project', 'card']) {
      await expect(page.getByTestId(`welcome-step-${key}`)).toBeVisible()
    }
    await page.getByTestId('welcome-step-project').click()
    const panel = page.getByTestId('welcome-step-panel')
    await expect(panel.getByRole('heading', { name: 'Create a project' })).toBeVisible()
    await expect(panel).toContainText('board of tasks')

    // Gallery: eight capabilities plus the phone card; 3D printing puts its
    // cards first.
    const cards = page.getByTestId('welcome-gallery').locator(':scope > li')
    await expect(cards).toHaveCount(9)
    await expect(cards.first()).toHaveAttribute('data-testid', 'welcome-card-views')
    await expect(cards.first()).toHaveAttribute('data-testid', 'welcome-card-views')
    await expect(welcome).not.toContainText(/homekit/i)

    // "Try it" starts the matching flow.
    await page.getByTestId('welcome-card-skills-try').click()
    await expect(page).toHaveURL(/\/settings\/skills$/)
    await page.goBack()
    await expect(welcome).toBeVisible()
    await page.getByTestId('welcome-card-chats-try').click()
    await expect(page.getByRole('heading', { name: 'New Session' })).toBeVisible()
  } finally {
    await request.put('/api/me/use-cases', { headers: auth, data: { use_cases: [] } })
  }
})

test('the Sessions summary links to the Welcome page', async ({ request, page }) => {
  const { token } = await authenticate(request)
  // Other specs in this shard may have done every step, which hides the
  // summary; with no projects at least one step stays open.
  await page.route('**/api/projects', (route) =>
    route.request().method() === 'GET' ? route.fulfill({ json: [] }) : route.continue(),
  )
  await loadAt(page, token, '/sessions')
  const summary = page.getByTestId('getting-started')
  await expect(summary).toBeVisible({ timeout: 10_000 })
  await expect(summary.getByTestId('getting-started-progress')).toContainText('done')
  await summary.getByTestId('getting-started-open-welcome').click()
  await expect(page).toHaveURL(/\/welcome$/)
  await expect(page.getByTestId('welcome-page')).toBeVisible()
})
