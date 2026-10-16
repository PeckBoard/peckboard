import { test, expect, type APIRequestContext, type Page } from '../harness'
import { spawnSync } from 'node:child_process'
import { mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * pr-review plugin (staged + approved by the e2e harness):
 *
 *  - The "PR Reviews" sidebar page starts reviews from a branch + target
 *    folder + model; each review is its own session, so two reviews run
 *    side by side and both rows show in the list.
 *  - A `mock:plan-review` run never calls the plugin tools, so the watchdog
 *    nudges it and then fails the review with an explicit error. (Plugins
 *    only see thinking models, hence not `mock:happy-path`.)
 *  - A `mock:mcp` run executes the ```mcp blocks carried by the per-review
 *    instructions (they ride in the kickoff message): it reports steps and
 *    submits a result, and the detail page renders summary / why / ticket /
 *    recommended changes / praise.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const PAGE_URL = '/plugin-page/pr-review/pr-review'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function skipUnlessStaged(request: APIRequestContext, auth: Record<string, string>) {
  const catalogRes = await request.get('/api/plugins', { headers: auth })
  const catalog = catalogRes.ok() ? await catalogRes.json() : { plugins: [] }
  test.skip(
    !JSON.stringify(catalog).includes('pr-review'),
    'pr-review wasm not built/staged — run peck-plugins/pr-review/build.sh',
  )
}

/** A folder whose root is a git repo with a `feature/ABC-1-login` branch. */
async function makeRepoFolder(request: APIRequestContext, auth: Record<string, string>) {
  const root = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-prr-'))
  const git = (args: string[]) => {
    const res = spawnSync('git', ['-C', root, ...args], { encoding: 'utf8' })
    expect(res.status, `git ${args.join(' ')}: ${res.stderr}`).toBe(0)
  }
  git(['init', '-b', 'main'])
  git(['config', 'user.email', 'e2e@example.com'])
  git(['config', 'user.name', 'e2e'])
  writeFileSync(path.join(root, 'README.md'), 'hello\n')
  git(['add', '.'])
  git(['commit', '-m', 'init'])
  git(['branch', 'feature/ABC-1-login'])
  const name = `e2e-prr-${Date.now()}`
  const res = await request.post('/api/folders', { headers: auth, data: { name, path: root } })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return { ...((await res.json()) as { id: string }), name }
}

async function openPage(page: Page, token: string) {
  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto(PAGE_URL)
  const frame = page.frameLocator('[data-testid="plugin-fullpage-frame"]')
  await expect(frame.getByTestId('pr-review-page')).toBeVisible({ timeout: 15_000 })
  return frame
}

async function startReview(
  frame: ReturnType<Page['frameLocator']>,
  opts: { branch: string; folderName: string; folderId: string; model: string; prompt?: string },
) {
  await frame.getByTestId('pr-review-new-btn').click()
  await expect(frame.getByTestId('pr-review-form')).toBeVisible()
  await frame.getByTestId('pr-review-branch').fill(opts.branch)
  await frame.getByTestId('pr-review-folder').fill(opts.folderName)
  await frame.locator(`[data-testid="pr-review-folder-option"][data-id="${opts.folderId}"]`).click()
  await frame.getByTestId('pr-review-model').selectOption(opts.model)
  if (opts.prompt) await frame.getByTestId('pr-review-custom-prompt').fill(opts.prompt)
  await frame.getByTestId('pr-review-start').click()
}

function mcpBlock(tool: string, args: Record<string, unknown>): string {
  return '```mcp\n' + JSON.stringify({ tool, args }) + '\n```'
}

test('parallel reviews from the page; a tool-less run fails via the watchdog', async ({
  page,
  request,
}) => {
  const { token, auth } = await authenticate(request)
  await skipUnlessStaged(request, auth)
  const folder = await makeRepoFolder(request, auth)
  const frame = await openPage(page, token)

  for (const branch of ['feature/ABC-1-login', 'main']) {
    await startReview(frame, {
      branch,
      folderName: folder.name,
      folderId: folder.id,
      model: 'mock:plan-review',
    })
    // Starting opens the review; return to the list for the next one.
    await expect(frame.getByTestId('pr-review-detail')).toBeVisible({ timeout: 15_000 })
    await frame.getByTestId('pr-review-back').click()
  }

  const rows = frame.getByTestId('pr-review-row')
  await expect(rows).toHaveCount(2)
  await expect(rows.filter({ hasText: 'feature/ABC-1-login' })).toHaveCount(1)

  // Both runs end without calling pr_review_submit → nudged, then failed.
  for (const row of await rows.all()) {
    await expect(row).toHaveAttribute('data-status', 'failed', { timeout: 60_000 })
  }
  await rows.first().click()
  await expect(frame.getByTestId('pr-review-error')).toContainText(/without submitting/, {
    timeout: 15_000,
  })
})

test('a submitted review renders summary, ticket, changes and praise', async ({
  page,
  request,
}) => {
  const { token, auth } = await authenticate(request)
  await skipUnlessStaged(request, auth)
  const folder = await makeRepoFolder(request, auth)
  const frame = await openPage(page, token)

  const result = {
    summary: 'Adds a login form backed by the session API.',
    why: 'Users could not sign in from the landing page.',
    ticket: { id: 'ABC-1', url: 'https://example.com/ABC-1', title: 'Login from landing' },
    verdict: 'request_changes',
    findings: [
      {
        kind: 'change',
        severity: 'blocker',
        file: 'src/login.ts',
        line: 12,
        end_line: null,
        title: 'Password logged in plain text',
        detail: 'The submit handler logs the whole form, including the password.',
        suggestion: 'Drop the console.log of the form payload.',
        reviewers: ['claude', 'codex'],
        confidence: 0.95,
      },
      {
        kind: 'praise',
        severity: null,
        file: 'src/login.test.ts',
        line: null,
        end_line: null,
        title: 'Thorough error-path tests',
        detail: 'Covers lockout and expired-session paths.',
        suggestion: null,
        reviewers: ['grok'],
        confidence: 0.9,
      },
    ],
    reviewers_failed: [],
    dropped: [],
  }
  const prompt = [
    mcpBlock('pr_review_step', { step: 'reviewing', note: 'three reviewers' }),
    mcpBlock('pr_review_step', { step: 'reviewing', provider: 'claude', provider_status: 'done' }),
    mcpBlock('pr_review_submit', { result }),
  ].join('\n')

  await startReview(frame, {
    branch: 'feature/ABC-1-login',
    folderName: folder.name,
    folderId: folder.id,
    model: 'mock:mcp',
    prompt,
  })

  const detail = frame.getByTestId('pr-review-detail')
  await expect(detail).toBeVisible({ timeout: 15_000 })
  await expect(frame.getByTestId('pr-review-summary')).toContainText(result.summary, {
    timeout: 30_000,
  })
  await expect(frame.getByTestId('pr-review-why')).toContainText(result.why)
  await expect(frame.getByTestId('pr-review-ticket')).toContainText('ABC-1')
  const finding = frame.getByTestId('pr-review-finding').filter({ hasText: 'Password logged' })
  await expect(finding).toHaveAttribute('data-severity', 'blocker')
  await expect(finding).toContainText('src/login.ts')
  await expect(finding.getByTestId('pr-review-consensus')).toBeVisible()
  await expect(frame.getByTestId('pr-review-praise-item')).toContainText('Thorough error-path')

  // Back on the list the row is done with its verdict and blocker count.
  await frame.getByTestId('pr-review-back').click()
  const row = frame.getByTestId('pr-review-row').first()
  await expect(row).toHaveAttribute('data-status', 'done')
  await expect(row.getByTestId('pr-review-verdict')).toHaveAttribute(
    'data-verdict',
    'request_changes',
  )
})

test('a session starts a review with pr_review_start and is told how it ended', async ({
  page,
  request,
}) => {
  const { token, auth } = await authenticate(request)
  await skipUnlessStaged(request, auth)
  const folder = await makeRepoFolder(request, auth)

  // A mock:mcp session runs the ```mcp block: pr_review_start in its own
  // folder with a tool-less review model, so the watchdog fails it and the
  // requester gets the FAILED notification.
  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'review requester', folder_id: folder.id, model: 'mock:mcp' },
  })
  expect(sessionRes.ok(), await sessionRes.text()).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  const send = await request.post(`/api/sessions/${session.id}/message`, {
    headers: auth,
    data: {
      text: mcpBlock('pr_review_start', {
        branch: 'feature/ABC-1-login',
        model: 'mock:plan-review',
      }),
      model: 'mock:mcp',
    },
  })
  expect(send.ok(), await send.text()).toBeTruthy()

  // The review shows up on the page, in the requester's folder, and fails.
  const frame = await openPage(page, token)
  const row = frame
    .getByTestId('pr-review-row')
    .filter({ hasText: 'feature/ABC-1-login' })
    .filter({ hasText: folder.name })
  await expect(row).toHaveCount(1, { timeout: 30_000 })
  await expect(row).toHaveAttribute('data-status', 'failed', { timeout: 60_000 })

  // The requesting session was messaged with the outcome: the delivery wakes
  // it for a second run (orchestrated messages are not `user` events).
  await expect
    .poll(
      async () => {
        const res = await request.get(`/api/sessions/${session.id}/events`, { headers: auth })
        const events = (await res.json()) as { kind: string }[]
        return events.filter((e) => e.kind === 'agent-start').length
      },
      { timeout: 30_000 },
    )
    .toBeGreaterThanOrEqual(2)
})
