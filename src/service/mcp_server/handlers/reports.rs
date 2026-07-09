use serde_json::Value;

use super::super::McpToolRegistry;
use crate::service::mcp_server::context::ToolCallContext;

/// Root data dir for report storage. Prefer the configured `--data-dir`
/// (threaded through the tool-call context); only fall back to the
/// `~/.peckboard` default when the context has none (unit tests). The
/// HTTP report routes read from `state.config.data_dir`, so writing
/// anywhere else makes reports invisible to the UI.
fn reports_base_dir(ctx: &ToolCallContext) -> std::path::PathBuf {
    ctx.data_dir.clone().unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".peckboard")
    })
}

impl McpToolRegistry {
    pub(crate) async fn handle_write_report(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let title = args
            .get("title")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("write_report requires 'title'"))?;

        let body = args
            .get("body")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("write_report requires 'body'"))?;

        // Write to disk: <dataDir>/reports/<date>/<sanitized-title>.md
        let now = chrono::Utc::now();
        let date_folder = now.format("%Y-%m-%d").to_string();
        let data_dir = reports_base_dir(ctx);
        let reports_dir = data_dir.join("reports").join(&date_folder);
        std::fs::create_dir_all(&reports_dir)?;

        // Sanitize title for filename
        let sanitized: String = title
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
            .replace(' ', "-")
            .to_lowercase();
        let sanitized = if sanitized.is_empty() {
            "report".to_string()
        } else {
            sanitized
        };

        // Collision avoidance
        let mut filename = format!("{sanitized}.md");
        let mut path = reports_dir.join(&filename);
        let mut counter = 1;
        while path.exists() {
            filename = format!("{sanitized}-{counter}.md");
            path = reports_dir.join(&filename);
            counter += 1;
        }

        // Resolve project name for frontmatter
        let project_name = if let Some(ref pid) = ctx.project_id {
            ctx.db.get_project(pid).await.ok().flatten().map(|p| p.name)
        } else {
            None
        };

        // Fetch the session once; used both for the card_id fallback and
        // for the session name / creation date recorded in the frontmatter.
        let session = ctx.db.get_session(&ctx.session_id).await.ok().flatten();

        // Resolve card_id
        let resolved_card_id = if ctx.card_id.is_some() {
            ctx.card_id.clone()
        } else {
            session.as_ref().and_then(|s| s.card_id.clone())
        };

        // Build markdown with YAML frontmatter
        let mut content = format!(
            "---\ntitle: \"{title}\"\ndate: \"{}\"\nsessionId: \"{}\"",
            now.to_rfc3339(),
            ctx.session_id
        );
        if let Some(ref pn) = project_name {
            content.push_str(&format!("\nprojectName: \"{pn}\""));
        }
        if let Some(ref cid) = resolved_card_id {
            content.push_str(&format!("\ncardId: \"{cid}\""));
        }
        if let Some(ref s) = session {
            content.push_str(&format!("\nsessionName: \"{}\"", s.name));
            content.push_str(&format!("\nsessionCreatedAt: \"{}\"", s.created_at));
        }
        content.push_str("\n---\n\n");
        content.push_str(body);

        std::fs::write(&path, &content)?;
        tracing::info!(session_id = %ctx.session_id, path = %path.display(), "Report written to disk");

        // Append system event so it shows in the chat
        ctx.db
            .append_event(
                &ctx.session_id,
                "system",
                serde_json::json!({
                    "text": format!("Report written: {title}"),
                    "reportFolder": date_folder,
                    "reportFile": filename,
                    "cardId": resolved_card_id,
                }),
            )
            .await?;

        // Broadcast card update so UI refreshes reports
        if let Some(ref cid) = resolved_card_id {
            if let Ok(Some(card)) = ctx.db.get_card(cid).await {
                ctx.broadcaster.broadcast(crate::ws::broadcaster::WsEvent {
                    event_type: "card-update".into(),
                    session_id: card.project_id.clone(),
                    data: serde_json::json!({ "card": card }),
                });
            }
        }

        Ok(serde_json::json!({
            "status": "ok",
            "folder": date_folder,
            "file": filename,
            "cardId": resolved_card_id,
        }))
    }

    pub(crate) async fn handle_attach_report_file(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        const ALLOWED_EXTENSIONS: &[&str] = &[
            "png", "pdf", "csv", "json", "txt", "md", "html", "svg", "jpg", "jpeg", "gif", "webp",
        ];
        const MAX_DECODED_SIZE: usize = 10 * 1024 * 1024; // 10 MB

        let folder = args
            .get("folder")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("attach_report_file requires 'folder'"))?;

        let file = args
            .get("file")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("attach_report_file requires 'file'"))?;

        let data_b64 = args
            .get("data")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("attach_report_file requires 'data'"))?;

        let extension = args
            .get("extension")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("attach_report_file requires 'extension'"))?;

        // Validate extension
        let ext_lower = extension.to_lowercase();
        if !ALLOWED_EXTENSIONS.contains(&ext_lower.as_str()) {
            anyhow::bail!(
                "extension '{extension}' not allowed; allowed: {}",
                ALLOWED_EXTENSIONS.join(", ")
            );
        }

        // Sanitize folder and file names to prevent path traversal
        let sanitize = |s: &str| -> String {
            s.chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect()
        };
        let safe_folder = sanitize(folder);
        let safe_file = sanitize(file);

        if safe_folder.is_empty() || safe_file.is_empty() {
            anyhow::bail!("folder and file names must not be empty after sanitization");
        }

        // Decode base64
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .map_err(|e| anyhow::anyhow!("invalid base64 data: {e}"))?;

        if decoded.len() > MAX_DECODED_SIZE {
            anyhow::bail!("file too large: {} bytes exceeds 10MB limit", decoded.len());
        }

        // Write to <dataDir>/reports/<folder>/<file>.<ext>
        let data_dir = reports_base_dir(ctx);
        let reports_dir = data_dir.join("reports").join(&safe_folder);
        std::fs::create_dir_all(&reports_dir)?;

        let filename = format!("{safe_file}.{ext_lower}");
        let path = reports_dir.join(&filename);
        std::fs::write(&path, &decoded)?;

        tracing::info!(path = %path.display(), size = decoded.len(), "Report file attached");

        Ok(serde_json::json!({
            "status": "ok",
            "folder": safe_folder,
            "file": filename,
            "size": decoded.len(),
        }))
    }

    pub(crate) async fn handle_list_project_reports(
        &self,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        tracing::info!(session_id = %ctx.session_id, folder_id = %ctx.folder_id, "MCP tool: list_project_reports");

        let project_id = self.resolve_project_id(ctx).await;
        let project_name = if let Some(ref pid) = project_id {
            ctx.db.get_project(pid).await.ok().flatten().map(|p| p.name)
        } else {
            None
        };

        // Build the set of project NAMES that live in the caller's
        // folder. Reports tag themselves with `projectName` (not id),
        // so we filter against names. Sibling-folder reports become
        // invisible.
        let in_folder_project_names: std::collections::HashSet<String> = ctx
            .db
            .list_projects_by_folder(&ctx.folder_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.name)
            .collect();

        // Scan reports directory for reports matching this project
        let data_dir = reports_base_dir(ctx);
        let reports_dir = data_dir.join("reports");

        let mut reports = Vec::new();
        if reports_dir.exists() {
            if let Ok(folders) = std::fs::read_dir(&reports_dir) {
                for folder_entry in folders.flatten() {
                    let folder_name = folder_entry.file_name().to_string_lossy().to_string();
                    if let Ok(files) = std::fs::read_dir(folder_entry.path()) {
                        for file_entry in files.flatten() {
                            let file_name = file_entry.file_name().to_string_lossy().to_string();
                            if !file_name.ends_with(".md") {
                                continue;
                            }

                            // Read frontmatter to check project match
                            if let Ok(content) = std::fs::read_to_string(file_entry.path()) {
                                let mut title = file_name.clone();
                                let mut session_id = None;
                                let mut report_project = None;
                                let mut session_name = None;
                                let mut session_created_at = None;

                                if content.starts_with("---") {
                                    if let Some(fm) = content.splitn(3, "---").nth(1) {
                                        for line in fm.lines() {
                                            if let Some(v) = line.strip_prefix("title: ") {
                                                title = v.trim_matches('"').to_string();
                                            }
                                            if let Some(v) = line.strip_prefix("sessionId: ") {
                                                session_id = Some(v.trim_matches('"').to_string());
                                            }
                                            if let Some(v) = line.strip_prefix("projectName: ") {
                                                report_project =
                                                    Some(v.trim_matches('"').to_string());
                                            }
                                            if let Some(v) = line.strip_prefix("sessionName: ") {
                                                session_name =
                                                    Some(v.trim_matches('"').to_string());
                                            }
                                            if let Some(v) = line.strip_prefix("sessionCreatedAt: ")
                                            {
                                                session_created_at =
                                                    Some(v.trim_matches('"').to_string());
                                            }
                                        }
                                    }
                                }

                                // Folder boundary:
                                // - A report tagged with a project name is
                                //   visible iff that project lives in the
                                //   caller's folder.
                                // - An untagged report (no projectName) is
                                //   ambient — included only when the caller
                                //   itself has no project context. A worker
                                //   token shouldn't see ambient reports it
                                //   doesn't own; a chat session can.
                                // Within a project, also keep the
                                //   exact-project-name preference so the
                                //   payload stays narrow when the caller has
                                //   a project.
                                let included = match (&project_name, &report_project) {
                                    (Some(pn), Some(rp)) => pn == rp,
                                    (Some(_), None) => false,
                                    (None, Some(rp)) => in_folder_project_names.contains(rp),
                                    (None, None) => true,
                                };
                                if included {
                                    reports.push(serde_json::json!({
                                        "folder": folder_name,
                                        "file": file_name,
                                        "title": title,
                                        "sessionId": session_id,
                                        "projectName": report_project,
                                        "sessionName": session_name,
                                        "sessionCreatedAt": session_created_at,
                                    }));
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(serde_json::json!({ "reports": reports, "count": reports.len() }))
    }

    pub(crate) async fn handle_read_report(
        &self,
        args: Value,
        ctx: &ToolCallContext,
    ) -> anyhow::Result<Value> {
        let folder = args
            .get("folder")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("read_report requires 'folder'"))?;
        let file = args
            .get("file")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("read_report requires 'file'"))?;

        tracing::info!(session_id = %ctx.session_id, folder = %folder, file = %file, "MCP tool: read_report");

        // Sanitize to prevent path traversal
        if folder.contains("..")
            || file.contains("..")
            || folder.contains('/')
            || file.contains('/')
        {
            anyhow::bail!("invalid path");
        }

        let data_dir = reports_base_dir(ctx);
        let path = data_dir.join("reports").join(folder).join(file);

        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|_| anyhow::anyhow!("report not found: {folder}/{file}"))?;

        // Folder boundary: parse the report's `projectName` from
        // frontmatter and require it (when present) to belong to a
        // project in the caller's folder. An untagged report is
        // readable only by a caller without a project context (chat
        // sessions). The "not found" framing mirrors the path-not-found
        // case so the caller can't distinguish "doesn't exist" from
        // "exists but in a sibling folder".
        let mut report_project: Option<String> = None;
        if content.starts_with("---") {
            if let Some(fm) = content.splitn(3, "---").nth(1) {
                for line in fm.lines() {
                    if let Some(v) = line.strip_prefix("projectName: ") {
                        report_project = Some(v.trim_matches('"').to_string());
                    }
                }
            }
        }
        let caller_project_name = match self.resolve_project_id(ctx).await {
            Some(pid) => ctx
                .db
                .get_project(&pid)
                .await
                .ok()
                .flatten()
                .map(|p| p.name),
            None => None,
        };
        let allowed = match (&caller_project_name, &report_project) {
            (Some(pn), Some(rp)) => pn == rp,
            (Some(_), None) => false,
            (None, Some(rp)) => {
                let in_folder: std::collections::HashSet<String> = ctx
                    .db
                    .list_projects_by_folder(&ctx.folder_id)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|p| p.name)
                    .collect();
                in_folder.contains(rp)
            }
            (None, None) => true,
        };
        if !allowed {
            anyhow::bail!("report not found: {folder}/{file}");
        }

        // Strip frontmatter for the body
        let body = if content.starts_with("---") {
            content
                .splitn(3, "---")
                .nth(2)
                .unwrap_or(&content)
                .trim()
                .to_string()
        } else {
            content
        };

        Ok(serde_json::json!({
            "status": "ok",
            "folder": folder,
            "file": file,
            "content": body,
        }))
    }
}
