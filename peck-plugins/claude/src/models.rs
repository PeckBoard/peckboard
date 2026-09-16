//! Seed catalog copied from `src/provider/claude/mod.rs::static_models`.

pub const PROVIDER_ID: &str = "claude";
pub const DISPLAY_NAME: &str = "Claude (CLI)";

pub fn seed_models() -> serde_json::Value {
    serde_json::json!([
        {
            "id": "claude-fable-5",
            "display_name": "Claude Fable 5",
            "capabilities": ["code", "reasoning", "vision"],
            "tier": 4
        },
        {
            "id": "claude-opus-5",
            "display_name": "Claude Opus 5",
            "capabilities": ["code", "reasoning", "vision"],
            "tier": 3
        },
        {
            "id": "claude-opus-4-8",
            "display_name": "Claude Opus 4.8",
            "capabilities": ["code", "reasoning", "vision"],
            "tier": 3
        },
        {
            "id": "claude-opus-4-7",
            "display_name": "Claude Opus 4.7",
            "capabilities": ["code", "reasoning", "vision"],
            "tier": 3
        },
        {
            "id": "claude-opus-4-6",
            "display_name": "Claude Opus 4.6",
            "capabilities": ["code", "reasoning", "vision"],
            "tier": 3
        },
        {
            "id": "claude-sonnet-5",
            "display_name": "Claude Sonnet 5",
            "capabilities": ["code", "vision"],
            "tier": 2
        },
        {
            "id": "claude-sonnet-4-6",
            "display_name": "Claude Sonnet 4.6",
            "capabilities": ["code", "vision"],
            "tier": 2
        },
        {
            "id": "claude-haiku-4-5",
            "display_name": "Claude Haiku 4.5",
            "capabilities": ["code"],
            "tier": 1
        }
    ])
}

/// Published Anthropic rates in USD per million tokens (input, output) —
/// the same tiers as core's `src/routes/usage/cost.rs::known_rates_for`
/// (OPUS 15/75, SONNET 3/15, HAIKU 0.8/4). Registration-time pricing backs
/// `AgentProvider::model_price`, which cheapest-model auto-pick (the
/// pre-hatcher) sorts on.
pub fn pricing() -> serde_json::Value {
    const OPUS: (f64, f64) = (15.0, 75.0);
    const SONNET: (f64, f64) = (3.0, 15.0);
    const HAIKU: (f64, f64) = (0.8, 4.0);
    let rate = |(input, output): (f64, f64)| serde_json::json!({ "input_usd_per_mtok": input, "output_usd_per_mtok": output });
    serde_json::json!({
        "claude-fable-5": rate(OPUS),
        "claude-opus-5": rate(OPUS),
        "claude-opus-4-8": rate(OPUS),
        "claude-opus-4-7": rate(OPUS),
        "claude-opus-4-6": rate(OPUS),
        "claude-sonnet-5": rate(SONNET),
        "claude-sonnet-4-6": rate(SONNET),
        "claude-haiku-4-5": rate(HAIKU),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_seed_model_has_a_price() {
        let pricing = super::pricing();
        for m in super::seed_models().as_array().unwrap() {
            let id = m.get("id").unwrap().as_str().unwrap();
            let p = pricing
                .get(id)
                .unwrap_or_else(|| panic!("no price for {id}"));
            assert!(p.get("input_usd_per_mtok").unwrap().as_f64().unwrap() > 0.0);
            assert!(p.get("output_usd_per_mtok").unwrap().as_f64().unwrap() > 0.0);
        }
        // Haiku must price below the Opus tier or auto-pick loses its point.
        let haiku = pricing["claude-haiku-4-5"]["input_usd_per_mtok"]
            .as_f64()
            .unwrap();
        let opus = pricing["claude-opus-5"]["input_usd_per_mtok"]
            .as_f64()
            .unwrap();
        assert!(haiku < opus);
    }
}
