import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Regression: answering or dismissing a worker question on the kanban
 * board ignored the HTTP status of the `question-resolved` POST — a
 * 4xx/5xx fell through to the success path, deleting the user's typed
 * answer and showing no error while the question stayed pending.
 *
 * Worker sessions can't be created through the public API (POST
 * /api/sessions hardcodes is_worker=false), so the pending question and
 * the failing POST are mocked with page.route. That keeps the test
 * deterministic and exercises exactly the frontend path under test.
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

test('failed question answer keeps the typed text and shows an error', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, auth } = await authenticate(request)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-qerr-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'e2e-qerr', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  // worker_count=0 keeps the orchestrator quiet.
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: { name: 'qerr project', folder_id: folder.id, worker_count: 0, workflow: 'task' },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  // Serve one fake pending question for this project.
  await page.route('**/api/projects/*/pending-questions', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        questions: [
          {
            eventId: 'q-err-1',
            sessionId: 'worker-err-1',
            ts: 1,
            questions: [{ question: 'Pick a color?', header: 'Setup' }],
            cardId: null,
            cardTitle: null,
            cardDescription: null,
          },
        ],
      }),
    }),
  )

  // The answer/dismiss POST fails server-side.
  let postCount = 0
  await page.route('**/api/sessions/worker-err-1/events', (route) => {
    postCount++
    return route.fulfill({
      status: 500,
      contentType: 'application/json',
      body: JSON.stringify({ error: 'session is gone' }),
    })
  })

  await loadAt(page, token, `/projects/${project.id}`)

  await page.locator('.worker-questions-trigger').click()
  const input = page.locator('.question-input')
  await input.fill('blue')
  await page.locator('.question-dialog-footer .btn-primary').click()

  // Error surfaces, the typed answer survives, and the dialog stays open.
  const error = page.getByTestId('question-dialog-error')
  await expect(error).toBeVisible()
  await expect(error).toContainText('session is gone')
  await expect(input).toHaveValue('blue')
  expect(postCount).toBe(1)

  // A failed dismiss also reports instead of pretending it worked.
  await page.locator('.question-dialog-footer .btn-danger-text').click()
  await expect(error).toBeVisible()
  await expect(error).toContainText('session is gone')
  await expect(page.locator('.question-dialog-footer')).toBeVisible()
  expect(postCount).toBe(2)

  // Once the server accepts, the dialog closes and the answer clears.
  await page.unroute('**/api/sessions/worker-err-1/events')
  await page.route('**/api/sessions/worker-err-1/events', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: '{}' }),
  )
  await page.unroute('**/api/projects/*/pending-questions')
  await page.route('**/api/projects/*/pending-questions', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: '{"questions":[]}' }),
  )
  await page.locator('.question-dialog-footer .btn-primary').click()
  await expect(page.locator('.question-dialog-footer')).toHaveCount(0)
})
