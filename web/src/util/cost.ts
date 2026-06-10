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

/** Usable context-window size (tokens) when a usage row carries no model, or
 *  one that isn't in `CONTEXT_WINDOWS`. Every shipping Claude tier exposes a
 *  200K-token window today, so this is the value the session context gauge
 *  measures occupancy against. Lives here, next to the rate table, so the
 *  limit is part of the shared cost/model module rather than hardcoded in a
 *  component. */
export const DEFAULT_CONTEXT_WINDOW = 200_000

/** Per-model context-window overrides, keyed by bare model id. Empty today —
 *  all current Claude models share the 200K default — but kept as the single
 *  place a future wider-window tier is registered, mirroring how `ratesFor`
 *  resolves per-model rates. */
const CONTEXT_WINDOWS: Record<string, number> = {}

/** Usable context-window size (tokens) for a model id, tolerating the
 *  `claude:` provider prefix. Falls back to [`DEFAULT_CONTEXT_WINDOW`] for an
 *  unknown or missing model so the gauge always has a denominator. */
export function contextWindowFor(model: string | null | undefined): number {
  if (model) {
    const bare = model.startsWith('claude:') ? model.slice('claude:'.length) : model
    const w = CONTEXT_WINDOWS[bare]
    if (w) return w
  }
  return DEFAULT_CONTEXT_WINDOW
}

const RATE_FIELD: Record<TokenKind, keyof ModelRates> = {
  input: 'input_per_mtok',
  output: 'output_per_mtok',
  cache_read: 'cache_read_per_mtok',
  cache_creation: 'cache_creation_per_mtok',
}

/** Rates for a model id from a fetched `CostTable`, tolerating the `claude:`
 *  provider prefix usage rows may carry. Falls back to the Opus tier
 *  (the backend's default) when the model is unknown, so an unrecognized
 *  model is never silently free. Returns null only for an empty table. */
export function ratesFor(table: CostTable, model: string | null): ModelRates | null {
  const rates = table.rates
  if (model) {
    const bare = model.startsWith('claude:') ? model.slice('claude:'.length) : model
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
