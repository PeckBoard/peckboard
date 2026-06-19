// Display formatters for the usage dashboard. Pricing is NEVER done here —
// `est_cost` always comes from the backend already in USD (see
// `src/routes/usage/cost.rs`); these only render numbers the server produced.

/** Compact token formatter: 1_234_567 -> "1.23M". Keeps figures legible in
 *  tight panel rows without a charting lib. */
export function fmtTokens(n: number): string {
  if (!Number.isFinite(n)) return '0'
  const abs = Math.abs(n)
  if (abs >= 1_000_000_000) return `${(n / 1_000_000_000).toFixed(2)}B`
  if (abs >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`
  if (abs >= 1_000) return `${(n / 1_000).toFixed(1)}K`
  return `${Math.round(n)}`
}

/** USD formatter that keeps small estimates legible (sub-cent costs still show
 *  a non-zero figure). Mirrors the backend's `est_cost`. */
export function fmtUsd(n: number): string {
  if (!Number.isFinite(n)) return '$0.00'
  if (n > 0 && n < 0.01) return '<$0.01'
  return `$${n.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`
}

/** Thousands-separated integer, e.g. 12_345 -> "12,345". For exact figures
 *  (tooltips, context-window denominators) where the compact `fmtTokens` form
 *  would hide the real number. */
export function fmtInt(n: number): string {
  if (!Number.isFinite(n)) return '0'
  return Math.round(n).toLocaleString()
}
