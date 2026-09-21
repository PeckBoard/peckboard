// Docs screenshot capture for plugin pages — run against a live Peckboard
// instance that has the registry plugins installed and demo data seeded.
//
//   PB_URL=http://127.0.0.1:24817 PB_USER=docs-user PB_PASS=docs-pass-1234 \
//     node scripts/docs-plugin-shots.mjs
//
// Unlike web/e2e/screenshots/screenshots.spec.ts (which boots its own
// server and stubs plugin surfaces), this script captures the REAL plugin
// pages served by real wasm plugins, so it needs a prepared instance:
// every plugin installed + approved, a folder with a git repo, a project
// with cards, a couple of mock sessions, SSH fleet hosts, an orchestrator,
// and a graphify-enabled repo with a built graph. Writes PNGs into
// docs/assets/screenshots/plugins/.
import { chromium } from '@playwright/test'
import { mkdirSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const BASE = process.env.PB_URL ?? 'http://127.0.0.1:24817'
const USER = process.env.PB_USER ?? 'docs-user'
const PASS = process.env.PB_PASS ?? 'docs-pass-1234'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const OUT = path.resolve(HERE, '..', '..', 'docs', 'assets', 'screenshots', 'plugins')
mkdirSync(OUT, { recursive: true })

const res = await fetch(`${BASE}/api/auth/login`, {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ username: USER, password: PASS }),
})
if (!res.ok) throw new Error(`login failed: ${await res.text()}`)
const { token } = await res.json()
const authHeader = { Authorization: `Bearer ${token}` }

async function api(pathname, init = {}) {
  const r = await fetch(`${BASE}${pathname}`, {
    ...init,
    headers: { 'Content-Type': 'application/json', ...authHeader, ...(init.headers ?? {}) },
  })
  if (!r.ok) throw new Error(`${pathname}: ${r.status} ${await r.text()}`)
  return r.json()
}

// Ids of the seeded fixtures, resolved by name so re-seeding is fine.
const folders = await api('/api/folders')
const folderList = Array.isArray(folders) ? folders : folders.folders
const acme = folderList.find((f) => f.name === 'Acme Shop')
const pb = folderList.find((f) => f.name === 'Peckboard')
const projects = await api('/api/projects')
const projectList = Array.isArray(projects) ? projects : projects.projects
const storefront = projectList.find((p) => p.name === 'Storefront v2')
const sessions = await api(`/api/sessions?folder_id=${acme.id}`)
const sessionList = Array.isArray(sessions) ? sessions : (sessions.items ?? sessions.sessions)
const chat = sessionList.find((s) => s.name === 'Storefront chat')

const browser = await chromium.launch()
const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } })
await ctx.addInitScript(([t]) => window.localStorage.setItem('peckboard_token', t), [token])
const page = await ctx.newPage()
page.setDefaultTimeout(8000)

// ONLY=name1,name2 re-runs a subset.
const only = process.env.ONLY ? new Set(process.env.ONLY.split(',')) : null

const results = []
async function shot(name, fn) {
  if (only && !only.has(name)) return
  try {
    await fn()
    await page.screenshot({ path: path.join(OUT, `${name}.png`) })
    results.push(`ok   ${name}`)
  } catch (e) {
    results.push(`FAIL ${name}: ${String(e).slice(0, 200)}`)
  }
}

async function goto(pathname, settleMs = 2500) {
  await page.goto(`${BASE}${pathname}`)
  await page.waitForLoadState('load')
  await page.waitForTimeout(settleMs)
}

// ---- global sidebar plugin pages ----------------------------------------
await shot('ssh-fleet', () => goto('/plugin-page/ssh-fleet/ssh-fleet'))
await shot('app-manager', () => goto('/plugin-page/app-manager/app-manager', 6000))
await shot('app-manager-request', () =>
  goto('/plugin-page/app-manager/app-manager?install=python3,pip,graphifyy&from=graphify', 6000),
)
await shot('ui-gauge', () => goto('/plugin-page/ui-gauge/ui-gauge'))
await shot('orchestrators', () => goto('/plugin-page/session-control/orchestrators'))
await shot('chicken-coop', () => goto('/plugin-page/chicken-coop/chicken-coop', 5000))

// ---- folder-scoped pages -------------------------------------------------
await shot('graphify', async () => {
  // Launch via the Folders row's Graphify button so the page carries scope,
  // then open the repo's graph inside the plugin iframe.
  await goto('/folders', 1500)
  const row = page.locator('.list-view-row, [class*=folder]', { hasText: 'Peckboard' }).first()
  await row.getByText('Graphify', { exact: true }).last().click()
  await page.waitForTimeout(4000)
  const frame = page.frameLocator('iframe').first()
  await frame.getByText('peckboard', { exact: false }).first().click()
  await page.waitForTimeout(9000)
})
await shot('project-planner', async () => {
  // The planner is offered per repo row (right-click / row menu) in the
  // folder's repo browser.
  await goto(`/folders/${acme.id}/repos`, 2000)
  const row = page.locator('.list-view-row', { hasText: 'acme-shop' }).first()
  await row.click({ button: 'right' })
  await page.waitForTimeout(500)
  await page.getByText('Project Planner', { exact: true }).first().click()
  await page.waitForTimeout(5000)
})
await shot('diff-viewer', async () => {
  // Session chat 3-dot menu → the plugin's menu entry, then open a file.
  await goto(`/sessions/${chat.id}`, 2000)
  await page.getByTestId('chat-toolbar-menu').click()
  await page.getByTestId('chat-menu-plugin-diff-viewer').click()
  await page.waitForTimeout(3000)
  await page.frameLocator('iframe').first().getByText('src/cart.py').first().click()
  await page.waitForTimeout(2500)
})

// ---- chat surfaces -------------------------------------------------------
await shot('pre-hatcher', async () => {
  // The pre-hatcher's model + prompt settings live on Settings → Chat & Models.
  await goto('/settings/chat', 2000)
  const anchor = page.locator('[data-settings-anchor="prehatch"]')
  await anchor.scrollIntoViewIfNeeded()
  await page.waitForTimeout(800)
})
await shot('registry', async () => {
  await goto('/settings/registry', 3000)
})
await shot('bridge-settings', async () => {
  // A representative MCP-bridge settings form (nginx-manager).
  await goto('/settings/plugins', 2000)
  const row = page.getByText('Nginx Proxy Manager', { exact: false }).first()
  await row.click()
  await page.waitForTimeout(1500)
})
await shot('api-keys', async () => {
  await goto('/', 1500)
  await page.locator('.user-menu').first().click()
  await page.waitForTimeout(400)
  await page.locator('[data-testid="user-menu-plugin-api-api-keys"]').click()
  await page.waitForTimeout(2500)
})
await browser.close()
console.log(results.join('\n'))
const failed = results.filter((r) => r.startsWith('FAIL'))
process.exitCode = failed.length ? 1 : 0
