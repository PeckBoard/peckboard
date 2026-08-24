// Token cost helpers — the TypeScript half of the usage cost model. Rates
// are NEVER hardcoded here: they come from the `CostTable` the frontend
// fetches from `GET /api/usage/costs`, which the backend builds from its
// single source of truth in `src/routes/usage/cost.rs`. These helpers mirror
// the Rust `token_cost` / `usage_cost` so client-side trend math matches the
// server's `est_cost` exactly.

import type { CostTable, ModelRates } from '../types/api'

/** Token kind a rate applies to — mirrors the Rust `TokenKind` enum and the
 *  four billed columns on `usage_events`. */
export type TokenKind = 'input' | 'output' | 'cache_read' | 'cache_creation'

/** Usable context-window size (tokens) assumed when a row carries no model,
 *  or one we hold no window for. Every standard Claude tier exposes a 200K
 *  window, so it is the safe assumption — but callers must LABEL it as a
 *  default rather than presenting it as the model's real limit (see
 *  [`contextWindowInfo`]). Lives here, next to the rate table, so the limit
 *  is part of the shared cost/model module rather than hardcoded in a
 *  component. */
export const DEFAULT_CONTEXT_WINDOW = 200_000

/** Window of the long-context model aliases the registry advertises. Those
 *  ids are spelled with a `[1m]` suffix on every tier (`opus[1m]`,
 *  `claude-fable-5[1m]`), so the suffix — not an id list that churns with
 *  each model release — is what [`contextWindowInfo`] matches on. */
export const LONG_CONTEXT_WINDOW = 1_000_000

/** Per-model context-window overrides, keyed by bare model id. Standard
 *  Claude tiers share the 200K default and long-context tiers are matched
 *  by their `[1m]` suffix; Grok 4.5/4.6 advertise a 500K window. */
const CONTEXT_WINDOWS: Record<string, number> = {
  'grok-4.5': 500_000,
  'grok-4.6': 500_000,
  // Legacy CLI ids still stored on old sessions; same window as 4.5.
  'grok-build': 500_000,
  'grok-build-0.1': 500_000,
}

/** Normalize a model id to its bare form: usage rows and `sessions.model`
 *  may carry a `provider:` prefix and an `@account` suffix. Mirrors
 *  `bare_model_id` in `src/routes/usage/cost.rs`. */
export function bareModelId(model: string): string {
  const withoutAccount = model.split('@')[0]
  const colon = withoutAccount.indexOf(':')
  return colon > 0 ? withoutAccount.slice(colon + 1) : withoutAccount
}

/** The context window to measure a session's occupancy against, plus whether
 *  it is the model's actual window (`known: true`) or just
 *  [`DEFAULT_CONTEXT_WINDOW`] standing in for a model we can't resolve.
 *  Callers MUST surface that distinction: a 1M-context session measured
 *  against 200K renders a full, red gauge that is simply false, and an
 *  unlabelled 200K denominator claims a certainty we don't have.
 *
 *  Pass the fetched `CostTable` to recognize the standard 200K tiers: the
 *  table is keyed by the ids the running binary actually advertises (see the
 *  Rust `cost_table`), so membership in it — rather than a model list
 *  duplicated here and left to rot — is what makes a window known. */
export function contextWindowInfo(
  model: string | null | undefined,
  table?: CostTable,
): { limit: number; known: boolean } {
  if (model) {
    const bare = bareModelId(model)
    const override = CONTEXT_WINDOWS[bare]
    if (override) return { limit: override, known: true }
    if (bare.endsWith('[1m]')) return { limit: LONG_CONTEXT_WINDOW, known: true }
    if (table && (table.rates[bare] || table.rates[model])) {
      return { limit: DEFAULT_CONTEXT_WINDOW, known: true }
    }
  }
  return { limit: DEFAULT_CONTEXT_WINDOW, known: false }
}

/** Just the denominator from [`contextWindowInfo`], for callers that already
 *  show the model alongside it. */
export function contextWindowFor(model: string | null | undefined, table?: CostTable): number {
  return contextWindowInfo(model, table).limit
}

const RATE_FIELD: Record<TokenKind, keyof ModelRates> = {
  input: 'input_per_mtok',
  output: 'output_per_mtok',
  cache_read: 'cache_read_per_mtok',
  cache_creation: 'cache_creation_per_mtok',
}

/** Rates for a model id from a fetched `CostTable`, tolerating a `provider:`
 *  prefix and `@account` suffix usage rows may carry (via `bareModelId`).
 *  Falls back to the Opus tier (the backend's default) when the model is
 *  unknown, so an unrecognized model is never silently free. Returns null
 *  only for an empty table. */
export function ratesFor(table: CostTable, model: string | null): ModelRates | null {
  const rates = table.rates
  if (model) {
    const bare = bareModelId(model)
    if (rates[bare]) return rates[bare]
    if (rates[model]) return rates[model]
  }
  // The backend's default tier is Opus; match it when present, otherwise
  // the priciest known rate, so estimates stay conservative.
  if (rates['claude-opus-4-8']) return rates['claude-opus-4-8']
  let fallback: ModelRates | null = null
  for (const r of Object.values(rates)) {
    if (!fallback || r.output_per_mtok > fallback.output_per_mtok) fallback = r
  }
  return fallback
}

/** USD cost of `tokens` tokens of one `kind` for `model`, priced against a
 *  fetched `CostTable`. Mirrors the Rust `token_cost`. */
export function tokenCost(
  table: CostTable,
  model: string | null,
  kind: TokenKind,
  tokens: number,
): number {
  const rates = ratesFor(table, model)
  if (!rates) return 0
  return (tokens / 1_000_000) * rates[RATE_FIELD[kind]]
}

/** The four billed token slices of one usage record. */
export interface BilledTokens {
  input_tokens: number
  output_tokens: number
  cache_read_tokens: number
  cache_creation_tokens: number
}
/** Sum of the four billed slices — the figure the dashboard labels “Billed
 *  Tokens”. Deliberately NOT the provider-reported `total_tokens` roll-up:
 *  only this sum is guaranteed to reconcile with the Input / Output / Cache
 *  figures shown beside it and with the per-session rows, so every tokens
 *  total on the dashboard is computed from here. */
export function billedTokens(slices: BilledTokens): number {
  return (
    slices.input_tokens +
    slices.output_tokens +
    slices.cache_read_tokens +
    slices.cache_creation_tokens
  )
}

/** Total USD cost of one usage record's four billed slices. Mirrors the Rust
 *  `usage_cost`; the `total`/`context` roll-ups are intentionally not priced
 *  (they overlap the billed slices and would double-count). */
export function usageCost(table: CostTable, model: string | null, slices: BilledTokens): number {
  return (
    tokenCost(table, model, 'input', slices.input_tokens) +
    tokenCost(table, model, 'output', slices.output_tokens) +
    tokenCost(table, model, 'cache_read', slices.cache_read_tokens) +
    tokenCost(table, model, 'cache_creation', slices.cache_creation_tokens)
  )
}
