import { test, expect, type APIRequestContext } from '@playwright/test'
import { spawnSync } from 'node:child_process'
import { mkdtempSync, writeFileSync, existsSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Worktree merge retry:
 *
 *  - A card worktree with uncommitted work fails to merge: the endpoint
 *    answers 409 with the git status text and the reason is persisted on
 *    the card payload (so it survives a restart).
 *  - Committing the work and retrying merges it, cleans the worktree +
 *    branch up, and clears the card flag.
 *  - The `worktree-done {merged:false}` transcript row renders the reason,
 *    the git output, and a "Retry merge" button that drives the endpoint.
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

function git(cwd: string, args: string[]) {
  const res = spawnSync('git', ['-C', cwd, ...args], { encoding: 'utf8' })
  if (res.status !== 0) {
    throw new Error(`git ${args.join(' ')} failed (${res.status}): ${res.stderr}${res.stdout}`)
  }
}

/** A git repo with one commit on `main`. */
function makeRepo() {
  const repo = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-wt-'))
  git(repo, ['init', '-b', 'main'])
  git(repo, ['config', 'user.email', 'e2e@example.com'])
  git(repo, ['config', 'user.name', 'E2E'])
  writeFileSync(path.join(repo, 'README.md'), 'hello\n')
  git(repo, ['add', '.'])
  git(repo, ['commit', '-m', 'init'])
  // The worker's `ensure_worktree` does this; the worktrees live inside the
  // repo, so without it the main folder reads as dirty and never merges.
  writeFileSync(path.join(repo, '.git', 'info', 'exclude'), '.peckboard/\n')
  return repo
}

/** id8 = first 8 hex chars of the card UUID (matches `card_id8` in Rust). */
function cardId8(cardId: string) {
  return (cardId.match(/[0-9a-fA-F]/g) ?? []).slice(0, 8).join('')
}

async function seedCard(request: APIRequestContext, authHeader: Record<string, string>) {
  const repo = makeRepo()
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `e2e-wt-${Date.now()}`, path: repo },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const projectRes = await request.post('/api/projects', {
    headers: authHeader,
    data: {
      name: 'e2e-worktree-retry',
      folder_id: folder.id,
      worker_count: 1,
      workflow: 'task',
      model: 'mock:happy-path',
      worktree_isolation: true,
    },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  const cardRes = await request.post(`/api/projects/${project.id}/cards`, {
    headers: authHeader,
    data: {
      title: 'worktree card',
      description: '',
      step: 'backlog',
      priority: 1,
      workflow: 'task',
    },
  })
  expect(cardRes.ok(), `create card failed: ${await cardRes.text()}`).toBeTruthy()
  const card = (await cardRes.json()) as { id: string }

  // Stand up the worktree the same way the worker does.
  const id8 = cardId8(card.id)
  const wt = path.join(repo, '.peckboard', 'worktrees', id8)
  git(repo, ['worktree', 'add', wt, '-b', `card/${id8}`])

  return { repo, wt, id8, folderId: folder.id, projectId: project.id, cardId: card.id }
}

async function readCard(
  request: APIRequestContext,
  authHeader: Record<string, string>,
  projectId: string,
  cardId: string,
) {
  const res = await request.get(`/api/projects/${projectId}/cards`, { headers: authHeader })
  expect(res.ok(), `list cards failed: ${await res.text()}`).toBeTruthy()
  const cards = (await res.json()) as Array<{
    id: string
    worktree_unmerged_reason?: string | null
  }>
  const card = cards.find((c) => c.id === cardId)
  expect(card, 'card missing from list').toBeTruthy()
  return card!
}

test('retry-merge reports the git error, persists the reason, then merges once resolved', async ({
  request,
}) => {
  const { authHeader } = await authenticate(request)
  const { repo, wt, id8, projectId, cardId } = await seedCard(request, authHeader)

  // Uncommitted work in the worktree: the merge can't run.
  writeFileSync(path.join(wt, 'scratch.txt'), 'wip\n')
  const dirtyRes = await request.post(`/api/projects/${projectId}/cards/${cardId}/retry-merge`, {
    headers: authHeader,
  })
  expect(dirtyRes.status()).toBe(409)
  const dirtyBody = (await dirtyRes.json()) as { error: string; reason: string }
  expect(dirtyBody.reason).toBe('dirty')
  expect(dirtyBody.error).toContain('scratch.txt')
  expect(existsSync(wt), 'worktree removed despite dirty tree').toBeTruthy()

  // The reason is durable on the card payload, not just in the transcript.
  const flagged = await readCard(request, authHeader, projectId, cardId)
  expect(flagged.worktree_unmerged_reason).toBe('dirty')

  // Resolve it the way a user would, then retry.
  git(wt, ['config', 'user.email', 'e2e@example.com'])
  git(wt, ['config', 'user.name', 'E2E'])
  git(wt, ['add', '.'])
  git(wt, ['commit', '-m', 'card work'])

  const okRes = await request.post(`/api/projects/${projectId}/cards/${cardId}/retry-merge`, {
    headers: authHeader,
  })
  expect(okRes.ok(), `retry failed: ${await okRes.text()}`).toBeTruthy()
  expect((await okRes.json()) as { merged: boolean }).toMatchObject({ merged: true })

  expect(existsSync(path.join(repo, 'scratch.txt')), 'merge did not land').toBeTruthy()
  expect(existsSync(wt), 'worktree not cleaned up').toBeFalsy()
  const cleared = await readCard(request, authHeader, projectId, cardId)
  expect(cleared.worktree_unmerged_reason ?? null).toBeNull()
  expect(id8).toHaveLength(8)
})

test('unmerged worktree row offers Retry merge in the transcript', async ({ request, page }) => {
  const { token, authHeader } = await authenticate(request)
  const { repo, wt, id8, folderId, projectId, cardId } = await seedCard(request, authHeader)

  // Committed card work, waiting to be merged back.
  git(wt, ['config', 'user.email', 'e2e@example.com'])
  git(wt, ['config', 'user.name', 'E2E'])
  writeFileSync(path.join(wt, 'feature.txt'), 'done\n')
  git(wt, ['add', '.'])
  git(wt, ['commit', '-m', 'card work'])

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'worktree worker', folder_id: folderId },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  const eventRes = await request.post(`/api/sessions/${session.id}/events`, {
    headers: authHeader,
    data: {
      kind: 'worktree-done',
      data: {
        merged: false,
        reason: 'conflict',
        detail: 'CONFLICT (content): Merge conflict in README.md',
        branch: `card/${id8}`,
        cardId,
        projectId,
      },
    },
  })
  expect(eventRes.ok(), `inject event failed: ${await eventRes.text()}`).toBeTruthy()

  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(`/sessions/${session.id}`)

  const row = page.getByTestId('chat-worktree-merge')
  await expect(row).toBeVisible()
  await expect(row).toContainText('Worktree not merged')
  await expect(row).toContainText('merge conflict')
  await page.getByTestId('chat-worktree-detail').click()
  await expect(row).toContainText('CONFLICT (content)')

  // The button drives the real endpoint against a now-mergeable worktree.
  await page.getByTestId('chat-worktree-retry').click()
  await expect(page.getByTestId('chat-worktree-retry-ok')).toBeVisible()
  expect(existsSync(path.join(repo, 'feature.txt')), 'merge did not land').toBeTruthy()
})
