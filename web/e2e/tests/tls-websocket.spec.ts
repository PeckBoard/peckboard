import { test, expect } from '../harness'
import tls from 'node:tls'
import { randomBytes } from 'node:crypto'

/**
 * WebSockets over the HTTPS listener (`serve_https`). The listener hands
 * each TLS stream to hyper itself, so it must serve connections with
 * upgrades enabled or every `wss://` handshake dies after the request.
 */

const HTTPS_PORT = Number(process.env.PECKBOARD_E2E_HTTPS_PORT ?? '4445')

/** Status line of a raw WebSocket upgrade request to `path` over TLS. */
function upgradeStatus(path: string): Promise<string> {
  return new Promise((resolve, reject) => {
    const sock = tls.connect(
      { host: '127.0.0.1', port: HTTPS_PORT, servername: 'localhost', rejectUnauthorized: false },
      () => {
        sock.write(
          `GET ${path} HTTP/1.1\r\n` +
            `Host: localhost:${HTTPS_PORT}\r\n` +
            'Upgrade: websocket\r\n' +
            'Connection: Upgrade\r\n' +
            `Sec-WebSocket-Key: ${randomBytes(16).toString('base64')}\r\n` +
            'Sec-WebSocket-Version: 13\r\n\r\n',
        )
      },
    )
    let buf = ''
    sock.setTimeout(10_000, () => {
      sock.destroy()
      resolve(buf.split('\r\n')[0] || 'timeout')
    })
    sock.on('data', (d) => {
      buf += d.toString('latin1')
      if (buf.includes('\r\n')) {
        sock.destroy()
        resolve(buf.split('\r\n')[0])
      }
    })
    sock.on('error', reject)
    sock.on('close', () => resolve(buf.split('\r\n')[0] || 'closed'))
  })
}

test('the HTTPS listener completes a WebSocket upgrade', async () => {
  expect(await upgradeStatus('/ws')).toMatch(/^HTTP\/1\.1 101/)
})

// A 101 alone is not enough: before the fix the listener sent 101 and then
// dropped the socket, so the app looped open → close → reconnect forever.
test('a browser page served over HTTPS keeps a wss:// socket open', async ({ browser }) => {
  const context = await browser.newContext({ ignoreHTTPSErrors: true })
  const page = await context.newPage()
  await page.goto(`https://localhost:${HTTPS_PORT}/`)
  const outcome = await page.evaluate(
    () =>
      new Promise<string>((resolve) => {
        const ws = new WebSocket(`wss://${location.host}/ws`)
        ws.onopen = () => {
          setTimeout(() => {
            ws.close()
            resolve('stayed open')
          }, 3_000)
        }
        ws.onclose = (e) => resolve(`closed (code ${e.code})`)
        setTimeout(() => resolve('timeout'), 10_000)
      }),
  )
  expect(outcome).toBe('stayed open')
  await context.close()
})
