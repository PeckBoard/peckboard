import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Voice assistant drives navigation.
 *
 * The voice session runs on `mock:mcp`, which executes every ```mcp block in
 * the message against the REAL MCP handler — so typing a `show_view` call
 * into the voice panel exercises name resolution, the `voice-navigate` WS
 * event on the voice session's stream, and the browser's navigation.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type Auth = { Authorization: string }

async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Auth; folderId: string }> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  const auth = { Authorization: `Bearer ${token}` }
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-voicenav-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `voicenav-${path.basename(folderPath)}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folderId = ((await folderRes.json()) as { id: string }).id
  return { token, auth, folderId }
}

/** Auth token + a silent speechSynthesis so replies don't hit real TTS. */
async function primePage(page: Page, token: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
    const synth = {
      speaking: false,
      pending: false,
      paused: false,
      getVoices: () => [],
      speak: (u: { onend?: () => void }) => setTimeout(() => u.onend?.(), 5),
      cancel: () => {},
      pause: () => {},
      resume: () => {},
      addEventListener: () => {},
      removeEventListener: () => {},
    }
    Object.defineProperty(window, 'speechSynthesis', { value: synth, configurable: true })
  }, token)
}

async function say(page: Page, tool: string, args: Record<string, unknown>) {
  // Single line: the typed fallback is an <input>, which drops newlines.
  const block = '```mcp ' + JSON.stringify({ tool, args }) + ' ```'
  await page.getByTestId('voice-type-input').fill(block)
  await page.getByTestId('voice-type-send').click()
}

test('show_view switches the page to the named project, then session', async ({
  request,
  page,
}) => {
  const { token, auth, folderId } = await authenticate(request)
  const suffix = Date.now().toString(36)

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: { name: `Stashify ${suffix}`, folder_id: folderId, worker_count: 1, workflow: 'task' },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const projectId = ((await projectRes.json()) as { id: string }).id

  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: `Infra ops ${suffix}`, folder_id: folderId },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const sessionId = ((await sessionRes.json()) as { id: string }).id

  const voiceRes = await request.post('/api/voice/session', {
    headers: auth,
    data: { model: 'mock:mcp' },
  })
  expect(voiceRes.ok(), `voice session failed: ${await voiceRes.text()}`).toBeTruthy()

  await primePage(page, token)
  await page.goto('/')
  await page.getByTestId('voice-fab').click()
  await expect(page.getByTestId('voice-panel')).toBeVisible()

  // Spoken-style, lowercase partial name → the project board opens.
  await say(page, 'show_view', { target: 'project', name: `stashify ${suffix}` })
  await expect(page).toHaveURL(new RegExp(`/projects/${projectId}$`), { timeout: 15_000 })

  // Then a session by partial name → its chat opens.
  await say(page, 'show_view', { target: 'session', name: `infra ${suffix}` })
  await expect(page).toHaveURL(new RegExp(`/sessions/${sessionId}$`), { timeout: 15_000 })

  // A top-level page.
  await say(page, 'show_view', { target: 'page', name: 'settings' })
  await expect(page).toHaveURL(/\/settings$/, { timeout: 15_000 })
  await expect(page.getByTestId('settings-page')).toBeVisible()
})
