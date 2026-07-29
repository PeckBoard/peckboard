import { test, expect, type APIRequestContext, type Page } from '@playwright/test'
import { execSync } from 'node:child_process'
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { WebSocketImpl, type WsMessageEvent } from './ws-compat'

/**
 * Document Review, desktop, end to end on the mock provider.
 *
 * The whole AI half of the feature runs on `mock:doc-review` (see
 * `src/provider/mock/mod.rs`), which calls the REAL MCP tools —
 * `get_review_doc`, `submit_review_revision`, `ask_user` — so a pass here
 * exercises the same handlers, status hooks and events a live reviewer
 * drives. The scenario picks its branch from a marker in the turn text:
 * `[mock:ask]` asks a clarifying question, `[mock:chat]` answers without
 * revising, anything else revises the document.
 *
 * A review's session is created lazily by the first pass, so every test
 * that runs a pass calls `armReviewer` first: it creates that session
 * pinned to the mock model (`POST /pass { model }`) and asserts the pin
 * took, because an unpinned review session would resolve to auto → a real
 * Claude model and spawn the CLI mid-test.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

/** Fixture document. Line numbers matter: the specs address blocks by
 *  `data-line-start`, which SafeMarkdown's block-anchor mode sets. */
const DOC = [
  '# Onboarding', // 1
  '', // 2
  'The team ships on Fridays.', // 3
  '', // 4
  '## On-call', // 5
  '', // 6
  'On-call rotates weekly.', // 7
  '',
].join('\n')

const REPORT_FOLDER = '2026-07-28'

type Auth = { token: string; auth: Record<string, string> }

async function authenticate(request: APIRequestContext): Promise<Auth> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

type WsEvent = { kind: string; data: Record<string, unknown>; seq: number }

/**
 * Open a WS connection, authenticate, subscribe, and collect every event
 * for `sessionId` until `untilKind` is observed. Copied from
 * mock-provider.spec.ts — the canonical event-wait for scripted runs.
 */
async function collectEventsUntil(
  baseURL: string,
  token: string,
  sessionId: string,
  untilKind: string,
  timeoutMs: number,
): Promise<WsEvent[]> {
  const wsUrl = baseURL.replace(/^http/, 'ws') + '/ws'
  const ws = new WebSocketImpl(wsUrl)
  const collected: WsEvent[] = []

  try {
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error(`WS handshake timed out after ${timeoutMs}ms`)),
        timeoutMs,
      )
      ws.addEventListener('open', () => {
        clearTimeout(timer)
        resolve()
      })
      ws.addEventListener('error', (err) => {
        clearTimeout(timer)
        reject(new Error(`WS error: ${String(err)}`))
      })
    })

    ws.send(JSON.stringify({ type: 'auth', token }))
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('WS auth_ok not received')), timeoutMs)
      const handler = (msg: WsMessageEvent) => {
        const frame = JSON.parse(String(msg.data))
        if (frame.type === 'auth_ok') {
          clearTimeout(timer)
          ws.removeEventListener('message', handler)
          resolve()
        }
      }
      ws.addEventListener('message', handler)
    })

    ws.send(JSON.stringify({ type: 'subscribe', session_id: sessionId }))

    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(
        () =>
          reject(
            new Error(
              `Did not see '${untilKind}' within ${timeoutMs}ms; got: ${collected
                .map((e) => e.kind)
                .join(', ')}`,
            ),
          ),
        timeoutMs,
      )
      ws.addEventListener('message', (msg) => {
        const frame = JSON.parse(String(msg.data))
        if (frame.type !== 'event' || frame.session_id !== sessionId) return
        const ev = frame.event as WsEvent
        collected.push(ev)
        if (ev.kind === untilKind) {
          clearTimeout(timer)
          resolve()
        }
      })
    })
  } finally {
    ws.close()
  }

  return collected
}

/** A registered workspace folder with `docs/onboarding.md` inside it. */
async function seedFolder(
  request: APIRequestContext,
  auth: Record<string, string>,
  suffix: string,
): Promise<{ id: string; dir: string; name: string }> {
  const dir = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-review-${suffix}-`))
  mkdirSync(path.join(dir, 'docs'), { recursive: true })
  writeFileSync(path.join(dir, 'docs', 'onboarding.md'), DOC)
  const name = `e2e-review-${suffix}-${Date.now()}`
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name, path: dir },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  const folder = (await res.json()) as { id: string }
  return { id: folder.id, dir, name }
}

/** Write a report straight into the server's reports tree — the only HTTP
 *  write endpoint is a PUT that needs the file to exist already. */
function writeReportFile(file: string, title: string, body: string) {
  const dataDir = process.env.PECKBOARD_E2E_DATA_DIR
  if (!dataDir) throw new Error('PECKBOARD_E2E_DATA_DIR must be set (see playwright.config.ts)')
  const dir = path.join(dataDir, 'reports', REPORT_FOLDER)
  mkdirSync(dir, { recursive: true })
  writeFileSync(
    path.join(dir, file),
    `---\ntitle: "${title}"\ndate: "${REPORT_FOLDER}T10:00:00Z"\n---\n\n${body}\n`,
  )
}

async function createReview(
  request: APIRequestContext,
  auth: Record<string, string>,
  data: Record<string, unknown>,
): Promise<string> {
  const res = await request.post('/api/doc-reviews', { headers: auth, data })
  expect(res.ok(), `create review failed: ${await res.text()}`).toBeTruthy()
  const body = (await res.json()) as { review: { id: string } }
  return body.review.id
}

async function getReview(request: APIRequestContext, auth: Record<string, string>, id: string) {
  const res = await request.get(`/api/doc-reviews/${id}?comments=all`, { headers: auth })
  expect(res.ok(), `get review failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as {
    review: { id: string; status: string; current_version: number; session_id: string | null }
    markdown: string
    comments: Array<{ id: string; status: string; kind: string; resolution_note: string | null }>
  }
}

/**
 * Create the review's AI session pinned to the mock reviewer and wait for
 * that first (chat-only) turn to land, so a later `collectEventsUntil`
 * can't catch this turn's `agent-end` by mistake.
 */
async function armReviewer(
  request: APIRequestContext,
  auth: Record<string, string>,
  reviewId: string,
): Promise<string> {
  const res = await request.post(`/api/doc-reviews/${reviewId}/pass`, {
    headers: auth,
    data: {
      message: '[mock:chat] warming up the reviewer',
      include_annotations: false,
      model: 'mock:doc-review',
    },
  })
  expect(res.ok(), `arm pass failed: ${await res.text()}`).toBeTruthy()
  const { session_id: sessionId } = (await res.json()) as { session_id: string }

  const session = await request.get(`/api/sessions/${sessionId}`, { headers: auth })
  expect((await session.json()).model, 'the review session must run on the mock provider').toBe(
    'mock:doc-review',
  )

  await expect
    .poll(
      async () => {
        const events = await request.get(`/api/sessions/${sessionId}/events?limit=1000`, {
          headers: auth,
        })
        if (!events.ok()) return 0
        return ((await events.json()) as WsEvent[]).filter((e) => e.kind === 'agent-end').length
      },
      { timeout: 20_000, message: 'the warm-up turn never finished' },
    )
    .toBeGreaterThan(0)

  return sessionId
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto(route)
}

/** Click the document block that starts on `line` and add an annotation. */
async function annotate(page: Page, line: number, action: string, body: string) {
  await page.locator(`[data-testid="review-block"][data-line-start="${line}"]`).first().click()
  await expect(page.getByTestId('review-popover')).toBeVisible()
  await page.getByTestId(`review-popover-${action}`).click()
  await page.getByTestId('review-annotation-editor').fill(body)
  await page.getByTestId('review-annotation-submit').click()
  await expect(page.getByTestId('review-popover')).toHaveCount(0)
}

/**
 * Leave no reviews behind. Opening a review opens a tab chip, and the
 * tab-strip specs later in the run count the chips in the strip — a review
 * this file forgot to delete fails a spec three files away.
 */
async function cleanupReviews(request: APIRequestContext, auth: Record<string, string>) {
  const res = await request.get('/api/doc-reviews', { headers: auth })
  if (!res.ok()) return
  for (const r of ((await res.json()) as { reviews: Array<{ id: string }> }).reviews) {
    await request.delete(`/api/doc-reviews/${r.id}`, { headers: auth })
  }
}

test.describe('document review — desktop', () => {
  test.afterEach(async ({ request }) => {
    const { auth } = await authenticate(request)
    await cleanupReviews(request, auth)
  })

  test('the wizard creates a review from a file, a report and a plan', async ({
    request,
    page,
    baseURL,
  }) => {
    expect(baseURL, 'baseURL configured').toBeTruthy()
    const { token, auth } = await authenticate(request)
    const folder = await seedFolder(request, auth, 'wizard')
    // A second markdown file so the file combobox has something to filter out.
    writeFileSync(path.join(folder.dir, 'docs', 'runbook.md'), '# Runbook\n\nRestart it.\n')
    writeReportFile('wizard-report.md', 'Wizard Report', '# Wizard Report\n\nFindings go here.')

    // A plan row, persisted by the deterministic plan mock through the same
    // `upsert_plan` path `propose_plan` uses.
    const planSession = await request.post('/api/sessions', {
      headers: auth,
      data: { name: 'plan seed', folder_id: folder.id, model: 'mock:plan-review' },
    })
    expect(planSession.ok(), `create plan session failed: ${await planSession.text()}`).toBeTruthy()
    const planSessionId = ((await planSession.json()) as { id: string }).id
    const sent = await request.post(`/api/sessions/${planSessionId}/message`, {
      headers: auth,
      data: { text: 'plan it' },
    })
    expect(sent.ok(), `plan message failed: ${await sent.text()}`).toBeTruthy()
    await expect
      .poll(
        async () => {
          const res = await request.get(`/api/plans?session_id=${planSessionId}`, { headers: auth })
          if (res.status() === 204 || !res.ok()) return null
          return ((await res.json()) as { plan: { title: string } }).plan.title
        },
        { timeout: 20_000, message: 'the plan mock never persisted a plan' },
      )
      .toBe('Widget plan')

    await loadAt(page, token, '/review')
    await expect(page.getByTestId('review-list')).toBeVisible({ timeout: 15_000 })

    // 1. A file — the combobox filters as you type.
    await page.getByTestId('review-new').click()
    await expect(page.getByTestId('review-wizard')).toBeVisible()
    await expect(page.getByTestId('review-wizard-source-kind')).toBeVisible()
    await page.getByTestId('review-wizard-kind-file').click()
    await page.getByTestId('review-wizard-next').click()
    // Pick THIS test's folder explicitly: the select defaults to whichever
    // folder is first in the workspace, and a full suite run has dozens.
    await page.getByTestId('review-wizard-folder').selectOption(folder.id)
    await page.getByTestId('review-wizard-file').click()
    await expect(page.getByRole('option', { name: /docs\/runbook\.md/ })).toBeVisible()
    await page.getByTestId('review-wizard-file-search').fill('onboarding')
    await expect(page.getByRole('option', { name: /docs\/runbook\.md/ })).toHaveCount(0)
    await page.getByRole('option', { name: /docs\/onboarding\.md/ }).click()
    await expect(page.getByTestId('review-wizard-preview')).toContainText('The team ships on')
    await page.getByTestId('review-wizard-title').fill('File review')
    await page.getByTestId('review-wizard-create').click()

    await expect(page.getByTestId('review-view')).toBeVisible({ timeout: 15_000 })
    await expect(page.getByTestId('review-title')).toHaveText('File review')
    await expect(page.getByTestId('review-doc')).toContainText('On-call rotates weekly.')
    await expect(page.getByTestId('review-version')).toHaveText('v1')

    // 2. A report.
    await page.getByTestId('review-nav').click()
    await expect(page.getByTestId('review-list')).toBeVisible()
    await page.getByTestId('review-new').click()
    await page.getByTestId('review-wizard-kind-report').click()
    await page.getByTestId('review-wizard-next').click()
    await page.getByTestId('review-wizard-file').click()
    await page.getByTestId('review-wizard-file-search').fill('Wizard Report')
    await page.getByRole('option', { name: /Wizard Report/ }).click()
    await page.getByTestId('review-wizard-title').fill('Report review')
    await page.getByTestId('review-wizard-create').click()
    await expect(page.getByTestId('review-view')).toBeVisible({ timeout: 15_000 })
    await expect(page.getByTestId('review-doc')).toContainText('Findings go here.')

    // 3. A plan.
    await page.getByTestId('review-nav').click()
    await page.getByTestId('review-new').click()
    await page.getByTestId('review-wizard-kind-plan').click()
    await page.getByTestId('review-wizard-next').click()
    await page.getByTestId('review-wizard-file').click()
    // Other specs drive the same plan mock, so several rows can carry the
    // mock's fixed title — any of them is the same document.
    await page
      .getByRole('option', { name: /Widget plan/ })
      .first()
      .click()
    await page.getByTestId('review-wizard-title').fill('Plan review')
    await page.getByTestId('review-wizard-create').click()
    await expect(page.getByTestId('review-view')).toBeVisible({ timeout: 15_000 })
    await expect(page.getByTestId('review-doc')).toContainText('Implement the widget')

    const list = await request.get('/api/doc-reviews', { headers: auth })
    const kinds = (
      (await list.json()) as { reviews: Array<{ title: string; source_kind: string }> }
    ).reviews
    expect(kinds.find((r) => r.title === 'File review')?.source_kind).toBe('file')
    expect(kinds.find((r) => r.title === 'Report review')?.source_kind).toBe('report')
    expect(kinds.find((r) => r.title === 'Plan review')?.source_kind).toBe('plan')
  })

  test('a pass revises the document, resolves the annotations, and history diffs, reverts and applies', async ({
    request,
    page,
    baseURL,
  }) => {
    const { token, auth } = await authenticate(request)
    const folder = await seedFolder(request, auth, 'pass')
    const reviewId = await createReview(request, auth, {
      source_kind: 'file',
      source_ref: `${folder.id}:docs/onboarding.md`,
      title: 'Pass review',
    })
    const sessionId = await armReviewer(request, auth, reviewId)

    await loadAt(page, token, `/review/${reviewId}`)
    await expect(page.getByTestId('review-view')).toBeVisible({ timeout: 15_000 })
    await expect(page.getByTestId('review-status')).toHaveAttribute('data-status', 'annotating')

    // Three annotations, three verbs, three passages.
    await annotate(page, 3, 'wrong', 'we ship on Tuesdays')
    await annotate(page, 7, 'comment', 'who covers the weekend?')
    await annotate(page, 1, 'suggest', 'Team onboarding')
    await expect(page.getByTestId('review-annotation-item')).toHaveCount(3)
    await expect(
      page.locator('[data-testid="review-annotation-item"][data-status="pending"]'),
    ).toHaveCount(3)

    // Run the pass and watch the reviewer's real MCP calls come back.
    const events = collectEventsUntil(baseURL!, token, sessionId, 'agent-end', 30_000)
    await page.getByTestId('review-run-pass').click()
    const turn = await events
    const tools = turn
      .filter((e) => e.kind === 'agent-tool-start')
      .map((e) => String(e.data.name ?? ''))
    expect(tools).toContain('mcp__peckboard__get_review_doc')
    expect(tools).toContain('mcp__peckboard__submit_review_revision')
    const revision = turn.find((e) => e.kind === 'doc-review-revision')
    expect(revision, 'the revision event lands on the session log').toBeTruthy()
    expect(revision!.data.version).toBe(2)

    await expect(page.getByTestId('review-version')).toHaveText('v2', { timeout: 15_000 })
    await expect(page.getByTestId('review-status')).toHaveAttribute('data-status', 'annotating')
    await expect(page.getByTestId('review-doc')).toContainText('Mock reviewer pass 2.')

    // Every annotation came back resolved, with the reviewer's note.
    await expect(
      page.locator('[data-testid="review-annotation-item"][data-status="pending"]'),
    ).toHaveCount(0)
    await expect(
      page.locator('[data-testid="review-annotation-item"][data-status="fixed"]'),
    ).toHaveCount(2)
    await expect(
      page.locator('[data-testid="review-annotation-item"][data-status="answered"]'),
    ).toHaveCount(1)
    await expect(page.getByTestId('review-annotation-rail')).toContainText(
      'mock reviewer: wrong fixed in pass 2',
    )

    // History: the default v1 → v2 diff has both sides of the change.
    await page.getByTestId('review-tab-history').click()
    const diff = page.getByTestId('review-diff')
    await expect(diff).toBeVisible({ timeout: 10_000 })
    await expect(
      diff.locator('.diff-line-del', { hasText: 'The team ships on Fridays.' }),
    ).toHaveCount(1)
    await expect(
      diff.locator('.diff-line-add', { hasText: 'Revised: The team ships on Fridays.' }),
    ).toHaveCount(1)
    await expect(diff.locator('.diff-line-add', { hasText: 'Mock reviewer pass 2.' })).toHaveCount(
      1,
    )

    // Revert v1 → a new head version that says so.
    await page
      .locator('[data-testid="review-version-item"][data-version="1"]')
      .getByTestId('review-revert')
      .click()
    await page.getByTestId('confirm-dialog-confirm').click()
    await expect(page.getByTestId('review-version')).toHaveText('v3', { timeout: 15_000 })
    await expect(
      page.locator('[data-testid="review-version-item"][data-version="3"]'),
    ).toContainText('revert to v1')
    await expect(page.getByTestId('review-doc')).not.toContainText('Mock reviewer pass 2.')

    // Apply writes the head version over the source file.
    await page.getByTestId('review-menu').click()
    await page.getByTestId('review-apply').click()
    await page.getByTestId('confirm-dialog-confirm').click()
    await expect
      .poll(
        async () => {
          const res = await request.get(
            `/api/folders/${folder.id}/markdown-file?path=docs/onboarding.md`,
            { headers: auth },
          )
          if (!res.ok()) return ''
          return ((await res.json()) as { markdown: string }).markdown
        },
        { timeout: 15_000, message: 'apply never reached the file' },
      )
      .toContain('The team ships on Fridays.')

    // Apply and finish approves the review.
    await page.getByTestId('review-menu').click()
    await page.getByTestId('review-finish').click()
    await page.getByTestId('confirm-dialog-confirm').click()
    await expect(page.getByTestId('review-status')).toHaveAttribute('data-status', 'approved', {
      timeout: 15_000,
    })
    const finished = await getReview(request, auth, reviewId)
    expect(finished.review.status).toBe('approved')
  })

  test('a clarifying question pins the card over the document and the answer resumes the pass', async ({
    request,
    page,
  }) => {
    const { token, auth } = await authenticate(request)
    const folder = await seedFolder(request, auth, 'ask')
    const reviewId = await createReview(request, auth, {
      source_kind: 'file',
      source_ref: `${folder.id}:docs/onboarding.md`,
      title: 'Question review',
    })
    await armReviewer(request, auth, reviewId)

    await loadAt(page, token, `/review/${reviewId}`)
    await expect(page.getByTestId('review-view')).toBeVisible({ timeout: 15_000 })

    // The marker makes the scripted reviewer ask instead of revise.
    await annotate(page, 3, 'comment', '[mock:ask] which reading did you mean?')
    await page.getByTestId('review-run-pass').click()

    const card = page.getByTestId('review-question-card')
    await expect(card).toBeVisible({ timeout: 20_000 })
    // The whole point of the inline card: the document stays on screen.
    await expect(page.getByTestId('review-doc')).toBeVisible()
    await expect(page.getByTestId('review-status')).toHaveAttribute('data-status', 'needs_input')
    // The composer is parked until the question is answered.
    await page.getByTestId('review-tab-chat').click()
    await expect(page.getByTestId('review-chat-input')).toBeDisabled()
    await page.getByTestId('review-tab-annotations').click()

    await card.getByRole('radio', { name: 'Rewrite it' }).click()
    await page.getByTestId('review-question-submit').click()

    // Answering resumes the pass, which then revises.
    await expect(page.getByTestId('review-version')).toHaveText('v2', { timeout: 20_000 })
    await expect(page.getByTestId('review-question-card')).toHaveCount(0)
    await expect(page.getByTestId('review-status')).toHaveAttribute('data-status', 'annotating')
    const after = await getReview(request, auth, reviewId)
    expect(after.markdown).toContain('Mock reviewer pass 2.')
  })

  test('the chat lane and the clarify action answer without touching the document', async ({
    request,
    page,
  }) => {
    const { token, auth } = await authenticate(request)
    const folder = await seedFolder(request, auth, 'chat')
    const reviewId = await createReview(request, auth, {
      source_kind: 'file',
      source_ref: `${folder.id}:docs/onboarding.md`,
      title: 'Chat review',
    })
    const sessionId = await armReviewer(request, auth, reviewId)

    await loadAt(page, token, `/review/${reviewId}`)
    await expect(page.getByTestId('review-view')).toBeVisible({ timeout: 15_000 })

    await page.getByTestId('review-tab-chat').click()
    await page.getByTestId('review-chat-input').fill('[mock:chat] what does "ship" mean here?')
    await page.getByTestId('review-chat-send').click()
    await expect(page.getByTestId('review-chat-reply').last()).toContainText(
      'Answering in the lane',
      { timeout: 20_000 },
    )
    await expect(page.getByTestId('review-version')).toHaveText('v1')

    // Clarify is the same conversation, started from a passage — and it
    // never consumes the annotation queue.
    await page.getByTestId('review-tab-annotations').click()
    await page.locator('[data-testid="review-block"][data-line-start="7"]').first().click()
    await expect(page.getByTestId('review-popover')).toBeVisible()
    await page.getByTestId('review-popover-clarify').click()
    await page.getByTestId('review-annotation-editor').fill('[mock:chat] who is on call?')
    await page.getByTestId('review-annotation-submit').click()
    await expect(page.getByTestId('review-popover')).toHaveCount(0)
    await expect(page.getByTestId('review-notice')).toContainText('clarification')

    await page.getByTestId('review-tab-chat').click()
    // Three assistant turns in the lane: the warm-up, the chat message and
    // the clarify — none of which touched the document.
    await expect(page.getByTestId('review-chat-reply')).toHaveCount(3, { timeout: 20_000 })
    await expect(page.getByTestId('review-version')).toHaveText('v1')

    const detail = await getReview(request, auth, reviewId)
    expect(detail.review.current_version, 'conversation never bumps the document').toBe(1)
    expect(detail.comments, 'clarify stores no annotation').toHaveLength(0)

    // Finish the review, then keep talking: the conversation survives
    // approval — the SAME session answers — and chatting never un-approves
    // the document.
    const done = await request.post(`/api/doc-reviews/${reviewId}/apply`, {
      headers: auth,
      data: { finish: true },
    })
    expect(done.ok(), `apply+finish failed: ${await done.text()}`).toBeTruthy()
    await expect(page.getByTestId('review-status')).toHaveAttribute('data-status', 'approved', {
      timeout: 10_000,
    })
    await page.getByTestId('review-chat-input').fill('[mock:chat] one last question')
    await page.getByTestId('review-chat-send').click()
    await expect(page.getByTestId('review-chat-reply')).toHaveCount(4, { timeout: 20_000 })
    await expect(page.getByTestId('review-status')).toHaveAttribute('data-status', 'approved')
    const settled = await getReview(request, auth, reviewId)
    expect(settled.review.session_id, 'the same conversation carries on').toBe(sessionId)
  })

  test('the working line narrates the run, Stop kills it, and the chat survives', async ({
    request,
    page,
  }) => {
    const { token, auth } = await authenticate(request)
    const folder = await seedFolder(request, auth, 'stop')
    const reviewId = await createReview(request, auth, {
      source_kind: 'file',
      source_ref: `${folder.id}:docs/onboarding.md`,
      title: 'Stop review',
    })
    const sessionId = await armReviewer(request, auth, reviewId)

    await loadAt(page, token, `/review/${reviewId}`)
    await expect(page.getByTestId('review-view')).toBeVisible({ timeout: 15_000 })

    await page.getByTestId('review-tab-chat').click()
    await page.getByTestId('review-chat-input').fill('[mock:block] read everything first')
    await page.getByTestId('review-chat-send').click()

    // ONE working row narrates the parked tool call in place — the lane
    // never grows a feed of tool rows.
    await expect(page.getByTestId('review-chat-activity')).toContainText('get review doc', {
      timeout: 20_000,
    })
    await expect(page.getByTestId('review-chat-working')).toHaveCount(1)
    await expect(page.getByTestId('review-status')).toHaveAttribute('data-status', 'running')

    // The header's Run pass flips into Stop while the run is live.
    await page.getByTestId('review-stop').click()
    await expect(page.getByTestId('review-status')).toHaveAttribute('data-status', 'annotating', {
      timeout: 15_000,
    })
    await expect(page.getByTestId('review-chat-working')).toHaveCount(0, { timeout: 15_000 })

    // The conversation survived the stop: the next message lands in the
    // same session and the earlier turns are still on screen.
    await page.getByTestId('review-chat-input').fill('[mock:chat] still with me?')
    await page.getByTestId('review-chat-send').click()
    await expect(page.getByTestId('review-chat-reply').last()).toContainText(
      'Answering in the lane',
      { timeout: 20_000 },
    )
    const after = await getReview(request, auth, reviewId)
    expect(after.review.session_id, 'stop never swaps the conversation').toBe(sessionId)
  })

  test('deleting every queued annotation kills the running pass', async ({ request, page }) => {
    const { token, auth } = await authenticate(request)
    const folder = await seedFolder(request, auth, 'kill')
    const reviewId = await createReview(request, auth, {
      source_kind: 'file',
      source_ref: `${folder.id}:docs/onboarding.md`,
      title: 'Kill review',
    })
    await armReviewer(request, auth, reviewId)

    await loadAt(page, token, `/review/${reviewId}`)
    await expect(page.getByTestId('review-view')).toBeVisible({ timeout: 15_000 })

    await annotate(page, 3, 'wrong', '[mock:block] hold this pass open')
    await page.getByTestId('review-run-pass').click()
    await expect(page.getByTestId('review-status')).toHaveAttribute('data-status', 'running', {
      timeout: 15_000,
    })

    // Deleting the only annotation empties the queue out from under the
    // pass — the run is killed instead of revising against nothing.
    await page.getByTestId('review-annotation-item').getByRole('button', { name: 'Delete' }).click()
    await expect(page.getByTestId('review-status')).toHaveAttribute('data-status', 'annotating', {
      timeout: 15_000,
    })
    await expect(page.getByTestId('review-annotation-item')).toHaveCount(0)
  })
  test('a review deep-links, keeps its tab chip, and deleting the last one shows the empty state', async ({
    request,
    page,
  }) => {
    const { token, auth } = await authenticate(request)
    const folder = await seedFolder(request, auth, 'nav')

    // This test owns the list: clear whatever the earlier tests left.
    const existing = await request.get('/api/doc-reviews', { headers: auth })
    for (const r of ((await existing.json()) as { reviews: Array<{ id: string }> }).reviews) {
      const del = await request.delete(`/api/doc-reviews/${r.id}`, { headers: auth })
      expect(del.ok(), `cleanup delete failed: ${await del.text()}`).toBeTruthy()
    }
    const reviewId = await createReview(request, auth, {
      source_kind: 'file',
      source_ref: `${folder.id}:docs/onboarding.md`,
      title: 'Nav review',
    })

    await loadAt(page, token, '/review')
    await expect(page.getByTestId('review-list')).toBeVisible({ timeout: 15_000 })
    await page.locator('.list-view-name', { hasText: 'Nav review' }).click()
    await expect(page).toHaveURL(new RegExp(`/review/${reviewId}$`))
    await expect(page.getByTestId('review-view')).toBeVisible()

    // Opening a review opens its tab, with the review icon.
    const tab = page.locator('.tab-opened', { hasText: 'Nav review' })
    await expect(tab).toBeVisible({ timeout: 10_000 })
    await expect(tab.locator('.tab-icon-doc-review')).toBeVisible()

    // Deep link: the same URL after a reload lands back on the review, and
    // the chip is still there (tabs are server-side, not per-page state).
    await page.reload()
    await expect(page.getByTestId('review-view')).toBeVisible({ timeout: 15_000 })
    await expect(page.getByTestId('review-title')).toHaveText('Nav review')
    await expect(page.locator('.tab-opened', { hasText: 'Nav review' })).toBeVisible()

    // Delete from the list row's menu → the empty state comes back.
    await page.getByTestId('review-nav').click()
    await expect(page.getByTestId('review-list')).toBeVisible()
    const row = page.locator('.list-view-row', { hasText: 'Nav review' })
    await row.locator('.list-view-menu').click()
    await page.getByRole('menuitem', { name: 'Delete' }).click()
    await page.getByTestId('confirm-dialog-confirm').click()
    await expect(page.getByTestId('review-list')).toContainText('No documents under review yet', {
      timeout: 10_000,
    })
    await expect(page.locator('.tab-opened', { hasText: 'Nav review' })).toHaveCount(0)
  })

  test('the header model picker pins the first pass and switches the live session', async ({
    request,
    page,
  }) => {
    const { token, auth } = await authenticate(request)
    const folder = await seedFolder(request, auth, 'model')
    const reviewId = await createReview(request, auth, {
      source_kind: 'file',
      source_ref: `${folder.id}:docs/onboarding.md`,
      title: 'Model review',
    })

    await loadAt(page, token, `/review/${reviewId}`)
    await expect(page.getByTestId('review-view')).toBeVisible({ timeout: 15_000 })

    // No session yet: the picker holds the choice the session will be
    // created on. Deliberately NOT armReviewer — picking `mock:doc-review`
    // here is the UI's own version of that pin.
    await expect(page.getByTestId('review-model')).toContainText('Auto')
    await page.getByTestId('review-model').click()
    await page.locator('.model-picker-search').fill('doc-review')
    await page.getByTestId('review-model-option-mock:doc-review').click()

    await annotate(page, 3, 'wrong', 'we ship on Tuesdays')
    await page.getByTestId('review-run-pass').click()

    // The pass creates the session on the picked model.
    let sessionId: string | null = null
    await expect
      .poll(
        async () => {
          const d = await getReview(request, auth, reviewId)
          sessionId = d.review.session_id
          return sessionId
        },
        { timeout: 20_000, message: 'the pass never created a session' },
      )
      .not.toBeNull()
    const session = await request.get(`/api/sessions/${sessionId!}`, { headers: auth })
    expect(((await session.json()) as { model: string | null }).model).toBe('mock:doc-review')

    // Let the pass finish before switching — a mid-turn PATCH is refused.
    await expect(page.getByTestId('review-version')).toHaveText('v2', { timeout: 30_000 })

    // With a live session the picker PATCHes it — the next pass runs on the
    // new model. Same provider, so no handover: the write is immediate.
    await page.getByTestId('review-model').click()
    await page.locator('.model-picker-search').fill('happy-path')
    await page.getByTestId('review-model-option-mock:happy-path').click()
    await expect
      .poll(
        async () => {
          const res = await request.get(`/api/sessions/${sessionId!}`, { headers: auth })
          return ((await res.json()) as { model: string | null }).model
        },
        { timeout: 10_000, message: 'the model switch never landed on the session' },
      )
      .toBe('mock:happy-path')
    await expect(page.getByTestId('review-model')).not.toContainText('Auto')
  })

  test('the wizard reviews a markdown file from a repo worktree', async ({ request, page }) => {
    const { token, auth } = await authenticate(request)
    const folder = await seedFolder(request, auth, 'worktree')

    // A real git repo in a SUBFOLDER of the workspace, with a real linked
    // worktree under the app's fixed layout — exactly what worker worktree
    // isolation produces. The wizard must find the repo by scanning the
    // folder's subfolders, then offer that repo's worktrees.
    const repoDir = path.join(folder.dir, 'repos', 'app')
    mkdirSync(path.join(repoDir, 'docs'), { recursive: true })
    writeFileSync(path.join(repoDir, 'docs', 'in-repo.md'), '# In repo\n')
    const git = (cmd: string) => execSync(`git ${cmd}`, { cwd: repoDir, stdio: 'pipe' })
    git('init -q')
    git('config user.email e2e@example.com')
    git('config user.name e2e')
    git('add .')
    git('commit -qm seed')
    git('worktree add .peckboard/worktrees/abcd1234 -b card/abcd1234')
    writeFileSync(
      path.join(repoDir, '.peckboard/worktrees/abcd1234/docs/wt-note.md'),
      '# Worktree note\n\nOnly in the worktree.\n',
    )

    await loadAt(page, token, '/review')
    await expect(page.getByTestId('review-list')).toBeVisible({ timeout: 15_000 })
    await page.getByTestId('review-new').click()
    await page.getByTestId('review-wizard-kind-file').click()
    await page.getByTestId('review-wizard-next').click()

    // The radio swaps the folder select for the repo → worktree cascade.
    // Search by the folder's unique name — a retried run leaves an older
    // registered folder with the same repos/app layout behind.
    await page.getByTestId('review-wizard-source-worktree').click()
    await expect(page.getByTestId('review-wizard-folder')).toHaveCount(0)
    await page.getByTestId('review-wizard-repo').click()
    await page.getByTestId('review-wizard-repo-search').fill(folder.name)
    await page.getByRole('option', { name: /repos\/app/ }).click()

    // The worktree picker offers the main checkout AND the card worktree.
    await page.getByTestId('review-wizard-worktree').click()
    await expect(page.getByRole('option', { name: /Main checkout/ })).toBeVisible()
    await page.getByTestId('review-wizard-worktree-search').fill('abcd1234')
    await page.getByRole('option', { name: /card\/abcd1234/ }).click()

    // The file picker lists the worktree's markdown, paths shown relative
    // to the worktree.
    await page.getByTestId('review-wizard-file').click()
    await expect(page.getByRole('option', { name: /wt-note\.md/ })).toBeVisible()
    await page.getByRole('option', { name: /wt-note\.md/ }).click()
    await expect(page.getByTestId('review-wizard-preview')).toContainText('Only in the worktree.')
    await page.getByTestId('review-wizard-create').click()

    await expect(page.getByTestId('review-view')).toBeVisible({ timeout: 15_000 })
    await expect(page.getByTestId('review-doc')).toContainText('Only in the worktree.')

    // The ref points into the worktree, still inside the folder's jail —
    // the existing folder flow is what every other test in this file runs.
    const list = await request.get('/api/doc-reviews', { headers: auth })
    const reviews = (
      (await list.json()) as { reviews: Array<{ title: string; source_ref: string }> }
    ).reviews
    const created = reviews.find((r) => r.title === 'wt-note')
    expect(created?.source_ref).toBe(
      `${folder.id}:repos/app/.peckboard/worktrees/abcd1234/docs/wt-note.md`,
    )
  })
})
