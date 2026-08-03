import { test, expect } from '@playwright/test'
import {
  capabilitiesForModel,
  imagesAllowedForModel,
  interruptAffordanceForModel,
  modelThinks,
  DEFAULT_PROVIDER_CAPABILITIES,
  CONSERVATIVE_PROVIDER_CAPABILITIES,
  type ProviderInfo,
} from '../../src/store/resources'

/**
 * Pure-logic tests for the provider-capabilities fallback helpers. There is
 * no vitest in `web/`, so these run in the Playwright suite — they touch no
 * page and no server, they just import the module under test.
 *
 * Covers the bug where a session's provider vanishes from `/api/models`
 * (hidden in Settings → Providers, or its plugin uninstalled) while the
 * session still runs: the UI must fall back to conservative capabilities,
 * not Claude-shaped defaults, for any model id carrying an explicit
 * non-claude provider prefix.
 */

const claudeProvider: ProviderInfo = {
  id: 'claude',
  display_name: 'Claude',
  models: [{ id: 'claude:opus', display_name: 'Opus' }],
  capabilities: DEFAULT_PROVIDER_CAPABILITIES,
}

const cursorProvider: ProviderInfo = {
  id: 'cursor',
  display_name: 'Cursor',
  models: [
    { id: 'cursor:fast', display_name: 'Fast' },
    { id: 'cursor:no-vision', display_name: 'No Vision', images_in: false },
    { id: 'cursor:no-thinking', display_name: 'No Thinking', thinking: false },
  ],
  capabilities: {
    supports_thinking: true,
    supports_images_in: true,
    supports_usage: true,
    supports_resume: true,
    interrupt_kind: 'cooperative',
    supports_mid_stream_injection: false,
    answer_transport: 'new_turn',
  },
}

test('capabilitiesForModel: known claude model uses its own capabilities', () => {
  expect(capabilitiesForModel('claude:opus', [claudeProvider])).toEqual(
    DEFAULT_PROVIDER_CAPABILITIES,
  )
})

test('capabilitiesForModel: known non-claude model uses its own capabilities', () => {
  expect(capabilitiesForModel('cursor:fast', [cursorProvider])).toEqual(cursorProvider.capabilities)
})

test('capabilitiesForModel: bare id with no providers falls back to Claude defaults', () => {
  expect(capabilitiesForModel('opus', [])).toEqual(DEFAULT_PROVIDER_CAPABILITIES)
})

test('capabilitiesForModel: explicit claude: prefix with no providers falls back to Claude defaults', () => {
  expect(capabilitiesForModel('claude:opus', [])).toEqual(DEFAULT_PROVIDER_CAPABILITIES)
})

test('capabilitiesForModel: vanished non-claude provider falls back to conservative caps, not Claude', () => {
  // Session references cursor:fast, but the cursor provider is no longer in
  // the catalogue (hidden or uninstalled) — must NOT claim Claude defaults.
  expect(capabilitiesForModel('cursor:fast', [])).toEqual(CONSERVATIVE_PROVIDER_CAPABILITIES)
  expect(capabilitiesForModel('cursor:fast', [claudeProvider])).toEqual(
    CONSERVATIVE_PROVIDER_CAPABILITIES,
  )
})

test('imagesAllowedForModel: vanished provider disallows attachments', () => {
  expect(imagesAllowedForModel('cursor:fast', [])).toBe(false)
})

test('imagesAllowedForModel: known provider gates on provider then per-model flag', () => {
  expect(imagesAllowedForModel('cursor:fast', [cursorProvider])).toBe(true)
  expect(imagesAllowedForModel('cursor:no-vision', [cursorProvider])).toBe(false)
})

test('modelThinks: vanished provider never claims thinking', () => {
  expect(modelThinks('cursor:fast', [])).toBe(false)
})

test('modelThinks: known provider gates on provider then per-model flag', () => {
  expect(modelThinks('cursor:fast', [cursorProvider])).toBe(true)
  expect(modelThinks('cursor:no-thinking', [cursorProvider])).toBe(false)
})

test('interruptAffordanceForModel: vanished provider labels a hard kill, not "Interrupt"', () => {
  const affordance = interruptAffordanceForModel('cursor:fast', [])
  expect(affordance.label).toBe('Stop')
  expect(affordance.title).toMatch(/killed/)
})

test('interruptAffordanceForModel: unknown interrupt_kind from a newer backend hits the default arm, not undefined', () => {
  // Simulates a newer/plugin backend sending an interrupt_kind value this
  // frontend build doesn't know about — the switch's default arm must still
  // return a label instead of leaving affordance.label undefined.
  const weirdCapabilities = {
    ...cursorProvider.capabilities!,
    interrupt_kind: 'future_kind',
  } as unknown as ProviderInfo['capabilities']
  const weirdProvider: ProviderInfo = {
    ...cursorProvider,
    id: 'weird',
    capabilities: weirdCapabilities,
  }
  const affordance = interruptAffordanceForModel('weird:model', [weirdProvider])
  expect(affordance.label).toBe('Stop')
  expect(affordance.title).toBeTruthy()
})
