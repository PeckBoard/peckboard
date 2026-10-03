import { test, expect, type APIRequestContext } from '../harness'
import { mkdtempSync, readFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * The agent data-dir sandbox, end to end.
 *
 * `mock:sandbox-probe` spawns a REAL child through `provider.spawn` — the
 * host path every CLI provider's turn uses — that tries
 * `cat "$PECKBOARD_DATA_DIR/jwt_secret"` and then writes + reads a file in
 * the project folder, reporting each result as text. Under the enforced
 * Landlock sandbox the secret read fails with "Permission denied" while
 * the project folder works, and the secret never reaches the transcript.
 * Settings → Security shows the sandbox as enforced.
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

type LoggedEvent = { kind: string; data: Record<string, unknown> }

test('an agent process cannot read the data dir but can use its project folder', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)

  const status = await request.get('/api/settings/agent-sandbox', { headers: authHeader })
  expect(status.ok()).toBeTruthy()
  const sandbox = (await status.json()) as { status: { enforced: boolean; supported: boolean } }
  test.skip(!sandbox.status.supported, 'Landlock is unavailable on this host')
  expect(sandbox.status.enforced).toBe(true)

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-sandbox-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-sandbox', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'sandbox probe', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  const send = await request.post(`/api/sessions/${session.id}/message`, {
    headers: authHeader,
    data: { text: 'probe', model: 'mock:sandbox-probe' },
  })
  expect(send.ok(), `send failed: ${await send.text()}`).toBeTruthy()

  let events: LoggedEvent[] = []
  await expect
    .poll(
      async () => {
        const res = await request.get(`/api/sessions/${session.id}/events?after_seq=0`, {
          headers: authHeader,
        })
        events = (await res.json()) as LoggedEvent[]
        return events.some((e) => e.kind === 'agent-end')
      },
      { timeout: 20_000 },
    )
    .toBe(true)

  const transcript = events
    .filter((e) => e.kind === 'agent-text')
    .map((e) => String(e.data.text ?? ''))
    .join('\n')
  expect(transcript).toContain('Permission denied')
  expect(transcript).toMatch(/SECRET_EXIT=[1-9]/)
  expect(transcript).toContain('PROJECT_OK')
  expect(transcript).toContain('SANDBOX=landlock')
  // The secret is 32 raw bytes; none of its encodings may show up.
  const secret = readFileSync(path.join(process.env.PECKBOARD_E2E_DATA_DIR!, 'jwt_secret'))
  expect(secret.length).toBe(32)
  expect(transcript).not.toContain(secret.toString('hex'))
  expect(transcript).not.toContain(secret.toString('base64'))
  expect(transcript).not.toContain(secret.toString('latin1'))

  // Settings → Security reports the sandbox as enforced, no warning banner.
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto('/settings/security')
  const section = page.getByTestId('agent-sandbox-section')
  await expect(section).toBeVisible({ timeout: 10_000 })
  await expect(section.getByTestId('agent-sandbox-status')).toContainText('Enforced')
  await expect(section.getByTestId('agent-sandbox-mode')).toHaveValue('enforce')
  await expect(section.getByTestId('agent-sandbox-banner')).toHaveCount(0)
})
