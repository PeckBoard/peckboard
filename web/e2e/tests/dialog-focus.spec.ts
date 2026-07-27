import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * The focus contract every dialog inherits from the two shared
 * primitives (`Modal`, `ConfirmDialog`) via `useDialogFocus`:
 *
 *   - the panel is a real dialog (`role`, `aria-modal`, `aria-labelledby`)
 *   - opening it moves focus inside, safe action first for a danger dialog
 *   - Tab / Shift+Tab cycle within the panel and never escape it
 *   - Escape closes it and focus returns to the trigger
 *
 * The ConfirmDialog case goes through a 3-dot menu on purpose: the menu
 * item unmounts as the dialog mounts, which is exactly the path that used
 * to drop focus onto `<body>`.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

let cachedAuth: { token: string; auth: Record<string, string> } | null = null

async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  if (cachedAuth) return cachedAuth
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  cachedAuth = { token, auth: { Authorization: `Bearer ${token}` } }
  return cachedAuth
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

async function createFolder(
  request: APIRequestContext,
  auth: Record<string, string>,
  slug: string,
): Promise<string> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-${slug}-`))
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-${slug}-${Date.now()}`, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

/** `data-testid` of the focused element, or null. */
function activeTestId(page: Page): Promise<string | null> {
  return page.evaluate(() => document.activeElement?.getAttribute('data-testid') ?? null)
}

/** Is focus inside the element matching `selector`? */
function focusInside(page: Page, selector: string): Promise<boolean> {
  return page.evaluate((sel) => !!document.activeElement?.closest(sel), selector)
}

/** Tab-order of `data-testid`s inside the dialog, walked with real key
 *  presses so the trap itself decides where focus goes. */
async function tabCycle(page: Page, selector: string, steps: number): Promise<string[]> {
  const seen: string[] = []
  for (let i = 0; i < steps; i++) {
    await page.keyboard.press('Tab')
    expect(await focusInside(page, selector), `focus left the dialog on Tab #${i + 1}`).toBe(true)
    seen.push((await activeTestId(page)) ?? '(untagged)')
  }
  return seen
}

test('a danger ConfirmDialog opened from a 3-dot menu traps and restores focus', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const folderId = await createFolder(request, auth, 'dialogfocus')

  // worker_count: 0 — no orchestrator moving the card under the test.
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: `dialog focus ${Date.now()}`,
      folder_id: folderId,
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const projectId = ((await projectRes.json()) as { id: string }).id

  const cardRes = await request.post(`/api/projects/${projectId}/cards`, {
    headers: auth,
    data: { title: 'Focusable', description: '', step: 'backlog', priority: 0 },
  })
  expect(cardRes.ok(), `seed card failed: ${await cardRes.text()}`).toBeTruthy()

  await loadAt(page, token, `/projects/${projectId}`)
  const card = page.locator('.kanban-card', { hasText: 'Focusable' })
  await expect(card).toBeVisible({ timeout: 10_000 })

  const trigger = card.locator('.kanban-card-menu-btn')
  await trigger.click()
  await page.locator('[data-testid="card-menu-wont-do"]').click()

  const dialogSel = '[data-testid="card-wont-do-confirm"]'
  const dialog = page.locator(dialogSel)
  await expect(dialog).toBeVisible()

  // Dialog semantics, with a name that actually resolves to the title.
  await expect(dialog).toHaveAttribute('role', 'alertdialog')
  await expect(dialog).toHaveAttribute('aria-modal', 'true')
  const labelId = await dialog.getAttribute('aria-labelledby')
  expect(labelId, 'aria-labelledby set').toBeTruthy()
  // React's useId ids contain guillemets, so match by attribute, not `#id`.
  await expect(page.locator(`[id="${labelId}"]`)).toHaveText(/Won't Do\?$/)

  // The destructive dialog opens on the safe action, inside the panel —
  // not on <body>, which is where the menu item's unmount used to leave it.
  expect(await focusInside(page, dialogSel)).toBe(true)
  expect(await activeTestId(page)).toBe('confirm-dialog-cancel')

  // Forward: Cancel → Confirm → wraps back to Cancel, never leaving.
  expect(await tabCycle(page, dialogSel, 3)).toEqual([
    'confirm-dialog-confirm',
    'confirm-dialog-cancel',
    'confirm-dialog-confirm',
  ])

  // Backward from the first element wraps to the last.
  await page.locator('[data-testid="confirm-dialog-cancel"]').focus()
  await page.keyboard.press('Shift+Tab')
  expect(await focusInside(page, dialogSel)).toBe(true)
  expect(await activeTestId(page)).toBe('confirm-dialog-confirm')

  // Escape closes and hands focus back to the 3-dot button that started it.
  await page.keyboard.press('Escape')
  await expect(dialog).toHaveCount(0)
  expect(await page.evaluate(() => document.activeElement?.className ?? '')).toContain(
    'kanban-card-menu-btn',
  )
})

test('a Modal is a labelled dialog that keeps and restores focus', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  await createFolder(request, auth, 'modalfocus')

  await loadAt(page, token, '/projects')

  const trigger = page.getByRole('button', { name: '+ New project' })
  await expect(trigger).toBeVisible({ timeout: 10_000 })
  await trigger.click()

  const dialogSel = '.modal-backdrop .modal'
  const dialog = page.locator(dialogSel)
  await expect(dialog).toBeVisible()

  await expect(dialog).toHaveAttribute('aria-modal', 'true')
  // Named from the heading the modal already rendered — no call-site change.
  const labelId = await dialog.getAttribute('aria-labelledby')
  expect(labelId, 'aria-labelledby set from the modal heading').toBeTruthy()
  await expect(page.locator(`[id="${labelId}"]`)).toHaveText('New Project')

  // Focus moved into the panel on open and stays there across a long walk.
  expect(await focusInside(page, dialogSel)).toBe(true)
  await tabCycle(page, dialogSel, 12)

  // Escape closes it and focus returns to the button that opened it.
  await page.keyboard.press('Escape')
  await expect(dialog).toHaveCount(0)
  await expect(trigger).toBeFocused()
})
