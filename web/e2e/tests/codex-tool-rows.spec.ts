import { test, expect } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Codex numbers its tool calls per turn (`item_0`, `item_1`, ...), so every
 * turn after the first reuses ids. The chat used to drop any tool start whose
 * id it had ever seen, which left each later turn's tool-end as an unnamed
 * "Tool" row. The events are served as Codex sessions actually store them.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

test('tool calls in a later Codex turn keep their names when item ids repeat', async ({
  request,
  page,
}) => {
  const login = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(login.ok()).toBeTruthy()
  const { token } = (await login.json()) as { token: string }
  const auth = { Authorization: `Bearer ${token}` }

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-codex-rows-'))
  const folder = (await (
    await request.post('/api/folders', {
      headers: auth,
      data: { name: 'codex rows', path: folderPath },
    })
  ).json()) as { id: string }
  const session = (await (
    await request.post('/api/sessions', {
      headers: auth,
      data: { name: 'codex tool rows', folder_id: folder.id },
    })
  ).json()) as { id: string }

  let seq = 0
  const ev = (kind: string, data: Record<string, unknown>) => {
    seq += 1
    return {
      id: `ev-${seq}`,
      session_id: session.id,
      seq,
      ts: 1_700_000_000_000 + seq * 1000,
      kind,
      data,
    }
  }
  const turn = (prompt: string, tools: [string, Record<string, unknown>][]) => [
    ev('user', { text: prompt }),
    ev('agent-start', { model: 'codex:gpt-5' }),
    ...tools.flatMap(([name, input], i) => [
      ev('agent-tool-start', { tool_use_id: `item_${i}`, name, input }),
      ev('agent-tool-end', { tool_use_id: `item_${i}`, output: 'ok', error: null }),
    ]),
    ev('agent-text', { text: `${prompt} done` }),
    ev('agent-end', { status: 'complete' }),
  ]
  const events = [
    ...turn('first', [
      ['mcp__peckboard__read_file', { path: 'package.json' }],
      ['mcp__peckboard__list_files', { path: '.' }],
    ]),
    ...turn('second', [
      ['mcp__peckboard__search_files', { query: 'TODO' }],
      ['mcp__peckboard__write_file', { path: 'notes.md' }],
    ]),
  ]

  await page.route(`**/api/sessions/${session.id}/events*`, (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(events) }),
  )
  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto(`/sessions/${session.id}`)

  const labels = page.locator('.tool-block .tool-label')
  await expect(labels).toHaveCount(4, { timeout: 10_000 })
  await expect(labels).toHaveText(['Read file', 'List files', 'Search content', 'Write file'])
})
