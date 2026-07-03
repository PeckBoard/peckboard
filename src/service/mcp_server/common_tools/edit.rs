//! `edit_file`: hash-guarded, position-addressed edits to a project file.
//!
//! Instead of resending a whole file, the caller sends the content hash it
//! last saw plus a list of insert/update/delete operations addressed by
//! 1-based line (and optionally column) numbers. The tool re-reads the file,
//! refuses to touch it if the on-disk hash no longer matches (someone else
//! changed it since the caller last read it), applies the operations against
//! the *original* positions, writes the result back, and returns the new hash
//! for the caller to carry into its next edit.
//!
//! Position semantics (documented to the model in `manifest.rs`):
//! - Lines and columns are 1-based; columns count **characters**, not bytes.
//! - All positions refer to the file BEFORE any of this call's edits.
//! - Line mode (columns omitted) operates on whole lines; column mode edits
//!   within lines, with `end_column` exclusive.
//! - Ranges must not overlap across the edit list.

use super::host_bridge::{HostCtx, HostFn};

/// First 16 hex chars (64 bits) of the SHA-256 of the text — ample for
/// change detection while staying cheap to carry around in agent context.
pub fn hash_text(s: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let digest = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(16);
    for b in &digest[..8] {
        let _ = write!(out, "{b:02x}");
    }
    out
}

// ── edit_file tool ────────────────────────────────────────────────────

pub fn edit_file_tool(args: serde_json::Value, ctx: &HostCtx) -> Result<serde_json::Value, String> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("`path` (project-relative) is required")?;
    let original_hash = args
        .get("original_hash")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .ok_or("`original_hash` is required — pass the `hash` returned by read_file, file_outline, read_symbol, write_file, or a previous edit_file")?;
    let edits = args
        .get("edits")
        .cloned()
        .ok_or("`edits` (array of operations) is required")?;

    let resp = ctx.call_host(HostFn::ReadFile, &serde_json::json!({ "path": path }))?;
    if resp["truncated"].as_bool().unwrap_or(false) {
        return Err(format!(
            "{path} exceeds the read cap, so a position-based edit could corrupt it; rewrite it with write_file instead"
        ));
    }
    let content = resp["content"].as_str().unwrap_or("");

    let current = hash_text(content);
    if current != original_hash {
        return Err(format!(
            "hash mismatch for {path}: you passed {original_hash} but the file on disk is {current}. \
             It changed since you last read it — re-read it (read_file / file_outline / read_symbol) \
             and retry against the current content with hash {current}."
        ));
    }

    let new_content = apply_edits(content, &edits)?;
    ctx.call_host(
        HostFn::WriteFile,
        &serde_json::json!({
            "path": path,
            "content": new_content,
            "append": false,
            "create_dirs": false,
        }),
    )?;

    Ok(serde_json::json!({
        "ok": true,
        "path": path,
        "edits_applied": edits.as_array().map(|a| a.len()).unwrap_or(0),
        "hash": hash_text(&new_content),
        "total_lines": new_content.lines().count(),
    }))
}

// ── pure edit engine ──────────────────────────────────────────────────

/// A resolved edit: replace the byte range `start..end` of the original
/// content with `text`. Inserts are zero-width ranges.
struct Splice {
    start: usize,
    end: usize,
    text: String,
}

/// Parse the `edits` JSON against `content` and apply them all, returning the
/// new content. Pure (no host calls) so it is unit-testable off-wasm.
pub fn apply_edits(content: &str, edits: &serde_json::Value) -> Result<String, String> {
    let list = edits.as_array().ok_or("`edits` must be an array")?;
    if list.is_empty() {
        return Err("`edits` is empty — nothing to apply".to_string());
    }

    let ls = line_starts(content);
    let n_lines = content.lines().count();

    let mut splices = Vec::with_capacity(list.len());
    for (i, e) in list.iter().enumerate() {
        splices.push(
            resolve_one(content, &ls, n_lines, e).map_err(|m| format!("edit {}: {m}", i + 1))?,
        );
    }

    // Sort by position (stable on ties via the original index) and reject
    // overlapping ranges — with overlaps the outcome would depend on
    // application order, which the caller can't reason about.
    let mut order: Vec<usize> = (0..splices.len()).collect();
    order.sort_by_key(|&i| (splices[i].start, splices[i].end, i));
    for w in order.windows(2) {
        let (a, b) = (&splices[w[0]], &splices[w[1]]);
        if a.end > b.start {
            return Err(format!(
                "edits {} and {} overlap — each edit must target a distinct range of the original file",
                w[0] + 1,
                w[1] + 1
            ));
        }
    }

    // Apply back-to-front so earlier positions stay valid. For two inserts at
    // the same position, the reverse pass places the earlier-listed one first.
    let mut out = content.to_string();
    for &i in order.iter().rev() {
        let sp = &splices[i];
        out.replace_range(sp.start..sp.end, &sp.text);
    }
    Ok(out)
}

/// Byte offset at which each 1-based line starts. Always begins with 0; a
/// trailing newline yields a final entry equal to `content.len()`.
fn line_starts(content: &str) -> Vec<usize> {
    let mut v = vec![0];
    for (i, b) in content.bytes().enumerate() {
        if b == b'\n' {
            v.push(i + 1);
        }
    }
    v
}

fn opt_pos(e: &serde_json::Value, key: &str) -> Result<Option<usize>, String> {
    match e.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => match v.as_u64() {
            Some(n) if n >= 1 => Ok(Some(n as usize)),
            _ => Err(format!("`{key}` must be a positive integer (1-based)")),
        },
    }
}

fn req_pos(e: &serde_json::Value, key: &str) -> Result<usize, String> {
    opt_pos(e, key)?.ok_or_else(|| format!("`{key}` is required"))
}

/// Byte offset of 1-based (line, column). Columns count characters; column
/// `len + 1` addresses the end of the line (before its newline).
fn byte_at(
    content: &str,
    ls: &[usize],
    n_lines: usize,
    line: usize,
    col: usize,
) -> Result<usize, String> {
    if line < 1 || line > n_lines {
        return Err(format!(
            "line {line} is out of range (file has {n_lines} lines)"
        ));
    }
    let start = ls[line - 1];
    let end = if line < ls.len() {
        ls[line] - 1
    } else {
        content.len()
    };
    let line_str = &content[start..end];
    let n_chars = line_str.chars().count();
    if col > n_chars + 1 {
        return Err(format!(
            "column {col} is beyond the end of line {line} ({n_chars} characters)"
        ));
    }
    let byte = line_str
        .char_indices()
        .nth(col - 1)
        .map(|(b, _)| b)
        .unwrap_or(line_str.len());
    Ok(start + byte)
}

fn resolve_one(
    content: &str,
    ls: &[usize],
    n_lines: usize,
    e: &serde_json::Value,
) -> Result<Splice, String> {
    let op = e
        .get("op")
        .and_then(|v| v.as_str())
        .ok_or("`op` must be \"insert\", \"update\", or \"delete\"")?;

    match op {
        "insert" => {
            let line = req_pos(e, "line")?;
            let text = e
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or("`text` is required for insert")?;
            if let Some(col) = opt_pos(e, "column")? {
                let pos = byte_at(content, ls, n_lines, line, col)?;
                Ok(Splice {
                    start: pos,
                    end: pos,
                    text: text.to_string(),
                })
            } else if line <= n_lines {
                // Whole-line insert before `line`.
                let pos = ls[line - 1];
                let mut t = text.to_string();
                if !t.ends_with('\n') {
                    t.push('\n');
                }
                Ok(Splice {
                    start: pos,
                    end: pos,
                    text: t,
                })
            } else if line == n_lines + 1 {
                // Append at end of file.
                let mut t = text.to_string();
                if !content.is_empty() && !content.ends_with('\n') {
                    t.insert(0, '\n');
                }
                Ok(Splice {
                    start: content.len(),
                    end: content.len(),
                    text: t,
                })
            } else {
                Err(format!(
                    "line {line} is out of range (file has {n_lines} lines; use line {} to append)",
                    n_lines + 1
                ))
            }
        }
        "update" | "delete" => {
            let sl = req_pos(e, "start_line")?;
            let el = req_pos(e, "end_line")?;
            let text = if op == "update" {
                e.get("text")
                    .and_then(|v| v.as_str())
                    .ok_or("`text` is required for update")?
            } else {
                ""
            };
            match (opt_pos(e, "start_column")?, opt_pos(e, "end_column")?) {
                (None, None) => {
                    // Whole-line range, newlines included.
                    if sl > el {
                        return Err(format!("start_line {sl} is after end_line {el}"));
                    }
                    if el > n_lines {
                        return Err(format!(
                            "end_line {el} is out of range (file has {n_lines} lines)"
                        ));
                    }
                    let start = ls[sl - 1];
                    let end = if el < ls.len() { ls[el] } else { content.len() };
                    let mut t = text.to_string();
                    if !t.is_empty() && !t.ends_with('\n') && content[..end].ends_with('\n') {
                        t.push('\n');
                    }
                    Ok(Splice {
                        start,
                        end,
                        text: t,
                    })
                }
                (Some(sc), Some(ec)) => {
                    let start = byte_at(content, ls, n_lines, sl, sc)?;
                    let end = byte_at(content, ls, n_lines, el, ec)?;
                    if end < start {
                        return Err(format!(
                            "range end ({el}:{ec}) is before range start ({sl}:{sc})"
                        ));
                    }
                    Ok(Splice {
                        start,
                        end,
                        text: text.to_string(),
                    })
                }
                _ => Err("provide both start_column and end_column, or neither".to_string()),
            }
        }
        other => Err(format!(
            "unknown op '{other}' — must be \"insert\", \"update\", or \"delete\""
        )),
    }
}

// ── tests (pure logic, run on the host target) ────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn apply(content: &str, edits: serde_json::Value) -> Result<String, String> {
        apply_edits(content, &edits)
    }

    #[test]
    fn hash_is_stable_and_short() {
        assert_eq!(hash_text("hello\n").len(), 16);
        assert_eq!(hash_text("hello\n"), hash_text("hello\n"));
        assert_ne!(hash_text("hello\n"), hash_text("hello"));
    }

    #[test]
    fn insert_whole_line() {
        let out = apply(
            "a\nb\nc\n",
            json!([{ "op": "insert", "line": 2, "text": "x" }]),
        )
        .unwrap();
        assert_eq!(out, "a\nx\nb\nc\n");
    }

    #[test]
    fn insert_appends_at_eof() {
        let out = apply("a\nb", json!([{ "op": "insert", "line": 3, "text": "c" }])).unwrap();
        assert_eq!(out, "a\nb\nc");
        let out = apply(
            "a\nb\n",
            json!([{ "op": "insert", "line": 3, "text": "c" }]),
        )
        .unwrap();
        assert_eq!(out, "a\nb\nc");
        let out = apply("", json!([{ "op": "insert", "line": 1, "text": "c" }])).unwrap();
        assert_eq!(out, "c");
    }

    #[test]
    fn insert_at_column() {
        let out = apply(
            "hello world\n",
            json!([{ "op": "insert", "line": 1, "column": 7, "text": "big " }]),
        )
        .unwrap();
        assert_eq!(out, "hello big world\n");
        // Column len+1 addresses end-of-line.
        let out = apply(
            "hi\n",
            json!([{ "op": "insert", "line": 1, "column": 3, "text": "!" }]),
        )
        .unwrap();
        assert_eq!(out, "hi!\n");
    }

    #[test]
    fn update_whole_lines() {
        let out = apply(
            "a\nb\nc\nd\n",
            json!([{ "op": "update", "start_line": 2, "end_line": 3, "text": "X\nY" }]),
        )
        .unwrap();
        assert_eq!(out, "a\nX\nY\nd\n");
    }

    #[test]
    fn update_last_line_without_trailing_newline() {
        let out = apply(
            "a\nb",
            json!([{ "op": "update", "start_line": 2, "end_line": 2, "text": "B" }]),
        )
        .unwrap();
        assert_eq!(out, "a\nB");
    }

    #[test]
    fn update_column_range() {
        let out = apply(
            "let x = 1;\n",
            json!([{ "op": "update", "start_line": 1, "start_column": 9, "end_line": 1, "end_column": 10, "text": "42" }]),
        )
        .unwrap();
        assert_eq!(out, "let x = 42;\n");
    }

    #[test]
    fn update_column_range_across_lines() {
        let out = apply(
            "foo(a,\n    b)\n",
            json!([{ "op": "update", "start_line": 1, "start_column": 5, "end_line": 2, "end_column": 6, "text": "x, y" }]),
        )
        .unwrap();
        assert_eq!(out, "foo(x, y)\n");
    }

    #[test]
    fn delete_lines_and_columns() {
        let out = apply(
            "a\nb\nc\n",
            json!([{ "op": "delete", "start_line": 2, "end_line": 2 }]),
        )
        .unwrap();
        assert_eq!(out, "a\nc\n");
        let out = apply(
            "abcdef\n",
            json!([{ "op": "delete", "start_line": 1, "start_column": 3, "end_line": 1, "end_column": 5 }]),
        )
        .unwrap();
        assert_eq!(out, "abef\n");
    }

    #[test]
    fn multiple_edits_use_original_positions() {
        // Both positions refer to the original file even though the first
        // edit shifts lines.
        let out = apply(
            "a\nb\nc\nd\n",
            json!([
                { "op": "insert", "line": 1, "text": "top" },
                { "op": "delete", "start_line": 4, "end_line": 4 },
            ]),
        )
        .unwrap();
        assert_eq!(out, "top\na\nb\nc\n");
    }

    #[test]
    fn same_position_inserts_keep_list_order() {
        let out = apply(
            "x\n",
            json!([
                { "op": "insert", "line": 1, "text": "first" },
                { "op": "insert", "line": 1, "text": "second" },
            ]),
        )
        .unwrap();
        assert_eq!(out, "first\nsecond\nx\n");
    }

    #[test]
    fn overlapping_edits_rejected() {
        let err = apply(
            "a\nb\nc\n",
            json!([
                { "op": "delete", "start_line": 1, "end_line": 2 },
                { "op": "update", "start_line": 2, "end_line": 3, "text": "x" },
            ]),
        )
        .unwrap_err();
        assert!(err.contains("overlap"), "{err}");
    }

    #[test]
    fn unicode_columns_count_characters() {
        let out = apply(
            "héllo\n",
            json!([{ "op": "insert", "line": 1, "column": 6, "text": "!" }]),
        )
        .unwrap();
        assert_eq!(out, "héllo!\n");
    }

    #[test]
    fn out_of_range_errors_are_descriptive() {
        let err = apply(
            "a\n",
            json!([{ "op": "update", "start_line": 1, "end_line": 5, "text": "x" }]),
        )
        .unwrap_err();
        assert!(err.contains("end_line 5"), "{err}");
        let err = apply("a\n", json!([{ "op": "insert", "line": 9, "text": "x" }])).unwrap_err();
        assert!(err.contains("out of range"), "{err}");
    }
}
