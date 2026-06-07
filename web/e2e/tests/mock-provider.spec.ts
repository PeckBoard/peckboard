import { test, expect, type APIRequestContext } from '@playwright/test';

/**
 * End-to-end smoke test for the mock agent provider.
 *
 * Boots peckboard (handled by playwright.config.ts), authenticates via
 * the public register-or-login flow, creates a folder + session, opens a
 * WebSocket, then POSTs a message with `model: "mock:happy-path"` and
 * asserts that the scripted ProviderEvent sequence arrives on the WS in
 * the expected order.
 *
 * This is the canonical example for future e2e specs — copy the auth +
 * folder + session + WS setup, change the model id + assertions.
 */

const E2E_USER = 'e2e-user';
const E2E_PASS = 'e2e-password-1234';

type AuthBundle = {
  token: string;
  authHeader: { Authorization: string };
};

async function authenticate(request: APIRequestContext): Promise<AuthBundle> {
  const status = await request.get('/api/auth/status');
  expect(status.ok()).toBeTruthy();
  const { has_users } = (await status.json()) as { has_users: boolean };

  const credentials = { username: E2E_USER, password: E2E_PASS };
  const endpoint = has_users ? '/api/auth/login' : '/api/auth/register';

  const res = await request.post(endpoint, { data: credentials });
  expect(res.ok(), `auth via ${endpoint} failed: ${await res.text()}`).toBeTruthy();
  const { token } = (await res.json()) as { token: string };

  return { token, authHeader: { Authorization: `Bearer ${token}` } };
}

type WsEvent = { kind: string; data: Record<string, unknown>; seq: number };

/**
 * Open a WS connection, authenticate, subscribe, and collect every event
 * for `sessionId` until `untilKind` is observed (typically `agent-end`).
 *
 * Resolves with the ordered list of `{ kind, data, seq }` for assertions.
 */
async function collectEventsUntil(
  baseURL: string,
  token: string,
  sessionId: string,
  untilKind: string,
  timeoutMs: number,
): Promise<WsEvent[]> {
  const wsUrl = baseURL.replace(/^http/, 'ws') + '/ws';
  const ws = new WebSocket(wsUrl);
  const collected: WsEvent[] = [];

  try {
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error(`WS handshake timed out after ${timeoutMs}ms`)),
        timeoutMs,
      );
      ws.addEventListener('open', () => {
        clearTimeout(timer);
        resolve();
      });
      ws.addEventListener('error', (err) => {
        clearTimeout(timer);
        reject(new Error(`WS error: ${String(err)}`));
      });
    });

    // Auth frame, then wait for auth_ok before subscribing.
    ws.send(JSON.stringify({ type: 'auth', token }));
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error('WS auth_ok not received')),
        timeoutMs,
      );
      const handler = (msg: MessageEvent) => {
        const frame = JSON.parse(String(msg.data));
        if (frame.type === 'auth_ok') {
          clearTimeout(timer);
          ws.removeEventListener('message', handler);
          resolve();
        }
      };
      ws.addEventListener('message', handler);
    });

    ws.send(JSON.stringify({ type: 'subscribe', session_id: sessionId }));

    // Drain events until the terminal kind shows up.
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error(`Did not see '${untilKind}' within ${timeoutMs}ms; got: ${collected.map((e) => e.kind).join(', ')}`)),
        timeoutMs,
      );
      ws.addEventListener('message', (msg) => {
        const frame = JSON.parse(String(msg.data));
        if (frame.type !== 'event' || frame.session_id !== sessionId) return;
        const ev = frame.event as WsEvent;
        collected.push(ev);
        if (ev.kind === untilKind) {
          clearTimeout(timer);
          resolve();
        }
      });
    });
  } finally {
    ws.close();
  }

  return collected;
}

test('mock:happy-path streams the scripted event sequence over WS', async ({ request, baseURL }) => {
  expect(baseURL, 'baseURL configured').toBeTruthy();

  const { token, authHeader } = await authenticate(request);

  // Folder must point at a path that exists on the server's filesystem.
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-mock', path: '/tmp' },
  });
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy();
  const folder = (await folderRes.json()) as { id: string };

  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'mock smoke', folder_id: folder.id },
  });
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy();
  const session = (await sessionRes.json()) as { id: string };

  // Open WS + start collecting BEFORE sending the message so we can't
  // miss the agent-start event.
  const collectorPromise = collectEventsUntil(
    baseURL!,
    token,
    session.id,
    'agent-end',
    15_000,
  );

  // Tiny delay to let the WS subscribe before the agent starts emitting.
  await new Promise((r) => setTimeout(r, 250));

  const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
    headers: authHeader,
    data: { text: 'go', model: 'mock:happy-path' },
  });
  expect(sendRes.ok(), `send message failed: ${await sendRes.text()}`).toBeTruthy();

  const events = await collectorPromise;
  const kinds = events.map((e) => e.kind);

  // The user event lands in the log too, but the order between the user
  // POST persisting and the agent-start firing depends on scheduling, so
  // just assert the agent sub-sequence is present and in order.
  const agentKinds = kinds.filter((k) => k !== 'user');
  expect(agentKinds).toEqual([
    'agent-start',
    'agent-text',
    'agent-tool-start',
    'agent-tool-end',
    'agent-text',
    'agent-end',
  ]);

  // Sanity-check the payloads carry the scripted values.
  const texts = events.filter((e) => e.kind === 'agent-text').map((e) => e.data.text);
  expect(texts).toEqual(['Working on it...', 'Done.']);

  const tool = events.find((e) => e.kind === 'agent-tool-start');
  expect(tool?.data.name).toBe('Bash');

  const end = events.find((e) => e.kind === 'agent-end');
  expect(end?.data.status).toBe('complete');
});
