import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Grok usage on the dashboard: 500K window, published rates, usage support.
 *
 * Parser occupancy vs billed-sum is unit-tested in the grok provider. This
 * file covers the user-visible surfaces that used to treat grok as Claude:
 * the cost table priced it at Opus, the gauge used 200K, and labels kept
 * the `grok:` prefix. A real grok turn can't run in e2e (no grok CLI
 * fixture), so the gauge is seeded by creating a grok-model session — idle
 * sessions still appear on the usage page (see usage-dashboard.spec.ts).
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

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t as string)
  }, token)
  await page.goto(route)
}

type CostTable = {
  rates: Record<
    string,
    {
      input_per_mtok: number
      output_per_mtok: number
      cache_read_per_mtok: number
      cache_creation_per_mtok: number
    }
  >
}

test('GET /api/usage/costs prices grok at xAI rates, not Opus', async ({ request }) => {
  const { authHeader } = await authenticate(request)
  const res = await request.get('/api/usage/costs', { headers: authHeader })
  expect(res.ok(), `costs failed: ${await res.text()}`).toBeTruthy()
  const table = (await res.json()) as CostTable
  expect(table.rates['grok-4.5']?.output_per_mtok).toBe(6)
  expect(table.rates['grok-4.5']?.cache_read_per_mtok).toBe(0.3)
  expect(table.rates['grok-4.6']?.cache_read_per_mtok).toBe(0.5)
  expect(table.rates['grok-4.5']?.output_per_mtok).not.toBe(75)
})

test('GET /api/models: grok advertises usage', async ({ request }) => {
  const { authHeader } = await authenticate(request)
  const res = await request.get('/api/models', { headers: authHeader })
  expect(res.ok(), `models failed: ${await res.text()}`).toBeTruthy()
  const body = (await res.json()) as {
    providers: Array<{ id: string; capabilities?: { supports_usage?: boolean } }>
  }
  const grok = body.providers.find((p) => p.id === 'grok')
  expect(grok, 'grok provider is registered').toBeTruthy()
  expect(grok!.capabilities?.supports_usage).toBe(true)
})

test('usage gauge sizes a grok session against 500K and strips the grok: prefix', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-grok-usage-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-grok-usage', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'grok gauge', folder_id: folder.id, model: 'grok:grok-4.5' },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string; model: string | null }
  expect(session.model).toBe('grok:grok-4.5')

  await loadAt(page, token, '/usage')
  await expect(page.getByTestId('usage-view')).toBeVisible()

  const row = page.getByTestId('usage-session-row').filter({ hasText: 'grok gauge' })
  await expect(row).toBeVisible()
  await expect(row).toContainText('/ 500.0K')
  await expect(row).toContainText('grok-4.5')
  await expect(row).not.toContainText('grok:grok-4.5')
  await expect(row).not.toContainText('default')

  await row.click()
  const detail = page.getByTestId('usage-detail-context')
  await expect(detail).toBeVisible()
  await expect(detail).toContainText('/ 500.0K')
  await expect(detail).toContainText('grok-4.5')
  await expect(detail).not.toContainText('grok:grok-4.5')
})
