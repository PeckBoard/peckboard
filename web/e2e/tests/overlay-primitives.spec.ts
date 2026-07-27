import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * The three overlay copies (ConfirmDialog's own backdrop, ChatView's
 * hand-rolled "Switch model?" portal, ModelPicker's hand-rolled popup) are
 * now one `Modal` and one `Dropdown`. These specs pin the behaviour that
 * consolidation had to preserve or gain:
 *
 *  1. ModelPicker's listbox keyboard model — arrows move a cursor, Enter
 *     picks, Escape closes the popup ONLY (the form behind it survives) and
 *     hands focus back to the trigger.
 *  2. The model-switch dialog is a real dialog: `role`/`aria-modal`, Escape
 *     cancels it, and cancelling changes nothing on the server.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

type AuthBundle = { token: string; authHeader: { Authorization: string } }

async function authenticate(request: APIRequestContext): Promise<AuthBundle> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

async function seedSession(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  model?: string,
): Promise<{ sessionId: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-overlay-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-overlay-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'overlay session', folder_id: folder.id, ...(model ? { model } : {}) },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  return { sessionId: session.id }
}

async function loadApp(page: Page, token: string, route: string) {
  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto(route)
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })
}

test('model picker: arrows + Enter pick a model, Escape closes only the popup', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedSession(request, authHeader)
  await loadApp(page, token, `/sessions/${sessionId}`)

  await page.locator('.tab-new').click()
  const modal = page.locator('.modal', { hasText: 'New Session' })
  await expect(modal).toBeVisible({ timeout: 10_000 })

  const trigger = page.getByTestId('new-session-model')
  await expect(trigger).toContainText('Auto')
  await trigger.click()

  // The popup is a combobox over a listbox — the semantics ModelPicker had
  // and the shared Dropdown now provides for every searchable menu.
  const search = page.getByTestId('new-session-model-search')
  await expect(search).toBeVisible()
  await expect(search).toHaveAttribute('role', 'combobox')
  const listId = await search.getAttribute('aria-controls')
  expect(listId, 'combobox points at its listbox').toBeTruthy()
  const listbox = page.locator(`#${listId}`)
  await expect(listbox).toHaveAttribute('role', 'listbox')
  await search.fill('mock')
  // Scoped to the popup: the form behind it has native <select>s, whose
  // <option>s carry the same ARIA role.
  const options = listbox.getByRole('option')
  await expect(options.first()).toBeVisible()
  const labels = await options.allTextContents()
  expect(labels.length, 'several mock models to arrow through').toBeGreaterThan(2)

  // Two ArrowDowns from the top move the cursor to the third row, and the
  // combobox advertises it via aria-activedescendant.
  await search.press('ArrowDown')
  await search.press('ArrowDown')
  const activeId = await search.getAttribute('aria-activedescendant')
  expect(activeId, 'aria-activedescendant tracks the cursor').toBeTruthy()
  await expect(page.locator(`#${activeId}`)).toHaveText(labels[2])

  await search.press('Enter')
  await expect(search).toHaveCount(0)
  await expect(trigger).toContainText(labels[2])
  await expect(trigger).toBeFocused()

  // ArrowDown reopens from the trigger — no mouse needed.
  await trigger.press('ArrowDown')
  await expect(page.getByTestId('new-session-model-search')).toBeVisible()

  // Escape closes the popup and NOTHING else: the half-filled New Session
  // form is still there, and focus is back on the trigger.
  await page.keyboard.press('Escape')
  await expect(page.getByTestId('new-session-model-search')).toHaveCount(0)
  await expect(modal).toBeVisible()
  await expect(trigger).toBeFocused()
  await expect(trigger).toContainText(labels[2])
})

test('cross-provider switch prompt is a real dialog: Escape cancels it', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()

  const { token, authHeader } = await authenticate(request)
  const { sessionId } = await seedSession(request, authHeader, 'mock:echo')

  // The prompt only appears for a session that HAS a conversation to lose.
  const send = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'hello overlay' },
  })
  expect(send.ok(), `send failed: ${await send.text()}`).toBeTruthy()
  await expect
    .poll(
      async () => {
        const res = await request.get(`/api/sessions/${sessionId}/events?limit=50`, {
          headers: authHeader,
        })
        if (!res.ok()) return 0
        return ((await res.json()) as unknown[]).length
      },
      { timeout: 20_000 },
    )
    .toBeGreaterThan(0)

  await loadApp(page, token, `/sessions/${sessionId}`)

  const trigger = page.getByTestId('chat-toolbar-model')
  await expect(trigger).toBeVisible({ timeout: 10_000 })
  await trigger.click()
  // "Auto" hands the session back to the default provider — a continuity-key
  // change, so the switch has to be confirmed.
  await page.getByTestId('chat-toolbar-model-option-default').click()

  const dialog = page.getByTestId('model-switch-prompt')
  await expect(dialog).toBeVisible()
  await expect(dialog).toHaveAttribute('role', 'dialog')
  await expect(dialog).toHaveAttribute('aria-modal', 'true')
  await expect(dialog).toHaveAttribute('aria-labelledby', /.+/)
  await expect(dialog).toHaveAttribute('aria-describedby', /.+/)
  await expect(page.getByTestId('model-switch-clear')).toBeVisible()
  await expect(page.getByTestId('model-switch-handover')).toBeVisible()

  // The hand-rolled copy had no Escape handler at all: a keyboard user was
  // stuck until they found the mouse.
  await page.keyboard.press('Escape')
  await expect(dialog).toHaveCount(0)
  await expect(trigger).toBeFocused()

  // Cancelling is a no-op on the server — the session keeps its model and
  // no handover was parked.
  const after = await request.get(`/api/sessions/${sessionId}`, { headers: authHeader })
  const detail = (await after.json()) as { model: string | null; handover_to_model: string | null }
  expect(detail.model).toBe('mock:echo')
  expect(detail.handover_to_model).toBeNull()
})
