import { test, expect, type APIRequestContext } from '../harness'
import { chmodSync, mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

/**
 * Image attachments on a Cursor session reach `cursor-agent --image`.
 *
 * Drives the real composer, server and cursor plugin against a fake CLI that
 * answers with what it found on disk for each `--image` it was given, so a
 * pass means the bytes were staged where the CLI can read them. The real CLI
 * needs a signed-in account and is not exercised here.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const FAKE_CURSOR = `#!/bin/sh
case " $* " in
  *" --print "*) ;;
  *) exit 0 ;;
esac
seen=""
while [ $# -gt 0 ]; do
  case "$1" in
    --image)
      shift
      seen="$seen $(basename "$1"):$(wc -c < "$1" | tr -d ' ')"
      ;;
    --) break ;;
  esac
  shift
done
echo '{"type":"system","subtype":"init","session_id":"fake-chat","model":"auto"}'
echo "{\\"type\\":\\"assistant\\",\\"message\\":{\\"role\\":\\"assistant\\",\\"content\\":[{\\"type\\":\\"text\\",\\"text\\":\\"SAW-IMAGES$seen\\"}]}}"
echo '{"type":"result","subtype":"success","session_id":"fake-chat","is_error":false}'
`

// A real PNG header so the backend sniffs `image/png`, padded past the old
// 1 MiB staging cap: a screenshot is routinely this size.
const LARGE_PNG = Buffer.concat([
  Buffer.from(
    'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==',
    'base64',
  ),
  Buffer.alloc(2 * 1024 * 1024),
])

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

test('a Cursor session sends an image to the CLI and reports a file it cannot send', async ({
  request,
  page,
}) => {
  const { token, authHeader } = await authenticate(request)

  const models = (await (await request.get('/api/models', { headers: authHeader })).json()) as {
    providers: Array<{ id: string; capabilities?: { supports_images_in: boolean } }>
  }
  expect(models.providers.find((p) => p.id === 'cursor')?.capabilities?.supports_images_in).toBe(
    true,
  )

  const dir = mkdtempSync(join(tmpdir(), 'peckboard-e2e-cursor-img-'))
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
      data: { name: 'e2e-cursor-img', path: dir },
    })
    expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
    const folder = (await folderRes.json()) as { id: string }
    const sessionRes = await request.post('/api/sessions', {
      headers: authHeader,
      data: { name: 'cursor images', folder_id: folder.id, model: 'cursor:auto' },
    })
    expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
    const session = (await sessionRes.json()) as { id: string }

    await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
    await page.goto(`/sessions/${session.id}`)
    await expect(page.locator('.chat-empty')).toBeVisible({ timeout: 10_000 })

    const attach = async (name: string, mimeType: string, buffer: Buffer) => {
      await page.locator('input[type="file"]').setInputFiles({ name, mimeType, buffer })
      await expect(page.locator('.input-bar .attachment-chip-name')).toContainText(name, {
        timeout: 10_000,
      })
    }
    const send = async (text: string) => {
      await page.locator('.input-textarea').fill(text)
      await page.locator('button[aria-label="Send message"]').click()
    }

    await attach('shot.png', 'image/png', LARGE_PNG)
    await send('what is in this image?')
    await expect(page.getByText(`SAW-IMAGES 0-shot.png:${LARGE_PNG.length}`)).toBeVisible({
      timeout: 20_000,
    })

    // Not an image: the turn still runs, and the transcript says what was left out.
    await attach('notes.pdf', 'application/pdf', Buffer.from('%PDF-1.4\n'))
    await send('and this file?')
    await expect(page.getByText('1 attachment on your message was not sent')).toBeVisible({
      timeout: 20_000,
    })
  } finally {
    await setCli('')
  }
})
