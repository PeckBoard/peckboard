import { test, expect, type APIRequestContext, type Page } from '../harness'
import { spawn, type ChildProcess } from 'node:child_process'
import { existsSync, mkdirSync, mkdtempSync } from 'node:fs'
import net from 'node:net'
import dgram from 'node:dgram'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Pairing v2, end to end with no mocks in the protocol path: a real local
 * relay (`peckboard-relay --dev-self-signed`), the real box, and the real
 * `peckboard-connect` as the device.
 *
 * - a one-time link enrolls a device: the row flips to Enrolled, the
 *   device reaches the box through the tunnel on its own key;
 * - the same link on a second device is refused ("already used") and the
 *   box row warns about it;
 * - an expired link is refused, "New link" issues a working one;
 * - revoking an enrolled device drops its connection.
 *
 * The box pins the relay's throwaway cert through the hidden
 * PECKBOARD_DEV_RELAY_CERT knob set in playwright.config.ts, which also
 * points PECKBOARD_E2E_TOOLS_DIR at the two binaries.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const TOOLS = process.env.PECKBOARD_E2E_TOOLS_DIR ?? ''
const RELAY_BIN = path.join(TOOLS, 'peckboard-relay')
const CONNECT_BIN = path.join(TOOLS, 'peckboard-connect')
const RELAY_STATE = process.env.PECKBOARD_E2E_RELAY_STATE_DIR ?? ''
const RELAY_CERT = path.join(RELAY_STATE, 'dev-cert.der')
const DEFAULT_RELAY = 'relay.peckboard.com'

test.describe.configure({ mode: 'serial' })
test.setTimeout(180_000)

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

async function openRemoteAccess(page: Page, token: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto('/')
  await expect(page.locator('.rail-brand')).toBeVisible({ timeout: 10_000 })
  await page.locator('.rail-avatar').click()
  await page.locator('.user-menu-dropdown').getByRole('menuitem', { name: 'Settings' }).click()
  const settings = page.getByTestId('settings-page')
  await settings.getByTestId('settings-nav-remote-access').click()
  const section = settings.getByTestId('remote-access-section')
  await expect(section).toBeVisible()
  return section
}

function freeTcpPort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = net.createServer()
    srv.listen(0, '127.0.0.1', () => {
      const { port } = srv.address() as net.AddressInfo
      srv.close(() => resolve(port))
    })
    srv.on('error', reject)
  })
}

function freeUdpPort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const sock = dgram.createSocket('udp4')
    sock.bind(0, '127.0.0.1', () => {
      const { port } = sock.address()
      sock.close(() => resolve(port))
    })
    sock.on('error', reject)
  })
}

function tcpOpen(port: number): Promise<boolean> {
  return new Promise((resolve) => {
    const s = net.connect(port, '127.0.0.1')
    s.once('connect', () => {
      s.destroy()
      resolve(true)
    })
    s.once('error', () => resolve(false))
  })
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))

/** A child process whose combined output can be awaited for a pattern. */
class Proc {
  out = ''
  exited: Promise<number | null>
  constructor(public child: ChildProcess) {
    child.stdout?.on('data', (d: Buffer) => (this.out += d.toString()))
    child.stderr?.on('data', (d: Buffer) => (this.out += d.toString()))
    this.exited = new Promise((resolve) => child.on('exit', (code) => resolve(code)))
  }
  async waitFor(re: RegExp, ms: number): Promise<RegExpMatchArray> {
    const deadline = Date.now() + ms
    while (Date.now() < deadline) {
      const m = this.out.match(re)
      if (m) return m
      await sleep(100)
    }
    throw new Error(`timed out waiting for ${re}; output so far:\n${this.out}`)
  }
  async stop() {
    if (this.child.exitCode === null) this.child.kill('SIGINT')
    await Promise.race([this.exited, sleep(5000)])
    if (this.child.exitCode === null) this.child.kill('SIGKILL')
  }
}

/** `peckboard-connect` with `link` on stdin and its own credential file. */
function connect(link: string, credDir: string, name: string): Proc {
  const child = spawn(
    CONNECT_BIN,
    [
      '--credential',
      path.join(credDir, `${name}.cred`),
      '--relay-cert',
      RELAY_CERT,
      '--listen',
      '127.0.0.1:0',
      '-',
    ],
    { stdio: ['pipe', 'pipe', 'pipe'], env: { ...process.env, RUST_LOG: 'info' } },
  )
  child.stdin?.end(`${link}\n`)
  return new Proc(child)
}

interface StatusView {
  state: string
  error: string | null
  local_port: number | null
}

/** Until the box's loop for `name` is registered at the relay (a UDP port
 *  is bound); a loop stuck in error fails with the box's own message. */
async function waitBoxRegistered(
  request: APIRequestContext,
  auth: Record<string, string>,
  name: string,
) {
  try {
    await expect
      .poll(
        async () => {
          const o = (await (await request.get('/api/remote-access', { headers: auth })).json()) as {
            relay_host: string
            devices: { name: string; status: StatusView }[]
          }
          const s = o.devices.find((d) => d.name === name)?.status
          if (!s) return 'no such device'
          return s.local_port !== null
            ? 'registered'
            : `${s.state}: ${s.error ?? ''} (relay_host=${o.relay_host}, spec relay=${relayHost})`
        },
        { timeout: 30_000, message: `box loop for ${name} never registered at the relay` },
      )
      .toBe('registered')
  } catch (e) {
    console.log(`relay output so far:\n${relay?.out ?? '(no relay)'}`)
    throw e
  }
}

let relay: Proc | null = null
let relayHost = ''
let credDir = ''
const createdDevices: string[] = []

test.beforeAll(async ({ request }) => {
  expect(
    existsSync(RELAY_BIN) && existsSync(CONNECT_BIN),
    `relay/connect binaries missing under ${TOOLS}; run scripts/build-local-release.sh`,
  ).toBeTruthy()
  expect(RELAY_STATE, 'PECKBOARD_E2E_RELAY_STATE_DIR unset').toBeTruthy()
  mkdirSync(RELAY_STATE, { recursive: true })
  credDir = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-cred-'))
  const tcp = await freeTcpPort()
  const udp = await freeUdpPort()
  relay = new Proc(
    spawn(
      RELAY_BIN,
      [
        '--dev-self-signed',
        '--domain',
        'localhost',
        '--listen',
        `127.0.0.1:${tcp}`,
        '--stun-listen',
        `127.0.0.1:${udp}`,
        '--state-dir',
        RELAY_STATE,
      ],
      { stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, RUST_LOG: 'info' } },
    ),
  )
  await relay.waitFor(/relay listening/, 15_000)
  await expect.poll(() => existsSync(RELAY_CERT) && tcpOpen(tcp), { timeout: 15_000 }).toBe(true)
  relayHost = `localhost:${tcp}`

  const token = await authenticate(request)
  const res = await request.put('/api/remote-access', {
    headers: { Authorization: `Bearer ${token}` },
    data: { enabled: true, relay_host: relayHost },
  })
  expect(res.ok(), await res.text()).toBeTruthy()
})

test.afterAll(async ({ request }) => {
  // Back to the defaults the other remote-access specs assume.
  const token = await authenticate(request).catch(() => null)
  if (token) {
    const auth = { Authorization: `Bearer ${token}` }
    for (const id of createdDevices) {
      await request.delete(`/api/remote-access/devices/${id}`, { headers: auth })
    }
    await request.put('/api/remote-access', {
      headers: auth,
      data: { enabled: false, relay_host: DEFAULT_RELAY },
    })
  }
  await relay?.stop()
})

test('a one-time link enrolls a device; the same link is refused for a second one; revoke drops it', async ({
  request,
  page,
}) => {
  const token = await authenticate(request)
  const auth = { Authorization: `Bearer ${token}` }
  const section = await openRemoteAccess(page, token)

  // ── Pair in the UI: the https link, the fingerprint, the expiry ────
  await section.getByTestId('remote-pair-device').click()
  await page.getByTestId('remote-pair-name').fill('e2e-phone')
  await page.getByTestId('remote-pair-submit').click()
  const linkModal = page.getByTestId('remote-pair-link-modal')
  const link = await linkModal.getByTestId('remote-pair-link').inputValue()
  expect(link).toMatch(
    /^https:\/\/peckboard\.com\/pair#v=2&s=[A-Za-z0-9_-]{43}&k=[A-Za-z0-9_-]{43}&e=\d+&r=localhost:\d+$/,
  )
  const fingerprint = await linkModal.getByTestId('remote-pair-fingerprint').innerText()
  expect(fingerprint).toMatch(/^[A-Z2-7]{4}-[A-Z2-7]{4}-[A-Z2-7]{4}-[A-Z2-7]{4}$/)
  await expect(section.getByTestId('remote-box-fingerprint')).toContainText(fingerprint)
  await linkModal.getByTestId('remote-pair-done').click()
  const overview = (await (await request.get('/api/remote-access', { headers: auth })).json()) as {
    devices: { id: string; name: string }[]
  }
  const device = overview.devices.find((d) => d.name === 'e2e-phone')!
  createdDevices.push(device.id)
  const badge = section.getByTestId('remote-device-enrollment-e2e-phone')
  await expect(badge).toContainText('Waiting for first connection')
  await waitBoxRegistered(request, auth, 'e2e-phone')

  // ── The device enrolls, then runs on its own key ───────────────────
  const first = connect(link, credDir, 'first')
  try {
    await first.waitFor(/Paired with your Peckboard \(box ([A-Z2-7-]+)\)/, 90_000)
    expect(first.out).toContain(`box ${fingerprint}`)
    const [, url] = await first.waitFor(
      /Peckboard available at (http:\/\/127\.0\.0\.1:\d+)/,
      60_000,
    )
    await expect(badge).toContainText('Enrolled', { timeout: 20_000 })
    await expect(badge).toContainText('peckboard-connect')
    await expect(badge).toContainText('from 127.0.0.1')
    await expect(section.getByTestId('remote-device-state-e2e-phone')).toHaveText('connected', {
      timeout: 20_000,
    })
    // Data flows through the tunnel authenticated by the device key.
    const through = await request.get(`${url}/api/auth/login`, { failOnStatusCode: false })
    expect([200, 405]).toContain(through.status())
    const detail = async () => {
      const o = (await (await request.get('/api/remote-access', { headers: auth })).json()) as {
        devices: { id: string; enrollment: string; enrolled_from: string | null }[]
      }
      return o.devices.find((d) => d.id === device.id)!
    }
    // Activation is the first handshake on the device's own key.
    await expect.poll(async () => (await detail()).enrollment, { timeout: 20_000 }).toBe('enrolled')
    expect((await detail()).enrolled_from).toMatch(/^127\.0\.0\.1:\d+$/)

    // ── The same link on another device: refused, and the box says so ─
    const second = connect(link, credDir, 'second')
    expect(await second.exited).toBe(1)
    expect(second.out).toContain('already used by another device')
    const reuse = section.getByTestId('remote-device-reuse-e2e-phone')
    await expect(reuse).toContainText('Link already used by another device', { timeout: 20_000 })
    await expect(reuse).toContainText('from 127.0.0.1')
    // The first device is unaffected.
    expect(first.child.exitCode).toBeNull()

    // ── Revoke: the enrolled device loses its connection ──────────────
    const row2 = section.locator('.list-view-row').filter({ hasText: 'e2e-phone' })
    await row2.locator('.list-view-menu').click()
    await page.getByRole('menuitem', { name: 'Revoke' }).click()
    await page
      .getByTestId('remote-device-revoke-confirm')
      .getByTestId('confirm-dialog-confirm')
      .click()
    await expect(section.getByTestId('remote-device-row-e2e-phone')).toHaveCount(0)
    await first.waitFor(/Connection lost/, 30_000)
    createdDevices.splice(createdDevices.indexOf(device.id), 1)
  } finally {
    await first.stop()
  }
})

test('an expired link is refused; New link issues a working one', async ({ request, page }) => {
  const token = await authenticate(request)
  const auth = { Authorization: `Bearer ${token}` }
  // A link that lives two seconds (per-link TTL, dev knob only).
  const created = await request.post('/api/remote-access/devices', {
    headers: auth,
    data: { name: 'e2e-late', ttl_secs: 2 },
  })
  expect(created.status(), await created.text()).toBe(201)
  const pairing = (await created.json()) as { device: { id: string }; pairing_link: string }
  createdDevices.push(pairing.device.id)
  const section = await openRemoteAccess(page, token)
  const badge = section.getByTestId('remote-device-enrollment-e2e-late')
  await expect(badge).toHaveText('Link expired', { timeout: 20_000 })
  await waitBoxRegistered(request, auth, 'e2e-late')

  const late = connect(pairing.pairing_link, credDir, 'late')
  expect(await late.exited).toBe(1)
  expect(late.out).toContain('This pairing link expired')

  // ── New link from the row menu: a fresh hour, shown once ───────────
  const row = section.locator('.list-view-row').filter({ hasText: 'e2e-late' })
  await row.locator('.list-view-menu').click()
  await page.getByRole('menuitem', { name: 'New link' }).click()
  const linkModal = page.getByTestId('remote-pair-link-modal')
  await expect(linkModal.getByTestId('remote-pair-expires')).toContainText('expires in 60 min')
  const fresh = await linkModal.getByTestId('remote-pair-link').inputValue()
  expect(fresh).not.toBe(pairing.pairing_link)
  expect(fresh).toMatch(/^https:\/\/peckboard\.com\/pair#v=2&s=/)
  await linkModal.getByTestId('remote-pair-done').click()
  await expect(badge).toContainText('Waiting for first connection')
  await waitBoxRegistered(request, auth, 'e2e-late')

  const again = connect(fresh, credDir, 'again')
  try {
    await again.waitFor(/Paired with your Peckboard/, 90_000)
    await expect(badge).toContainText('Enrolled', { timeout: 20_000 })
  } finally {
    await again.stop()
  }
})
