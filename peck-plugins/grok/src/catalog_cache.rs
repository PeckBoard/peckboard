//! Last-good model catalog cache backed by the plugin data store.
//!
//! Live CLI/HTTP discovery is preferred. When that fails, serve the last
//! successfully discovered catalog instead of falling straight through to
//! the compile-time seed — so a temporary CLI outage still shows recent
//! models. Fresh installs with no prior probe keep using the seed.

use serde_json::{Value, json};

use crate::host::{self, HostFn};

const COLLECTION: &str = "model_catalog";
const KEY: &str = "last_good";

/// Persist a successful discovery so a later failed probe can reuse it.
pub fn store_last_good(models: &[Value]) {
    if models.is_empty() {
        return;
    }
    let _ = host::call_host(
        HostFn::StorePut,
        &json!({
            "collection": COLLECTION,
            "key": KEY,
            "data": {
                "fetched_at": now_secs(),
                "models": models,
            },
        }),
    );
}

/// Load the last successfully stored catalog, if any.
pub fn load_last_good() -> Option<Vec<Value>> {
    let out = host::call_host(
        HostFn::StoreGet,
        &json!({ "collection": COLLECTION, "key": KEY }),
    )
    .ok()?;
    let value = out.get("value")?;
    if value.is_null() {
        return None;
    }
    let models = value.get("models")?.as_array()?.clone();
    if models.is_empty() {
        None
    } else {
        Some(models)
    }
}

/// Choose the base catalog: live discovery → last-good → seed.
///
/// When `discovered` is non-empty it is stored and returned. Otherwise the
/// last-good cache is tried; the compile-time `seed` is the final fallback.
/// Returns `(catalog, source)` where source is `"discovery"`, `"last_good"`,
/// or `"seed"`.
pub fn resolve_base(
    discovered: Option<Vec<Value>>,
    seed: Vec<Value>,
) -> (Vec<Value>, &'static str) {
    let (models, should_store, source) = pick_base(discovered, load_last_good(), seed);
    if should_store {
        store_last_good(&models);
    }
    (models, source)
}

/// Pure selection used by [`resolve_base`] and unit tests.
pub fn pick_base(
    discovered: Option<Vec<Value>>,
    last_good: Option<Vec<Value>>,
    seed: Vec<Value>,
) -> (Vec<Value>, bool, &'static str) {
    if let Some(models) = discovered.filter(|m| !m.is_empty()) {
        return (models, true, "discovery");
    }
    if let Some(models) = last_good.filter(|m| !m.is_empty()) {
        return (models, false, "last_good");
    }
    (seed, false, "seed")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::pick_base;
    use serde_json::json;

    fn m(id: &str) -> serde_json::Value {
        json!({ "id": id, "display_name": id, "capabilities": ["code"], "tier": 0 })
    }

    #[test]
    fn discovery_wins_and_requests_store() {
        let (got, store, src) = pick_base(
            Some(vec![m("live-a"), m("live-b")]),
            Some(vec![m("cached")]),
            vec![m("seed")],
        );
        assert_eq!(src, "discovery");
        assert!(store);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["id"], "live-a");
    }

    #[test]
    fn empty_discovery_falls_through_to_last_good() {
        let (got, store, src) = pick_base(Some(vec![]), Some(vec![m("cached")]), vec![m("seed")]);
        assert_eq!(src, "last_good");
        assert!(!store);
        assert_eq!(got[0]["id"], "cached");
    }

    #[test]
    fn none_discovery_falls_through_to_last_good() {
        let (got, store, src) = pick_base(None, Some(vec![m("cached")]), vec![m("seed")]);
        assert_eq!(src, "last_good");
        assert!(!store);
        assert_eq!(got[0]["id"], "cached");
    }

    #[test]
    fn missing_cache_uses_seed() {
        let (got, store, src) = pick_base(None, None, vec![m("seed-a"), m("seed-b")]);
        assert_eq!(src, "seed");
        assert!(!store);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["id"], "seed-a");
    }

    #[test]
    fn empty_last_good_uses_seed() {
        let (got, store, src) = pick_base(None, Some(vec![]), vec![m("seed")]);
        assert_eq!(src, "seed");
        assert!(!store);
        assert_eq!(got[0]["id"], "seed");
    }
}
