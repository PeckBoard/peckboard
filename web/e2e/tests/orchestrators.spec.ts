import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * session-control orchestrators (plugin staged + approved by the e2e
 * harness):
 *
 *  - The global sidebar offers an "Orchestrators" page (manifest
 *    `sidebar_items`) served by the plugin.
 *  - Creating an orchestrator from the page (name, goal, folder, mock
 *    model, watched session) renders a card with counters, goal, and ETA.
 *  - When the watched mock session's agent turn ends
 *    (`session.agent.ended`), the engine fires: the trigger prompt is
 *    delivered to a lazily-created brain session and the card's fires /
 *    actions counters + activity feed update — the full autonomous loop,
 *    minus a real thinking model.
 *  - Dry run renders the prompt without sending; Pause all flips the
 *    global kill switch.
 *
 * Engine timing rides the core `timer.tick` (~30s), so the spec waits for
 * the plugin's clock before triggering. The scoring/goal tools are
 * unit-tested in the plugin crate; this spec proves the hook wiring, the
 * page, and the fire path end to end.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { Authorization: `Bearer ${token}` }
}

async function loginUi(page: Page, baseURL: string) {
  await page.goto(baseURL)
  const username = page.getByLabel('Username')
  await Promise.race([
    username.waitFor({ state: 'visible', timeout: 10_000 }).catch(() => {}),
    page
      .locator('.rail')
      .waitFor({ state: 'visible', timeout: 10_000 })
      .catch(() => {}),
  ])
  if (await username.isVisible().catch(() => false)) {
    await username.fill(E2E_USER)
    await page.getByLabel('Password').fill(E2E_PASS)
    await page.getByRole('button', { name: /sign in/i }).click()
  }
  await expect(page.locator('.rail')).toBeVisible()
}

test('orchestrator: create on the page → watched session idles → fire lands on the brain', async ({
  page,
  baseURL,
  request,
}) => {
  test.setTimeout(180_000)
  expect(baseURL).toBeTruthy()
  const auth = await authenticate(request)

  const catalogRes = await request.get('/api/plugins', { headers: auth })
  const catalog = catalogRes.ok() ? await catalogRes.json() : { plugins: [] }
  test.skip(
    !JSON.stringify(catalog).includes('session-control'),
    'session-control wasm not built/staged — run peck-plugins/session-control/build.sh',
  )

  // A folder for the orchestrator's brain + the watched session.
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-orch-'))
  const folderName = `e2e-orch-${Date.now()}`
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: folderName, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const watchedName = `watched-worker-${Date.now()}`
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: watchedName, folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const watched = (await sessionRes.json()) as { id: string }

  await loginUi(page, baseURL!)
  await page.getByTestId('plugin-sidebar-session-control-orchestrators').click()
  const frame = page.frameLocator('[data-testid="plugin-fullpage-frame"]')

  // The engine's clock is host-fed by timer.tick (~30s cadence); the page
  // exposes it on the list response. Wait for the first tick so the
  // agent-ended trigger below can't race an unset clock.
  await expect
    .poll(
      async () => {
        const r = await request.get('/api/plugin-ui/session-control/orchestrators', {
          headers: auth,
        })
        if (!r.ok()) return ''
        return ((await r.json()) as { clock: string }).clock
      },
      { timeout: 60_000, message: 'timer.tick never set the plugin clock' },
    )
    .not.toBe('')

  // Create the orchestrator from the page. A preset fills Goal + Trigger
  // prompt; both stay editable (the goal is overwritten with a test value).
  await frame.getByTestId('orch-new').click()
  await frame.getByTestId('orch-preset').selectOption('project-def')
  await expect(frame.getByTestId('orch-goal')).toHaveValue(/PROJECT_DEFINITION\.md/)
  await frame.getByTestId('orch-name').fill('Ship the widget')
  await frame.getByTestId('orch-goal').fill('Build the widget end to end')
  await frame.getByTestId('orch-folder').selectOption(folder.id)
  // The mock provider's thinking model keeps the brain deterministic.
  const mockModel = await frame
    .locator('#f-model option', { hasText: /mock/i })
    .first()
    .getAttribute('value')
  expect(mockModel, 'a mock thinking model in the picker').toBeTruthy()
  await frame.locator('#f-model').selectOption(mockModel!)
  // Watchdog off + no cooldown: the only fire in this test is the
  // watched-session trigger, so the assertion is unambiguous.
  await frame.locator('#f-watchdog').fill('0')
  await frame.locator('#f-cooldown').fill('0')
  await frame.locator('.watchbox label', { hasText: watchedName }).locator('input').check()
  await frame.getByTestId('orch-save').click()

  const card = frame.getByTestId('orch-card')
  await expect(card).toBeVisible({ timeout: 15_000 })
  await expect(card).toContainText('Ship the widget')
  await expect(card).toContainText('Build the widget end to end')
  await expect(frame.getByTestId('orch-eta')).toContainText('no estimate yet')

  // Dry run renders the prompt (with the trigger var substituted) without
  // sending anything.
  await card.getByRole('button', { name: 'Dry run' }).click()
  await expect(frame.getByTestId('orch-dryrun')).toBeVisible({ timeout: 10_000 })
  await expect(frame.getByTestId('orch-dryrun')).toContainText('dry-run')

  // Trigger: run the watched mock session to completion. Its
  // session.agent.ended fires the orchestrator, which creates the brain
  // and delivers the rendered prompt.
  const sendRes = await request.post(`/api/sessions/${watched.id}/message`, {
    headers: auth,
    data: { text: 'go', model: 'mock:happy-path' },
  })
  expect(sendRes.ok(), `send message failed: ${await sendRes.text()}`).toBeTruthy()

  // The card's counters + feed update on the page's 5s poll.
  await expect
    .poll(
      async () => {
        const txt = await frame.getByTestId('orch-actions').textContent()
        return Number(txt ?? '0')
      },
      { timeout: 90_000, message: 'the watched-idle trigger never fired' },
    )
    .toBeGreaterThan(0)
  const feed = frame.getByTestId('orch-feed')
  await expect(feed).toContainText('brain session created')
  await expect(feed).toContainText(`watched session "${watchedName}"`)

  // The brain session really exists (created via orchestrate_create_session).
  await expect(card).toContainText('brain:')
  await expect(card).not.toContainText('(created on first fire)')

  // Pause all flips the global kill switch.
  await frame.locator('#pause-all').click()
  await expect(frame.locator('#global-state')).toContainText('ALL PAUSED', { timeout: 10_000 })
  await frame.locator('#pause-all').click()
  await expect(frame.locator('#global-state')).not.toContainText('ALL PAUSED', {
    timeout: 10_000,
  })
})
