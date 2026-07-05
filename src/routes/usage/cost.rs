//! Per-model token pricing — the single source of truth for usage cost.
//!
//! Rates are USD per million tokens, split by token kind, and the whole
//! usage dashboard prices tokens through [`token_cost`] / [`usage_cost`] so
//! rates live in exactly one place (this module). Backend aggregation calls
//! these helpers directly; the frontend fetches the serialized [`CostTable`]
//! from `GET /api/usage/costs` and prices client-side trends with the same
//! numbers — so Rust and TS never hardcode rates independently.
//!
//! When the model registry (`crate::provider::claude::discover_models`)
//! gains or renames a model, update [`rates_for`] here. The
//! `every_registry_model_is_priced` test fails if a registry model is left
//! without an explicit tier.

use std::collections::BTreeMap;

use serde::Serialize;

/// Which slice of a turn's token usage a rate applies to. Mirrors the
/// per-kind columns on the `usage_events` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    Input,
    Output,
    CacheRead,
    CacheCreation,
}

/// USD-per-million-token rates for one model, by token kind. Serialized as a
/// value in the [`CostTable`] map the frontend fetches.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ModelRates {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cache_read_per_mtok: f64,
    pub cache_creation_per_mtok: f64,
}

impl ModelRates {
    /// The rate (USD per million tokens) for a single token kind.
    pub fn rate(&self, kind: TokenKind) -> f64 {
        match kind {
            TokenKind::Input => self.input_per_mtok,
            TokenKind::Output => self.output_per_mtok,
            TokenKind::CacheRead => self.cache_read_per_mtok,
            TokenKind::CacheCreation => self.cache_creation_per_mtok,
        }
    }
}

// Published rate tiers (USD per million tokens). Cache-read is the cheap
// hit; cache-creation (cache write) carries a premium over base input.
const OPUS: ModelRates = ModelRates {
    input_per_mtok: 15.0,
    output_per_mtok: 75.0,
    cache_read_per_mtok: 1.5,
    cache_creation_per_mtok: 18.75,
};
const SONNET: ModelRates = ModelRates {
    input_per_mtok: 3.0,
    output_per_mtok: 15.0,
    cache_read_per_mtok: 0.3,
    cache_creation_per_mtok: 3.75,
};
const HAIKU: ModelRates = ModelRates {
    input_per_mtok: 0.8,
    output_per_mtok: 4.0,
    cache_read_per_mtok: 0.08,
    cache_creation_per_mtok: 1.0,
};

/// Fallback for an unrecognized model id (a renamed/removed registry model
/// or a non-Claude provider). Priced at the Opus tier so an unknown model
/// is never silently free — the most expensive tier keeps cost estimates
/// conservative rather than misleadingly low.
const DEFAULT_RATES: ModelRates = OPUS;

/// Exact published rates for a known Claude model id, or `None` for a model
/// we carry no price for. Accepts ids with or without the `claude:` provider
/// prefix and with or without an `@account` suffix.
pub fn known_rates_for(model: &str) -> Option<ModelRates> {
    let (model, _acct) = crate::provider::registry::split_model_account(model);
    let id = model.strip_prefix("claude:").unwrap_or(model);
    match id {
        "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6" | "claude-fable-5" => Some(OPUS),
        "claude-sonnet-4-6" => Some(SONNET),
        "claude-haiku-4-5" => Some(HAIKU),
        _ => None,
    }
}

/// Rates for a model id. Accepts ids with or without the `claude:` provider
/// prefix (usage rows store either form — see `usage_events.model`). Unknown
/// models fall back to the conservative default tier.
pub fn rates_for(model: Option<&str>) -> ModelRates {
    match model {
        Some(m) => known_rates_for(m).unwrap_or(DEFAULT_RATES),
        None => DEFAULT_RATES,
    }
}

/// Cost in USD of `tokens` tokens of a single `kind` for `model`.
pub fn token_cost(model: Option<&str>, kind: TokenKind, tokens: i64) -> f64 {
    tokens as f64 / 1_000_000.0 * rates_for(model).rate(kind)
}

/// Total USD cost of one usage record's four token slices. `total_tokens`
/// and `context_tokens` are intentionally NOT priced here — they are
/// roll-ups/snapshots that overlap the four billed slices, so pricing them
/// would double-count.
pub fn usage_cost(
    model: Option<&str>,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_creation_tokens: i64,
) -> f64 {
    token_cost(model, TokenKind::Input, input_tokens)
        + token_cost(model, TokenKind::Output, output_tokens)
        + token_cost(model, TokenKind::CacheRead, cache_read_tokens)
        + token_cost(model, TokenKind::CacheCreation, cache_creation_tokens)
}

/// The full per-model rate table the frontend fetches once at boot. Keyed by
/// bare model id; values are the same rates [`token_cost`] applies.
#[derive(Debug, Clone, Serialize)]
pub struct CostTable {
    pub rates: BTreeMap<String, ModelRates>,
}

/// Build the rate table from the model registry so every advertised model
/// carries a published rate. The frontend caches this and prices its own
/// trend lines with it, matching the backend's `est_cost` exactly.
pub fn cost_table() -> CostTable {
    let rates = crate::provider::claude::discover_models()
        .into_iter()
        .map(|m| {
            let r = rates_for(Some(&m.id));
            (m.id, r)
        })
        .collect();
    CostTable { rates }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected {b}, got {a}");
    }

    #[test]
    fn token_cost_matches_known_rate_times_tokens() {
        // 1M output tokens on Opus = $75.00.
        approx(
            token_cost(Some("claude-opus-4-8"), TokenKind::Output, 1_000_000),
            75.0,
        );
        // 2M input tokens on Opus = 2 * $15.00 = $30.00.
        approx(
            token_cost(Some("claude-opus-4-8"), TokenKind::Input, 2_000_000),
            30.0,
        );
        // The `claude:` provider prefix is stripped before lookup.
        approx(
            token_cost(
                Some("claude:claude-sonnet-4-6"),
                TokenKind::Output,
                1_000_000,
            ),
            15.0,
        );
        // 500k cache-read tokens on Haiku = 0.5 * $0.08 = $0.04.
        approx(
            token_cost(Some("claude-haiku-4-5"), TokenKind::CacheRead, 500_000),
            0.04,
        );

        // A full record sums its four billed slices and ignores the
        // total/context roll-ups.
        let total = usage_cost(Some("claude-opus-4-8"), 1_000_000, 1_000_000, 0, 0);
        approx(total, 90.0);

        // Unknown / missing model falls back to the default (Opus) tier
        // rather than pricing at zero.
        approx(
            token_cost(Some("some-future-model"), TokenKind::Output, 1_000_000),
            token_cost(Some("claude-opus-4-8"), TokenKind::Output, 1_000_000),
        );
        approx(token_cost(None, TokenKind::Output, 1_000_000), 75.0);
    }

    #[test]
    fn every_registry_model_is_priced() {
        let table = cost_table();
        for model in crate::provider::claude::discover_models() {
            let rates = table
                .rates
                .get(&model.id)
                .unwrap_or_else(|| panic!("no rate for registry model {}", model.id));
            assert!(
                rates.input_per_mtok > 0.0 && rates.output_per_mtok > 0.0,
                "registry model {} priced at zero",
                model.id
            );
        }
    }
}
