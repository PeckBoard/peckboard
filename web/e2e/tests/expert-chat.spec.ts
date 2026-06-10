import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e: an expert session can be VIEWED and ASKED from the Experts view.
 *
 * Clicking an expert row opens that expert's transcript (the same ChatView
 * the chat sessions use) at `/experts/:id`, where the user can read the
 * conversation and type a question to the expert. We drive the PROJECT's
 * question-expert, which inherits the project's `mock:echo` model, so the
 * round-trip is deterministic (no real `claude` CLI): the mock echoes the
 * question straight back as an assistant bubble.
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

type Expert = { id: string; name: string; expert_kind: string | null; project_id: string | null }

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t as string)
  }, token)
  await page.goto(route)
}

test('an expert can be opened from the Experts view, viewed, and asked a question', async ({
  request,
  page,
  baseURL,
}) => {
  expect(baseURL, 'baseURL configured').toBeTruthy()
  const { token, authHeader } = await authenticate(request)

  // A folder + a mock:echo project. Creating the project provisions it a
  // per-project question-expert that inherits the mock:echo model.
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-expert-chat', path: mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-ec-')) },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: authHeader,
    data: { name: 'Expert Chat', folder_id: folder.id, model: 'mock:echo' },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string; name: string }

  // The per-project question-expert is created in the background — poll for it.
  let questionExpert: Expert | undefined
  await expect
    .poll(
      async () => {
        const res = await request.get('/api/experts', { headers: authHeader })
        if (!res.ok()) return false
        const experts = (await res.json()) as Expert[]
        questionExpert = experts.find(
          (e) => e.expert_kind === 'question' && e.project_id === project.id,
        )
        return Boolean(questionExpert)
      },
      { message: 'project question-expert created', timeout: 10_000 },
    )
    .toBeTruthy()

  // ── Open the expert from the Experts view ──
  await loadAt(page, token, '/experts')

  const row = page.getByTestId('expert-row').filter({ hasText: questionExpert!.name })
  await expect(row).toBeVisible()
  await row.click()

  // The URL reflects the opened expert and its transcript (ChatView) renders.
  await expect(page).toHaveURL(new RegExp(`/experts/${questionExpert!.id}$`))
  await expect(page.locator('.chat-empty').or(page.locator('.chat-bubble').first())).toBeVisible({
    timeout: 10_000,
  })

  // ── Ask the expert a question ──
  const question = 'What did the user decide about commit style?'
  await page.locator('.input-textarea').fill(question)
  await page.locator('.send-btn').click()

  // The user's question shows immediately…
  await expect(page.locator('.chat-bubble-user', { hasText: question })).toBeVisible({
    timeout: 5_000,
  })
  // …and the mock:echo expert answers (echoes the question back).
  await expect(page.locator('.chat-bubble-assistant', { hasText: question })).toBeVisible({
    timeout: 10_000,
  })
})
