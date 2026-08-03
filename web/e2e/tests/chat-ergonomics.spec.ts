import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Chat ergonomics batch:
 *
 *  - Message bubbles carry a hover/focus-revealed Copy button that copies
 *    the raw markdown source (tool output already had one via ClampedPre).
 *  - Dragging files over the window shows a drop-to-attach overlay on the
 *    composer; dropping uploads through the same pipeline as paste/picker.
 *  - Global shortcuts: `?` opens the cheat sheet, `n` opens New Session
 *    (both suppressed while typing), Ctrl/Cmd+K jumps to the sessions
 *    list and focuses its search box, Ctrl/Cmd+1..9 activates the Nth tab.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
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
  name: string,
) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-ergo-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-ergo-${name}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name, folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  return session.id
}

async function injectEvent(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  sessionId: string,
  kind: string,
  data: Record<string, unknown>,
) {
  const res = await request.post(`/api/sessions/${sessionId}/events`, {
    headers: authHeader,
    data: { kind, data },
  })
  expect(res.ok(), `inject ${kind} failed: ${await res.text()}`).toBeTruthy()
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

/** Replace navigator.clipboard so the test can read what got copied
 *  without clipboard permissions. */
async function stubClipboard(page: Page) {
  await page.addInitScript(() => {
    ;(window as unknown as { __copiedText: string | null }).__copiedText = null
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: {
        writeText: (t: string) => {
          ;(window as unknown as { __copiedText: string | null }).__copiedText = t
          return Promise.resolve()
        },
      },
    })
  })
}

test('message bubbles expose a Copy button on hover that copies the markdown source', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader, 'bubble copy')
  await injectEvent(request, authHeader, sessionId, 'user', { text: 'copy me user' })
  await injectEvent(request, authHeader, sessionId, 'agent-text', {
    text: '**bold** assistant reply',
  })

  await stubClipboard(page)
  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.getByText('bold', { exact: true })).toBeVisible({ timeout: 15_000 })

  // Hidden at rest, revealed by hovering the bubble.
  const assistantBtn = page.locator('.chat-bubble-assistant .chat-bubble-copy').first()
  await expect(assistantBtn).toHaveCSS('opacity', '0')
  await page.locator('.chat-bubble-assistant').first().hover()
  await expect(assistantBtn).toHaveCSS('opacity', '1')

  // Copies the raw markdown, not the rendered text, and confirms inline.
  await assistantBtn.click()
  await expect(assistantBtn).toHaveText('Copied')
  expect(
    await page.evaluate(() => (window as unknown as { __copiedText: string | null }).__copiedText),
  ).toBe('**bold** assistant reply')

  // User bubbles have the same affordance.
  const userBtn = page.locator('.chat-bubble-user .chat-bubble-copy').first()
  await page.locator('.chat-bubble-user').first().hover()
  await userBtn.click()
  expect(
    await page.evaluate(() => (window as unknown as { __copiedText: string | null }).__copiedText),
  ).toBe('copy me user')
})

test('dragging files over the chat shows the drop overlay and dropping attaches them', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader, 'drag drop')

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.locator('.input-textarea')).toBeVisible({ timeout: 15_000 })

  // Simulated OS file drag: dragenter raises the overlay…
  await page.evaluate(() => {
    const dt = new DataTransfer()
    dt.items.add(new File(['hello from a drop'], 'dropped.txt', { type: 'text/plain' }))
    document.dispatchEvent(new DragEvent('dragenter', { bubbles: true, dataTransfer: dt }))
  })
  await expect(page.getByTestId('composer-drop-overlay')).toBeVisible()

  // …dragging back out clears it…
  await page.evaluate(() => {
    const dt = new DataTransfer()
    dt.items.add(new File(['hello from a drop'], 'dropped.txt', { type: 'text/plain' }))
    document.dispatchEvent(new DragEvent('dragleave', { bubbles: true, dataTransfer: dt }))
  })
  await expect(page.getByTestId('composer-drop-overlay')).not.toBeVisible()

  // …and dropping uploads through the normal attachment pipeline.
  await page.evaluate(() => {
    const dt = new DataTransfer()
    dt.items.add(new File(['hello from a drop'], 'dropped.txt', { type: 'text/plain' }))
    document.dispatchEvent(new DragEvent('dragenter', { bubbles: true, dataTransfer: dt }))
    document.dispatchEvent(new DragEvent('drop', { bubbles: true, dataTransfer: dt }))
  })
  await expect(page.getByTestId('composer-drop-overlay')).not.toBeVisible()
  await expect(page.locator('.attachment-chip-name', { hasText: 'dropped.txt' })).toBeVisible({
    timeout: 10_000,
  })
})

test('global shortcuts: ?, n, Ctrl+K, g-then-digit, and the typing guard', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader, 'shortcut target')
  await injectEvent(request, authHeader, sessionId, 'agent-text', { text: 'shortcut chat body' })

  // Opening the session stores its tab, which Ctrl+1 later re-activates.
  await loadAppAt(page, token, `/sessions/${sessionId}`)
  await expect(page.getByText('shortcut chat body')).toBeVisible({ timeout: 15_000 })

  // `?` opens the cheat sheet; Escape closes it.
  await page.keyboard.press('?')
  const sheet = page.getByTestId('shortcuts-modal')
  await expect(sheet).toBeVisible()
  await expect(sheet).toContainText('New session')
  await page.keyboard.press('Escape')
  await expect(sheet).not.toBeVisible()

  // `n` opens New Session; while it is up, `?` must NOT stack the sheet.
  await page.keyboard.press('n')
  await expect(page.locator('#new-session-name')).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(page.locator('#new-session-name')).not.toBeVisible()

  // Typing `n` in the composer is just typing.
  const composer = page.locator('.input-textarea')
  await composer.click()
  await composer.press('n')
  await expect(page.locator('#new-session-name')).not.toBeVisible()
  await expect(composer).toHaveValue('n')

  // Ctrl+K jumps to the sessions list and focuses the search box.
  await page.keyboard.press('Control+k')
  const search = page.getByTestId('session-filter')
  await expect(search).toBeVisible()
  await expect(search).toBeFocused()

  // `g` then `1` activates the first open tab — back to the session's chat.
  // The sequence must be typed with focus OUT of the search input, since the
  // shortcut (like `n` / `?`) deliberately ignores keys aimed at a text field.
  // Do NOT "simplify" this back to Ctrl/Cmd+1: browsers reserve those for
  // their own tab strip and never deliver the keydown to the page, so the
  // shortcut would only ever work under Playwright's synthetic events.
  await page.getByTestId('session-filter').blur()
  await page.keyboard.press('g')
  await page.keyboard.press('1')
  await expect(page.getByText('shortcut chat body')).toBeVisible({ timeout: 10_000 })
  expect(page.url()).toContain(`/sessions/${sessionId}`)
})
