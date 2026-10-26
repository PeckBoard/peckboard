import { spawn, type ChildProcess } from 'node:child_process'
import { existsSync, mkdtempSync } from 'node:fs'
import { createServer, type AddressInfo } from 'node:net'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { test, expect, type APIRequestContext, type Page } from '../harness'

/**
 * UI e2e for the OpenAI-Compatible provider (`openai-compat`) settings on
 * Settings → Providers & Accounts (`/settings/providers`).
 *
 * Verifies:
 *
 * 1. The provider gets its own section rendering the shared per-plugin
 *    settings form with the declared fields (base URL, API key, servers,
 *    default model, discovery, tools, additional models, headers).
 * 2. A base URL edit round-trips through the PUT endpoint and survives a
 *    reload; the API key is secret and comes back masked.
 * 3. An Additional Model (an HF-style id with a slash) appears in
 *    `/api/models` as `openai-compat:<id>` — live, without a restart.
 * 4. Against `mock-openai-api` (a real OpenAI-compatible server): discovery
 *    lists its models, a chat turn returns the mock reply and renders in
 *    the session view, and a tool call round-trips through the MCP loop.
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

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

function compatForm(page: Page) {
  return page
    .getByTestId('openai-compat-settings-section')
    .getByTestId('plugin-settings-openai-compat')
}

test('OpenAI-Compatible section renders and round-trips base URL + API key', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await loadAppAt(page, token, '/settings/providers')

  const section = page.getByTestId('openai-compat-settings-section')
  await expect(section).toBeVisible({ timeout: 10_000 })
  await expect(section.locator('h3')).toHaveText('OpenAI-Compatible')
  const settings = compatForm(page)
  await expect(settings).toBeVisible({ timeout: 10_000 })
  for (const field of [
    'base_url',
    'api_key',
    'servers',
    'default_model',
    'request_timeout_secs',
    'discover_models',
    'enable_tools',
    'additional_models',
    'additional_headers',
  ]) {
    await expect(settings.locator(`[data-field="${field}"]`)).toBeVisible()
  }
  await expect(settings.locator('[data-field="base_url"] input')).toHaveValue(
    'http://localhost:8000',
  )

  // Point discovery at a dead host so it can't pick up anything local.
  const baseUrlInput = settings.locator('[data-field="base_url"] input')
  await baseUrlInput.fill('http://vllm.test.local:8000/v1')
  await baseUrlInput.blur()
  await expect(settings.locator('.plugin-settings-success')).toBeVisible({ timeout: 5_000 })

  const keyInput = settings.locator('[data-field="api_key"] input')
  await keyInput.fill('sk-e2e-do-not-leak')
  await keyInput.blur()
  await expect(settings.locator('.plugin-settings-success')).toBeVisible({ timeout: 5_000 })

  await page.reload()
  const after = compatForm(page)
  await expect(after.locator('[data-field="base_url"] input')).toHaveValue(
    'http://vllm.test.local:8000/v1',
    { timeout: 10_000 },
  )
  // Secret: never echoed back.
  await expect(after.locator('[data-field="api_key"] input')).toHaveValue('')
  await expect(after.locator('[data-field="api_key"] .plugin-setting-secret-set')).toBeVisible()
  const raw = await request.get('/api/plugins/openai-compat/settings', {
    headers: { Authorization: `Bearer ${token}` },
  })
  expect(raw.ok()).toBeTruthy()
  expect(await raw.text()).not.toContain('sk-e2e-do-not-leak')
})

test('OpenAI-Compatible additional model appears in the model catalog', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  await loadAppAt(page, token, '/settings/providers')

  const settings = compatForm(page)
  await expect(settings).toBeVisible({ timeout: 10_000 })
  const modelsField = settings.locator('[data-field="additional_models"]')
  await modelsField.locator('.plugin-setting-kv-add').click()
  const input = modelsField.locator('.plugin-setting-kv-row input').first()
  await input.fill('Qwen/Qwen3-32B')
  await input.blur()
  await expect(settings.locator('.plugin-settings-success')).toBeVisible({ timeout: 5_000 })

  await expect
    .poll(
      async () => {
        const res = await request.get('/api/models', {
          headers: { Authorization: `Bearer ${token}` },
        })
        const body = (await res.json()) as { models: { id: string; display_name: string }[] }
        return body.models.find((m) => m.id === 'openai-compat:Qwen/Qwen3-32B')?.display_name
      },
      { timeout: 20_000 },
    )
    .toBe('Qwen/Qwen3-32B (OpenAI-Compatible)')
})

// ── Chat path against a real OpenAI-compatible server ─────────────────────────
//
// `mock-openai-api` (web devDependency) is a canned OpenAI-compatible
// server: `/v1/models` lists its models, `mock-gpt-thinking` answers
// "Hello" with a fixed reply, and `gpt-4-mock` answers with a tool call.
// Skipped when the package isn't installed (`npm install` in web/).

const MOCK_CLI = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  '../../node_modules/mock-openai-api/dist/cli.js',
)

type LoggedEvent = { kind: string; data: Record<string, unknown> }

async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = createServer()
    srv.once('error', reject)
    srv.listen(0, '127.0.0.1', () => {
      const { port } = srv.address() as AddressInfo
      srv.close(() => resolve(port))
    })
  })
}

test.describe('OpenAI-Compatible chat against mock-openai-api', () => {
  test.skip(!existsSync(MOCK_CLI), 'mock-openai-api not installed — run npm install in web/')

  let mock: ChildProcess | undefined
  let mockUrl = ''

  test.beforeAll(async () => {
    const port = await freePort()
    mock = spawn(process.execPath, [MOCK_CLI, '-p', String(port), '-H', '127.0.0.1'], {
      stdio: 'ignore',
    })
    mockUrl = `http://127.0.0.1:${port}`
    await expect
      .poll(
        async () => {
          try {
            return (await fetch(`${mockUrl}/health`)).ok
          } catch {
            return false
          }
        },
        { timeout: 15_000 },
      )
      .toBe(true)
  })

  test.afterAll(async ({ request }) => {
    mock?.kill()
    // Point discovery back at the (dead) default so later specs on this
    // server don't probe a port that is gone.
    const token = await authenticate(request)
    await request.put('/api/plugins/openai-compat/settings', {
      headers: { Authorization: `Bearer ${token}` },
      data: { updates: { base_url: 'http://localhost:8000' } },
    })
  })

  /** Point the provider at the mock (with a `/v1` suffix and an API key, as
   *  users paste them), then wait for discovery to list `model`. */
  async function useMock(request: APIRequestContext, token: string, model: string) {
    const auth = { Authorization: `Bearer ${token}` }
    const put = await request.put('/api/plugins/openai-compat/settings', {
      headers: auth,
      data: { updates: { base_url: `${mockUrl}/v1`, api_key: 'mock-key', discover_models: true } },
    })
    expect(put.ok(), `settings PUT failed: ${await put.text()}`).toBeTruthy()
    await expect
      .poll(
        async () => {
          const res = await request.get('/api/models', { headers: auth })
          const body = (await res.json()) as { models: { id: string; display_name: string }[] }
          return body.models.find((m) => m.id === model)?.display_name
        },
        { timeout: 20_000 },
      )
      .toBe(`${model.slice('openai-compat:'.length)} (OpenAI-Compatible)`)
  }

  async function runTurn(
    request: APIRequestContext,
    token: string,
    model: string,
    text: string,
  ): Promise<{ sessionId: string; events: LoggedEvent[] }> {
    const auth = { Authorization: `Bearer ${token}` }
    const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-oc-'))
    const folderRes = await request.post('/api/folders', {
      headers: auth,
      data: { name: 'e2e-openai-compat', path: folderPath },
    })
    expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
    const folder = (await folderRes.json()) as { id: string }
    const sessionRes = await request.post('/api/sessions', {
      headers: auth,
      data: { name: `openai-compat ${model}`, folder_id: folder.id },
    })
    expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
    const session = (await sessionRes.json()) as { id: string }
    const send = await request.post(`/api/sessions/${session.id}/message`, {
      headers: auth,
      data: { text, model },
    })
    expect(send.ok(), `send failed: ${await send.text()}`).toBeTruthy()

    let events: LoggedEvent[] = []
    await expect
      .poll(
        async () => {
          const res = await request.get(`/api/sessions/${session.id}/events?after_seq=0`, {
            headers: auth,
          })
          events = (await res.json()) as LoggedEvent[]
          return events.some((e) => e.kind === 'agent-end')
        },
        { timeout: 30_000 },
      )
      .toBe(true)
    return { sessionId: session.id, events }
  }

  test('discovered model completes a chat turn and renders the reply', async ({
    request,
    page,
  }) => {
    const token = await authenticate(request)
    const model = 'openai-compat:mock-gpt-thinking'
    await useMock(request, token, model)

    const { sessionId, events } = await runTurn(request, token, model, 'Hello')
    const kinds = events.map((e) => e.kind).filter((k) => k !== 'user')
    expect(kinds).toEqual(['agent-start', 'agent-text', 'agent-usage', 'agent-end'])
    const reply = 'Hello! How can I help you today? 😊'
    expect(events.find((e) => e.kind === 'agent-text')?.data.text).toBe(reply)
    expect(events.find((e) => e.kind === 'agent-usage')?.data.model).toBe(model)
    expect(events.find((e) => e.kind === 'agent-end')?.data.status).toBe('complete')

    // The reply renders in the session's chat view.
    await loadAppAt(page, token, `/sessions/${sessionId}`)
    await expect(page.getByText(reply)).toBeVisible({ timeout: 10_000 })
  })

  test('tool calls round-trip through the MCP tool loop', async ({ request }) => {
    const token = await authenticate(request)
    const model = 'openai-compat:gpt-4-mock'
    await useMock(request, token, model)

    const { events } = await runTurn(request, token, model, 'What time is it now?')
    const starts = events.filter((e) => e.kind === 'agent-tool-start')
    const ends = events.filter((e) => e.kind === 'agent-tool-end')
    expect(starts.length).toBeGreaterThan(0)
    expect(starts[0].data.name).toBe('get_time')
    expect(String(starts[0].data.toolUseId)).toMatch(/^openai-compat-tool-\d+$/)
    // Every call gets its result (here: an error — get_time isn't a
    // Peckboard tool) fed back before the next round.
    expect(ends.map((e) => e.data.toolUseId)).toEqual(starts.map((e) => e.data.toolUseId))
    expect(String(ends[0].data.error)).toContain('get_time')
    // The mock answers every request with the same tool call (it keys on the
    // last user message), so the turn ends at the per-turn round cap rather
    // than hanging.
    const end = events.find((e) => e.kind === 'agent-end')
    expect(end?.data.status).toBe('crashed')
    expect(end?.data.reason).toBe('model kept calling tools past the per-turn cap')
  })
})
