import { test, expect } from '@playwright/test'
import { appendEventOrdered, nextLastSeq } from '../../src/store/eventOrder'
import { latestTodoSnapshot } from '../../src/types/todo'
import type { Event } from '../../src/types/api'

/**
 * Pure-logic tests for the WS store's event-ordering guard. There is no vitest
 * in `web/`, so these run in the Playwright suite — they touch no page and no
 * server, they just import the module under test.
 *
 * The regression: the server's socket loop can forward a live broadcast event
 * between the client's `subscribe` and `resume` frames, so a resume replay
 * (seq K+1..K+4) lands AFTER the live tail (K+5). A blind push left the OLD
 * event last, and `latestTodoSnapshot` walks from the end — so Kanban todo
 * badges showed a stale snapshot. The blind last-seq write also regressed the
 * persisted watermark to K+4, forcing redundant replay on the next resume.
 */

function ev(seq: number, kind: string, data: Record<string, unknown> = {}): Event {
  return {
    id: `e${seq}`,
    session_id: 's1',
    seq,
    ts: 1_700_000_000_000 + seq * 1000,
    kind,
    data,
  }
}

function todo(seq: number, content: string): Event {
  return ev(seq, 'todo', { todos: [{ content, status: 'pending' }] })
}

const K = 10

test('interleaved resume replay stays seq-ordered and keeps the newest todo', () => {
  // Live tail arrives first (K+5 carries the newest todo snapshot), then the
  // resume replay of K+1..K+4 lands behind it — K+2 is an older todo.
  const incoming: Event[] = [
    todo(K + 5, 'newest'),
    ev(K + 1, 'user'),
    todo(K + 2, 'stale'),
    ev(K + 3, 'agent-start'),
    ev(K + 4, 'assistant'),
  ]

  let events: Event[] = []
  let lastSeq: number | undefined
  for (const e of incoming) {
    if (events.some((x) => x.seq === e.seq)) continue
    events = appendEventOrdered(events, e)
    lastSeq = nextLastSeq(lastSeq, e.seq)
  }

  expect(events.map((e) => e.seq)).toEqual([K + 1, K + 2, K + 3, K + 4, K + 5])
  expect(latestTodoSnapshot(events)).toEqual([{ content: 'newest', status: 'pending' }])
  // Persisted watermark must not regress to the replay's last seq.
  expect(lastSeq).toBe(K + 5)
})

test('full resync replay below the live tail does not reorder or regress', () => {
  let events: Event[] = [ev(K + 5, 'assistant')]
  let lastSeq: number | undefined = K + 5

  // A `resume` from 0 full-replays every event, including ones already held.
  for (let seq = 1; seq <= K + 5; seq++) {
    const e = seq === K + 2 ? todo(seq, 'stale') : ev(seq, 'user')
    if (events.some((x) => x.seq === e.seq)) continue
    events = appendEventOrdered(events, e)
    lastSeq = nextLastSeq(lastSeq, e.seq)
  }

  expect(events.map((e) => e.seq)).toEqual(Array.from({ length: K + 5 }, (_, i) => i + 1))
  expect(events[events.length - 1].seq).toBe(K + 5)
  expect(lastSeq).toBe(K + 5)
})

test('in-order appends take the push path unchanged', () => {
  let events: Event[] = []
  for (let seq = 1; seq <= 4; seq++) events = appendEventOrdered(events, ev(seq, 'user'))
  expect(events.map((e) => e.seq)).toEqual([1, 2, 3, 4])
})

test('nextLastSeq never regresses', () => {
  expect(nextLastSeq(undefined, 7)).toBe(7)
  expect(nextLastSeq(9, 4)).toBe(9)
  expect(nextLastSeq(4, 9)).toBe(9)
})
