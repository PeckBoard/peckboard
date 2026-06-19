import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Project-page modals (New Card, question dialog, etc.) must render
 * through a portal at <body>, NOT inside the kanban board's scroll
 * container — otherwise the modal scrolls horizontally with the board
 * and the backdrop only covers the visible slice.
 *
 * They must also scroll the page (backdrop) when content overflows,
 * not scroll the modal panel itself — clamping the modal made tall
 * forms feel cramped and hid the form actions behind a nested scroll.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

async function makeProject(
  request: APIRequestContext,
  auth: Record<string, string>,
  suffix: string,
): Promise<{ id: string }> {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-modal-${suffix}-`))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-modal-${suffix}-${Date.now()}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: {
      name: `modal portal ${suffix}`,
      folder_id: folder.id,
      // No spawning — keeps the board quiet so we can interact with the
      // modal without racing worker UI updates.
      worker_count: 0,
      workflow: 'task',
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  return (await projectRes.json()) as { id: string }
}

test('New Card modal is rendered via a portal, outside the board scroll container', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)
  const project = await makeProject(request, auth, 'portal')

  await loadAt(page, token, `/projects/${project.id}`)

  // Open the New Card form.
  await page.locator('.kanban-board-scroll').waitFor({ state: 'visible', timeout: 10_000 })
  await page.getByRole('button', { name: /Add Card/i }).click()

  const backdrop = page.locator('.modal-backdrop')
  await expect(backdrop).toBeVisible({ timeout: 5_000 })

  // The backdrop must be a direct child of <body> — that is, NOT
  // nested inside the kanban board's horizontal scroller. If a future
  // refactor drops the portal, the backdrop ends up inside
  // `.kanban-board-scroll` and this assertion catches it.
  const insideBoard = await backdrop.evaluate((el) => !!el.closest('.kanban-board-scroll'))
  expect(insideBoard, 'modal must not be a descendant of .kanban-board-scroll').toBe(false)

  // Belt-and-braces: backdrop should be a direct child of body.
  const parentIsBody = await backdrop.evaluate((el) => el.parentElement === document.body)
  expect(parentIsBody, 'modal backdrop should be portaled into <body>').toBe(true)
})

test('Long modal scrolls the backdrop (page), not the modal panel itself', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, auth } = await authenticate(request)
  const project = await makeProject(request, auth, 'scroll')

  // Tight viewport so the New Card form (title + description + priority
  // + workflow + model + effort + blocked + actions) is taller than the
  // visible region. The form ships with a reasonable amount of content
  // even on a fresh project.
  await page.setViewportSize({ width: 480, height: 360 })

  await loadAt(page, token, `/projects/${project.id}`)
  await page.locator('.kanban-board-scroll').waitFor({ state: 'visible', timeout: 10_000 })
  await page.getByRole('button', { name: /Add Card/i }).click()

  const backdrop = page.locator('.modal-backdrop')
  await expect(backdrop).toBeVisible({ timeout: 5_000 })
  const panel = backdrop.locator('.modal').first()
  await expect(panel).toBeVisible()

  const { backdropScrollable, panelScrollable } = await page.evaluate(() => {
    const bd = document.querySelector('.modal-backdrop') as HTMLElement | null
    const md = document.querySelector('.modal-backdrop .modal') as HTMLElement | null
    if (!bd || !md) return { backdropScrollable: false, panelScrollable: false }
    // A pixel of slop guards against subpixel rounding.
    return {
      backdropScrollable: bd.scrollHeight - bd.clientHeight > 1,
      panelScrollable: md.scrollHeight - md.clientHeight > 1,
    }
  })

  // The page must be the scroll container, not the panel — this is the
  // behavior the user asked for.
  expect(backdropScrollable, 'backdrop should scroll when modal overflows').toBe(true)
  expect(panelScrollable, 'modal panel itself should not scroll').toBe(false)

  // And the panel must NOT clamp its height — auto margins handle
  // centering; the panel's height is the natural content height.
  const panelOverflowY = await panel.evaluate((el) => getComputedStyle(el).overflowY)
  expect(['visible', 'clip']).toContain(panelOverflowY)
})
