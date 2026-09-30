import { test, expect, type APIRequestContext } from '../harness'

/**
 * Settings → Voice → Assistant Prompt: the voice assistant's editable system
 * prompt. Drives the real `/api/voice/prompt*` routes end to end: edit +
 * save, the history list, viewing a version's diff and restoring it, and
 * reset to the built-in default.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

test('edit, save, restore a version and reset the assistant prompt', async ({ request, page }) => {
  const token = await authenticate(request)
  const headers = { Authorization: `Bearer ${token}` }
  // Server state is shared across specs in a shard: start from the default.
  const reset = await request.post('/api/voice/prompt/reset', { headers })
  expect(reset.ok()).toBeTruthy()

  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto('/settings/voice')
  const section = page.getByTestId('voice-prompt-section')
  await expect(section).toBeVisible({ timeout: 10_000 })

  const status = section.getByTestId('voice-prompt-status')
  const textarea = section.getByTestId('voice-prompt-textarea')
  const save = section.getByTestId('voice-prompt-save')
  await expect(status).toHaveText('default')
  await expect(textarea).not.toHaveValue('')
  const original = await textarea.inputValue()
  await expect(save).toBeDisabled()
  await expect(section.getByTestId('voice-prompt-disabled-reason')).toHaveText(
    'No changes to save.',
  )
  await expect(section.getByTestId('voice-prompt-reset')).toBeDisabled()

  // History is loaded with the status; earlier runs on this server may
  // have left rows, so count relative to what's there now.
  const rows = section.locator('.list-view-row')
  const base = await rows.count()
  const first = `${original}\nE2E rule one: always answer in one sentence.`
  await textarea.fill(first)
  await expect(save).toBeEnabled()
  await save.click()
  await expect(section.getByTestId('voice-prompt-saved')).toBeVisible()
  await expect(status).toHaveText('modified')
  await expect(save).toBeDisabled()
  await expect(rows).toHaveCount(base + 1)
  await expect(rows.first()).toContainText('you')

  // Empty is refused with a stated reason.
  await textarea.fill('   ')
  await expect(save).toBeDisabled()
  await expect(section.getByTestId('voice-prompt-disabled-reason')).toHaveText(
    'The prompt can’t be empty.',
  )

  // ── A second edit, then restore the first version from history ────
  await textarea.fill(`${original}\nE2E rule two: speak like a pirate.`)
  await save.click()
  await expect(rows).toHaveCount(base + 2)
  await rows.nth(1).locator('.list-view-item').click()
  const modal = page.getByTestId('voice-prompt-version')
  await expect(modal).toBeVisible()
  await expect(modal.getByTestId('voice-prompt-diff')).toContainText(
    '+E2E rule one: always answer in one sentence.',
  )
  await modal.getByTestId('voice-prompt-restore').click()
  await expect(modal).toBeHidden()
  await expect(textarea).toHaveValue(first)
  await expect(rows).toHaveCount(base + 3)

  const cur = await request.get('/api/voice/prompt', { headers })
  expect(((await cur.json()) as { content: string }).content).toBe(first)

  // ── Reset to default (confirmed) ──────────────────────────────────
  await section.getByTestId('voice-prompt-reset').click()
  const confirm = page.getByTestId('voice-prompt-reset-confirm')
  await expect(confirm).toBeVisible()
  await confirm.getByTestId('confirm-dialog-confirm').click()
  await expect(confirm).toBeHidden()
  await expect(status).toHaveText('default')
  await expect(textarea).toHaveValue(original)
  await expect(rows.first()).toContainText('Reset to default')
})
