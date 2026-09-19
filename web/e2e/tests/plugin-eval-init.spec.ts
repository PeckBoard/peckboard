import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * "Plugin evals → Init eval suite" — the claude-only session-menu action
 * that sends the canned eval-authoring prompt (the in-session equivalent
 * of the interactive `claude plugin eval init` interview).
 *
 * The claude test points `cli_path` at a nonexistent binary first, so the
 * turn deterministically crashes at spawn instead of invoking a real CLI;
 * the user event is appended before dispatch, so the prompt bubble is
 * asserted independently of the spawn outcome.
 */

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

async function seedSession(
  request: APIRequestContext,
  auth: Record<string, string>,
  model: string,
) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-plugineval-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-plugineval-${path.basename(folderPath)}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'plugin eval init', folder_id: folder.id, model },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  return session.id
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-bubble').first())).toBeVisible({
    timeout: 10_000,
  })
}

async function setClaudeCliPath(
  request: APIRequestContext,
  auth: Record<string, string>,
  cliPath: string | null,
) {
  const res = await request.put('/api/plugins/claude/settings', {
    headers: auth,
    data: { updates: { cli_path: cliPath } },
  })
  expect(res.ok(), `update cli_path failed: ${await res.text()}`).toBeTruthy()
}

test('hidden on non-claude sessions', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const sessionId = await seedSession(request, auth, 'mock:happy-path')

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  await page.locator('.chat-toolbar-menu').click()
  // Menu is open (Rename always renders) but the claude-only row is absent.
  await expect(page.getByTestId('chat-menu-rename')).toBeVisible()
  await expect(page.getByTestId('chat-menu-plugin-evals')).toHaveCount(0)
})

test('sends the eval-authoring prompt on a claude session', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  // Deterministic spawn failure: no real CLI run, no model usage.
  await setClaudeCliPath(request, auth, '/nonexistent/peckboard-e2e-claude')
  const sessionId = await seedSession(request, auth, 'claude:claude-opus-5')

  try {
    await loadAppAt(page, token, `/sessions/${sessionId}`)

    await page.locator('.chat-toolbar-menu').click()
    const row = page.getByTestId('chat-menu-plugin-evals')
    await expect(row).toBeVisible()
    await row.click()
    await page.getByTestId('chat-menu-plugin-evals-init').click()

    // Confirm dialog: cancel first — nothing is sent.
    const dialog = page.locator('[data-testid="confirm-plugin-eval-init"]')
    await expect(dialog).toBeVisible()
    await dialog.locator('[data-testid="confirm-dialog-cancel"]').click()
    await expect(dialog).toHaveCount(0)
    await expect(page.locator('.chat-bubble-user')).toHaveCount(0)

    // Again, confirmed this time — the canned prompt lands in the feed as
    // a user message (appended server-side before any spawn attempt).
    await page.locator('.chat-toolbar-menu').click()
    await page.getByTestId('chat-menu-plugin-evals').click()
    await page.getByTestId('chat-menu-plugin-evals-init').click()
    await expect(dialog).toBeVisible()
    await dialog.locator('[data-testid="confirm-dialog-confirm"]').click()
    await expect(dialog).toHaveCount(0)
    await expect(page.locator('.chat-bubble-user')).toContainText('Author an eval suite', {
      timeout: 10_000,
    })

    // Let the doomed spawn settle before restoring the setting, so the
    // dead cli_path can't leak into a later spec's dispatch.
    await expect
      .poll(
        async () => {
          const res = await request.get(`/api/sessions/${sessionId}/events?limit=50`, {
            headers: auth,
          })
          if (!res.ok()) return []
          const events = (await res.json()) as { kind: string }[]
          return events.map((e) => e.kind)
        },
        { timeout: 15_000 },
      )
      .toContain('agent-end')
  } finally {
    await setClaudeCliPath(request, auth, null)
  }
})
