//! Durable user-Q&A export for the question-experts (card C7).
//!
//! LOCKED DESIGN: the "user Q&A export" IS a set of report files — one
//! GLOBAL set plus one per project — written into the same
//! `<data_dir>/reports/` tree the rest of the report system uses, so they
//! show up unchanged through `GET /api/reports`, `read_report`, and
//! `list_project_reports`.
//!
//! Each resolved user answer fed back to a question-expert (see
//! [`crate::service::question_expert::record_user_answer`]) is appended
//! EAGERLY to the scope's report file. That keeps the export always
//! current and means rehydration after a context reset can read the
//! accumulated Q&A straight back out of the file — a fresh session under
//! the same stable id picks up where it left off
//! ([`crate::service::question_expert::rehydrate_question_expert`]).
//!
//! Unlike `write_report` (date-foldered, collision-suffixed, one file per
//! call), the Q&A export lives at a STABLE, well-known location per scope
//! so rehydration can find it deterministically and entries accumulate
//! into a single growing file.

use std::path::{Path, PathBuf};

/// File name used for every scope's Q&A export within its folder.
pub const QA_REPORT_FILE: &str = "qa.md";

/// Stable report-folder name for a given scope. `None` → the global
/// export; `Some(project_id)` → that project's export. The id is
/// sanitized to the `[A-Za-z0-9_-]` charset the report routes accept as a
/// path segment (project ids are UUIDs, so this is a no-op in practice).
pub fn qa_scope_folder(project_id: Option<&str>) -> String {
    match project_id {
        None => "qa-export-global".to_string(),
        Some(pid) => {
            let safe: String = pid
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect();
            format!("qa-export-project-{safe}")
        }
    }
}

fn qa_report_path(data_dir: &Path, project_id: Option<&str>) -> PathBuf {
    data_dir
        .join("reports")
        .join(qa_scope_folder(project_id))
        .join(QA_REPORT_FILE)
}

/// Append one resolved Q&A entry to the scope's export file, creating the
/// file (with frontmatter) on first write. `now` is an RFC3339 timestamp
/// supplied by the caller so the write is deterministic in tests. Returns
/// the `(folder, file)` the entry landed in.
///
/// The frontmatter is kept compatible with the existing report readers
/// (`routes::reports::parse_frontmatter` and the MCP `list_project_reports`
/// handler): `title:`/`date:`/`sessionId:`/`projectName:` each followed by
/// a single space, values double-quoted. `date` is refreshed to the latest
/// entry's timestamp on every append.
pub fn append_qa_entry(
    data_dir: &Path,
    project_id: Option<&str>,
    project_name: Option<&str>,
    session_id: &str,
    qa_context: &str,
    now: &str,
) -> anyhow::Result<(String, String)> {
    let folder = qa_scope_folder(project_id);
    let dir = data_dir.join("reports").join(&folder);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(QA_REPORT_FILE);

    // Preserve any accumulated entries, then append the new one.
    let existing_body = match std::fs::read_to_string(&path) {
        Ok(content) => strip_frontmatter(&content),
        Err(_) => String::new(),
    };

    let mut body = existing_body.trim_end().to_string();
    if !body.is_empty() {
        body.push_str("\n\n");
    }
    body.push_str(&format!("## Q&A Captured {now}\n\n{}\n", qa_context.trim()));

    let title = match project_name {
        Some(name) => format!("User Q&A Export ({name})"),
        None => "User Q&A Export (Global)".to_string(),
    };

    let mut content =
        format!("---\ntitle: \"{title}\"\ndate: \"{now}\"\nsessionId: \"{session_id}\"");
    if let Some(name) = project_name {
        content.push_str(&format!("\nprojectName: \"{name}\""));
    }
    content.push_str("\n---\n\n");
    content.push_str(body.trim_end());
    content.push('\n');

    std::fs::write(&path, content)?;
    Ok((folder, QA_REPORT_FILE.to_string()))
}

/// Read the accumulated Q&A export body for a scope, or `None` when the
/// scope has no export yet (or it is empty). Frontmatter is stripped — the
/// returned string is the running Q&A log.
pub fn read_qa_export(data_dir: &Path, project_id: Option<&str>) -> anyhow::Result<Option<String>> {
    let path = qa_report_path(data_dir, project_id);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let body = strip_frontmatter(&content);
            let trimmed = body.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Wrap an accumulated Q&A export body into the bootstrap message a fresh
/// question-expert session is seeded with on rehydration, so it resumes
/// with everything it had previously learned from the user.
pub fn build_rehydration_prompt(export_body: &str) -> String {
    format!(
        "[Rehydration — your accumulated user Q&A knowledge] (NOT from the \
         user — restored by Peckboard from your Q&A export so you resume \
         where you left off)\n\nBelow is the user Q&A you have previously \
         captured. Treat it as known context: when a session consults you, \
         answer from this if it covers the question, and only escalate to \
         the user for genuinely new questions.\n\n{}",
        export_body.trim()
    )
}

/// Strip a leading `---\n...\n---` YAML frontmatter block, mirroring the
/// reader in `routes::reports`. Returns the body unchanged when there is
/// no frontmatter.
pub(crate) fn strip_frontmatter(content: &str) -> String {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return content.to_string();
    }
    let rest = &content[3..];
    match rest.find("\n---") {
        Some(end) => rest[end + 4..].trim_start().to_string(),
        None => content.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_folder_global_and_project() {
        assert_eq!(qa_scope_folder(None), "qa-export-global");
        assert_eq!(
            qa_scope_folder(Some("abc-123")),
            "qa-export-project-abc-123"
        );
        // Unsafe chars in a project id are scrubbed to a safe segment.
        assert_eq!(qa_scope_folder(Some("a/b c")), "qa-export-project-a-b-c");
    }

    #[test]
    fn append_creates_then_accumulates() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        let (folder, file) = append_qa_entry(
            dir,
            None,
            None,
            "question-expert-global",
            "**Which DB?**: PostgreSQL",
            "2026-06-09T00:00:00Z",
        )
        .unwrap();
        assert_eq!(folder, "qa-export-global");
        assert_eq!(file, "qa.md");

        append_qa_entry(
            dir,
            None,
            None,
            "question-expert-global",
            "**Which port?**: 8080",
            "2026-06-09T01:00:00Z",
        )
        .unwrap();

        let body = read_qa_export(dir, None).unwrap().unwrap();
        // Both entries survive; the file accumulates.
        assert!(body.contains("PostgreSQL"));
        assert!(body.contains("8080"));
        assert!(body.contains("Which DB?"));
        assert!(body.contains("Which port?"));

        // Frontmatter date refreshes to the latest write and stays valid.
        let raw =
            std::fs::read_to_string(dir.join("reports").join("qa-export-global").join("qa.md"))
                .unwrap();
        assert!(raw.starts_with("---\n"));
        assert!(raw.contains("date: \"2026-06-09T01:00:00Z\""));
        assert!(raw.contains("title: \"User Q&A Export (Global)\""));
    }

    #[test]
    fn project_and_global_exports_are_separate() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        append_qa_entry(
            dir,
            None,
            None,
            "g",
            "global answer",
            "2026-06-09T00:00:00Z",
        )
        .unwrap();
        append_qa_entry(
            dir,
            Some("p1"),
            Some("Proj"),
            "question-expert-project-p1",
            "project answer",
            "2026-06-09T00:00:00Z",
        )
        .unwrap();

        let global = read_qa_export(dir, None).unwrap().unwrap();
        let project = read_qa_export(dir, Some("p1")).unwrap().unwrap();
        assert!(global.contains("global answer"));
        assert!(!global.contains("project answer"));
        assert!(project.contains("project answer"));
        assert!(!project.contains("global answer"));

        // The per-project file carries the project name in frontmatter so
        // the reports listing can attribute it.
        let raw = std::fs::read_to_string(
            dir.join("reports")
                .join("qa-export-project-p1")
                .join("qa.md"),
        )
        .unwrap();
        assert!(raw.contains("projectName: \"Proj\""));
    }

    #[test]
    fn read_missing_export_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_qa_export(tmp.path(), None).unwrap().is_none());
        assert!(read_qa_export(tmp.path(), Some("nope")).unwrap().is_none());
    }

    #[test]
    fn rehydration_prompt_embeds_export() {
        let prompt = build_rehydration_prompt("**Which DB?**: PostgreSQL");
        assert!(prompt.contains("Rehydration"));
        assert!(prompt.contains("PostgreSQL"));
    }
}
