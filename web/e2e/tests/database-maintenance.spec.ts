import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync, readFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

// Screenshots for a visual check of the section (light, dark, narrow).
// Written next to the repo; not asserted on.
const SHOTS = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../../.tmp-db-shots')

/**
 * E2E for Settings → Data → Database (`/api/settings/database/*`): the size
 * readout, cleanup preview → confirm → result, compaction and the integrity
 * check. Host-wide and destructive, so admin-gated.
 */

const ADMIN_USER = 'e2e-user'
const ADMIN_PASS = 'e2e-password-1234'

async function authenticateAdmin(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  const res = await request.post('/api/auth/login', {
    data: { username: ADMIN_USER, password: ADMIN_PASS },
  })
  expect(res.ok(), `admin login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

/**
 * Leave one row for the cleanup to find: a plan whose session was deleted.
 * A plan is written through the session's MCP token (the `propose_plan`
 * tool), and deleting one session does not take its plan along.
 */
async function seedOrphanPlan(request: APIRequestContext, auth: Record<string, string>) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-db-upkeep-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'e2e-db-upkeep', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'db upkeep', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  // A live `mock:block` agent keeps the per-session MCP token valid.
  const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
    headers: auth,
    data: { text: 'go', model: 'mock:block' },
  })
  expect(sendRes.ok(), `send message failed: ${await sendRes.text()}`).toBeTruthy()
  const cfgPath = path.join(process.env.PECKBOARD_E2E_DATA_DIR!, 'worker-mcp', `${session.id}.json`)
  await expect
    .poll(() => {
      try {
        readFileSync(cfgPath, 'utf8')
        return true
      } catch {
        return false
      }
    })
    .toBe(true)
  const cfg = JSON.parse(readFileSync(cfgPath, 'utf8')) as {
    mcpServers: { peckboard: { headers: { Authorization: string } } }
  }
  const plan = await request.post('/mcp', {
    headers: { Authorization: cfg.mcpServers.peckboard.headers.Authorization },
    data: {
      jsonrpc: '2.0',
      id: 1,
      method: 'tools/call',
      params: { name: 'propose_plan', arguments: { title: 'Leftover', markdown: '# Leftover' } },
    },
  })
  expect(plan.ok(), `propose_plan failed: ${await plan.text()}`).toBeTruthy()
  const gone = await request.delete(`/api/sessions/${session.id}`, { headers: auth })
  expect(gone.ok(), `delete session failed: ${await gone.text()}`).toBeTruthy()
}

async function openDataSettings(page: Page, token: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto('/settings')
  const settings = page.getByTestId('settings-page')
  await expect(settings).toBeVisible({ timeout: 10_000 })
  await settings.getByTestId('settings-nav-data').click()
  const section = settings.getByTestId('database-maintenance-section')
  await expect(section).toBeVisible()
  return section
}

test('a non-admin is refused by the database upkeep API', async ({ request }) => {
  const { auth: adminAuth } = await authenticateAdmin(request)
  const username = `db-upkeep-${Date.now()}`
  const password = 'gate-password-1234'
  const created = await request.post('/api/users', {
    headers: adminAuth,
    data: { username, password, role: 'user' },
  })
  expect(created.ok(), `create user failed: ${await created.text()}`).toBeTruthy()
  const login = await request.post('/api/auth/login', { data: { username, password } })
  const { token } = (await login.json()) as { token: string }
  const auth = { Authorization: `Bearer ${token}` }

  expect((await request.get('/api/settings/database/stats', { headers: auth })).status()).toBe(403)
  expect((await request.get('/api/settings/database/slow', { headers: auth })).status()).toBe(403)
  for (const route of ['cleanup/preview', 'cleanup', 'compact', 'check']) {
    const res = await request.post(`/api/settings/database/${route}`, { headers: auth })
    expect(res.status(), route).toBe(403)
  }
})

test('an admin previews a cleanup, runs it, compacts and checks the database', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticateAdmin(request)
  // Specs that ran earlier on this server leave their own leftovers (plans
  // of sessions they deleted); clear those so the counts below are ours.
  const baseline = await request.post('/api/settings/database/cleanup', { headers: auth })
  expect(baseline.ok(), `baseline cleanup failed: ${await baseline.text()}`).toBeTruthy()
  await seedOrphanPlan(request, auth)

  const section = await openDataSettings(page, token)

  // Size readout.
  await expect(section.getByTestId('database-size')).toContainText('Size on disk:')
  await expect(section.getByTestId('database-stats')).toContainText('Largest tables:')
  // Slow-operation readout: a list, or the all-clear, both say since when.
  await expect(section.getByTestId('database-slow')).toContainText('since the server started')
  const slow = await request.get('/api/settings/database/slow', { headers: auth })
  expect(slow.ok()).toBeTruthy()
  expect(await slow.json()).toMatchObject({ threshold_ms: 100, operations: expect.any(Array) })

  // Preview lists the leftover plan and removes nothing.
  await section.getByTestId('database-cleanup-preview').click()
  const result = section.getByTestId('database-cleanup-result')
  await expect(result.getByTestId('database-cleanup-item-plans')).toContainText(
    'Plans of deleted sessions: 1',
  )
  await expect(result).toContainText('Nothing has been removed yet.')
  const still = await request.post('/api/settings/database/cleanup/preview', { headers: auth })
  const stillBody = (await still.json()) as { items: Array<{ key: string; rows: number }> }
  expect(stillBody.items.find((i) => i.key === 'plans')?.rows).toBe(1)

  for (const scheme of ['light', 'dark'] as const) {
    await page.emulateMedia({ colorScheme: scheme })
    await section.screenshot({ path: path.join(SHOTS, `database-${scheme}.png`) })
  }
  await page.setViewportSize({ width: 380, height: 800 })
  await section.screenshot({ path: path.join(SHOTS, 'database-dark-380.png') })
  await page.emulateMedia({ colorScheme: null })
  await page.setViewportSize({ width: 1280, height: 720 })

  // Cancel keeps the rows; confirm removes them and says what went.
  await section.getByTestId('database-cleanup-run').click()
  const dialog = page.getByTestId('database-cleanup-confirm')
  await expect(dialog).toContainText('cannot be undone')
  await dialog.getByTestId('confirm-dialog-cancel').click()
  await expect(section.getByTestId('database-cleanup-done')).toHaveCount(0)
  await section.getByTestId('database-cleanup-run').click()
  await page.getByTestId('database-cleanup-confirm').getByTestId('confirm-dialog-confirm').click()
  const done = section.getByTestId('database-cleanup-done')
  await expect(done).toContainText('Removed')
  await expect(done).toContainText('Plans of deleted sessions: 1')

  // A second preview has nothing left.
  await section.getByTestId('database-cleanup-preview').click()
  await expect(section.getByTestId('database-cleanup-result')).toContainText('Nothing to clean up.')

  // Compact: asks first; confirming reports the sizes.
  await section.getByTestId('database-compact').click()
  const compact = page.getByTestId('database-compact-confirm')
  await expect(compact).toContainText('stops responding')
  await compact.getByTestId('confirm-dialog-confirm').click()
  await expect(section.getByTestId('database-compact-done')).toContainText('Compacted:')
  await expect(section.getByTestId('database-size')).toContainText('no unused space')

  // Integrity check.
  await section.getByTestId('database-check').click()
  await expect(section.getByTestId('database-check-result')).toContainText('No problems found.')

  // The server still answers after a compaction.
  const stats = await request.get('/api/settings/database/stats', { headers: auth })
  expect(stats.ok()).toBeTruthy()
})
