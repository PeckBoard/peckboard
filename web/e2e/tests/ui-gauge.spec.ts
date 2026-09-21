import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * ui-gauge plugin 0.3.0 (staged + approved by the e2e harness):
 *
 *  - Generate: the page spawns a temp `mock:mcp` session whose dispatched
 *    prompt embeds the brief; the mock provider runs the ```mcp block in it
 *    against the REAL MCP handler, so `ui_gauge_submit_page` stores a page
 *    exactly the way a live agent would.
 *  - Review: the submitted page renders in a script-less same-origin frame
 *    with one pin per marked element; per-element 👍/👎 + comment + star,
 *    with the auto-star suggestion on liked elements.
 *  - Learn: feedback composes the preference prompt (Do / Avoid / Visual
 *    references) shown on the page; starring captures a screenshot the
 *    plugin serves back.
 *  - Attach: toggling a folder on injects the prompt into that folder's
 *    chat sessions on their next turn; toggling off removes it.
 *
 * Prompt composition, validation, and the sync decision are unit-tested in
 * peck-plugins/ui-gauge; this spec proves the loop end to end.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const PAGE_HTML =
  '<html><body style="background:#f5f6f8;font-family:sans-serif">' +
  '<div data-uig-id="hero" data-uig-label="Hero header" style="padding:24px;background:#4f6bed;color:#fff">Acme Analytics</div>' +
  '<div data-uig-id="cards" data-uig-label="Metric cards" style="display:flex;gap:8px;padding:16px">' +
  '<div style="border:1px solid #ddd;padding:12px;background:#fff">Users: 42</div>' +
  '<div style="border:1px solid #ddd;padding:12px;background:#fff">Revenue: $7</div>' +
  '</div></body></html>'

const SUBMISSION = {
  tool: 'ui_gauge_submit_page',
  args: {
    name: 'Mock dashboard',
    html: PAGE_HTML,
    elements: [
      { id: 'hero', label: 'Hero header', kind: 'header' },
      { id: 'cards', label: 'Metric cards', kind: 'card' },
    ],
    design_notes: 'Mock design notes.',
  },
}

// The brief carries the mcp block verbatim into the generation prompt,
// where the mock:mcp scenario finds and runs it.
const BRIEF = 'a dashboard\n```mcp\n' + JSON.stringify(SUBMISSION) + '\n```'

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

async function skipUnlessStaged(request: APIRequestContext, auth: { Authorization: string }) {
  const catalogRes = await request.get('/api/plugins', { headers: auth })
  const catalog = catalogRes.ok() ? await catalogRes.json() : { plugins: [] }
  test.skip(
    !JSON.stringify(catalog).includes('ui-gauge'),
    'ui-gauge wasm not built/staged — run peck-plugins/ui-gauge/build.sh',
  )
}

test('ui-gauge loop: generate via mock, review elements, star, and attach the folder prompt', async ({
  page,
  baseURL,
  request,
}) => {
  expect(baseURL).toBeTruthy()
  const auth = await authenticate(request)
  await skipUnlessStaged(request, auth)

  // The folder the generation session runs in AND the folder whose chat
  // sessions later receive the prompt. Unique path per run.
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-uig-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'e2e-uig', path: folderPath },
  })
  expect(folderRes.ok(), await folderRes.text()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  await loginUi(page, baseURL!)
  await page.getByTestId('plugin-sidebar-ui-gauge-ui-gauge').click()
  const frame = page.frameLocator('[data-testid="plugin-fullpage-frame"]')

  // Generate: pick the folder + the mock mcp-driver model, brief in hand.
  await expect(frame.getByTestId('gauge-generate')).toBeVisible({ timeout: 15_000 })
  await expect(
    frame.locator('[data-testid="gauge-gen-folder"] option[value="' + folder.id + '"]'),
  ).toHaveCount(1, { timeout: 15_000 })
  await frame.getByTestId('gauge-gen-folder').selectOption(folder.id)
  await frame.getByTestId('gauge-gen-model').selectOption('mock:mcp')
  await frame.getByTestId('gauge-gen-brief').fill(BRIEF)
  await frame.getByTestId('gauge-generate').click()

  // The mock session submits almost immediately; the page never
  // auto-refreshes, so poll via the Refresh button until the card lands.
  await expect(async () => {
    await frame.getByTestId('gauge-refresh').click()
    await expect(frame.getByTestId('gauge-page')).toBeVisible({ timeout: 2_000 })
  }).toPass({ timeout: 30_000 })
  await expect(frame.getByTestId('gauge-page')).toContainText('Mock dashboard')

  // Review: pins over both marked elements, rail = whole page + 2 elements.
  await frame.getByTestId('gauge-open-review').click()
  await expect(frame.getByTestId('gauge-review')).toBeVisible()
  await expect(frame.getByTestId('gauge-pin')).toHaveCount(2, { timeout: 15_000 })
  const railItems = frame.getByTestId('gauge-element')
  await expect(railItems).toHaveCount(3)
  await expect(railItems.nth(1)).toContainText('Hero header')

  // 👍 the hero with a comment; 👎 the cards.
  const hero = railItems.nth(1)
  await hero.getByTestId('gauge-verdict-up').click()
  await hero.getByTestId('gauge-comment').fill('bold header, great contrast')
  await hero.getByTestId('gauge-save-feedback').click()
  const cards = railItems.nth(2)
  await cards.getByTestId('gauge-verdict-down').click()
  await cards.getByTestId('gauge-comment').fill('cards feel cramped')
  await cards.getByTestId('gauge-save-feedback').click()

  // The liked element suggests a star; accepting captures a screenshot and
  // lists the element as a visual reference.
  await expect(hero.getByTestId('gauge-star-suggest')).toBeVisible()
  await hero.getByTestId('gauge-star-suggest').getByRole('button', { name: 'Star' }).click()
  await expect(hero.getByTestId('gauge-star')).toHaveClass(/active/)

  // The composed prompt reflects all of it.
  const prompt = frame.getByTestId('gauge-prompt')
  await expect(prompt).toContainText('### Do')
  await expect(prompt).toContainText('bold header, great contrast')
  await expect(prompt).toContainText('### Avoid')
  await expect(prompt).toContainText('cards feel cramped')
  await expect(prompt).toContainText('Visual references')
  await expect(prompt).toContainText('ui_gauge_reference_image')

  // The captured screenshot is stored and served back.
  const stateRes = await request.get('/api/plugin-ui/ui-gauge/state', { headers: auth })
  expect(stateRes.ok(), await stateRes.text()).toBeTruthy()
  const state = (await stateRes.json()) as { pages: { id: string }[] }
  expect(state.pages.length).toBe(1)
  const shotRes = await request.get(`/api/plugin-ui/ui-gauge/shots/${state.pages[0].id}:hero`, {
    headers: auth,
  })
  expect(shotRes.ok(), await shotRes.text()).toBeTruthy()
  const shot = (await shotRes.json()) as { image_base64: string; mime_type: string }
  expect(shot.image_base64.length).toBeGreaterThan(100)

  // Attach: toggle the folder on; a chat session in it receives the block
  // on its next turn. Wait for the row chip to confirm the toggle landed
  // before dispatching turns — the checkbox handler POSTs asynchronously.
  const uigRow = frame.getByTestId('gauge-folder-row').filter({ hasText: 'e2e-uig' })
  await uigRow.getByTestId('gauge-folder-toggle').check()
  await expect(uigRow).toContainText('prompt attached')

  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'uig chat', folder_id: folder.id },
  })
  expect(sessionRes.ok(), await sessionRes.text()).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  // Each retry sends a fresh turn — the block lands on the next turn after
  // the toggle, whichever turn that is.
  await expect(async () => {
    const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
      headers: auth,
      data: { text: 'hello', model: 'mock:echo' },
    })
    expect(sendRes.ok(), await sendRes.text()).toBeTruthy()
    const res = await request.get(`/api/sessions/${session.id}`, { headers: auth })
    expect(res.ok()).toBeTruthy()
    const detail = (await res.json()) as { system_prompt?: string | null }
    expect(detail.system_prompt || '').toContain('UI taste (ui-gauge)')
  }).toPass({ timeout: 20_000 })

  // Detach: toggle off; the next turn removes the block.
  await uigRow.getByTestId('gauge-folder-toggle').uncheck()
  await expect(uigRow).toContainText('off')
  await expect(async () => {
    const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
      headers: auth,
      data: { text: 'again', model: 'mock:echo' },
    })
    expect(sendRes.ok(), await sendRes.text()).toBeTruthy()
    const res = await request.get(`/api/sessions/${session.id}`, { headers: auth })
    expect(res.ok()).toBeTruthy()
    const detail = (await res.json()) as { system_prompt?: string | null }
    expect(detail.system_prompt || '').not.toContain('UI taste (ui-gauge)')
  }).toPass({ timeout: 20_000 })
})

test('ui-gauge never auto-refreshes: external writes light the stale chip and in-progress edits survive', async ({
  page,
  baseURL,
  request,
}) => {
  expect(baseURL).toBeTruthy()
  const auth = await authenticate(request)
  await skipUnlessStaged(request, auth)

  // A folder to toggle from outside — the external write that must only
  // light the chip, never re-render.
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-uig-stale-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'e2e-uig-stale', path: folderPath },
  })
  expect(folderRes.ok(), await folderRes.text()).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  await loginUi(page, baseURL!)
  await page.getByTestId('plugin-sidebar-ui-gauge-ui-gauge').click()
  const frame = page.frameLocator('[data-testid="plugin-fullpage-frame"]')
  await expect(frame.getByTestId('gauge-gen-brief')).toBeVisible({ timeout: 15_000 })

  // Start an in-progress edit the page must not lose.
  await frame.getByTestId('gauge-gen-brief').fill('wip brief must survive')

  // An external write, like agents make via the ui_gauge_* tools. The
  // page's WebSocket may still be connecting, so retry until the chip
  // lights.
  let enabled = true
  await expect(async () => {
    const res = await request.post('/api/plugin-ui/ui-gauge/folders', {
      headers: auth,
      data: { folder_id: folder.id, enabled },
    })
    expect(res.ok(), await res.text()).toBeTruthy()
    enabled = !enabled
    await expect(frame.getByTestId('gauge-stale')).toBeVisible({ timeout: 2_000 })
  }).toPass({ timeout: 20_000 })

  // No auto-refresh happened: the typed value is still in the input.
  await expect(frame.getByTestId('gauge-gen-brief')).toHaveValue('wip brief must survive')

  // Manual Refresh clears the chip (the brief input is never rewritten by
  // a render, so the draft survives that too).
  await frame.getByTestId('gauge-refresh').click()
  await expect(frame.getByTestId('gauge-stale')).toBeHidden()
  await expect(frame.getByTestId('gauge-gen-brief')).toHaveValue('wip brief must survive')
})
