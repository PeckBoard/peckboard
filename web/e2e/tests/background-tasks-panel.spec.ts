import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e: the Background Tasks panel for peckboard-managed background
 * processes (`run_background`).
 *
 * Driven by `mock:mcp`, which runs every ```mcp block in the prompt against
 * the REAL MCP handler — so these are real processes: one that succeeds
 * (`echo`), one that fails (`false`), and a long `sleep` the test stops
 * from the panel. `run_background` is approval-gated like `run_command`;
 * the host-wide bypass setting is flipped on for the test and restored
 * after, so no approval prompt interrupts the flow.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type AuthHeader = { Authorization: string }

async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; authHeader: AuthHeader }> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

async function seedSession(
  request: APIRequestContext,
  authHeader: AuthHeader,
  name: string,
): Promise<string> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-bg-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    // Pin the model: the completion reports start new turns on the
    // session's own model, which must stay on the mock (the default would
    // be the real Claude CLI).
    data: { name, folder_id: folder.id, model: 'mock:mcp' },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  return ((await sessionRes.json()) as { id: string }).id
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto(route)
}

function mcpBlock(tool: string, args: Record<string, unknown>): string {
  return '```mcp\n' + JSON.stringify({ tool, args }) + '\n```'
}

test('background tasks: panel lists tasks, shows output, stops a running one', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)

  const permsRes = await request.get('/api/settings/tool-permissions', { headers: authHeader })
  expect(permsRes.ok()).toBeTruthy()
  const { bypass: bypassBefore } = (await permsRes.json()) as { bypass: boolean }
  await request.put('/api/settings/tool-permissions', {
    headers: authHeader,
    data: { bypass: true },
  })

  try {
    const sessionId = await seedSession(request, authHeader, 'e2e-background-tasks')
    await loadAppAt(page, token, `/sessions/${sessionId}`)
    await expect(page.locator('.chat-empty').or(page.locator('.chat-bubble').first())).toBeVisible({
      timeout: 10_000,
    })
    // No tasks yet → no toolbar chip.
    await expect(page.getByTestId('bg-tasks-toggle')).toHaveCount(0)

    const reason = 'e2e background task'
    const prompt = [
      'start three background tasks',
      mcpBlock('run_background', {
        command: 'echo',
        args: ['hello-from-background'],
        label: 'say hello',
        reason,
      }),
      mcpBlock('run_background', { command: 'false', label: 'always fails', reason }),
      mcpBlock('run_background', { command: 'sleep', args: ['30'], label: 'long sleep', reason }),
    ].join('\n')
    const sendRes = await request.post(`/api/sessions/${sessionId}/message`, {
      headers: authHeader,
      data: { text: prompt, model: 'mock:mcp' },
    })
    expect(sendRes.ok(), `send failed: ${await sendRes.text()}`).toBeTruthy()

    // Tool rows read as background tasks, with the command as the summary.
    const startRow = page.locator('.tool-block', { hasText: 'Background task' }).first()
    await expect(startRow).toBeVisible({ timeout: 15_000 })
    await expect(startRow.locator('.tool-summary')).toContainText('echo hello-from-background')

    // Completion notices render as compact system rows, not user bubbles.
    const okNotice = page.locator('[data-testid="chat-bg-notice"][data-status="succeeded"]')
    const failNotice = page.locator('[data-testid="chat-bg-notice"][data-status="failed"]')
    await expect(okNotice).toBeVisible({ timeout: 20_000 })
    await expect(failNotice).toBeVisible({ timeout: 20_000 })
    await expect(okNotice).toContainText('say hello')
    await expect(failNotice).toContainText('exit 1')
    // The only user bubble is the prompt itself — the reports aren't bubbles.
    await expect(page.locator('.chat-bubble-user')).toHaveCount(1)

    // Toolbar chip: one task still running.
    const toggle = page.getByTestId('bg-tasks-toggle')
    await expect(toggle).toHaveAttribute('data-running', '1')
    await expect(toggle).toContainText('1 running')

    // Clicking the success notice opens the panel on that task's output.
    await okNotice.getByTestId('chat-bg-notice-open').click()
    const panel = page.getByTestId('bg-tasks-panel')
    await expect(panel).toBeVisible()
    const rows = panel.getByTestId('bg-task-row')
    await expect(rows).toHaveCount(3)
    // Newest first.
    await expect(rows.nth(0)).toContainText('long sleep')
    await expect(rows.nth(0).getByTestId('bg-task-status')).toHaveText('Running')
    await expect(rows.nth(1)).toContainText('always fails')
    await expect(rows.nth(1).getByTestId('bg-task-exit')).toHaveText('exit 1')
    await expect(rows.nth(2).getByTestId('bg-task-status')).toHaveText('Succeeded')
    await expect(panel.getByTestId('bg-task-output-pre')).toContainText('hello-from-background')

    // Select the running task and stop it through the confirmation.
    await rows.nth(0).click()
    const output = panel.getByTestId('bg-task-output')
    await expect(output).toContainText('long sleep')
    await output.getByTestId('bg-task-stop').click()
    const confirm = page.getByTestId('bg-task-stop-confirm')
    await expect(confirm).toBeVisible()
    await confirm.getByTestId('confirm-dialog-confirm').click()
    await expect(confirm).toBeHidden()

    await expect(rows.nth(0).getByTestId('bg-task-status')).toHaveText('Stopped', {
      timeout: 15_000,
    })
    await expect(toggle).toHaveAttribute('data-running', '0')
    await expect(output.getByTestId('bg-task-stop')).toHaveCount(0)
    await expect(page.locator('[data-testid="chat-bg-notice"][data-status="stopped"]')).toBeVisible(
      { timeout: 15_000 },
    )

    await page.screenshot({ path: 'test-results/background-tasks-panel.png', fullPage: true })

    // Closing hides the panel; the chip stays (the session has tasks).
    await panel.getByTestId('bg-tasks-close').click()
    await expect(panel).toBeHidden()
    await expect(toggle).toBeVisible()
  } finally {
    await request.put('/api/settings/tool-permissions', {
      headers: authHeader,
      data: { bypass: bypassBefore },
    })
  }
})
