import { test, expect, type APIRequestContext } from '../harness'
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { execSync } from 'node:child_process'

/**
 * A Cursor turn gets every MCP server: Peckboard's own, the ones from
 * Settings, and the ones a project wrote into its `.cursor/mcp.json`.
 *
 * cursor-agent reads that file only from the git root, and headless turns skip
 * any server that wasn't approved with `mcp enable`. The fake CLI records each
 * `mcp enable`, and the spec reads the file the real CLI would load. The real
 * CLI needs a signed-in account and is not exercised here.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

const fakeCursor = (log: string) => `#!/bin/sh
case " $* " in
  *" mcp enable "*)
    case "$3" in
      refuse-me) echo "cannot enable this one" >&2; exit 1 ;;
    esac
    echo "enable $3" >> "${log}"
    exit 0
    ;;
  *" --print "*)
    echo "turn $*" >> "${log}"
    echo '{"type":"system","subtype":"init","session_id":"fake-chat","model":"auto"}'
    echo '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"TURN-DONE"}]}}'
    echo '{"type":"result","subtype":"success","session_id":"fake-chat","is_error":false}'
    exit 0
    ;;
esac
exit 0
`

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { Authorization: `Bearer ${token}` }
}

test('a Cursor turn approves Peckboard, Settings and project MCP servers at the git root', async ({
  request,
}) => {
  const headers = await authenticate(request)

  const dir = mkdtempSync(join(tmpdir(), 'peckboard-e2e-cursor-mcp-'))
  const log = join(dir, 'cli.log')
  const fakeCli = join(dir, 'cursor-agent')
  writeFileSync(fakeCli, fakeCursor(log))
  chmodSync(fakeCli, 0o755)

  // The session folder sits below the repo root, and the project's own servers
  // are in the root's file, which is the only one the CLI reads.
  const repo = join(dir, 'repo')
  const sessionDir = join(repo, 'packages', 'app')
  mkdirSync(sessionDir, { recursive: true })
  execSync('git init -q .', { cwd: repo })
  mkdirSync(join(repo, '.cursor'))
  const rootConfig = join(repo, '.cursor', 'mcp.json')
  writeFileSync(
    rootConfig,
    JSON.stringify({
      mcpServers: {
        'project-server': { type: 'http', url: 'https://project.example/mcp' },
        'refuse-me': { type: 'http', url: 'https://refused.example/mcp' },
      },
    }),
  )

  const before = await (await request.get('/api/settings/mcp-servers', { headers })).json()
  const setSettings = (servers: unknown) =>
    request.put('/api/settings/mcp-servers', { headers, data: { servers } })
  const setCli = (cliPath: string) =>
    request.put('/api/plugins/cursor/settings', {
      headers,
      data: { updates: { cli_path: cliPath } },
    })

  try {
    const put = await setCli(fakeCli)
    expect(put.ok(), `setting cli_path failed: ${await put.text()}`).toBeTruthy()
    const putServers = await setSettings([
      {
        id: 'e2e-settings-server',
        name: 'settings-server',
        transport: 'http',
        url: 'https://settings.example/mcp',
        enabled: true,
        providers: [],
      },
    ])
    expect(putServers.ok(), `setting servers failed: ${await putServers.text()}`).toBeTruthy()

    const folderRes = await request.post('/api/folders', {
      headers,
      data: { name: 'e2e-cursor-mcp', path: sessionDir },
    })
    expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
    const folder = (await folderRes.json()) as { id: string }
    const sessionRes = await request.post('/api/sessions', {
      headers,
      data: { name: 'cursor mcp', folder_id: folder.id, model: 'cursor:auto' },
    })
    expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
    const session = (await sessionRes.json()) as { id: string }

    const send = await request.post(`/api/sessions/${session.id}/message`, {
      headers,
      data: { text: 'hello', model: 'cursor:auto' },
    })
    expect(send.ok(), `send failed: ${await send.text()}`).toBeTruthy()

    const eventTexts = async () => {
      const res = await request.get(`/api/sessions/${session.id}/events`, { headers })
      return JSON.stringify(await res.json())
    }
    await expect.poll(eventTexts, { timeout: 20_000 }).toContain('TURN-DONE')

    // Everything is merged into the file the CLI reads; nothing the project
    // wrote is lost, and the token never lands on disk.
    const config = readFileSync(rootConfig, 'utf8')
    const servers = Object.keys(JSON.parse(config).mcpServers)
    expect(servers).toEqual(
      expect.arrayContaining(['peckboard', 'settings-server', 'project-server', 'refuse-me']),
    )
    expect(config).toContain('${env:PECKBOARD_MCP_TOKEN}')

    const cli = readFileSync(log, 'utf8')
    for (const name of ['peckboard', 'settings-server', 'project-server']) {
      expect(cli).toContain(`enable ${name}\n`)
    }
    expect(cli).toContain('--approve-mcps')

    // A server that can't be approved says so instead of silently missing.
    const events = await eventTexts()
    expect(events).toContain('MCP server `refuse-me` couldn')
    expect(events).toContain('cannot enable this one')
  } finally {
    await setCli('')
    await setSettings((before as { servers?: unknown }).servers ?? [])
  }
})
