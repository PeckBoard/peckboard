import { test, expect } from '@playwright/test'
import { deriveAgentStatus } from '../../src/components/chat/events'
import type { Event } from '../../src/types/api'

/**
 * Pure-logic tests for `deriveAgentStatus` (the chat toolbar's status pill).
 * There is no vitest in `web/`, so these run in the Playwright suite — they
 * touch no page and no server, they just import the module under test.
 */

let seq = 0
function ev(kind: string, data: Record<string, unknown> = {}, id?: string): Event {
  seq += 1
  return {
    id: id ?? `e${seq}`,
    session_id: 's1',
    seq,
    ts: 1_700_000_000_000 + seq * 1000,
    kind,
    data,
  }
}

test('deriveAgentStatus: empty feed is idle', () => {
  expect(deriveAgentStatus([])).toBe('idle')
})

test('deriveAgentStatus: a clean agent-end is idle', () => {
  expect(deriveAgentStatus([ev('user'), ev('agent-start'), ev('agent-end', {})])).toBe('idle')
})

test('deriveAgentStatus: a crashed agent-end is crashed, not idle', () => {
  const events = [
    ev('user'),
    ev('agent-start'),
    ev('agent-end', { status: 'crashed', reason: 'mock scenario crash (exit 1)', exitCode: 1 }),
  ]
  expect(deriveAgentStatus(events)).toBe('crashed')
})

test('deriveAgentStatus: crash clears once the next turn is dispatched', () => {
  const events = [
    ev('agent-end', { status: 'crashed' }),
    ev('user', { text: 'try again' }),
    ev('agent-start'),
  ]
  expect(deriveAgentStatus(events)).toBe('working')
})

test('deriveAgentStatus: a user message with no agent-start yet is working', () => {
  expect(deriveAgentStatus([ev('user', { text: 'hi' })])).toBe('working')
})

test('deriveAgentStatus: an unfinished tool call is tool, a finished one is working', () => {
  const start = ev('agent-tool-start', { toolUseId: 'tu-1', name: 'Bash' })
  expect(deriveAgentStatus([ev('agent-start'), start])).toBe('tool')
  expect(
    deriveAgentStatus([ev('agent-start'), start, ev('agent-tool-end', { toolUseId: 'tu-1' })]),
  ).toBe('working')
})

test('deriveAgentStatus: an open question is questioning, a resolved one is not', () => {
  const q = ev('question', { questions: [] }, 'q-1')
  expect(deriveAgentStatus([ev('agent-start'), q])).toBe('questioning')
  expect(
    deriveAgentStatus([ev('agent-start'), q, ev('question-resolved', { questionId: 'q-1' })]),
  ).toBe('working')
})

test('deriveAgentStatus: pre-hatch is working before any agent event', () => {
  expect(deriveAgentStatus([ev('pre-hatch', {})])).toBe('working')
})
