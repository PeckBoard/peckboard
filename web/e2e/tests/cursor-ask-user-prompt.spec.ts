import { test, expect, type APIRequestContext } from '../harness'
import { chmodSync, mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

/**
 * Cursor sessions are told to use Peckboard's `ask_user` instead of the CLI's
 * built-in question tool, on the first turn and on resumed turns.
 *
 * Drives the real server and cursor plugin against a fake CLI that reports what
 * its prompt argument said. This pins what is sent, not how a model reacts: the
 * real CLI needs a signed-in account and is not exercised here.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const FAKE_CURSOR = `#!/bin/sh
case " $* " in
  *" --print "*) ;;
  *) exit 0 ;;
esac
for last; do :; done
case "$last" in
  *"built-in AskQuestion"*) rule=present ;;
  *) rule=absent ;;
esac
case " $* " in
  *" --resume "*) kind=resumed ;;
  *) kind=first ;;
esac
echo '{"type":"system","subtype":"init","session_id":"fake-chat","model":"auto"}'
echo "{\\"type\\":\\"assistant\\",\\"message\\":{\\"role\\":\\"assistant\\",\\"content\\":[{\\"type\\":\\"text\\",\\"text\\":\\"RULE-$kind-$rule\\"}]}}"
echo '{"type":"result","subtype":"success","session_id":"fake-chat","is_error":false}'
`

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

test('a Cursor session is told to use ask_user on first and resumed turns', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)

  const dir = mkdtempSync(join(tmpdir(), 'peckboard-e2e-cursor-ask-'))
  const fakeCli = join(dir, 'cursor-agent')
  writeFileSync(fakeCli, FAKE_CURSOR)
  chmodSync(fakeCli, 0o755)
  const setCli = (cliPath: string) =>
    request.put('/api/plugins/cursor/settings', {
      headers: authHeader,
      data: { updates: { cli_path: cliPath } },
    })
  const put = await setCli(fakeCli)
  expect(put.ok(), `setting cli_path failed: ${await put.text()}`).toBeTruthy()

  try {
    const folderRes = await request.post('/api/folders', {
      headers: authHeader,
      data: { name: 'e2e-cursor-ask', path: dir },
    })
    expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
    const folder = (await folderRes.json()) as { id: string }
    const sessionRes = await request.post('/api/sessions', {
      headers: authHeader,
      data: { name: 'cursor ask', folder_id: folder.id, model: 'cursor:auto' },
    })
    expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
    const session = (await sessionRes.json()) as { id: string }

    await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
    await page.goto(`/sessions/${session.id}`)
    await expect(page.locator('.chat-empty')).toBeVisible({ timeout: 10_000 })

    const send = async (text: string) => {
      await page.locator('.input-textarea').fill(text)
      await page.locator('button[aria-label="Send message"]').click()
    }

    await send('first message')
    await expect(page.getByText('RULE-first-present')).toBeVisible({ timeout: 20_000 })

    await send('second message')
    await expect(page.getByText('RULE-resumed-present')).toBeVisible({ timeout: 20_000 })
  } finally {
    await setCli('')
  }
})
