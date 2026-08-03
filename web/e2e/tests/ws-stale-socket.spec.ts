import { test, expect } from '@playwright/test'

/**
 * Regression tests for the stale-socket guard in `src/store/ws.ts`.
 *
 * `disconnect()` closes the current socket and `connect()` synchronously
 * installs a new one, but the OLD socket's `close` event only lands a tick
 * later. Without a guard that handler nulled the module-level `socket`
 * pointer (by then the NEW socket), flipped the UI to disconnected and
 * scheduled a reconnect that opened a THIRD socket while the second was
 * still open and unreachable — so every frame dispatched twice. React's
 * StrictMode double-mount runs exactly that connect → disconnect → connect
 * sequence on every dev mount.
 *
 * There is no vitest in `web/`, so these run in the Playwright suite against
 * a fake `WebSocket` global — no page, no server (same approach as
 * `derive-agent-status.spec.ts`).
 */

type Listener = (ev: unknown) => void

class FakeWebSocket {
  static readonly CONNECTING = 0
  static readonly OPEN = 1
  static readonly CLOSING = 2
  static readonly CLOSED = 3
  static instances: FakeWebSocket[] = []

  readyState: number = FakeWebSocket.CONNECTING
  readonly sent: string[] = []
  closeCalls = 0
  private readonly listeners = new Map<string, Listener[]>()

  constructor(readonly url: string) {
    FakeWebSocket.instances.push(this)
  }

  addEventListener(type: string, fn: Listener) {
    const list = this.listeners.get(type) ?? []
    list.push(fn)
    this.listeners.set(type, list)
  }

  send(data: string) {
    this.sent.push(data)
  }

  /** Real sockets do not fire `close` synchronously from `close()` — the
   *  event lands later, which is what makes the stale handler reachable. */
  close() {
    this.closeCalls += 1
    this.readyState = FakeWebSocket.CLOSING
  }

  private emit(type: string, ev: unknown) {
    for (const fn of this.listeners.get(type) ?? []) fn(ev)
  }

  /** The server accepted the handshake. */
  fireOpen() {
    this.readyState = FakeWebSocket.OPEN
    this.emit('open', {})
  }

  /** The delayed close event for a socket `close()` already ran on. */
  fireClose() {
    this.readyState = FakeWebSocket.CLOSED
    this.emit('close', {})
  }

  frame(msg: Record<string, unknown>) {
    this.emit('message', { data: JSON.stringify(msg) })
  }
}

const dispatched: { type: string }[] = []

function makeStorage() {
  const map = new Map<string, string>()
  return {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => {
      map.set(k, v)
    },
    removeItem: (k: string) => {
      map.delete(k)
    },
  }
}

function stubBrowserGlobals() {
  const g = globalThis as unknown as Record<string, unknown>
  g.localStorage ??= makeStorage()
  g.sessionStorage ??= makeStorage()
  g.location ??= { protocol: 'http:', host: 'ws-store.test' }
  g.window ??= {
    dispatchEvent: (ev: { type: string }) => {
      dispatched.push(ev)
      return true
    },
  }
  g.WebSocket = FakeWebSocket
}

let useWsStore: (typeof import('../../src/store/ws'))['useWsStore']
let useUiStore: (typeof import('../../src/store/ui'))['useUiStore']

test.beforeAll(async () => {
  stubBrowserGlobals()
  ;({ useWsStore } = await import('../../src/store/ws'))
  ;({ useUiStore } = await import('../../src/store/ui'))
})

test.beforeEach(() => {
  // Drops whatever socket a previous test left current and cancels any timer.
  useWsStore.getState().disconnect()
  FakeWebSocket.instances.length = 0
  dispatched.length = 0
})

/** connect → disconnect → connect, leaving the first socket's close pending. */
function remountLikeStrictMode() {
  const ws = useWsStore.getState()
  ws.connect()
  const first = FakeWebSocket.instances[0]
  first.fireOpen()
  first.frame({ type: 'auth_ok', user_id: 'e2e' })

  ws.disconnect() // closes `first`; its close event has not fired yet
  ws.connect()
  expect(FakeWebSocket.instances).toHaveLength(2)
  const second = FakeWebSocket.instances[1]
  second.fireOpen()
  second.frame({ type: 'auth_ok', user_id: 'e2e' })
  return { first, second }
}

test('a superseded socket close leaves exactly one live socket and no reconnect', async () => {
  const { first, second } = remountLikeStrictMode()

  first.fireClose() // the late close event lands

  // The first backoff is 1s ±25%, so 2s is long enough to catch a reconnect
  // that the stale handler should never have scheduled.
  await new Promise((resolve) => setTimeout(resolve, 2000))

  expect(FakeWebSocket.instances).toHaveLength(2)
  expect(second.readyState).toBe(FakeWebSocket.OPEN)
  expect(second.closeCalls).toBe(0)
  // No spurious "disconnected" flap: the banner state stays connected.
  expect(useUiStore.getState().connected).toBe(true)

  // The live socket is still the store's current one, so sends reach it.
  const before = second.sent.length
  useWsStore.getState().resume('sess-1', 0)
  expect(second.sent).toHaveLength(before + 1)
})

test('a single card-update frame dispatches exactly once after a remount', () => {
  const { first, second } = remountLikeStrictMode()
  first.fireClose()

  second.frame({ type: 'card-update', card_id: 'c1' })
  expect(dispatched.filter((e) => e.type === 'peckboard:card-update')).toHaveLength(1)

  // Even if a superseded socket delivered the same frame, it is not ours.
  first.frame({ type: 'card-update', card_id: 'c1' })
  expect(dispatched.filter((e) => e.type === 'peckboard:card-update')).toHaveLength(1)
})

test('the current socket still reconnects when the server drops it', async () => {
  const ws = useWsStore.getState()
  ws.connect()
  const only = FakeWebSocket.instances[0]
  only.fireOpen()
  only.frame({ type: 'auth_ok', user_id: 'e2e' })

  only.fireClose() // unexpected drop, no `disconnect()` first

  expect(useUiStore.getState().connected).toBe(false)
  await expect
    .poll(() => FakeWebSocket.instances.length, { timeout: 10_000 })
    .toBeGreaterThanOrEqual(2)
})
