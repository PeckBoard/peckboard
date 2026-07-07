import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Screenshot check for the session system-prompt control after it was
 * moved out of the chat toolbar and into the 3-dot (kebab) session menu.
 *
 * Seeds two named prompts, opens the kebab, and captures both the menu
 * (showing the "System prompt" row) and its submenu (showing "(none)"
 * plus the seeded prompts).
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

async function seed(request: APIRequestContext, authHeader: Record<string, string>) {
  for (const name of ['research', 'debug']) {
    const res = await request.post('/api/system-prompts', {
      headers: authHeader,
      data: { name, body: `You are in ${name} mode.` },
    })
    // Idempotent: a reused server may already hold the prompt.
    if (!res.ok()) {
      expect(await res.text(), `create prompt failed`).toContain('already exists')
    }
  }

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-sysprompt-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-sysprompt', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'system prompt UI', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  return { sessionId: session.id }
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

test('System prompt control lives in the kebab menu', async ({ request, page, baseURL }) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seed(request, authHeader)

  await loadAppAt(page, token, `/sessions/${sessionId}`)

  await expect(page.locator('.chat-empty').or(page.locator('.chat-bubble').first())).toBeVisible({
    timeout: 10_000,
  })

  // The old standalone toolbar picker must be gone.
  await expect(page.getByTestId('chat-toolbar-system-prompt')).toHaveCount(0)

  // Open the kebab menu — the "System prompt" row must be present.
  await page.locator('.chat-toolbar-menu').click()
  const row = page.getByRole('menuitem', { name: /System prompt/i })
  await expect(row).toBeVisible()
  await page.screenshot({ path: 'test-results/system-prompt-menu.png' })

  // Open its submenu (opens on click) — "(none)" plus the two seeded
  // prompts must appear. `(none)` is matched exactly so it doesn't also
  // hit the row's "(none)" hint.
  await row.click()
  await expect(page.getByRole('menuitem', { name: '(none)', exact: true })).toBeVisible()
  await expect(page.getByRole('menuitem', { name: 'research' })).toBeVisible()
  await expect(page.getByRole('menuitem', { name: 'debug' })).toBeVisible()
  await page.screenshot({ path: 'test-results/system-prompt-submenu.png' })
})
