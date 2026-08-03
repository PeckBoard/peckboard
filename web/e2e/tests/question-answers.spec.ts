import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import {
  type AnswerValue,
  answerParts,
  answerText,
  selectedOptions,
  toggleOption,
} from '../../src/lib/questionAnswers'

/**
 * Pure-logic tests for multi-select answer state. There is no vitest in
 * `web/`, so these run in the Playwright suite — they touch no page and no
 * server, they just import the module under test.
 *
 * The regression: selections were kept as one comma-joined string and read
 * back with `split(',')`, so an agent-supplied option label containing a
 * comma ("Yes, restart it") never read back as selected — a second tap
 * appended a duplicate instead of toggling off, and the submitted answer
 * carried the garbled value.
 */

const COMMA_OPT = 'Yes, restart it'

test('a comma-bearing label toggles on and reads back as selected', () => {
  const on = toggleOption(undefined, COMMA_OPT)
  expect(on).toEqual([COMMA_OPT])
  expect(selectedOptions(on).includes(COMMA_OPT)).toBe(true)
})

test('a second toggle removes the comma-bearing label instead of duplicating it', () => {
  const on = toggleOption([], COMMA_OPT)
  const off = toggleOption(on, COMMA_OPT)
  expect(off).toEqual([])
})

test('mixed selections keep every label intact through toggle and submit', () => {
  let sel: AnswerValue = toggleOption(undefined, 'No')
  sel = toggleOption(sel, COMMA_OPT)
  expect(selectedOptions(sel)).toEqual(['No', COMMA_OPT])
  expect(answerText(sel)).toBe(`No, ${COMMA_OPT}`)

  sel = toggleOption(sel, 'No')
  expect(selectedOptions(sel)).toEqual([COMMA_OPT])
  expect(answerText(sel)).toBe(COMMA_OPT)
})

test('string answers (text input / radio) pass through untouched', () => {
  expect(answerText('  typed answer ')).toBe('typed answer')
  expect(answerText(undefined)).toBe('')
  expect(selectedOptions('Other, with comma')).toEqual([])
  expect(answerParts('Other, with comma')).toEqual(['Other, with comma'])
  expect(answerParts([])).toEqual([])
})

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function login(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  return ((await res.json()) as { token: string }).token
}

async function loadAs(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
  await expect(page.locator('.rail-avatar')).toBeVisible({ timeout: 10_000 })
}

test('chat multi-select with a comma-bearing option toggles and submits the exact label', async ({
  request,
  page,
}) => {
  const token = await login(request)
  const auth = { Authorization: `Bearer ${token}` }

  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: 'qa-comma', path: mkdtempSync(path.join(tmpdir(), 'pb-qa-comma-')) },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'qa-comma', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  // Plant the question the way the ask_user MCP tool would.
  const qRes = await request.post(`/api/sessions/${session.id}/events`, {
    headers: auth,
    data: {
      kind: 'question',
      data: {
        questions: [
          {
            question: 'The server crashed. What should I do?',
            header: 'Confirm',
            multiSelect: true,
            options: ['Leave it down', COMMA_OPT, 'Collect logs first'],
          },
        ],
      },
    },
  })
  expect(qRes.ok(), `seed question failed: ${await qRes.text()}`).toBeTruthy()
  const question = (await qRes.json()) as { id: string }

  await loadAs(page, token, `/sessions/${session.id}`)

  const card = page.locator('.question-card.question-active')
  await expect(card).toBeVisible({ timeout: 10_000 })

  const commaBox = card
    .locator('.question-option-label', { hasText: COMMA_OPT })
    .locator('input[type=checkbox]')
  const logsBox = card
    .locator('.question-option-label', { hasText: 'Collect logs first' })
    .locator('input[type=checkbox]')

  // The regression: this check never stuck, and a second click duplicated
  // the label instead of unchecking.
  await commaBox.check()
  await expect(commaBox).toBeChecked()
  await commaBox.uncheck()
  await expect(commaBox).not.toBeChecked()

  await commaBox.check()
  await logsBox.check()
  await expect(commaBox).toBeChecked()
  await expect(logsBox).toBeChecked()

  await card.getByRole('button', { name: 'Submit' }).click()

  await expect(async () => {
    const eventsRes = await request.get(`/api/sessions/${session.id}/events`, { headers: auth })
    expect(eventsRes.ok()).toBeTruthy()
    const events = (await eventsRes.json()) as { kind: string; data: Record<string, unknown> }[]
    const resolved = events.find(
      (e) => e.kind === 'question-resolved' && e.data.question_id === question.id,
    )
    expect(resolved, 'question-resolved emitted').toBeTruthy()
    const answers = resolved?.data.answers as Record<string, string>
    // Exact labels survive, in pick order, joined for transport.
    expect(answers['0']).toBe(`${COMMA_OPT}, Collect logs first`)
  }).toPass({ timeout: 10_000 })
})
