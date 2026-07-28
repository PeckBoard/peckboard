//! Source adapters for Document Review.
//!
//! A review is always *of something* — a markdown file in a workspace folder,
//! a report under the data dir, or a stored plan. The review itself only ever
//! deals in markdown + versions; these adapters are the two places that know
//! where that markdown came from and how to put it back:
//!
//! - [`load`] — read the source, resolve a display title, return the markdown
//!   that becomes version 1 of the review;
//! - [`write`] — push the review's current version back to the source (the
//!   `apply` endpoint).
//!
//! Every `source_ref` is a string chosen so a review row addresses its source
//! without a second column per kind:
//!
//! | kind     | `source_ref`                     |
//! | -------- | -------------------------------- |
//! | `file`   | `<folder_id>:<relative/path.md>` |
//! | `report` | `<YYYY-MM-DD>/<file.md>`         |
//! | `plan`   | `<plan_id>`                      |
//!
//! Reports carry YAML frontmatter that is *metadata about the report*, not
//! part of the document a human reviews. So the report adapter loads the body
//! only and re-attaches the existing frontmatter on write — the same
//! body-only replacement `PUT /api/reports/:folder/:file` performs. Losing
//! that frontmatter would orphan the report from its session and date.

use std::path::{Path, PathBuf};

use crate::db::Db;
use crate::routes::reports::{extract_frontmatter, safe_segment, strip_frontmatter};
use crate::service::fs_jail;
use crate::state::AppState;

/// The three source kinds a review may be created from.
pub const SOURCE_KINDS: [&str; 3] = ["file", "report", "plan"];

/// A document as its source hands it over.
pub struct SourceDoc {
    pub title: String,
    pub markdown: String,
}

/// Read the document a review points at. `Err` is a human-readable reason the
/// source can't be reviewed (unknown kind, malformed ref, missing file, not
/// text, too large) — the create endpoint surfaces it verbatim.
pub async fn load(
    state: &AppState,
    source_kind: &str,
    source_ref: &str,
) -> Result<SourceDoc, String> {
    match source_kind {
        "file" => load_file(&state.db, source_ref).await,
        "report" => load_report(&state.config.data_dir, source_ref),
        "plan" => load_plan(&state.db, source_ref).await,
        other => Err(format!("unknown source_kind {other:?}")),
    }
}

/// Write `markdown` back to the source. Used by `POST
/// /api/doc-reviews/{id}/apply`; never called implicitly by a revision, so a
/// review can run any number of passes without touching the original.
pub async fn write(
    state: &AppState,
    source_kind: &str,
    source_ref: &str,
    markdown: &str,
) -> Result<(), String> {
    match source_kind {
        "file" => write_file(&state.db, source_ref, markdown).await,
        "report" => write_report(&state.config.data_dir, source_ref, markdown),
        "plan" => write_plan(&state.db, source_ref, markdown).await,
        other => Err(format!("unknown source_kind {other:?}")),
    }
}

/// The folder a review is scoped to, derived from its source. Only `file`
/// sources live in a workspace folder; reports and plans don't.
pub fn folder_id_for(source_kind: &str, source_ref: &str) -> Option<String> {
    match source_kind {
        "file" => split_file_ref(source_ref)
            .ok()
            .map(|(folder_id, _)| folder_id.to_string()),
        _ => None,
    }
}

// ── file ─────────────────────────────────────────────────────────────

/// Split `"<folder_id>:<relative/path.md>"`. Rejects a non-`.md` path here so
/// a caller can't use the review surface as a generic file reader.
fn split_file_ref(source_ref: &str) -> Result<(&str, &str), String> {
    let (folder_id, rel) = source_ref
        .split_once(':')
        .ok_or_else(|| "file source_ref must be \"<folder_id>:<relative/path.md>\"".to_string())?;
    if folder_id.is_empty() || rel.is_empty() {
        return Err("file source_ref must be \"<folder_id>:<relative/path.md>\"".to_string());
    }
    if !is_markdown(Path::new(rel)) {
        return Err("only .md files can be reviewed".to_string());
    }
    Ok((folder_id, rel))
}

/// `.md`, case-insensitively — the only extension the review surface reads.
pub fn is_markdown(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("md"))
}

/// Canonicalized on-disk root of a workspace folder — the jail root every
/// file-source path is resolved against.
pub async fn folder_root(db: &Db, folder_id: &str) -> Result<PathBuf, String> {
    let folder = db
        .get_folder(folder_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "folder not found".to_string())?;
    std::fs::canonicalize(&folder.path).map_err(|e| format!("folder path unavailable: {e}"))
}

/// Read one jailed markdown file under a folder root, capped at
/// [`fs_jail::MAX_READ_BYTES`]. Shared with the folder markdown-file route so
/// both refuse the same things for the same reasons.
pub fn read_markdown(root: &Path, rel: &str) -> Result<String, String> {
    let path = fs_jail::resolve_read(root, Path::new(rel))?;
    let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    if meta.len() as usize > fs_jail::MAX_READ_BYTES {
        return Err(format!(
            "file exceeds the {}-byte read limit",
            fs_jail::MAX_READ_BYTES
        ));
    }
    std::fs::read_to_string(&path).map_err(|_| "file is not valid UTF-8 text".to_string())
}

async fn load_file(db: &Db, source_ref: &str) -> Result<SourceDoc, String> {
    let (folder_id, rel) = split_file_ref(source_ref)?;
    let root = folder_root(db, folder_id).await?;
    let markdown = read_markdown(&root, rel)?;
    let fallback = file_stem(rel);
    Ok(SourceDoc {
        title: resolve_title(&markdown, &fallback),
        markdown,
    })
}

async fn write_file(db: &Db, source_ref: &str, markdown: &str) -> Result<(), String> {
    let (folder_id, rel) = split_file_ref(source_ref)?;
    let root = folder_root(db, folder_id).await?;
    // `create_dirs: false` — apply overwrites a document that already exists;
    // it never materializes a new tree in the user's workspace.
    let path = fs_jail::resolve_write(&root, Path::new(rel), false)?;
    std::fs::write(&path, markdown).map_err(|e| format!("write failed: {e}"))
}

// ── report ───────────────────────────────────────────────────────────

/// Split `"<YYYY-MM-DD>/<file.md>"` into validated path segments. Reuses the
/// reports module's strict segment validator, so the review surface can't
/// reach outside the reports tree either.
fn split_report_ref(source_ref: &str) -> Result<(String, String), String> {
    let (folder, file) = source_ref
        .split_once('/')
        .ok_or_else(|| "report source_ref must be \"<folder>/<file.md>\"".to_string())?;
    match (safe_segment(folder, false), safe_segment(file, true)) {
        (Some(f), Some(fi)) => Ok((f, fi)),
        _ => Err("invalid report path".to_string()),
    }
}

fn report_path(data_dir: &Path, source_ref: &str) -> Result<PathBuf, String> {
    let (folder, file) = split_report_ref(source_ref)?;
    Ok(data_dir.join("reports").join(folder).join(file))
}

fn load_report(data_dir: &Path, source_ref: &str) -> Result<SourceDoc, String> {
    let path = report_path(data_dir, source_ref)?;
    let raw = std::fs::read_to_string(&path).map_err(|_| "report not found".to_string())?;
    let fallback = file_stem(source_ref);
    // Title comes from the *raw* content (frontmatter included); the body the
    // reviewer sees is frontmatter-free.
    Ok(SourceDoc {
        title: resolve_title(&raw, &fallback),
        markdown: strip_frontmatter(&raw),
    })
}

fn write_report(data_dir: &Path, source_ref: &str, markdown: &str) -> Result<(), String> {
    let path = report_path(data_dir, source_ref)?;
    let existing = std::fs::read_to_string(&path).map_err(|_| "report not found".to_string())?;
    let content = merge_report_frontmatter(&existing, markdown);
    std::fs::write(&path, content).map_err(|e| format!("write failed: {e}"))
}

/// Re-attach `existing`'s YAML frontmatter to a replacement body. A report
/// whose frontmatter was dropped loses its session link, date, and title —
/// the reports list would then sort it last and label it by filename.
pub(crate) fn merge_report_frontmatter(existing: &str, body: &str) -> String {
    match extract_frontmatter(existing) {
        Some(fm) => format!("---\n{fm}\n---\n\n{body}"),
        None => body.to_string(),
    }
}

// ── plan ─────────────────────────────────────────────────────────────

async fn load_plan(db: &Db, plan_id: &str) -> Result<SourceDoc, String> {
    let plan = db
        .get_plan(plan_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "plan not found".to_string())?;
    Ok(SourceDoc {
        title: plan.title,
        markdown: plan.markdown,
    })
}

async fn write_plan(db: &Db, plan_id: &str, markdown: &str) -> Result<(), String> {
    let plan = db
        .get_plan(plan_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "plan not found".to_string())?;
    // `upsert_plan` revises the authoring session's plan in place and bumps
    // its version — a session only ever has the one plan row, so this updates
    // exactly the plan we just read, and the plan viewer sees a new version
    // rather than a silent edit.
    db.upsert_plan(
        &plan.session_id,
        plan.card_id.as_deref(),
        plan.project_id.as_deref(),
        &plan.title,
        markdown,
    )
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())
}

// ── titles ───────────────────────────────────────────────────────────

/// The last path segment without its `.md` suffix.
fn file_stem(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.trim_end_matches(".md").to_string()
}

/// Display title for a document: a frontmatter `title:`, else the first
/// ATX `#` heading, else `fallback` (the filename).
pub fn resolve_title(content: &str, fallback: &str) -> String {
    if let Some(fm) = extract_frontmatter(content) {
        for line in fm.lines() {
            if let Some(val) = line.strip_prefix("title:") {
                let t = val.trim().trim_matches('"').trim().to_string();
                if !t.is_empty() {
                    return t;
                }
            }
        }
    }
    for line in strip_frontmatter(content).lines() {
        if let Some(heading) = line.strip_prefix("# ") {
            let t = heading.trim().to_string();
            if !t.is_empty() {
                return t;
            }
        }
    }
    if fallback.is_empty() {
        "Untitled".to_string()
    } else {
        fallback.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_write_preserves_frontmatter_and_replaces_body() {
        let existing = "---\ntitle: Weekly Report\ndate: 2026-07-28T10:00:00Z\nsession_id: s1\n---\n\n# Weekly Report\n\nold body\n";
        let merged = merge_report_frontmatter(existing, "# Weekly Report\n\nnew body\n");
        assert!(
            merged.starts_with(
                "---\ntitle: Weekly Report\ndate: 2026-07-28T10:00:00Z\nsession_id: s1\n---\n\n"
            ),
            "frontmatter must survive the write: {merged}"
        );
        assert!(merged.contains("new body"), "body replaced: {merged}");
        assert!(!merged.contains("old body"), "old body gone: {merged}");
        // A frontmatter-less report round-trips as the plain body.
        assert_eq!(merge_report_frontmatter("# Plain\n", "# New\n"), "# New\n");
    }

    #[test]
    fn report_roundtrip_through_the_adapter_keeps_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("reports").join("2026-07-28");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("audit.md"),
            "---\ntitle: Audit\nsession_id: s9\n---\n\n# Audit\n\nfindings\n",
        )
        .unwrap();

        let doc = load_report(dir.path(), "2026-07-28/audit.md").unwrap();
        assert_eq!(doc.title, "Audit");
        assert!(
            !doc.markdown.contains("session_id"),
            "body is frontmatter-free: {}",
            doc.markdown
        );

        write_report(dir.path(), "2026-07-28/audit.md", "# Audit\n\nrevised\n").unwrap();
        let raw = std::fs::read_to_string(folder.join("audit.md")).unwrap();
        assert!(raw.contains("session_id: s9"), "metadata kept: {raw}");
        assert!(raw.contains("revised"), "body applied: {raw}");
    }

    #[test]
    fn titles_prefer_frontmatter_then_heading_then_filename() {
        assert_eq!(
            resolve_title("---\ntitle: From Meta\n---\n\n# Heading\n", "file"),
            "From Meta"
        );
        assert_eq!(resolve_title("intro\n\n# Heading\n", "file"), "Heading");
        assert_eq!(resolve_title("no headings here\n", "notes"), "notes");
    }

    #[test]
    fn file_refs_reject_non_markdown_and_malformed_shapes() {
        assert!(split_file_ref("f1:docs/plan.md").is_ok());
        assert!(split_file_ref("f1:docs/plan.txt").is_err());
        assert!(split_file_ref("no-colon").is_err());
        assert!(split_file_ref("f1:").is_err());
        // Traversal is refused by the jail, not by the ref parser, so the
        // parser accepts the shape and `resolve_read` rejects the path.
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        assert!(
            read_markdown(&root, "../escape.md")
                .unwrap_err()
                .contains("within the project folder")
        );
    }
}
