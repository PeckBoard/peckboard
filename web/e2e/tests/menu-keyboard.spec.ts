import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Menus, popovers and @-mentions must be operable with no pointer at all.
 *
 * Every popup in the app claims menu/listbox ARIA semantics; this spec is
 * the acceptance test for the behaviour those roles promise (see
 * `web/src/hooks/useMenuKeyboard.ts`):
 *  - a card's priority popover opens, roves and commits from the keyboard,
 *  - the @-mention list takes Arrow/Enter/Escape BEFORE the send branch, so
 *    Enter completes the mention instead of firing off "@foo",
 *  - Shift+F10 reaches the right-click menu, and Escape returns focus.
 *
 * The 3-dot menu's own keyboard path is covered by
 * `kanban-move-without-drag.spec.ts` ("keyboard only").
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function makeFolder(
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

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
    localStorage.setItem('peckboard_skip_backlog_confirm', '1')
  }, token)
  await page.goto(route)
}

test('a card priority changes from the keyboard alone', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const folderId = await makeFolder(request, auth, 'kbprio')

  // worker_count: 0 so the orchestrator can't pick the card up mid-assertion.
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: `keyboard priority ${Date.now()}`,
      folder_id: folderId,
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const projectId = ((await projectRes.json()) as { id: string }).id

  const cardRes = await request.post(`/api/projects/${projectId}/cards`, {
    headers: auth,
    data: { title: 'Repriorise Me', description: '', step: 'backlog', priority: 2 },
  })
  expect(cardRes.ok(), `seed card failed: ${await cardRes.text()}`).toBeTruthy()

  await loadAt(page, token, `/projects/${projectId}`)
  const card = page.locator('.kanban-card', { hasText: 'Repriorise Me' })
  await expect(card).toBeVisible({ timeout: 10_000 })

  // Native <button>, so it is tab-reachable; focus() only skips the walk.
  const trigger = card.locator('.priority-chevron')
  await trigger.focus()
  await expect(trigger).toHaveAttribute('aria-expanded', 'false')

  // ArrowDown opens the popover and lands on the CURRENT priority (Medium).
  await page.keyboard.press('ArrowDown')
  await expect(trigger).toHaveAttribute('aria-expanded', 'true')
  const menu = page.locator('.priority-chevron-menu')
  await expect(menu).toBeVisible()
  const medium = menu.locator('[role="menuitemradio"]', { hasText: 'Medium' })
  await expect(medium).toBeFocused()
  await expect(medium).toHaveAttribute('aria-checked', 'true')

  // Roving focus, then commit.
  await page.keyboard.press('ArrowDown')
  const low = menu.locator('[role="menuitemradio"]', { hasText: 'Low' })
  await expect(low).toBeFocused()
  await page.keyboard.press('Enter')

  await expect(menu).toHaveCount(0)
  // Closing hands focus back to the trigger, not to <body>.
  await expect(trigger).toBeFocused()

  await expect
    .poll(
      async () => {
        const res = await request.get(`/api/projects/${projectId}/cards`, { headers: auth })
        const cards = (await res.json()) as { title: string; priority: number }[]
        return cards.find((c) => c.title === 'Repriorise Me')?.priority
      },
      { timeout: 10_000 },
    )
    .toBe(3)

  // Escape closes without committing and returns focus.
  await page.keyboard.press('ArrowDown')
  await expect(page.locator('.priority-chevron-menu')).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(page.locator('.priority-chevron-menu')).toHaveCount(0)
  await expect(trigger).toBeFocused()
})

test('an @-mention completes from the keyboard instead of sending', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const folderId = await makeFolder(request, auth, 'kbmention')

  const stamp = Date.now()
  const mk = async (name: string) => {
    const res = await request.post('/api/sessions', {
      headers: auth,
      data: { name, folder_id: folderId, model: 'mock:happy-path' },
    })
    expect(res.ok(), `create session failed: ${await res.text()}`).toBeTruthy()
    return ((await res.json()) as { id: string }).id
  }
  // Two mentionable targets sharing a run-unique prefix, so the filtered list
  // is exactly these two — including across a retry of this spec.
  const prefix = `kb${stamp}`
  const targetA = await mk(`${prefix}-alpha`)
  const targetB = await mk(`${prefix}-beta`)
  const idByLabel: Record<string, string> = {
    [`${prefix}-alpha`]: targetA,
    [`${prefix}-beta`]: targetB,
  }
  // A session is excluded from its own mention list, so the host adds no row.
  const hostId = await mk(`${prefix}-host`)

  await loadAt(page, token, `/sessions/${hostId}`)
  await expect(page.locator('.chat-empty')).toBeVisible({ timeout: 10_000 })

  const composer = page.locator('.input-textarea')
  await composer.focus()
  await composer.pressSequentially(`hi @${prefix}-`)

  const list = page.locator('[role="listbox"][aria-label="Mention suggestions"]')
  await expect(list).toBeVisible({ timeout: 10_000 })
  const options = list.locator('[role="option"]')
  await expect(options).toHaveCount(2)

  // Focus never leaves the composer — the cursor moves via aria-activedescendant.
  await expect(composer).toBeFocused()
  const firstId = await options.nth(0).getAttribute('id')
  await expect(composer).toHaveAttribute('aria-activedescendant', String(firstId))
  await expect(options.nth(0)).toHaveAttribute('aria-selected', 'true')

  await page.keyboard.press('ArrowDown')
  const secondId = await options.nth(1).getAttribute('id')
  await expect(composer).toHaveAttribute('aria-activedescendant', String(secondId))
  await expect(options.nth(1)).toHaveAttribute('aria-selected', 'true')
  const chosenLabel = (await options.nth(1).locator('.autocomplete-item-title').innerText())
    .replace(/^session\s*/i, '')
    .trim()

  // Enter completes the mention. It must NOT send: the send branch only runs
  // once the list is closed.
  await page.keyboard.press('Enter')
  await expect(list).toHaveCount(0)
  const expectedRef = `[session:${idByLabel[chosenLabel]}]`
  await expect(composer).toHaveValue(`hi ${expectedRef}`)
  await expect(page.locator('.chat-empty')).toBeVisible()

  // Escape dismisses the list and leaves the draft alone.
  await composer.pressSequentially(` @${prefix}-`)
  await expect(list).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(list).toHaveCount(0)
  await expect(composer).toHaveValue(`hi ${expectedRef} @${prefix}-`)
})

test('Shift+F10 opens the right-click menu and Escape gives focus back', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  const folderId = await makeFolder(request, auth, 'kbctx')
  const res = await request.post('/api/sessions', {
    headers: auth,
    data: { name: `kb context ${Date.now()}`, folder_id: folderId, model: 'mock:happy-path' },
  })
  expect(res.ok(), `create session failed: ${await res.text()}`).toBeTruthy()
  const sessionId = ((await res.json()) as { id: string }).id

  await loadAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.chat-empty')).toBeVisible({ timeout: 10_000 })

  const tab = page.locator('.tab-opened').first()
  await expect(tab).toBeVisible({ timeout: 10_000 })
  await tab.focus()
  await page.keyboard.press('Shift+F10')

  const menu = page.locator('.context-menu')
  await expect(menu).toBeVisible()
  const enabled = menu.locator('[role="menuitem"]:not([disabled])')
  await expect(enabled.first()).toBeFocused()

  // Home/End rove regardless of how many rows this tab kind contributes.
  await page.keyboard.press('End')
  await expect(enabled.last()).toBeFocused()
  await page.keyboard.press('Home')
  await expect(enabled.first()).toBeFocused()

  await page.keyboard.press('Escape')
  await expect(menu).toHaveCount(0)
  await expect(tab).toBeFocused()
})
