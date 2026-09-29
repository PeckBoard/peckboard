import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * The chat's pending-question UI is a centered modal, not a card in the
 * feed:
 *
 *  - an `ask_user` question opens an `alertdialog` centered in the
 *    viewport; answering it posts `question-resolved` and closes it, and
 *    the answered question stays in the feed as history.
 *  - Escape / backdrop only HIDE the modal (nothing is posted): the feed
 *    shows a compact "Input needed" row whose "Answer" button reopens it.
 *  - a question that arrives while the modal is hidden opens it again;
 *    "Dismiss" still rejects (`rejected: true`); the older, still-open
 *    question the user put away stays put away.
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

async function seedSession(request: APIRequestContext, authHeader: Record<string, string>) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-qmodal-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-qmodal', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'question modal', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  return session.id
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
  await expect(page.locator('.chat-empty').or(page.locator('.chat-vrow').first())).toBeVisible({
    timeout: 10_000,
  })
}

/** Plant a `question` event the way the ask_user MCP tool would. */
async function plantQuestion(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  sessionId: string,
  question: string,
): Promise<string> {
  const res = await request.post(`/api/sessions/${sessionId}/events`, {
    headers: authHeader,
    data: { kind: 'question', data: { questions: [{ question, header: 'Setup' }] } },
  })
  expect(res.ok(), `seed question failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { id: string }).id
}

type LoggedEvent = { id: string; kind: string; data: Record<string, unknown> }

async function sessionEvents(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  sessionId: string,
): Promise<LoggedEvent[]> {
  const res = await request.get(`/api/sessions/${sessionId}/events?after_seq=0`, {
    headers: authHeader,
  })
  expect(res.ok()).toBeTruthy()
  return (await res.json()) as LoggedEvent[]
}

test('the question opens as a centered alertdialog; answering closes it', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)
  await loadAppAt(page, token, `/sessions/${sessionId}`)

  const send = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: authHeader,
    data: { text: 'ask me', model: 'mock:ask' },
  })
  expect(send.ok()).toBeTruthy()

  const modal = page.getByTestId('question-modal')
  await expect(modal).toBeVisible({ timeout: 10_000 })
  await expect(modal).toHaveAttribute('role', 'alertdialog')
  await expect(modal).toContainText('Input needed')
  await expect(modal).toContainText('Continue?')
  // The pending card no longer lives in the feed.
  await expect(page.locator('.chat-messages .question-card.question-active')).toHaveCount(0)

  // Centered in the viewport (the backdrop centers both axes). Polled so
  // the entry animation has settled.
  const viewport = page.viewportSize()
  expect(viewport, 'viewport known').toBeTruthy()
  await expect
    .poll(async () => {
      const box = await modal.boundingBox()
      if (!box) return Number.POSITIVE_INFINITY
      return Math.max(
        Math.abs(box.x + box.width / 2 - viewport!.width / 2),
        Math.abs(box.y + box.height / 2 - viewport!.height / 2),
      )
    })
    .toBeLessThan(4)

  await modal.getByPlaceholder('Type your answer...').fill('yes go ahead')
  await modal.getByRole('button', { name: 'Submit' }).click()

  await expect(modal).toHaveCount(0, { timeout: 10_000 })
  // History stays inline: the answered question is a resolved card.
  const resolved = page.locator('.question-card.question-resolved')
  await expect(resolved).toContainText('Question answered', { timeout: 10_000 })
  await expect(resolved).toContainText('yes go ahead')
  await expect(page.getByTestId('question-pending-row')).toHaveCount(0)

  const all = await sessionEvents(request, authHeader, sessionId)
  const q = all.find((e) => e.kind === 'question')
  const res = all.find((e) => e.kind === 'question-resolved')
  expect(q, 'question logged').toBeTruthy()
  expect(res?.data.question_id).toBe(q!.id)
  expect(res?.data.answers).toEqual({ '0': 'yes go ahead' })
})

test('Escape hides the modal without rejecting; the inline row reopens it', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)
  const sessionId = await seedSession(request, authHeader)
  const first = await plantQuestion(request, authHeader, sessionId, 'Pick a colour?')
  await loadAppAt(page, token, `/sessions/${sessionId}`)

  const modal = page.getByTestId('question-modal')
  await expect(modal).toBeVisible({ timeout: 10_000 })
  await expect(modal).toContainText('Pick a colour?')

  await page.keyboard.press('Escape')
  await expect(modal).toHaveCount(0)

  // Hidden, not rejected: the question is still open and the feed says so.
  const row = page.getByTestId('question-pending-row')
  await expect(row).toBeVisible()
  await expect(row).toContainText('Input needed')
  expect(
    (await sessionEvents(request, authHeader, sessionId)).some(
      (e) => e.kind === 'question-resolved',
    ),
    'no question-resolved after Escape',
  ).toBe(false)

  // The row's "Answer" brings the modal back, for the same question.
  await row.getByRole('button', { name: 'Answer' }).click()
  await expect(modal).toBeVisible()
  await expect(modal).toContainText('Pick a colour?')

  // Backdrop click hides it too.
  await page.locator('.modal-backdrop').click({ position: { x: 5, y: 5 } })
  await expect(modal).toHaveCount(0)
  await expect(row).toBeVisible()

  // A NEW question arriving while hidden opens the modal again, showing
  // the new one with a fresh (empty) answer.
  const second = await plantQuestion(request, authHeader, sessionId, 'Which font?')
  await expect(modal).toBeVisible({ timeout: 10_000 })
  await expect(modal).toContainText('Which font?')
  await expect(modal.getByPlaceholder('Type your answer...')).toHaveValue('')

  // "Dismiss" is the real rejection.
  await modal.getByRole('button', { name: 'Dismiss' }).click()
  await expect(modal).toHaveCount(0, { timeout: 10_000 })
  await expect(async () => {
    const all = await sessionEvents(request, authHeader, sessionId)
    const rejected = all.find(
      (e) => e.kind === 'question-resolved' && e.data.question_id === second,
    )
    expect(rejected, 'question-resolved for the second question').toBeTruthy()
    expect(rejected?.data.rejected).toBe(true)
  }).toPass({ timeout: 10_000 })

  // The first question is still open and still hidden by the earlier
  // Escape, so the feed offers its row rather than popping the modal
  // back up uninvited.
  await expect(row).toBeVisible()
  await expect(modal).toHaveCount(0)
  await row.getByRole('button', { name: 'Answer' }).click()
  await expect(modal).toContainText('Pick a colour?')
  await modal.getByPlaceholder('Type your answer...').fill('blue')
  await modal.getByRole('button', { name: 'Submit' }).click()
  await expect(modal).toHaveCount(0, { timeout: 10_000 })
  await expect(
    page.locator('.question-card.question-resolved').filter({ hasText: 'blue' }),
  ).toBeVisible({ timeout: 10_000 })
  await expect(row).toHaveCount(0)
  const answered = (await sessionEvents(request, authHeader, sessionId)).find(
    (e) => e.kind === 'question-resolved' && e.data.question_id === first,
  )
  expect(answered?.data.answers).toEqual({ '0': 'blue' })
})
