import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync, readFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * UI e2e for the Agents panel (remote-control devices).
 *
 * One test walks the full user-visible lifecycle: enroll (one-time token
 * reveal) → row appears offline → a page-context WebSocket dials
 * `/ws/agent` with the enrollment token and sends the versioned `hello`
 * frame, proving the device-update broadcast flips the row online live →
 * disconnect flips it back → rename → disable (kill-switch badge) →
 * delete with confirm. A second test locks in the token being shown only
 * once (it never appears in any later API response). A third test drives
 * the `remote_agent_echo` MCP bridge end to end through a mock device,
 * proving the in-flight count and the recent-actions audit row.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

async function loadApp(page: Page, token: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/')
  await expect(page.locator('.rail-brand')).toBeVisible({ timeout: 10_000 })
}

/** Enroll via the UI; returns the one-time token and closes the reveal. */
async function enrollViaUi(page: Page, name: string): Promise<string> {
  await page.getByTestId('agents-enroll').click()
  await expect(page.getByTestId('enroll-modal')).toBeVisible()
  await page.getByTestId('enroll-name').fill(name)
  await page.getByTestId('enroll-platform').selectOption('linux')
  await page.getByTestId('enroll-submit').click()
  const tokenEl = page.getByTestId('enroll-token')
  await expect(tokenEl).toBeVisible()
  const enrollToken = ((await tokenEl.textContent()) ?? '').trim()
  expect(enrollToken).toHaveLength(64)
  // The exact install command, with the one-time token embedded.
  const cmdEl = page.getByTestId('enroll-command')
  await expect(cmdEl).toBeVisible()
  const cmd = ((await cmdEl.textContent()) ?? '').trim()
  expect(cmd).toContain('peckboard-agent enroll --server ')
  expect(cmd).toContain(`--token ${enrollToken}`)
  await page.getByTestId('enroll-done').click()
  await expect(page.getByTestId('enroll-token-modal')).toBeHidden()
  return enrollToken
}

test('agents panel: enroll, live online dot, rename, disable, delete', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await loadApp(page, token)

  await page.getByTestId('rail-agents').click()
  await expect(page.getByTestId('agents-view')).toBeVisible()

  const enrollToken = await enrollViaUi(page, 'E2E Box')

  // The row renders offline, with the platform label.
  const row = page.locator('.list-view-row', { hasText: 'E2E Box' })
  await expect(row).toBeVisible()
  await expect(row.getByTestId('agent-dot-offline')).toBeVisible()
  await expect(row).toContainText('Linux')

  // A daemon connects: page-context WebSocket to /ws/agent with the
  // enrollment token smuggled as a `token.<t>` subprotocol entry (browsers
  // can't set an Authorization header; the server never accepts tokens in
  // the URL, which proxies would log). First frame is the versioned hello.
  // The device-update broadcast must flip the dot online without a reload.
  await page.evaluate((tok) => {
    const proto = location.protocol === 'https:' ? 'wss' : 'ws'
    const ws = new WebSocket(`${proto}://${location.host}/ws/agent`, [
      'peckboard-agent',
      `token.${tok}`,
    ])
    ;(window as unknown as { __agentWs?: WebSocket }).__agentWs = ws
    ws.addEventListener('open', () => {
      ws.send(
        JSON.stringify({
          v: 1,
          type: 'hello',
          agent_version: '0.0.1-e2e',
          platform: 'linux',
          hostname: 'e2e-box',
          capabilities: ['echo'],
        }),
      )
    })
  }, enrollToken)
  await expect(row.getByTestId('agent-dot-online')).toBeVisible({ timeout: 10_000 })

  // Daemon disconnects → live flip back to offline.
  await page.evaluate(() => {
    ;(window as unknown as { __agentWs?: WebSocket }).__agentWs?.close()
  })
  await expect(row.getByTestId('agent-dot-offline')).toBeVisible({ timeout: 10_000 })

  // Rename from the row menu.
  await row.locator('.list-view-menu').click()
  await page.getByRole('menuitem', { name: 'Rename' }).click()
  await page.getByTestId('rename-input').fill('E2E Box Renamed')
  await page.getByTestId('rename-submit').click()
  const renamedRow = page.locator('.list-view-row', { hasText: 'E2E Box Renamed' })
  await expect(renamedRow).toBeVisible()

  // Kill-switch: Disable shows the badge and the menu now offers Enable.
  await renamedRow.locator('.list-view-menu').click()
  await page.getByRole('menuitem', { name: 'Disable' }).click()
  await expect(renamedRow.getByTestId('agent-disabled-badge')).toBeVisible()
  await renamedRow.locator('.list-view-menu').click()
  await expect(page.getByRole('menuitem', { name: 'Enable' })).toBeVisible()
  await page.keyboard.press('Escape')

  // Delete (danger) goes through the confirm dialog and empties the list.
  await renamedRow.locator('.list-view-menu').click()
  await page.getByRole('menuitem', { name: 'Delete' }).click()
  await page.getByTestId('confirm-dialog-confirm').click()
  await expect(page.getByTestId('agents-empty')).toBeVisible()
})

test('enrollment token is shown exactly once', async ({ request, page }) => {
  const token = await authenticate(request)
  await loadApp(page, token)

  await page.getByTestId('rail-agents').click()
  await expect(page.getByTestId('agents-view')).toBeVisible()
  const enrollToken = await enrollViaUi(page, 'One-Time Token Box')

  // The list endpoint never carries the token or its hash.
  const res = await request.get('/api/devices', {
    headers: { Authorization: `Bearer ${token}` },
  })
  expect(res.ok()).toBeTruthy()
  const body = await res.text()
  expect(body).not.toContain(enrollToken)
  expect(body).not.toContain('secret_hash')

  // Cleanup so this spec leaves no row behind for others.
  const rowMenu = page
    .locator('.list-view-row', { hasText: 'One-Time Token Box' })
    .locator('.list-view-menu')
  await rowMenu.click()
  await page.getByRole('menuitem', { name: 'Delete' }).click()
  await page.getByTestId('confirm-dialog-confirm').click()
  await expect(page.getByTestId('agents-empty')).toBeVisible()
})

test('recent-actions drawer opens from the row and shows the empty state', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await loadApp(page, token)

  await page.getByTestId('rail-agents').click()
  await expect(page.getByTestId('agents-view')).toBeVisible()
  await enrollViaUi(page, 'Drawer Box')

  const row = page.locator('.list-view-row', { hasText: 'Drawer Box' })
  await row.locator('.list-view-menu').click()
  await page.getByRole('menuitem', { name: 'Recent actions' }).click()
  await expect(page.getByTestId('agent-activity-modal')).toBeVisible()
  await expect(page.getByTestId('agent-activity-empty')).toBeVisible()
  await page.getByRole('button', { name: 'Close' }).click()
  await expect(page.getByTestId('agent-activity-modal')).toBeHidden()

  // Cleanup so this spec leaves no row behind for others.
  await row.locator('.list-view-menu').click()
  await page.getByRole('menuitem', { name: 'Delete' }).click()
  await page.getByTestId('confirm-dialog-confirm').click()
  await expect(page.getByTestId('agents-empty')).toBeVisible()
})

test('remote_agent_echo round-trips through a mock device: in-flight count and audit row', async ({
  request,
  page,
  baseURL,
}) => {
  const token = await authenticate(request)
  const authHeader = { Authorization: `Bearer ${token}` }
  await loadApp(page, token)

  await page.getByTestId('rail-agents').click()
  await expect(page.getByTestId('agents-view')).toBeVisible()
  const enrollToken = await enrollViaUi(page, 'Echo Box')

  const row = page.locator('.list-view-row', { hasText: 'Echo Box' })
  await expect(row).toBeVisible()

  const devicesRes = await request.get('/api/devices', { headers: authHeader })
  expect(devicesRes.ok()).toBeTruthy()
  const devices = (await devicesRes.json()) as { devices: { id: string; name: string }[] }
  const deviceId = devices.devices.find((d) => d.name === 'Echo Box')?.id
  expect(deviceId, 'enrolled device id resolvable from /api/devices').toBeTruthy()

  // A session that will drive the MCP bridge: sending one message makes the
  // server write the per-session MCP config (worker-mcp/<id>.json) *before*
  // the POST responds, so the token is readable from the response alone.
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-agents-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-agents-echo', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'echo bridge', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }

  const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
    headers: authHeader,
    data: { text: 'go', model: 'mock:happy-path' },
  })
  expect(sendRes.ok(), `send message failed: ${await sendRes.text()}`).toBeTruthy()

  expect(baseURL, 'baseURL configured').toBeTruthy()
  const dataDir = process.env.PECKBOARD_E2E_DATA_DIR
  expect(dataDir, 'PECKBOARD_E2E_DATA_DIR exported by playwright.config.ts').toBeTruthy()
  const mcpCfgPath = path.join(dataDir!, 'worker-mcp', `${session.id}.json`)
  const mcpCfg = JSON.parse(readFileSync(mcpCfgPath, 'utf8')) as {
    mcpServers: { peckboard: { headers: { Authorization: string } } }
  }
  const mcpToken = mcpCfg.mcpServers.peckboard.headers.Authorization.replace(/^Bearer /, '')

  // The mock device: connects, then on every request frame waits briefly
  // (so the in-flight tag is observable) before answering with a Result
  // that echoes the payload back, exactly like a real capability executor.
  await page.evaluate((tok) => {
    const proto = location.protocol === 'https:' ? 'wss' : 'ws'
    const ws = new WebSocket(`${proto}://${location.host}/ws/agent`, [
      'peckboard-agent',
      `token.${tok}`,
    ])
    ;(window as unknown as { __agentWs?: WebSocket }).__agentWs = ws
    ws.addEventListener('open', () => {
      ws.send(
        JSON.stringify({
          v: 1,
          type: 'hello',
          agent_version: '0.0.1-e2e',
          platform: 'linux',
          hostname: 'e2e-box',
          capabilities: ['echo'],
        }),
      )
    })
    ws.addEventListener('message', (ev) => {
      const frame = JSON.parse(String(ev.data))
      if (frame.type !== 'request') return
      setTimeout(() => {
        ws.send(
          JSON.stringify({
            v: 1,
            type: 'result',
            corr_id: frame.corr_id,
            ok: true,
            payload: { capability: frame.capability, echo: frame.args },
          }),
        )
      }, 1200)
    })
  }, enrollToken)
  await expect(row.getByTestId('agent-dot-online')).toBeVisible({ timeout: 10_000 })

  // Fire remote_agent_echo over the loopback /mcp JSON-RPC endpoint
  // (same path a real Claude session's tool call takes) but don't await
  // it yet — the mock device holds the reply for 1.2s so the in-flight
  // tag has a window to appear.
  const echoPromise = request.post('/mcp', {
    headers: { Authorization: `Bearer ${mcpToken}` },
    data: {
      jsonrpc: '2.0',
      id: 1,
      method: 'tools/call',
      params: {
        name: 'remote_agent_echo',
        arguments: { device_id: deviceId, message: 'ping' },
      },
    },
  })

  await expect(row.getByTestId('agent-in-flight')).toContainText('1 in flight', {
    timeout: 10_000,
  })

  const echoRes = await echoPromise
  expect(echoRes.ok(), `remote_agent_echo failed: ${await echoRes.text()}`).toBeTruthy()
  const echoBody = (await echoRes.json()) as {
    result?: { content: { text: string }[] }
    error?: unknown
  }
  expect(echoBody.error, `MCP error: ${JSON.stringify(echoBody.error)}`).toBeFalsy()
  const toolText = echoBody.result?.content?.[0]?.text ?? ''
  const toolResult = JSON.parse(toolText) as { ok: boolean; result: { echo: { message: string } } }
  expect(toolResult.ok).toBe(true)
  expect(toolResult.result.echo).toEqual({ message: 'ping' })

  await expect(row.getByTestId('agent-in-flight')).toBeHidden({ timeout: 10_000 })

  // The recent-actions drawer shows the bridged call, newest first.
  await row.locator('.list-view-menu').click()
  await page.getByRole('menuitem', { name: 'Recent actions' }).click()
  await expect(page.getByTestId('agent-activity-modal')).toBeVisible()
  const activityRow = page.getByTestId('agent-activity-row').first()
  await expect(activityRow).toContainText('echo')
  await expect(activityRow).toContainText('ok')
  await page.getByRole('button', { name: 'Close' }).click()
  await expect(page.getByTestId('agent-activity-modal')).toBeHidden()

  // Kill-switch: disabling severs the live socket immediately.
  await row.locator('.list-view-menu').click()
  await page.getByRole('menuitem', { name: 'Disable' }).click()
  await expect(row.getByTestId('agent-dot-offline')).toBeVisible({ timeout: 10_000 })
  await expect(row.getByTestId('agent-disabled-badge')).toBeVisible()

  // Cleanup so this spec leaves no row behind for others.
  await row.locator('.list-view-menu').click()
  await page.getByRole('menuitem', { name: 'Delete' }).click()
  await page.getByTestId('confirm-dialog-confirm').click()
  await expect(page.getByTestId('agents-empty')).toBeVisible()
})

test('remote_agent_screenshot targets a monitor: monitor forwarded, display aliased, image returned', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const authHeader = { Authorization: `Bearer ${token}` }
  await loadApp(page, token)

  await page.getByTestId('rail-agents').click()
  await expect(page.getByTestId('agents-view')).toBeVisible()
  const enrollToken = await enrollViaUi(page, 'Screen Box')

  const row = page.locator('.list-view-row', { hasText: 'Screen Box' })
  await expect(row).toBeVisible()

  const devicesRes = await request.get('/api/devices', { headers: authHeader })
  expect(devicesRes.ok()).toBeTruthy()
  const devices = (await devicesRes.json()) as { devices: { id: string; name: string }[] }
  const deviceId = devices.devices.find((d) => d.name === 'Screen Box')?.id
  expect(deviceId, 'enrolled device id resolvable from /api/devices').toBeTruthy()

  // MCP token via a fresh session (same mechanics as the echo test above).
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-agents-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-agents-screenshot', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'screenshot bridge', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
    headers: authHeader,
    data: { text: 'go', model: 'mock:happy-path' },
  })
  expect(sendRes.ok(), `send message failed: ${await sendRes.text()}`).toBeTruthy()
  const dataDir = process.env.PECKBOARD_E2E_DATA_DIR
  expect(dataDir, 'PECKBOARD_E2E_DATA_DIR exported by playwright.config.ts').toBeTruthy()
  const mcpCfgPath = path.join(dataDir!, 'worker-mcp', `${session.id}.json`)
  const mcpCfg = JSON.parse(readFileSync(mcpCfgPath, 'utf8')) as {
    mcpServers: { peckboard: { headers: { Authorization: string } } }
  }
  const mcpToken = mcpCfg.mcpServers.peckboard.headers.Authorization.replace(/^Bearer /, '')

  // Screenshot-executor stand-in: replies with a tiny image plus the args
  // the daemon actually received, so the test can assert the wire contract
  // (the daemon-side selector key is `monitor`).
  await page.evaluate((tok) => {
    const proto = location.protocol === 'https:' ? 'wss' : 'ws'
    const ws = new WebSocket(`${proto}://${location.host}/ws/agent`, [
      'peckboard-agent',
      `token.${tok}`,
    ])
    ;(window as unknown as { __agentWs?: WebSocket }).__agentWs = ws
    ws.addEventListener('open', () => {
      ws.send(
        JSON.stringify({
          v: 1,
          type: 'hello',
          agent_version: '0.0.1-e2e',
          platform: 'linux',
          hostname: 'e2e-box',
          capabilities: ['screenshot'],
        }),
      )
    })
    ws.addEventListener('message', (ev) => {
      const frame = JSON.parse(String(ev.data))
      if (frame.type !== 'request') return
      ws.send(
        JSON.stringify({
          v: 1,
          type: 'result',
          corr_id: frame.corr_id,
          ok: true,
          payload: { image_base64: 'aGVsbG8=', mime: 'image/png', received: frame.args },
        }),
      )
    })
  }, enrollToken)
  await expect(row.getByTestId('agent-dot-online')).toBeVisible({ timeout: 10_000 })

  const shoot = async (args: Record<string, unknown>) => {
    const res = await request.post('/mcp', {
      headers: { Authorization: `Bearer ${mcpToken}` },
      data: {
        jsonrpc: '2.0',
        id: 1,
        method: 'tools/call',
        params: { name: 'remote_agent_screenshot', arguments: { device_id: deviceId, ...args } },
      },
    })
    expect(res.ok(), `remote_agent_screenshot failed: ${await res.text()}`).toBeTruthy()
    const body = (await res.json()) as {
      result?: { content: { type: string; data?: string; mimeType?: string; text?: string }[] }
      error?: unknown
    }
    expect(body.error, `MCP error: ${JSON.stringify(body.error)}`).toBeFalsy()
    return body.result!.content
  }

  // `monitor` reaches the daemon verbatim, and the image comes back as an
  // MCP image content block ahead of the text block.
  let content = await shoot({ monitor: 1 })
  expect(content[0]).toMatchObject({ type: 'image', data: 'aGVsbG8=', mimeType: 'image/png' })
  let toolResult = JSON.parse(content[1].text ?? '') as {
    ok: boolean
    result: { received: Record<string, unknown> }
  }
  expect(toolResult.ok).toBe(true)
  expect(toolResult.result.received).toEqual({ monitor: 1 })

  // Legacy `display` is aliased to `monitor` before it hits the wire.
  content = await shoot({ display: 2 })
  toolResult = JSON.parse(content[1].text ?? '') as {
    ok: boolean
    result: { received: Record<string, unknown> }
  }
  expect(toolResult.result.received).toEqual({ monitor: 2 })

  // Cleanup so this spec leaves no row behind for others.
  await row.locator('.list-view-menu').click()
  await page.getByRole('menuitem', { name: 'Delete' }).click()
  await page.getByTestId('confirm-dialog-confirm').click()
  await expect(page.getByTestId('agents-empty')).toBeVisible()
})

test('remote_agent window targets: old agents refused, window capture + window-relative mouse forwarded', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const authHeader = { Authorization: `Bearer ${token}` }
  await loadApp(page, token)

  await page.getByTestId('rail-agents').click()
  await expect(page.getByTestId('agents-view')).toBeVisible()
  const enrollToken = await enrollViaUi(page, 'Window Box')
  const row = page.locator('.list-view-row', { hasText: 'Window Box' })
  await expect(row).toBeVisible()

  const devicesRes = await request.get('/api/devices', { headers: authHeader })
  const devices = (await devicesRes.json()) as { devices: { id: string; name: string }[] }
  const deviceId = devices.devices.find((d) => d.name === 'Window Box')?.id
  expect(deviceId).toBeTruthy()

  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-agents-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-agents-windows', path: folderPath },
  })
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'window bridge', folder_id: folder.id },
  })
  const session = (await sessionRes.json()) as { id: string }
  const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
    headers: authHeader,
    data: { text: 'go', model: 'mock:happy-path' },
  })
  expect(sendRes.ok(), `send message failed: ${await sendRes.text()}`).toBeTruthy()
  const mcpCfgPath = path.join(
    process.env.PECKBOARD_E2E_DATA_DIR!,
    'worker-mcp',
    `${session.id}.json`,
  )
  const mcpCfg = JSON.parse(readFileSync(mcpCfgPath, 'utf8')) as {
    mcpServers: { peckboard: { headers: { Authorization: string } } }
  }
  const mcpToken = mcpCfg.mcpServers.peckboard.headers.Authorization.replace(/^Bearer /, '')

  // Daemon stand-in. `features` decides whether it advertises window-target
  // support; every request's args are recorded, and a window-targeted
  // request gets a `window` block back like the real agent's.
  const connectAgent = (features: string[]) =>
    page.evaluate(
      ({ tok, features }) => {
        const w = window as unknown as { __agentWs?: WebSocket; __agentReqs: unknown[] }
        w.__agentWs?.close()
        w.__agentReqs = []
        const proto = location.protocol === 'https:' ? 'wss' : 'ws'
        const ws = new WebSocket(`${proto}://${location.host}/ws/agent`, [
          'peckboard-agent',
          `token.${tok}`,
        ])
        w.__agentWs = ws
        ws.addEventListener('open', () => {
          ws.send(
            JSON.stringify({
              v: 1,
              type: 'hello',
              agent_version: '0.0.1-e2e',
              platform: 'linux',
              hostname: 'e2e-box',
              capabilities: ['screenshot', 'mouse'],
              features,
            }),
          )
        })
        ws.addEventListener('message', (ev) => {
          const frame = JSON.parse(String(ev.data))
          if (frame.type !== 'request') return
          w.__agentReqs.push({ capability: frame.capability, args: frame.args })
          const targeted = frame.args.window_id !== undefined || frame.args.app !== undefined
          const payload: Record<string, unknown> = { received: frame.args }
          if (frame.capability === 'screenshot') {
            payload.image_base64 = 'aGVsbG8='
            payload.mime = 'image/png'
          }
          if (targeted) {
            payload.window = {
              window_id: 5,
              app_name: 'firefox',
              title: 'Docs',
              x: 100,
              y: 50,
              width: 800,
              height: 600,
              scale: 1,
            }
          }
          ws.send(
            JSON.stringify({ v: 1, type: 'result', corr_id: frame.corr_id, ok: true, payload }),
          )
        })
      },
      { tok: enrollToken, features },
    )
  const agentRequests = () =>
    page.evaluate(
      () => (window as unknown as { __agentReqs: { capability: string }[] }).__agentReqs,
    )

  const call = async (tool: string, args: Record<string, unknown>) => {
    const res = await request.post('/mcp', {
      headers: { Authorization: `Bearer ${mcpToken}` },
      data: {
        jsonrpc: '2.0',
        id: 1,
        method: 'tools/call',
        params: { name: tool, arguments: { device_id: deviceId, ...args } },
      },
    })
    expect(res.ok()).toBeTruthy()
    return (await res.json()) as {
      result?: { content: { type: string; data?: string; text?: string }[] }
      error?: { message: string }
    }
  }

  // 1. An agent without the feature would silently ignore window targets
  //    (a window-relative click landing at absolute coords), so the server
  //    refuses before anything reaches the daemon.
  await connectAgent([])
  await expect(row.getByTestId('agent-dot-online')).toBeVisible({ timeout: 10_000 })
  for (const [tool, args] of [
    ['remote_agent_screenshot', { app: 'firefox' }],
    ['remote_agent_mouse', { action: 'click', x: 1, y: 1, window_id: 5 }],
  ] as const) {
    const body = await call(tool, args)
    expect(body.error?.message, `${tool} must be refused`).toContain('too old for window targets')
  }
  expect(await agentRequests()).toEqual([])

  // 2. A window-aware agent: capture by app returns the image plus the
  //    window's screen bounds and a coordinate recipe.
  await connectAgent(['window-targets'])
  await expect(row.getByTestId('agent-dot-online')).toBeVisible({ timeout: 10_000 })
  await expect
    .poll(
      async () =>
        (await call('remote_agent_screenshot', { list_windows: true })).error?.message ?? 'ok',
      {
        timeout: 10_000,
      },
    )
    .toBe('ok')

  let body = await call('remote_agent_screenshot', { app: 'firefox' })
  expect(body.error).toBeFalsy()
  expect(body.result!.content[0]).toMatchObject({ type: 'image', data: 'aGVsbG8=' })
  const shot = JSON.parse(body.result!.content[1].text ?? '') as {
    coordinates: string
    result: { received: unknown; window: { x: number; y: number } }
  }
  expect(shot.result.received).toEqual({ app: 'firefox' })
  expect(shot.result.window).toMatchObject({ x: 100, y: 50 })
  expect(shot.coordinates).toContain('window.x + px')

  // 3. Schema-shaped mouse args reach the daemon in its own shape, with
  //    the window target intact for window-relative mapping.
  body = await call('remote_agent_mouse', {
    action: 'drag',
    from_x: 1,
    from_y: 2,
    x: 3,
    y: 4,
    window_id: 5,
  })
  expect(body.error).toBeFalsy()
  const drag = JSON.parse(body.result!.content[0].text ?? '') as { result: { received: unknown } }
  expect(drag.result.received).toEqual({ op: 'drag', from: [1, 2], to: [3, 4], window_id: 5 })

  await row.locator('.list-view-menu').click()
  await page.getByRole('menuitem', { name: 'Delete' }).click()
  await page.getByTestId('confirm-dialog-confirm').click()
  await expect(page.getByTestId('agents-empty')).toBeVisible()
})
