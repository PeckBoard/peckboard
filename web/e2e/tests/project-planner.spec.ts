import { test, expect, type APIRequestContext } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * project-planner plugin (staged + approved by the e2e harness):
 *
 *  - The Folders page offers a per-folder "Project Planner" button
 *    (manifest `folder_items`), which opens the sandboxed slideshow page.
 *  - The start slide renders with a model picker (thinking models only —
 *    the mock provider contributes "Mock: plan review (thinking)").
 *  - Beginning the interview creates + dispatches a real temp session and
 *    the page flips to the thinking state.
 *  - The mock model never calls the planner's MCP tools, so the plugin's
 *    stall watchdog nudges it and then fails the interview with its
 *    explicit message — proving the session wiring, event polling, and
 *    status machine work end to end. (The Q→A→definition cycle itself is
 *    unit-tested in peck-plugins/project-planner/test/planner.test.ts,
 *    where the tool calls can be driven directly.)
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

async function loginUi(page: import('@playwright/test').Page, baseURL: string) {
  await page.goto(baseURL)
  await page.getByLabel('Username').fill(E2E_USER)
  await page.getByLabel('Password').fill(E2E_PASS)
  await page.getByRole('button', { name: /sign in/i }).click()
  await expect(page.locator('.rail')).toBeVisible()
}

test('slideshow page: start → thinking → watchdog verdict on a tool-less run', async ({
  page,
  baseURL,
  request,
}) => {
  expect(baseURL).toBeTruthy()
  const auth = await authenticate(request)

  // The plugin is staged from peck-plugins/project-planner/dist/plugin.wasm
  // at config-eval time; a checkout that never built it (each plugin is its
  // own repo) simply doesn't have the plugin — skip rather than fail.
  const catalogRes = await request.get('/api/plugins', { headers: auth })
  const catalog = catalogRes.ok() ? await catalogRes.json() : { plugins: [] }
  const staged = JSON.stringify(catalog).includes('project-planner')
  test.skip(
    !staged,
    'project-planner wasm not built/staged — run peck-plugins/project-planner/build.sh',
  )

  const folderName = `e2e-planner-${Date.now()}`
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: folderName, path: mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-planner-')) },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  await loginUi(page, baseURL!)
  await page.locator('.rail-btn[title="Folders"]').click()

  // The plugin contributed a per-folder page item.
  await page.getByTestId(`folder-plugin-project-planner-${folderName}`).click()
  await expect(page).toHaveURL(new RegExp(`/folders/${folder.id}/plugin/project-planner$`))
  const frame = page.frameLocator('[data-testid="plugin-fullpage-frame"]')

  // Start slide: hero copy + model picker with the mock thinking model.
  await expect(frame.getByText('Plan this project, one question at a time')).toBeVisible({
    timeout: 15_000,
  })
  const select = frame.locator('.model-select')
  await expect(select).toBeVisible()
  await select.selectOption({ label: 'Mock: plan review (thinking)' })
  await frame.getByRole('button', { name: 'Begin the interview' }).click()

  // The interview is live: a real temp session was created and dispatched.
  await expect(frame.locator('.dots')).toBeVisible({ timeout: 15_000 })

  // The mock run ends without calling project_planner_ask; the watchdog
  // nudges it MAX_NUDGES times and then fails with its explicit verdict.
  await expect(frame.getByText('The interview stopped')).toBeVisible({ timeout: 60_000 })
  await expect(frame.getByText(/without showing a slide/)).toBeVisible()

  // Reset offers a clean restart.
  await frame.getByRole('button', { name: 'Reset and start again' }).click()
  await expect(frame.getByText('Plan this project, one question at a time')).toBeVisible({
    timeout: 15_000,
  })
})
