//! `spin_up_experts` — partition a project's codebase across long-lived
//! knowledge-expert sessions and have each eagerly capture its scope.
//!
//! The partition is size-balanced with small per-expert windows and groups
//! adjacent (alphabetically neighbouring) top-level directories together so
//! related topics share an expert. Eager capture is throttled to
//! [`MAX_CONCURRENT_GATHER`] experts at a time to cap token burn.

use std::path::{Path, PathBuf};

use futures_util::stream::{FuturesUnordered, StreamExt};
use serde_json::{Value, json};

use super::super::McpToolRegistry;
use crate::db::models::{NewSession, UpdateSession};
use crate::service::mcp_server::context::ToolCallContext;

/// Default number of experts when the caller gives no `max_experts` hint.
const DEFAULT_MAX_EXPERTS: usize = 4;
/// LOCKED DESIGN: never gather/read with more than this many experts at
/// once, to bound token burn during spin-up.
const MAX_CONCURRENT_GATHER: usize = 3;
/// Target upper bound on a single expert's window. Small on purpose: more
/// experts each holding less context beats one expert holding everything.
const SMALL_WINDOW_BYTES: u64 = 50_000;
/// Cap on how many files a knowledge summary enumerates.
const MAX_FILES_LISTED: usize = 40;
/// Don't descend past this directory depth while scanning.
const MAX_WALK_DEPTH: usize = 8;

#[derive(Clone, Debug)]
struct FileInfo {
    rel: String,
    bytes: u64,
    lang: &'static str,
}

/// One expert's slice of the codebase: one or more adjacent top-level
/// directories grouped together, with the source files they contain.
#[derive(Clone, Debug)]
struct Partition {
    area: String,
    dirs: Vec<String>,
    files: Vec<FileInfo>,
    est_bytes: u64,
}

impl Partition {
    /// Comma-joined directory list stored in `scope_path`.
    fn scope_display(&self) -> String {
        self.dirs.join(", ")
    }
}

#[derive(Clone, Debug)]
struct Topic {
    name: String,
    rel: String,
    files: Vec<FileInfo>,
    bytes: u64,
}

impl McpToolRegistry {
    pub(crate) async fn handle_spin_up_experts(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        // Scope-check the target project against the caller's token.
        let scoped = ctx.scope_project(args.get("project_id").and_then(|v| v.as_str()))?;
        let project_id = scoped.into_string();

        let max_experts = args
            .get("max_experts")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).max(1))
            .unwrap_or(DEFAULT_MAX_EXPERTS);

        tracing::info!(
            session_id = %ctx.session_id,
            project_id = %project_id,
            max_experts,
            "MCP tool: spin_up_experts"
        );

        let project = ctx
            .db
            .get_project(&project_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("project not found: {project_id}"))?;
        let folder = ctx
            .db
            .get_folder(&project.folder_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("project folder not found: {}", project.folder_id))?;
        let root = PathBuf::from(&folder.path);

        // Ensure this project has its question-expert (consult-before-ask).
        // Idempotent: a repeated spin-up never clobbers the accumulated row.
        if let Err(e) =
            crate::service::question_expert::ensure_project_question_expert(&ctx.db, &project).await
        {
            tracing::warn!(project_id = %project_id, "failed to ensure project question-expert: {e}");
        }

        // And its PM expert, so projects created before the PM-expert feature
        // gain one here too. Equally idempotent and non-fatal.
        if let Err(e) = crate::service::pm_expert::ensure_project_pm_expert(&ctx.db, &project).await
        {
            tracing::warn!(project_id = %project_id, "failed to ensure project PM expert: {e}");
        }

        let partitions = partition_codebase(&root, max_experts);
        if partitions.is_empty() {
            return Ok(json!({
                "status": "ok",
                "project_id": project_id,
                "experts": [],
                "count": 0,
                "message": "no source files found under the project folder to partition",
            }));
        }

        let now = chrono::Utc::now().to_rfc3339();

        // Phase 1 (sequential): create one long-lived expert session per
        // partition. Knowledge fields are filled in during phase 2.
        let mut created: Vec<(usize, String, Partition, String)> = Vec::new();
        for (idx, part) in partitions.into_iter().enumerate() {
            let session_id = uuid::Uuid::new_v4().to_string();
            let scope_path = part.scope_display();
            ctx.db
                .create_session(NewSession {
                    id: session_id.clone(),
                    name: format!("expert: {}", part.area),
                    folder_id: project.folder_id.clone(),
                    model: project.model.clone(),
                    effort: project.effort.clone(),
                    is_worker: false,
                    project_id: Some(project_id.clone()),
                    card_id: None,
                    conversation_id: None,
                    created_at: now.clone(),
                    last_activity: now.clone(),
                    is_expert: true,
                    expert_kind: Some("knowledge".into()),
                    knowledge_summary: None,
                    knowledge_area: Some(part.area.clone()),
                    scope_path: Some(scope_path.clone()),
                    is_permanent: false,
                    repeating_task_id: None,
                })
                .await?;
            created.push((idx, session_id, part, scope_path));
        }

        // Phase 2 (throttled to MAX_CONCURRENT_GATHER): each expert reads &
        // summarizes its scope, writes the knowledge back to its row, and —
        // when a live dispatcher is available — kicks off an agent run so
        // the expert holds the file context for later consultation.
        let mut results = run_throttled(
            created,
            MAX_CONCURRENT_GATHER,
            |(idx, session_id, part, scope_path)| async move {
                let summary = build_knowledge_summary(&part);

                let _ = ctx
                    .db
                    .update_session(
                        &session_id,
                        UpdateSession {
                            knowledge_summary: Some(Some(summary.clone())),
                            last_activity: Some(chrono::Utc::now().to_rfc3339()),
                            ..Default::default()
                        },
                    )
                    .await;

                if let Some(dispatcher) = ctx.expert_dispatcher.as_ref() {
                    let prompt = build_capture_prompt(&part, &scope_path);
                    if let Err(e) = dispatcher.dispatch_capture(&session_id, &prompt).await {
                        tracing::warn!(expert = %session_id, "expert capture dispatch failed: {e}");
                    }
                }

                (
                    idx,
                    json!({
                        "session_id": session_id,
                        "area": part.area,
                        "scope_path": scope_path,
                        "file_count": part.files.len(),
                        "knowledge_summary": summary,
                    }),
                )
            },
        )
        .await;

        results.sort_by_key(|(idx, _)| *idx);
        let experts: Vec<Value> = results.into_iter().map(|(_, j)| j).collect();

        Ok(json!({
            "status": "ok",
            "project_id": project_id,
            "experts": experts,
            "count": experts.len(),
        }))
    }

    /// `list_experts` — the expert sessions the caller may consult: experts
    /// scoped to the caller's project plus globally-scoped experts
    /// (`project_id IS NULL`). Compact summaries only — callers use this to
    /// choose a target for `ask_expert`.
    pub(crate) async fn handle_list_experts(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        // Scope-check the target project against the caller's token. With no
        // explicit arg this resolves to the token's own project.
        let scoped = ctx.scope_project(args.get("project_id").and_then(|v| v.as_str()))?;
        let project_id = scoped.into_string();

        tracing::info!(
            session_id = %ctx.session_id,
            project_id = %project_id,
            "MCP tool: list_experts"
        );

        let experts = ctx.db.list_expert_sessions_by_scope(&project_id).await?;

        let items: Vec<Value> = experts
            .iter()
            .map(|e| {
                json!({
                    "session_id": e.id,
                    "name": e.name,
                    "expert_kind": e.expert_kind,
                    "knowledge_area": e.knowledge_area,
                    "knowledge_summary": e.knowledge_summary,
                    "scope_path": e.scope_path,
                    "project_id": e.project_id,
                    "is_permanent": e.is_permanent,
                    "last_activity": e.last_activity,
                })
            })
            .collect();

        Ok(json!({
            "status": "ok",
            "project_id": project_id,
            "experts": items,
            "count": items.len(),
        }))
    }
}

/// Run `f` over `items` with at most `max` running concurrently. Used to
/// enforce the 3-expert gather cap. Results come back in completion order;
/// callers that need input order should carry an index and re-sort.
async fn run_throttled<T, F, Fut, R>(items: Vec<T>, max: usize, f: F) -> Vec<R>
where
    F: Fn(T) -> Fut,
    Fut: std::future::Future<Output = R>,
{
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(max.max(1)));
    let mut futs = FuturesUnordered::new();
    for item in items {
        let sem = sem.clone();
        let f = &f;
        futs.push(async move {
            let _permit = sem.acquire_owned().await.expect("semaphore not closed");
            f(item).await
        });
    }
    let mut out = Vec::with_capacity(futs.len());
    while let Some(r) = futs.next().await {
        out.push(r);
    }
    out
}

/// Partition the codebase rooted at `root` into at most `max_experts`
/// size-balanced slices of adjacent top-level directories.
fn partition_codebase(root: &Path, max_experts: usize) -> Vec<Partition> {
    let mut topics = collect_topics(root);
    // Drop topics with no source files — nothing to know about them.
    topics.retain(|t| !t.files.is_empty());
    if topics.is_empty() {
        return Vec::new();
    }
    // Alphabetical order makes "adjacent" mean neighbouring names, so a
    // contiguous split keeps related dirs (auth / authz / account) together.
    topics.sort_by(|a, b| a.name.cmp(&b.name));

    let total: u64 = topics.iter().map(|t| t.bytes).sum();
    let desired_by_size = total.div_ceil(SMALL_WINDOW_BYTES).max(1) as usize;
    let k = desired_by_size
        .min(max_experts.max(1))
        .min(topics.len())
        .max(1);

    split_contiguous(topics, k)
        .into_iter()
        .map(|group| {
            let dirs: Vec<String> = group.iter().map(|t| t.rel.clone()).collect();
            let mut files: Vec<FileInfo> = Vec::new();
            let mut est_bytes = 0u64;
            for t in &group {
                est_bytes += t.bytes;
                files.extend(t.files.iter().cloned());
            }
            let area = area_label(&group);
            Partition {
                area,
                dirs,
                files,
                est_bytes,
            }
        })
        .collect()
}

/// Split `topics` (already ordered) into `k` contiguous, size-balanced
/// groups. Greedy: close the current group once it reaches the per-group
/// target, while always leaving enough topics to fill the remaining groups.
fn split_contiguous(topics: Vec<Topic>, k: usize) -> Vec<Vec<Topic>> {
    if k <= 1 {
        return vec![topics];
    }
    let total: u64 = topics.iter().map(|t| t.bytes).sum();
    let target = (total as f64 / k as f64).max(1.0);

    let n = topics.len();
    let mut groups: Vec<Vec<Topic>> = Vec::with_capacity(k);
    let mut cur: Vec<Topic> = Vec::new();
    let mut cur_sum = 0u64;

    for (i, t) in topics.into_iter().enumerate() {
        cur_sum += t.bytes;
        cur.push(t);

        let groups_left = k - groups.len(); // including the one we're filling
        let topics_left_after = n - 1 - i; // topics not yet placed
        // Each still-empty later group needs at least one topic.
        let need_for_later = groups_left.saturating_sub(1);

        // Close the current group when it has reached its size target, OR
        // when we must (only just enough topics remain to give every later
        // group one). Never close if it would starve a later group.
        if groups_left > 1
            && topics_left_after >= need_for_later
            && (cur_sum as f64 >= target || topics_left_after == need_for_later)
        {
            groups.push(std::mem::take(&mut cur));
            cur_sum = 0;
        }
    }
    if !cur.is_empty() {
        groups.push(cur);
    }
    groups
}

/// A short human label for a group of topics (its `knowledge_area`).
fn area_label(group: &[Topic]) -> String {
    let names: Vec<&str> = group.iter().map(|t| t.name.as_str()).collect();
    match names.len() {
        0 => "(empty)".to_string(),
        1 => names[0].to_string(),
        2 => format!("{} + {}", names[0], names[1]),
        _ => format!("{} + {} (+{} more)", names[0], names[1], names.len() - 2),
    }
}

/// Scan the immediate children of `root`: each non-ignored subdirectory is a
/// topic; loose source files directly under `root` form a "(root)" topic.
fn collect_topics(root: &Path) -> Vec<Topic> {
    let mut topics = Vec::new();
    let rd = match std::fs::read_dir(root) {
        Ok(rd) => rd,
        Err(_) => return topics,
    };

    let mut root_files: Vec<FileInfo> = Vec::new();
    let mut root_bytes = 0u64;

    for entry in rd.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().to_string();

        if file_type.is_dir() {
            if is_ignored_dir(&name) {
                continue;
            }
            let mut files = Vec::new();
            let mut bytes = 0u64;
            scan_dir(&path, root, 1, &mut files, &mut bytes);
            topics.push(Topic {
                name,
                rel: name_rel(&path, root),
                files,
                bytes,
            });
        } else if file_type.is_file()
            && let Some(lang) = lang_for_path(&path)
        {
            let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
            root_files.push(FileInfo {
                rel: name_rel(&path, root),
                bytes,
                lang,
            });
            root_bytes += bytes;
        }
    }

    if !root_files.is_empty() {
        topics.push(Topic {
            name: "(root)".to_string(),
            rel: ".".to_string(),
            files: root_files,
            bytes: root_bytes,
        });
    }

    topics
}

fn name_rel(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn scan_dir(dir: &Path, base: &Path, depth: usize, files: &mut Vec<FileInfo>, total: &mut u64) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let path = entry.path();
        // `file_type()` uses lstat, so symlinks are reported as symlinks
        // (not dirs/files) and skipped — no symlink-cycle risk.
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if is_ignored_dir(&name) {
                continue;
            }
            scan_dir(&path, base, depth + 1, files, total);
        } else if file_type.is_file()
            && let Some(lang) = lang_for_path(&path)
        {
            let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
            files.push(FileInfo {
                rel: name_rel(&path, base),
                bytes,
                lang,
            });
            *total += bytes;
        }
    }
}

fn is_ignored_dir(name: &str) -> bool {
    // Hidden dirs (.git, .venv, .idea, …) plus common build/vendor output.
    if name.starts_with('.') {
        return true;
    }
    matches!(
        name,
        "node_modules"
            | "target"
            | "dist"
            | "build"
            | "vendor"
            | "out"
            | "bin"
            | "obj"
            | "coverage"
            | "__pycache__"
            | "venv"
    )
}

fn build_knowledge_summary(part: &Partition) -> String {
    let mut files = part.files.clone();
    files.sort_by_key(|f| std::cmp::Reverse(f.bytes));

    let mut langs: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    for f in &files {
        *langs.entry(f.lang).or_insert(0) += 1;
    }
    let lang_summary = langs
        .iter()
        .map(|(lang, n)| format!("{lang}×{n}"))
        .collect::<Vec<_>>()
        .join(", ");

    let mut out = String::new();
    out.push_str(&format!("Knowledge area: {}\n", part.area));
    out.push_str(&format!("Scope (dirs): {}\n", part.dirs.join(", ")));
    out.push_str(&format!(
        "Files: {} source files, ~{} KB. Languages: {}\n",
        files.len(),
        part.est_bytes / 1024,
        if lang_summary.is_empty() {
            "n/a".into()
        } else {
            lang_summary
        }
    ));
    out.push_str("Key files (largest first):\n");
    for f in files.iter().take(MAX_FILES_LISTED) {
        out.push_str(&format!("- {} ({}, {} B)\n", f.rel, f.lang, f.bytes));
    }
    if files.len() > MAX_FILES_LISTED {
        out.push_str(&format!(
            "- … and {} more\n",
            files.len() - MAX_FILES_LISTED
        ));
    }
    out
}

fn build_capture_prompt(part: &Partition, scope_path: &str) -> String {
    format!(
        "You are a long-lived KNOWLEDGE EXPERT for this codebase. Your area is \
         \"{area}\" and your scope is these directories: {scope}.\n\n\
         Eagerly read and summarize the source files in your scope and KEEP that \
         context loaded — you will be asked questions about this area later and \
         should answer from memory without re-reading everything each time. Focus \
         on: the public surface (types, functions, routes, exports), how the pieces \
         fit together, key invariants, and where things live. Do not modify any \
         files; you are read-only. When done, give a concise summary of what you now \
         know about \"{area}\".",
        area = part.area,
        scope = scope_path,
    )
}

fn lang_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let lang = match ext.as_str() {
        "rs" => "Rust",
        "ts" | "mts" | "cts" => "TypeScript",
        "tsx" => "TSX",
        "js" | "mjs" | "cjs" => "JavaScript",
        "jsx" => "JSX",
        "py" => "Python",
        "go" => "Go",
        "java" => "Java",
        "kt" | "kts" => "Kotlin",
        "rb" => "Ruby",
        "php" => "PHP",
        "c" | "h" => "C",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "C++",
        "cs" => "C#",
        "swift" => "Swift",
        "scala" => "Scala",
        "sh" | "bash" | "zsh" => "Shell",
        "sql" => "SQL",
        "css" => "CSS",
        "scss" | "sass" => "Sass",
        "html" | "htm" => "HTML",
        "vue" => "Vue",
        "svelte" => "Svelte",
        "json" => "JSON",
        "toml" => "TOML",
        "yaml" | "yml" => "YAML",
        "md" | "markdown" => "Markdown",
        _ => return None,
    };
    Some(lang)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn write_file(path: &Path, bytes: usize) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, "x".repeat(bytes)).unwrap();
    }

    #[test]
    fn partition_splits_multiple_topics() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Four topic dirs, ~20 KB of source each → ~80 KB total.
        for dir in ["auth", "billing", "core", "ui"] {
            write_file(&root.join(dir).join("mod.rs"), 20_000);
        }
        // Ignored dirs must not become topics or inflate sizes.
        write_file(&root.join("target").join("junk.rs"), 999_999);
        write_file(&root.join("node_modules").join("dep.js"), 999_999);

        let parts = partition_codebase(root, DEFAULT_MAX_EXPERTS);
        assert!(parts.len() > 1, "expected >1 expert, got {}", parts.len());
        // Every partition has a scope and at least one source file.
        for p in &parts {
            assert!(!p.dirs.is_empty());
            assert!(!p.files.is_empty());
            assert!(p.est_bytes > 0);
        }
        // Ignored output never appears in any scope.
        let all_dirs: Vec<&str> = parts
            .iter()
            .flat_map(|p| p.dirs.iter().map(|s| s.as_str()))
            .collect();
        assert!(
            !all_dirs
                .iter()
                .any(|d| d.contains("target") || d.contains("node_modules"))
        );
    }

    #[test]
    fn partition_respects_max_experts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for i in 0..10 {
            write_file(&root.join(format!("dir{i}")).join("a.rs"), 60_000);
        }
        let parts = partition_codebase(root, 3);
        assert!(parts.len() <= 3, "max_experts=3 violated: {}", parts.len());
        assert!(parts.len() >= 2);
    }

    #[test]
    fn partition_empty_when_no_source() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join("docs").join("notes.bin"), 100);
        assert!(partition_codebase(tmp.path(), 4).is_empty());
    }

    #[tokio::test]
    async fn run_throttled_never_exceeds_cap() {
        let inflight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let items: Vec<usize> = (0..30).collect();

        let results = run_throttled(items, MAX_CONCURRENT_GATHER, |i| {
            let inflight = inflight.clone();
            let max_seen = max_seen.clone();
            async move {
                let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(now, Ordering::SeqCst);
                // Yield so concurrent tasks actually overlap.
                tokio::task::yield_now().await;
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                inflight.fetch_sub(1, Ordering::SeqCst);
                i
            }
        })
        .await;

        assert_eq!(results.len(), 30);
        assert!(
            max_seen.load(Ordering::SeqCst) <= MAX_CONCURRENT_GATHER,
            "observed {} concurrent gatherers, cap is {}",
            max_seen.load(Ordering::SeqCst),
            MAX_CONCURRENT_GATHER
        );
    }

    #[test]
    fn split_contiguous_keeps_order_and_count() {
        let topics: Vec<Topic> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|n| Topic {
                name: n.to_string(),
                rel: n.to_string(),
                files: vec![FileInfo {
                    rel: format!("{n}/x.rs"),
                    bytes: 1000,
                    lang: "Rust",
                }],
                bytes: 1000,
            })
            .collect();
        let groups = split_contiguous(topics, 3);
        assert_eq!(groups.len(), 3);
        // Contiguity: flattening restores the original order.
        let flat: Vec<String> = groups
            .iter()
            .flat_map(|g| g.iter().map(|t| t.name.clone()))
            .collect();
        assert_eq!(flat, vec!["a", "b", "c", "d", "e"]);
    }
}
