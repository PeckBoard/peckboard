import { test, expect } from '@playwright/test'
import {
  bareModelId,
  contextWindowInfo,
  DEFAULT_CONTEXT_WINDOW,
  LONG_CONTEXT_WINDOW,
} from '../../src/util/cost'
import type { CostTable } from '../../src/types/api'

/**
 * Pure-logic tests for the shared cost/window helpers. No vitest in `web/`,
 * so these run in Playwright — no page, no server.
 *
 * Locks the grok 500K window (and provider-prefix stripping) that used to
 * measure grok sessions against Claude's 200K default.
 */

const table: CostTable = {
  rates: {
    'claude-opus-4-8': {
      input_per_mtok: 15,
      output_per_mtok: 75,
      cache_read_per_mtok: 1.5,
      cache_creation_per_mtok: 18.75,
    },
    'grok-4.5': {
      input_per_mtok: 2,
      output_per_mtok: 6,
      cache_read_per_mtok: 0.3,
      cache_creation_per_mtok: 2,
    },
    'grok-4.6': {
      input_per_mtok: 2,
      output_per_mtok: 6,
      cache_read_per_mtok: 0.5,
      cache_creation_per_mtok: 2,
    },
  },
}

test('bareModelId strips any provider prefix and @account suffix', () => {
  expect(bareModelId('grok:grok-4.5@acc_g')).toBe('grok-4.5')
  expect(bareModelId('claude:opus[1m]@work')).toBe('opus[1m]')
  expect(bareModelId('grok-4.6')).toBe('grok-4.6')
})
test('contextWindowInfo: grok 4.5/4.6 are 500K, known, even with prefix/@account', () => {
  const ids = ['grok-4.5', 'grok:grok-4.5', 'grok:grok-4.5@acc', 'grok-4.6', 'grok:grok-4.6']
  for (const id of ids) {
    expect(contextWindowInfo(id, table)).toEqual({ limit: 500_000, known: true })
  }
})

test('contextWindowInfo: [1m] aliases stay 1M; table members stay 200K', () => {
  expect(contextWindowInfo('claude:opus[1m]', table)).toEqual({
    limit: LONG_CONTEXT_WINDOW,
    known: true,
  })
  expect(contextWindowInfo('claude-opus-4-8', table)).toEqual({
    limit: DEFAULT_CONTEXT_WINDOW,
    known: true,
  })
})

test('contextWindowInfo: unknown model is the 200K default and says so', () => {
  expect(contextWindowInfo('mock:usage', table)).toEqual({
    limit: DEFAULT_CONTEXT_WINDOW,
    known: false,
  })
  expect(contextWindowInfo(null)).toEqual({ limit: DEFAULT_CONTEXT_WINDOW, known: false })
})
