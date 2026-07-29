import { test, expect, type APIRequestContext } from '@playwright/test'
import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http'
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * PR-linked reviews, both directions, against a stub GitHub.
 *
 * `playwright.config.ts` boots the server with `GITHUB_TOKEN` set and
 * `PECKBOARD_GITHUB_API_BASE` pointed at the port this spec listens on, so
 * the real client code runs — same requests, same parsing, same replies —
 * and nothing leaves the machine. The stub records what it was asked, which
 * is how the outbound half is asserted.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const STUB_PORT = Number(process.env.PECKBOARD_E2E_GITHUB_PORT ?? '4446')

/** The document under review, and the file the PR comments on. */
const DOC = [
  '# Onboarding',
  '',
  'The team ships on Fridays.',
  '',
  'On-call rotates weekly.',
  '',
].join('\n')

interface Recorded {
  method: string
  url: string
  body: string
}

/** A fake GitHub that serves two review comments and records every reply. */
function startStub(): Promise<{ server: Server; seen: Recorded[] }> {
  const seen: Recorded[] = []
  const comments = [
    {
      id: 90001,
      path: 'docs/onboarding.md',
      line: 5,
      original_line: 5,
      body: 'who covers the weekend?',
      user: { login: 'octocat' },
    },
    {
      id: 90002,
      path: 'docs/onboarding.md',
      line: null,
      original_line: 3,
      body: 'is Friday still right?',
      user: { login: 'hubot' },
    },
    // A comment on another file in the same PR: not about this document.
    {
      id: 90003,
      path: 'src/main.rs',
      line: 12,
      original_line: 12,
      body: 'unrelated',
      user: { login: 'octocat' },
    },
  ]

  const handler = (req: IncomingMessage, res: ServerResponse) => {
    let body = ''
    req.on('data', (chunk) => (body += String(chunk)))
    req.on('end', () => {
      seen.push({ method: req.method ?? '', url: req.url ?? '', body })
      res.setHeader('Content-Type', 'application/json')
      if (req.method === 'GET' && (req.url ?? '').includes('/comments')) {
        // Page 1 holds everything; the client stops when a page is short.
        res.end(JSON.stringify((req.url ?? '').includes('page=1') ? comments : []))
        return
      }
      if (req.method === 'POST' && (req.url ?? '').includes('/replies')) {
        res.statusCode = 201
        res.end(JSON.stringify({ id: 99 }))
        return
      }
      res.end(JSON.stringify([]))
    })
  }

  const server = createServer(handler)
  return new Promise((resolve) => {
    server.listen(STUB_PORT, '127.0.0.1', () => resolve({ server, seen }))
  })
}

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

test.describe('document review — pull request link', () => {
  let stub: { server: Server; seen: Recorded[] }

  test.beforeAll(async () => {
    stub = await startStub()
  })
  test.afterAll(async () => {
    // The server holds keep-alive sockets open to the stub, so `close()`
    // alone waits for them and the run never ends. Drop them first.
    stub.server.closeAllConnections()
    await new Promise<void>((resolve) => stub.server.close(() => resolve()))
  })

  test('a linked PR imports its comments and a resolution replies on the thread', async ({
    request,
    page,
  }) => {
    const { token, auth } = await authenticate(request)

    // A folder holding a git checkout, so the link can be detected rather
    // than typed: `.git` with an origin remote and a branch.
    const dir = mkdtempSync(path.join(tmpdir(), 'peckboard-pr-'))
    const repo = path.join(dir, 'app')
    mkdirSync(path.join(repo, '.git'), { recursive: true })
    mkdirSync(path.join(repo, 'docs'), { recursive: true })
    writeFileSync(path.join(repo, 'docs', 'onboarding.md'), DOC)
    writeFileSync(
      path.join(repo, '.git', 'config'),
      '[remote "origin"]\n\turl = git@github.com:acme/app.git\n',
    )
    writeFileSync(path.join(repo, '.git', 'HEAD'), 'ref: refs/heads/feature/onboarding\n')

    const folderRes = await request.post('/api/folders', {
      headers: auth,
      data: { name: `pr-${Date.now()}`, path: dir },
    })
    expect(folderRes.ok(), `folder create failed: ${await folderRes.text()}`).toBeTruthy()
    const folder = (await folderRes.json()) as { id: string }

    const reviewRes = await request.post('/api/doc-reviews', {
      headers: auth,
      data: {
        source_kind: 'file',
        source_ref: `${folder.id}:app/docs/onboarding.md`,
        title: 'PR review',
      },
    })
    expect(reviewRes.ok(), `review create failed: ${await reviewRes.text()}`).toBeTruthy()
    const reviewId = ((await reviewRes.json()) as { review: { id: string } }).review.id

    // The checkout is enough to work out owner/repo and the file's path
    // inside the repo, with no typing.
    const state = await request.get(`/api/doc-reviews/${reviewId}/pr`, { headers: auth })
    const detected = (await state.json()) as {
      configured: boolean
      link: unknown
      suggestion: { owner: string; repo: string; file_path: string } | null
    }
    expect(detected.configured, 'the stub token counts as configured').toBeTruthy()
    expect(detected.link).toBeNull()
    expect(detected.suggestion?.owner).toBe('acme')
    expect(detected.suggestion?.repo).toBe('app')
    expect(detected.suggestion?.file_path).toBe('docs/onboarding.md')

    const linked = await request.put(`/api/doc-reviews/${reviewId}/pr`, {
      headers: auth,
      data: { owner: 'acme', repo: 'app', number: 42 },
    })
    expect(linked.ok(), `link failed: ${await linked.text()}`).toBeTruthy()

    // Inbound: the two comments on this file arrive as annotations; the one
    // on another file does not.
    const synced = await request.post(`/api/doc-reviews/${reviewId}/pr/sync`, { headers: auth })
    expect(synced.ok(), `sync failed: ${await synced.text()}`).toBeTruthy()
    expect((await synced.json()).imported).toBe(2)

    // Syncing again brings in nothing new — the ids are already known.
    const again = await request.post(`/api/doc-reviews/${reviewId}/pr/sync`, { headers: auth })
    expect((await again.json()).imported).toBe(0)

    const detail = await request.get(`/api/doc-reviews/${reviewId}?comments=all`, { headers: auth })
    const comments = (
      (await detail.json()) as {
        comments: Array<{
          id: string
          body: string
          start_line: number
          external_id: string | null
        }>
      }
    ).comments
    expect(comments).toHaveLength(2)
    const weekend = comments.find((c) => c.body.includes('weekend'))
    expect(weekend?.body).toContain('@octocat on GitHub:')
    expect(weekend?.start_line, 'anchored where GitHub says the line is').toBe(5)
    // The outdated comment falls back to the line it was written against.
    expect(comments.find((c) => c.body.includes('Friday'))?.start_line).toBe(3)

    // The screen shows the link and marks where those annotations came from.
    await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
    await page.goto(`/review/${reviewId}`)
    await expect(page.getByTestId('review-view')).toBeVisible({ timeout: 15_000 })
    await expect(page.getByTestId('review-pr-chip')).toHaveText('acme/app#42')
    await expect(page.getByTestId('review-annotation-origin')).toHaveCount(2)

    // Outbound: resolving an imported annotation answers its thread.
    const resolved = await request.patch(`/api/doc-reviews/${reviewId}/comments/${weekend!.id}`, {
      headers: auth,
      data: { status: 'answered', resolution_note: 'Rotation covers it.' },
    })
    expect(resolved.ok(), `resolve failed: ${await resolved.text()}`).toBeTruthy()

    await expect
      .poll(
        () => stub.seen.filter((r) => r.method === 'POST' && r.url.includes('/replies')).length,
        {
          timeout: 10_000,
          message: 'no reply reached GitHub',
        },
      )
      .toBe(1)
    const reply = stub.seen.find((r) => r.url.includes('/replies'))!
    expect(reply.url).toContain(`/repos/acme/app/pulls/42/comments/${weekend!.external_id}/replies`)
    expect(JSON.parse(reply.body).body).toContain('Rotation covers it.')

    // Unlinking keeps what came in — it was read and answered like any other.
    const unlinked = await request.delete(`/api/doc-reviews/${reviewId}/pr`, { headers: auth })
    expect(unlinked.ok()).toBeTruthy()
    const after = await request.get(`/api/doc-reviews/${reviewId}?comments=all`, { headers: auth })
    expect(((await after.json()) as { comments: unknown[] }).comments).toHaveLength(2)

    await request.delete(`/api/doc-reviews/${reviewId}`, { headers: auth })
    await request.delete(`/api/folders/${folder.id}`, { headers: auth })
  })
})
