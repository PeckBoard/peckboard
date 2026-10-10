//! A project's card dependency graph, shared by the
//! `get_card_dependency_tree` MCP tool and `GET /api/dashboard/dependencies`.

use std::collections::{HashMap, HashSet};

use crate::db::Db;
use crate::db::models::Card;

/// Every card of one project plus its dependency edges
/// `(card_id, depends_on_card_id)`.
pub struct ProjectDeps {
    pub cards: Vec<Card>,
    pub edges: Vec<(String, String)>,
}

impl ProjectDeps {
    /// Load `project_id`'s cards and edges. An edge-query failure degrades
    /// to "no edges" rather than failing the whole read.
    pub async fn load(db: &Db, project_id: &str) -> anyhow::Result<Self> {
        let cards = db.list_cards_by_project(project_id).await?;
        let edges = db
            .list_dependencies_by_project(project_id)
            .await
            .unwrap_or_default();
        Ok(Self { cards, edges })
    }

    /// Each card mapped to the cards it depends on.
    pub fn deps_by_card(&self) -> HashMap<&str, Vec<&str>> {
        let mut out: HashMap<&str, Vec<&str>> = HashMap::new();
        for (cid, dep_id) in &self.edges {
            out.entry(cid.as_str()).or_default().push(dep_id.as_str());
        }
        out
    }

    pub fn info_by_id(&self) -> HashMap<&str, &Card> {
        self.cards.iter().map(|c| (c.id.as_str(), c)).collect()
    }
}

/// Collect every transitive dependency id of `card_id` (excluding itself)
/// into `seen`. The `seen` set doubles as cycle protection.
pub fn collect_transitive_deps(
    card_id: &str,
    deps_by_card: &HashMap<&str, Vec<&str>>,
    seen: &mut HashSet<String>,
) {
    if let Some(deps) = deps_by_card.get(card_id) {
        for dep in deps {
            if seen.insert(dep.to_string()) {
                collect_transitive_deps(dep, deps_by_card, seen);
            }
        }
    }
}

/// A card still waiting to run: not finished either way.
fn is_open(step: &str) -> bool {
    step != "done" && step != "wont_do"
}

/// The (up to) `limit` not-done cards with the most open cards transitively
/// waiting on them, as `(card_id, blocks)`, most first (ties by id). Cards
/// that block nothing are left out. Only cards in `info_by_id` count.
///
/// A `wont_do` card still counts as a blocker — dispatch only treats a
/// `done` prerequisite as met — but not as a waiter.
pub fn top_blockers(
    deps_by_card: &HashMap<&str, Vec<&str>>,
    info_by_id: &HashMap<&str, &Card>,
    limit: usize,
) -> Vec<(String, usize)> {
    // Reverse edges: each card mapped to the cards that depend on it.
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for (cid, deps) in deps_by_card {
        for dep in deps {
            dependents.entry(dep).or_default().push(cid);
        }
    }
    let mut out: Vec<(String, usize)> = info_by_id
        .values()
        .filter(|c| c.step != "done")
        .filter_map(|c| {
            let mut waiting = HashSet::new();
            collect_transitive_deps(&c.id, &dependents, &mut waiting);
            let blocks = waiting
                .iter()
                .filter(|id| {
                    id.as_str() != c.id
                        && info_by_id
                            .get(id.as_str())
                            .is_some_and(|w| is_open(&w.step))
                })
                .count();
            (blocks > 0).then(|| (c.id.clone(), blocks))
        })
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::NewCard;

    async fn card(db: &Db, id: &str, step: &str) {
        let now = chrono::Utc::now().to_rfc3339();
        db.create_card(NewCard {
            id: id.into(),
            project_id: "p1".into(),
            title: id.into(),
            description: String::new(),
            step: step.into(),
            priority: 0,
            workflow: "default".into(),
            model: None,
            effort: None,
            blocked: false,
            block_reason: None,
            created_at: now.clone(),
            updated_at: now,
            system_prompt_name: None,
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn top_blockers_counts_open_transitive_waiters() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::auth::middleware::tests::test_state(dir.path());
        crate::auth::middleware::tests::seed_session(&state, "s", None, Some("p1")).await;
        let db = &state.db;
        // a ← b ← c, a ← d (done), e ← f; done card g ← h blocks nothing.
        for (id, step) in [
            ("a", "todo"),
            ("b", "todo"),
            ("c", "todo"),
            ("d", "done"),
            ("e", "todo"),
            ("f", "wont_do"),
            ("g", "done"),
            ("h", "todo"),
        ] {
            card(db, id, step).await;
        }
        for (cid, dep) in [("b", "a"), ("c", "b"), ("d", "a"), ("f", "e"), ("h", "g")] {
            db.set_card_dependencies(cid, vec![dep.into()])
                .await
                .unwrap();
        }
        let graph = ProjectDeps::load(db, "p1").await.unwrap();
        let top = top_blockers(&graph.deps_by_card(), &graph.info_by_id(), 5);
        assert_eq!(top, vec![("a".to_string(), 2), ("b".to_string(), 1)]);
    }
}
