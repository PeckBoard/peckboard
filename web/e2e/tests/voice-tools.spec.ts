import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { openVoicePanel, voiceTypeInput } from './voice-harness'

/**
 * The voice assistant may call every MCP tool Peckboard offers, unconfirmed.
 *
 * The voice session runs on `mock:mcp`, which executes every ```mcp block in
 * the message against the REAL MCP dispatcher, so typing a call into the
 * panel is exactly the model calling the tool. A tool that used to be denied
 * (`list_files`) and one that used to wait for a Confirm card
 * (`delete_project`) both run right away.
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
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-voicetools-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `voicetools-${path.basename(folderPath)}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folderId = ((await folderRes.json()) as { id: string }).id
  return { token, auth, folderId }
}

/** Auth token and a silent speechSynthesis. */
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

test('the voice assistant runs any Peckboard tool without a confirmation', async ({
  request,
  page,
}) => {
  const { token, auth, folderId } = await authenticate(request)
  const res = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: `Doomed ${Date.now().toString(36)}`,
      folder_id: folderId,
      worker_count: 1,
      workflow: 'task',
    },
  })
  expect(res.ok(), `create project failed: ${await res.text()}`).toBeTruthy()
  const projectId = ((await res.json()) as { id: string }).id

  const voiceRes = await request.post('/api/voice/session', {
    headers: auth,
    data: { model: 'mock:mcp' },
  })
  expect(voiceRes.ok(), `voice session failed: ${await voiceRes.text()}`).toBeTruthy()
  await primePage(page, token)
  await page.goto('/')
  await openVoicePanel(page)

  // Single line: the typed fallback is an <input>, which drops newlines.
  const block = (tool: string, args: Record<string, unknown>) =>
    '```mcp ' + JSON.stringify({ tool, args }) + ' ```'
  await voiceTypeInput(page).fill(
    block('list_files', {}) + ' ' + block('delete_project', { project_id: projectId }),
  )
  await page.getByTestId('voice-type-send').click()

  await expect(page.getByTestId('voice-panel')).toContainText('ran 2/2 mcp block(s)', {
    timeout: 15_000,
  })
  const gone = await request.get(`/api/projects/${projectId}`, { headers: auth })
  expect(gone.ok()).toBe(false)
  await expect(page.getByTestId('voice-action')).toHaveCount(0)
})
