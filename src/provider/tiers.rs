//! User-chosen model tiers, per provider (Settings → Providers & Accounts).
//!
//! A provider's catalog ranks its own models through `ModelInfo::tier`. An
//! admin can assign models to the four tier levels here; the registry
//! overlays the result on every effective catalog, and Auto routing prefers
//! the assigned models. Models nobody assigned keep the provider's own tier,
//! so a provider with no assignments behaves as if this module didn't exist.
//!
//! Assignments name BASE models (the `@account` part stripped): a model is
//! the same model on every account or server.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Context as _;

use super::registry::{ProviderRegistry, split_model_account};
use super::stream::ModelInfo;
use crate::db::Db;
use crate::routes::settings::{SETTINGS_COLLECTION, SETTINGS_NS};

/// The assignable tier levels, lowest first, with their display names.
pub const TIER_LEVELS: [(i32, &str); 4] = [(1, "Low"), (2, "Medium"), (3, "High"), (4, "Max")];

/// Plugin-store key: `{ provider_id: { "<tier>": ["base-model", …] } }`.
const MODEL_TIERS_KEY: &str = "provider_model_tiers";

/// Serialises read-modify-write saves, and keeps the stored blob and the
/// registry's in-memory copy updating in the same order.
static SAVE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// One provider's tier assignments.
///
/// Proof token: bearer holds known tier levels, normalised base model ids,
/// and no model twice within one tier. A model may sit in several tiers
/// (Auto picks per tier); its catalog tier is then the highest of them.
/// [`TierAssignments::new`] is the only way to build a
/// non-empty one; see `routes::settings::set_provider_tiers` for an example.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TierAssignments {
    tiers: BTreeMap<i32, Vec<String>>,
}

impl TierAssignments {
    /// Validate raw assignments. Errors are user-readable (the settings
    /// route returns them as a 400). Tiers with no models are dropped.
    pub fn new(raw: BTreeMap<i32, Vec<String>>) -> Result<Self, String> {
        let mut tiers = BTreeMap::new();
        for (tier, models) in raw {
            if !TIER_LEVELS.iter().any(|(level, _)| *level == tier) {
                return Err(format!("unknown tier {tier}"));
            }
            let mut seen = HashSet::new();
            let mut bases = Vec::with_capacity(models.len());
            for model in &models {
                let base = split_model_account(model.trim()).0.trim();
                if base.is_empty() {
                    return Err("model id cannot be empty".to_string());
                }
                if !seen.insert(base.to_string()) {
                    return Err(format!("{base} is in this tier more than once"));
                }
                bases.push(base.to_string());
            }
            if !bases.is_empty() {
                tiers.insert(tier, bases);
            }
        }
        Ok(TierAssignments { tiers })
    }

    pub fn is_empty(&self) -> bool {
        self.tiers.is_empty()
    }

    /// The models assigned to `tier`, in the user's preference order.
    pub fn models_in(&self, tier: i32) -> &[String] {
        self.tiers.get(&tier).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Overlay these assignments on a catalog. Unassigned models are left
    /// alone.
    pub fn apply(&self, models: &mut [ModelInfo]) {
        if self.is_empty() {
            return;
        }
        // A model in several tiers takes the highest one in the catalog, so
        // cost guidance never treats it as cheaper than its priciest role.
        let mut tier_by_base: HashMap<&str, i32> = HashMap::new();
        for (tier, bases) in &self.tiers {
            for base in bases {
                let entry = tier_by_base.entry(base.as_str()).or_insert(*tier);
                *entry = (*entry).max(*tier);
            }
        }
        for model in models {
            if let Some(tier) = tier_by_base.get(split_model_account(&model.id).0) {
                model.tier = *tier;
            }
        }
    }

    /// The catalog entry for the first model assigned to `tier` that the
    /// catalog still offers. An assigned model missing from the catalog is
    /// skipped, not an error: it may only be gone temporarily.
    pub fn first_available<'a>(&self, tier: i32, models: &'a [ModelInfo]) -> Option<&'a ModelInfo> {
        self.models_in(tier).iter().find_map(|base| {
            models
                .iter()
                .find(|m| split_model_account(&m.id).0 == base.as_str())
        })
    }
}

type StoredTiers = BTreeMap<String, BTreeMap<i32, Vec<String>>>;

fn read_stored(db: &Db) -> anyhow::Result<StoredTiers> {
    let raw = db
        .plugin_store_get_blocking(SETTINGS_NS, SETTINGS_COLLECTION, MODEL_TIERS_KEY)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    match raw {
        Some(json) => serde_json::from_str(&json).context("parsing stored model tiers"),
        None => Ok(StoredTiers::new()),
    }
}

/// Every provider's stored assignments. A provider whose stored value no
/// longer validates is skipped with a warning, so one bad entry can't take
/// the others down; missing or unreadable storage means no assignments.
pub async fn load(db: &Db) -> HashMap<String, TierAssignments> {
    let db = db.clone();
    let stored = match tokio::task::spawn_blocking(move || read_stored(&db)).await {
        Ok(Ok(stored)) => stored,
        Ok(Err(e)) => {
            tracing::warn!("Could not read model tiers: {e:#}");
            return HashMap::new();
        }
        Err(e) => {
            tracing::warn!("Could not read model tiers: {e}");
            return HashMap::new();
        }
    };
    stored
        .into_iter()
        .filter_map(|(provider, raw)| match TierAssignments::new(raw) {
            Ok(assignments) => Some((provider, assignments)),
            Err(e) => {
                tracing::warn!("Ignoring stored model tiers for '{provider}': {e}");
                None
            }
        })
        .collect()
}

/// Persist one provider's assignments and make them live. Empty assignments
/// clear the provider's entry, restoring its catalog's own tiers.
pub async fn save(
    db: &Db,
    registry: &ProviderRegistry,
    provider_id: &str,
    assignments: TierAssignments,
) -> anyhow::Result<()> {
    let _guard = SAVE_LOCK.lock().await;
    let db = db.clone();
    let id = provider_id.to_string();
    let tiers = assignments.tiers.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        // Unlike `load`, an unreadable blob fails the save: writing over it
        // would discard every other provider's assignments.
        let mut stored = read_stored(&db)?;
        if tiers.is_empty() {
            stored.remove(&id);
        } else {
            stored.insert(id, tiers);
        }
        let value = serde_json::to_string(&stored)?;
        db.plugin_store_put_blocking(SETTINGS_NS, SETTINGS_COLLECTION, MODEL_TIERS_KEY, &value)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(())
    })
    .await??;
    registry.set_tier_assignments(provider_id, assignments);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, tier: i32) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            display_name: id.into(),
            capabilities: vec![],
            tier,
        }
    }

    fn assignments(rows: &[(i32, &[&str])]) -> TierAssignments {
        TierAssignments::new(
            rows.iter()
                .map(|(t, ms)| (*t, ms.iter().map(|m| m.to_string()).collect()))
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn new_rejects_bad_input_and_normalises_account_scoped_ids() {
        let raw = |t: i32, ms: &[&str]| {
            BTreeMap::from([(t, ms.iter().map(|m| m.to_string()).collect::<Vec<_>>())])
        };
        // The same model twice in one tier is a duplicate, even across accounts.
        assert!(TierAssignments::new(raw(1, &["a@x", "a@y"])).is_err());
        // One model may serve several tiers; its catalog tier is the highest.
        let multi = TierAssignments::new(BTreeMap::from([
            (1, vec!["a".to_string()]),
            (3, vec!["a".to_string()]),
        ]))
        .unwrap();
        assert_eq!(multi.models_in(1), ["a".to_string()]);
        assert_eq!(multi.models_in(3), ["a".to_string()]);
        let mut models = vec![model("a@x", 0)];
        multi.apply(&mut models);
        assert_eq!(models[0].tier, 3);
        let ok = TierAssignments::new(raw(2, &["a@acct"])).unwrap();
        assert_eq!(ok.models_in(2), ["a".to_string()]);
        assert!(TierAssignments::new(raw(1, &[])).unwrap().is_empty());
    }

    #[test]
    fn apply_covers_every_account_and_leaves_unassigned_models_alone() {
        let mut models = vec![model("a@x", 0), model("a@y", 1), model("b@x", 2)];
        assignments(&[(4, &["a"])]).apply(&mut models);
        let tiers: Vec<i32> = models.iter().map(|m| m.tier).collect();
        assert_eq!(tiers, [4, 4, 2]);
    }

    #[test]
    fn first_available_skips_models_missing_from_the_catalog() {
        let models = vec![model("b@x", 0)];
        let a = assignments(&[(1, &["gone", "b"])]);
        assert_eq!(a.first_available(1, &models).unwrap().id, "b@x");
        assert!(a.first_available(2, &models).is_none());
    }

    #[tokio::test]
    async fn save_round_trips_and_clearing_removes_only_that_provider() {
        let db = Db::in_memory().unwrap();
        let registry = ProviderRegistry::new();
        save(&db, &registry, "p1", assignments(&[(1, &["a"])]))
            .await
            .unwrap();
        save(&db, &registry, "p2", assignments(&[(2, &["b"])]))
            .await
            .unwrap();
        let loaded = load(&db).await;
        assert_eq!(loaded["p1"].models_in(1), ["a".to_string()]);
        assert_eq!(
            registry.tier_assignments("p2").models_in(2),
            ["b".to_string()]
        );

        save(&db, &registry, "p1", TierAssignments::default())
            .await
            .unwrap();
        let loaded = load(&db).await;
        assert!(!loaded.contains_key("p1"));
        assert!(loaded.contains_key("p2"));
        assert!(registry.tier_assignments("p1").is_empty());
    }
}
