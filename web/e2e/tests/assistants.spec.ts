import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Several named assistants on the Assistant page (`/assistant`):
 *  - the page starts on the default assistant, shown in the switcher chip;
 *  - "Add assistant…" opens a dialog with ready-made jobs; a template fills
 *    the name and job, and the new assistant becomes the one talked to,
 *    with its own voice session;
 *  - a name already in use is refused at the field, before saving;
 *  - switching back and forth asks the server for that assistant's topics;
 *  - an assistant can be renamed and deleted; deleting returns to the
 *    default one, which itself cannot be deleted.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

interface AssistantRow {
  id: string
  name: string
  job: string
  is_default: boolean
  session_id: string | null
  template: string | null
}

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  const authHeader = { Authorization: `Bearer ${token}` }
  // A voice session needs a folder to live in.
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-assistants-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `assistants-${path.basename(folderPath)}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  return { token, authHeader }
}

async function listAssistants(
  request: APIRequestContext,
  authHeader: Record<string, string>,
): Promise<AssistantRow[]> {
  const res = await request.get('/api/assistants', { headers: authHeader })
  expect(res.ok()).toBeTruthy()
  return ((await res.json()) as { assistants: AssistantRow[] }).assistants
}

async function openMenu(page: Page) {
  await page.getByTestId('assistant-switcher').click()
}

test('add, switch between, rename and delete assistants', async ({ request, page }) => {
  const { token, authHeader } = await authenticate(request)

  // The default assistant on a mock model, so nothing reaches a real CLI.
  const first = await request.post('/api/voice/session', {
    headers: authHeader,
    data: { model: 'mock:happy-path' },
  })
  expect(first.ok(), await first.text()).toBeTruthy()
  const defaultSession = ((await first.json()) as { session_id: string }).session_id

  // Leftovers from an earlier run of this spec on the same server.
  for (const a of await listAssistants(request, authHeader)) {
    if (!a.is_default) {
      await request.delete(`/api/assistants/${a.id}`, { headers: authHeader })
    }
  }

  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto('/assistant')
  await expect(page.getByTestId('assistant-page')).toBeVisible({ timeout: 10_000 })
  const chipName = page.getByTestId('assistant-switcher-name')
  const defaultName = (await listAssistants(request, authHeader)).find((a) => a.is_default)!.name
  await expect(chipName).toHaveText(defaultName)

  // Add one from the Coder template.
  await openMenu(page)
  await page.getByTestId('assistant-add').click()
  const dialog = page.getByTestId('assistant-dialog')
  await expect(dialog).toBeVisible()
  await dialog.getByTestId('assistant-template-coder').click()
  await expect(dialog.getByTestId('assistant-name')).toHaveValue('Dev')
  await expect(dialog.getByTestId('assistant-job')).not.toHaveValue('')
  await dialog.getByTestId('assistant-name').fill('Nova')
  await dialog.getByTestId('assistant-save').click()
  await expect(dialog).toBeHidden()
  // The new assistant is the one being talked to, with its own session.
  await expect(chipName).toHaveText('Nova')
  let nova: AssistantRow | undefined
  await expect
    .poll(async () => {
      nova = (await listAssistants(request, authHeader)).find((a) => a.name === 'Nova')
      return nova?.session_id ?? null
    })
    .not.toBeNull()
  expect(nova!.template).toBe('coder')
  expect(nova!.session_id).not.toBe(defaultSession)

  // A name already in use is refused at the field.
  await openMenu(page)
  await page.getByTestId('assistant-add').click()
  await dialog.getByTestId('assistant-name').fill('nova')
  await dialog.getByTestId('assistant-name').blur()
  await expect(dialog.getByTestId('assistant-name-error')).toContainText('already called')
  await expect(dialog.getByTestId('assistant-save')).toBeDisabled()
  await expect(dialog.getByTestId('assistant-dialog-reason')).toContainText('already called')
  await dialog.getByRole('button', { name: 'Cancel' }).click()
  await expect(dialog).toBeHidden()

  // Switching asks the server for that assistant's own topics.
  const defaultId = (await listAssistants(request, authHeader)).find((a) => a.is_default)!.id
  const topicsForDefault = page.waitForRequest(
    (r) => r.url().includes('/api/voice/topics') && r.url().includes(`assistant=${defaultId}`),
  )
  await openMenu(page)
  await page.getByTestId(`assistant-switch-${defaultName}`).click()
  await expect(chipName).toHaveText(defaultName)
  await topicsForDefault
  const topicsForNova = page.waitForRequest(
    (r) => r.url().includes('/api/voice/topics') && r.url().includes(`assistant=${nova!.id}`),
  )
  await openMenu(page)
  await page.getByTestId('assistant-switch-Nova').click()
  await expect(chipName).toHaveText('Nova')
  await topicsForNova

  // Rename.
  await openMenu(page)
  await page.getByTestId('assistant-edit').click()
  await expect(dialog.getByTestId('assistant-name')).toHaveValue('Nova')
  await dialog.getByTestId('assistant-name').fill('Max')
  await dialog.getByTestId('assistant-save').click()
  await expect(dialog).toBeHidden()
  await expect(chipName).toHaveText('Max')

  // Delete: back to the default assistant, and its session is gone.
  await openMenu(page)
  await page.getByTestId('assistant-edit').click()
  await dialog.getByTestId('assistant-delete').click()
  const confirm = page.getByTestId('assistant-delete-confirm')
  await expect(confirm).toContainText('Delete Max?')
  await confirm.getByTestId('confirm-dialog-confirm').click()
  await expect(chipName).toHaveText(defaultName)
  // The page moves to the default assistant first, then deletes.
  await expect(confirm).toBeHidden()
  await expect
    .poll(async () => (await listAssistants(request, authHeader)).map((a) => a.name))
    .toEqual([defaultName])
  const gone = await request.get(`/api/sessions/${nova!.session_id}`, { headers: authHeader })
  expect(gone.status()).toBe(404)

  // The default assistant offers no Delete, and the API refuses it too.
  await openMenu(page)
  await page.getByTestId('assistant-edit').click()
  await expect(dialog.getByTestId('assistant-delete')).toHaveCount(0)
  await dialog.getByRole('button', { name: 'Cancel' }).click()
  const refused = await request.delete(`/api/assistants/${defaultId}`, { headers: authHeader })
  expect(refused.status()).toBe(409)
})
