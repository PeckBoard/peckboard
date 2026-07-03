//! File tools: `search_files`, `list_files`, `read_file`, and `write_file`.
//!
//! All go through the host's folder-scoped file access
//! (`peckboard_list_project_files` / `peckboard_read_file` /
//! `peckboard_write_file`), so they can only ever see — or modify — the
//! caller's own project folder; the host enforces containment (including
//! symlink-escape checks on writes).

use super::edit::hash_text;
use super::host_bridge::{HostCtx, HostFn};

/// Hard caps so a search over a large repo can't run away or flood the agent.
const SEARCH_MAX_FILES: usize = 4000;
const SEARCH_MAX_RESULTS_CAP: u64 = 1000;
const SEARCH_LINE_PREVIEW_CHARS: usize = 400;

// ── list_files ────────────────────────────────────────────────────────

pub fn list_files_tool(
    args: serde_json::Value,
    ctx: &HostCtx,
) -> Result<serde_json::Value, String> {
    let path_contains = args.get("path_contains").and_then(|v| v.as_str());
    let max = args.get("max").and_then(|v| v.as_u64()).unwrap_or(1000) as usize;

    let resp = ctx.call_host(HostFn::ListProjectFiles, &serde_json::json!({}))?;
    let truncated_host = resp["truncated"].as_bool().unwrap_or(false);
    let empty = vec![];
    let all = resp["files"].as_array().unwrap_or(&empty);

    let mut files = Vec::new();
    for f in all {
        let path = f["path"].as_str().unwrap_or("");
        if let Some(sub) = path_contains
            && !path.contains(sub)
        {
            continue;
        }
        files.push(serde_json::json!({ "path": path, "size": f["size"] }));
        if files.len() >= max {
            break;
        }
    }
    Ok(serde_json::json!({
        "files": files,
        "count": files.len(),
        "truncated": truncated_host || files.len() >= max,
    }))
}

// ── read_file ─────────────────────────────────────────────────────────

pub fn read_file_tool(args: serde_json::Value, ctx: &HostCtx) -> Result<serde_json::Value, String> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("`path` (project-relative) is required")?;

    let resp = ctx.call_host(HostFn::ReadFile, &serde_json::json!({ "path": path }))?;
    let content = resp["content"].as_str().unwrap_or("").to_string();
    let truncated = resp["truncated"].as_bool().unwrap_or(false);
    // Whole-file content hash for edit_file's optimistic-concurrency check.
    // A truncated read can't produce a meaningful hash, so omit it.
    let hash = (!truncated).then(|| hash_text(&content));

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    // Optional line window.
    if args.get("start_line").is_some() || args.get("line_count").is_some() {
        let start = args
            .get("start_line")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1) as usize;
        let count = args
            .get("line_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(200)
            .clamp(1, 10_000) as usize;
        let from = (start - 1).min(total);
        let to = (from + count).min(total);
        return Ok(serde_json::json!({
            "path": path,
            "start_line": start,
            "returned_lines": to - from,
            "total_lines": total,
            "has_more": to < total,
            "file_truncated": truncated,
            "hash": hash,
            "content": lines[from..to].join("\n"),
        }));
    }

    Ok(serde_json::json!({
        "path": path,
        "total_lines": total,
        "file_truncated": truncated,
        "hash": hash,
        "content": content,
    }))
}

// ── write_file ────────────────────────────────────────────────────────

pub fn write_file_tool(
    args: serde_json::Value,
    ctx: &HostCtx,
) -> Result<serde_json::Value, String> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("`path` (project-relative) is required")?;
    let content = args
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or("`content` (string) is required")?;
    let append = args
        .get("append")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Default to creating missing parent directories — the common case for an
    // agent writing a new file into a fresh path.
    let create_dirs = args
        .get("create_dirs")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let mut resp = ctx.call_host(
        HostFn::WriteFile,
        &serde_json::json!({
            "path": path,
            "content": content,
            "append": append,
            "create_dirs": create_dirs,
        }),
    )?;
    // Hand back the content hash so a follow-up edit_file can prove it is
    // editing what it just wrote. Unknowable cheaply for appends.
    if !append && let Some(obj) = resp.as_object_mut() {
        obj.insert("hash".into(), serde_json::Value::String(hash_text(content)));
    }
    Ok(resp)
}

// ── search_files ──────────────────────────────────────────────────────

pub fn search_files_tool(
    args: serde_json::Value,
    ctx: &HostCtx,
) -> Result<serde_json::Value, String> {
    use regex::RegexBuilder;

    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or("`query` (string) is required")?;
    let is_regex = args.get("regex").and_then(|v| v.as_bool()).unwrap_or(false);
    let ci = args
        .get("case_insensitive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let path_contains = args.get("path_contains").and_then(|v| v.as_str());
    let max_results = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(200)
        .clamp(1, SEARCH_MAX_RESULTS_CAP) as usize;

    let pattern = if is_regex {
        query.to_string()
    } else {
        regex::escape(query)
    };
    let re = RegexBuilder::new(&pattern)
        .case_insensitive(ci)
        .build()
        .map_err(|e| format!("invalid regex: {e}"))?;

    let resp = ctx.call_host(HostFn::ListProjectFiles, &serde_json::json!({}))?;
    let empty = vec![];
    let all = resp["files"].as_array().unwrap_or(&empty);

    let mut matches = Vec::new();
    let mut files_scanned = 0usize;
    let mut hit_result_cap = false;

    for f in all.iter().take(SEARCH_MAX_FILES) {
        let path = match f["path"].as_str() {
            Some(p) => p,
            None => continue,
        };
        if let Some(sub) = path_contains
            && !path.contains(sub)
        {
            continue;
        }
        // Read the file; skip ones the host won't return (binary/oversized/etc.).
        let file = match ctx.call_host(HostFn::ReadFile, &serde_json::json!({ "path": path })) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let content = file["content"].as_str().unwrap_or("");
        files_scanned += 1;
        for (i, line) in content.lines().enumerate() {
            if re.is_match(line) {
                matches.push(serde_json::json!({
                    "path": path,
                    "line": i + 1,
                    "text": line.chars().take(SEARCH_LINE_PREVIEW_CHARS).collect::<String>(),
                }));
                if matches.len() >= max_results {
                    hit_result_cap = true;
                    break;
                }
            }
        }
        if hit_result_cap {
            break;
        }
    }

    Ok(serde_json::json!({
        "query": query,
        "regex": is_regex,
        "match_count": matches.len(),
        "files_scanned": files_scanned,
        "truncated": hit_result_cap || all.len() > SEARCH_MAX_FILES,
        "matches": matches,
    }))
}
