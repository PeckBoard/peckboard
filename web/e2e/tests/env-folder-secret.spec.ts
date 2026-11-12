import { test, expect, type APIRequestContext } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Save = unlock for encrypted ("secret") env vars scoped to a folder.
 *
 * Saving a secret verifies the owner's password, so the save itself unlocks
 * ALL of their secrets for the window they pick ("Keep secrets unlocked
 * for"). A secret added while the vault is locked must therefore reach the
 * commands sessions in that folder run — with no unlock prompt in between.
 *
 * Regression: the unlock cache was a snapshot taken at unlock time and the
 * save route never touched it, so a secret added later was silently missing
 * from every command (and "no unlock prompt, var missing" was all the user
 * saw).
 *
 * Driven by `mock:mcp`, which runs the ```mcp block in the prompt against
 * the REAL `run_command` tool — a real `sh` that prints the variable's
 * LENGTH (the value itself would be masked as ********). Had the save not
 * unlocked, the command would raise an unlock prompt nobody answers here,
 * wait it out, and print `secretlen=0`. `run_command` is approval-gated in
 * chat sessions, so the host-wide bypass is switched on for the test and
 * always restored. A leftover encrypted var would make later specs'
 * dispatches block on an unlock prompt, so env vars are wiped and the window
 * locked on the way out.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const SECRET_NAME = 'E2E_FOLDER_SECRET'
const SECRET_VALUE = 'folder-secret-xyz' // 17 chars

type Auth = { Authorization: string }

async function authenticate(request: APIRequestContext): Promise<Auth> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { Authorization: `Bearer ${token}` }
}

async function wipeEnvVars(request: APIRequestContext, auth: Auth) {
  const res = await request.get('/api/env-vars', { headers: auth })
  expect(res.ok()).toBeTruthy()
  const { vars } = (await res.json()) as { vars: Array<{ id: string }> }
  for (const v of vars) {
    const del = await request.delete(`/api/env-vars/${encodeURIComponent(v.id)}`, { headers: auth })
    expect(del.ok(), `wipe env var ${v.id} failed`).toBeTruthy()
  }
}

async function lockNow(request: APIRequestContext, auth: Auth) {
  const res = await request.post('/api/env-vars/lock', { headers: auth, data: {} })
  expect(res.ok(), `lock failed: ${await res.text()}`).toBeTruthy()
}

function mcpBlock(tool: string, args: Record<string, unknown>): string {
  return '```mcp\n' + JSON.stringify({ tool, args }) + '\n```'
}

let priorBypass: boolean | undefined

test.afterEach(async ({ request }) => {
  const auth = await authenticate(request)
  await wipeEnvVars(request, auth)
  await lockNow(request, auth)
  if (priorBypass !== undefined) {
    // Host-wide setting — never leave it loosened for the rest of the suite.
    await request.put('/api/settings/tool-permissions', {
      headers: auth,
      data: { bypass: priorBypass },
    })
    priorBypass = undefined
  }
})

test('saving a folder secret unlocks it, and commands in that folder get it without a prompt', async ({
  request,
}) => {
  const auth = await authenticate(request)
  await wipeEnvVars(request, auth)
  await lockNow(request, auth)

  const perms = await request.get('/api/settings/tool-permissions', { headers: auth })
  expect(perms.ok()).toBeTruthy()
  priorBypass = ((await perms.json()) as { bypass: boolean }).bypass
  const on = await request.put('/api/settings/tool-permissions', {
    headers: auth,
    data: { bypass: true },
  })
  expect(on.ok(), `enable bypass failed: ${await on.text()}`).toBeTruthy()

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-folder-secret-'))
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: path.basename(folderPath), path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  // The vault starts locked.
  const before = await request.get('/api/env-vars/unlock-status', { headers: auth })
  expect(((await before.json()) as { unlocked: boolean }).unlocked).toBe(false)

  // Add the folder secret with a window: the save itself unlocks.
  const save = await request.post('/api/env-vars', {
    headers: auth,
    data: {
      name: SECRET_NAME,
      value: SECRET_VALUE,
      encrypt: true,
      password: E2E_PASS,
      folder_id: folder.id,
      unlock_duration: '1h',
    },
  })
  expect(save.ok(), `save encrypted var failed: ${await save.text()}`).toBeTruthy()

  const after = await request.get('/api/env-vars/unlock-status', { headers: auth })
  const status = (await after.json()) as { unlocked: boolean; expires_in_secs: number | null }
  expect(status.unlocked).toBe(true)
  expect(status.expires_in_secs).toBeGreaterThan(59 * 60)

  const sessionRes = await request.post('/api/sessions', {
    headers: auth,
    data: { name: 'folder-secret', folder_id: folder.id, model: 'mock:mcp' },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const sessionId = ((await sessionRes.json()) as { id: string }).id

  const prompt = [
    'print the secret length',
    mcpBlock('run_command', {
      command: 'sh',
      args: ['-c', `echo secretlen=\${#${SECRET_NAME}}; echo $${SECRET_NAME}`],
      reason: 'e2e: check the folder secret reaches the command',
    }),
  ].join('\n')
  const send = await request.post(`/api/sessions/${sessionId}/message`, {
    headers: auth,
    data: { text: prompt, model: 'mock:mcp' },
    timeout: 60_000,
  })
  expect(send.ok(), `send message failed: ${await send.text()}`).toBeTruthy()

  const transcript = async () => {
    const res = await request.get(`/api/sessions/${sessionId}/events?after_seq=0`, {
      headers: auth,
    })
    expect(res.ok(), `list events failed: ${await res.text()}`).toBeTruthy()
    return JSON.stringify(await res.json())
  }
  // The command got the full value — 0 would mean it never reached it.
  // Quick, too: no unlock prompt sat in between (that waits ~30s).
  await expect
    .poll(async () => (await transcript()).match(/secretlen=\d+/)?.[0], { timeout: 20_000 })
    .toBe(`secretlen=${SECRET_VALUE.length}`)
  const text = await transcript()
  // Printing it is masked, and the value itself never shows up anywhere.
  expect(text).toContain('********')
  expect(text).not.toContain(SECRET_VALUE)
  // Nothing was locked, so no locked-secret notice either.
  expect(text).not.toContain('secret env vars locked')
})
