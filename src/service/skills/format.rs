//! `SKILL.md` (and team-memory `.md`) files: a YAML-ish frontmatter block
//! between `---` fences, then a markdown body.
//!
//! Only the flat subset the contract uses is understood — `key: value`
//! scalars (plain, `"double"` or `'single'` quoted) and string lists, inline
//! (`[a, b]`) or block (`- a` lines). Unknown keys are ignored, so a
//! Claude-native skill with extra fields still parses. No YAML dependency:
//! the format is ours on both ends.

use std::collections::BTreeMap;

/// A parsed frontmatter value.
#[derive(Debug, Clone, PartialEq)]
pub enum FmValue {
    Scalar(String),
    List(Vec<String>),
}

impl FmValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            FmValue::Scalar(s) => Some(s),
            FmValue::List(_) => None,
        }
    }

    pub fn as_list(&self) -> Vec<String> {
        match self {
            FmValue::List(v) => v.clone(),
            // `tags: review` — a lone scalar reads as a one-item list.
            FmValue::Scalar(s) if !s.is_empty() => vec![s.clone()],
            FmValue::Scalar(_) => Vec::new(),
        }
    }
}

/// Split `text` into its frontmatter map and body. Text with no leading
/// `---` fence is all body.
pub fn split_frontmatter(text: &str) -> (BTreeMap<String, FmValue>, String) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().unwrap_or("");
    if first.trim_end() != "---" {
        return (BTreeMap::new(), text.to_string());
    }
    let mut fm_lines: Vec<&str> = Vec::new();
    let mut consumed = first.len();
    let mut closed = false;
    for line in lines.by_ref() {
        consumed += line.len();
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        fm_lines.push(line.trim_end_matches(['\n', '\r']));
    }
    if !closed {
        return (BTreeMap::new(), text.to_string());
    }
    let body = text[consumed..].to_string();
    (parse_fm_lines(&fm_lines), body)
}

fn parse_fm_lines(lines: &[&str]) -> BTreeMap<String, FmValue> {
    let mut map = BTreeMap::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.trim().is_empty() || line.trim_start().starts_with('#') || line.starts_with(' ') {
            continue;
        }
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_string();
        let rest = strip_comment(rest.trim());
        if rest.is_empty() {
            // Block list (`- item` lines) or an empty scalar.
            let mut items = Vec::new();
            while i < lines.len() {
                let l = lines[i].trim_start();
                if let Some(item) = l.strip_prefix("- ").or_else(|| (l == "-").then_some("")) {
                    items.push(unquote(item.trim()));
                    i += 1;
                } else if l.is_empty() {
                    i += 1;
                } else {
                    break;
                }
            }
            if items.is_empty() {
                map.insert(key, FmValue::Scalar(String::new()));
            } else {
                map.insert(key, FmValue::List(items));
            }
        } else if let Some(inner) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            let items = split_inline_list(inner)
                .into_iter()
                .map(|s| unquote(s.trim()))
                .filter(|s| !s.is_empty())
                .collect();
            map.insert(key, FmValue::List(items));
        } else {
            map.insert(key, FmValue::Scalar(unquote(rest)));
        }
    }
    map
}

/// Drop a trailing ` # comment` from an unquoted value.
fn strip_comment(v: &str) -> &str {
    if v.starts_with('"') || v.starts_with('\'') {
        return v;
    }
    match v.find(" #") {
        Some(i) => v[..i].trim_end(),
        None => v,
    }
}

/// Split `a, "b, c", d` on commas outside quotes.
fn split_inline_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match (quote, c) {
            (None, '"' | '\'') => {
                quote = Some(c);
                cur.push(c);
            }
            (Some(q), _) if c == q => {
                quote = None;
                cur.push(c);
            }
            (None, ',') => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

fn unquote(v: &str) -> String {
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        let inner = &v[1..v.len() - 1];
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(o) => out.push(o),
                    None => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        out
    } else if v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'') {
        v[1..v.len() - 1].replace("''", "'")
    } else {
        v.to_string()
    }
}

/// Quote a scalar when plain YAML would misread it.
pub fn yaml_scalar(v: &str) -> String {
    let needs = v.is_empty()
        || v.contains(": ")
        || v.contains(" #")
        || v.contains('\n')
        || v.ends_with(':')
        || v.starts_with(|c: char| {
            matches!(
                c,
                '-' | '['
                    | ']'
                    | '{'
                    | '}'
                    | '#'
                    | '&'
                    | '*'
                    | '!'
                    | '|'
                    | '>'
                    | '\''
                    | '"'
                    | '%'
                    | '@'
                    | '`'
                    | ' '
            )
        })
        || v.ends_with(' ');
    if !needs {
        return v.to_string();
    }
    let escaped = v
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

fn yaml_list(items: &[String]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|s| {
            if s.contains(',') || s.contains(']') || s.contains('[') {
                format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
            } else {
                yaml_scalar(s)
            }
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

/// A `SKILL.md` document.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillDoc {
    pub name: String,
    pub description: String,
    pub version: String,
    pub tags: Vec<String>,
    pub groups: Vec<String>,
    pub body: String,
}

/// Parse a `SKILL.md`. `name` and `version` are required.
pub fn parse_skill(text: &str) -> Result<SkillDoc, String> {
    let (fm, body) = split_frontmatter(text);
    let get = |k: &str| fm.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let name = get("name");
    if !valid_name(&name) {
        return Err(format!("invalid or missing skill name '{name}'"));
    }
    let version = get("version");
    if semver::Version::parse(&version).is_err() {
        return Err(format!("invalid or missing semver version '{version}'"));
    }
    Ok(SkillDoc {
        name,
        description: get("description"),
        version,
        tags: fm.get("tags").map(FmValue::as_list).unwrap_or_default(),
        groups: fm.get("groups").map(FmValue::as_list).unwrap_or_default(),
        body: body.trim_start_matches(['\n', '\r']).to_string(),
    })
}

/// Render a `SKILL.md` (the exact shape the contract specifies).
pub fn serialize_skill(doc: &SkillDoc) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("name: {}\n", doc.name));
    out.push_str(&format!("description: {}\n", yaml_scalar(&doc.description)));
    out.push_str(&format!("version: {}\n", doc.version));
    out.push_str(&format!("tags: {}\n", yaml_list(&doc.tags)));
    out.push_str(&format!("groups: {}\n", yaml_list(&doc.groups)));
    out.push_str("---\n");
    out.push_str(&doc.body);
    if !doc.body.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// A team-memory note (`memory/<slug>.md`, frontmatter `title`, `updated`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MemoryDoc {
    pub slug: String,
    pub title: String,
    pub updated: String,
    pub body: String,
}

pub fn parse_memory(slug: &str, text: &str) -> MemoryDoc {
    let (fm, body) = split_frontmatter(text);
    let get = |k: &str| fm.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let title = get("title");
    MemoryDoc {
        slug: slug.to_string(),
        title: if title.is_empty() {
            slug.to_string()
        } else {
            title
        },
        updated: get("updated"),
        body: body.trim_start_matches(['\n', '\r']).to_string(),
    }
}

pub fn serialize_memory(doc: &MemoryDoc) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("title: {}\n", yaml_scalar(&doc.title)));
    out.push_str(&format!("updated: {}\n", yaml_scalar(&doc.updated)));
    out.push_str("---\n");
    out.push_str(&doc.body);
    if !doc.body.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// `^[a-z0-9][a-z0-9-]{0,63}$` — skill names and group/memory slugs.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// Semver bump kinds accepted by `PUT /api/skills/{id}`.
pub fn bump_version(current: &str, kind: &str) -> Result<String, String> {
    let mut v = semver::Version::parse(current)
        .map_err(|_| format!("current version '{current}' is not semver"))?;
    match kind {
        "patch" => v.patch += 1,
        "minor" => {
            v.minor += 1;
            v.patch = 0;
        }
        "major" => {
            v.major += 1;
            v.minor = 0;
            v.patch = 0;
        }
        other => {
            return Err(format!(
                "invalid bump '{other}' — use patch, minor or major"
            ));
        }
    }
    v.pre = semver::Prerelease::EMPTY;
    v.build = semver::BuildMetadata::EMPTY;
    Ok(v.to_string())
}

/// Sort version labels newest first (unparseable labels last, by text).
pub fn sort_versions_desc(versions: &mut [String]) {
    versions.sort_by(
        |a, b| match (semver::Version::parse(a), semver::Version::parse(b)) {
            (Ok(x), Ok(y)) => y.cmp(&x),
            (Ok(_), Err(_)) => std::cmp::Ordering::Less,
            (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
            (Err(_), Err(_)) => b.cmp(a),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_roundtrips_through_frontmatter() {
        let doc = SkillDoc {
            name: "pr-review-checklist".into(),
            description: "Use it: when reviewing a PR # carefully".into(),
            version: "1.2.0".into(),
            tags: vec!["review".into(), "git".into()],
            groups: vec!["pr-review".into()],
            body: "# Steps\n\n1. Read the diff.\n".into(),
        };
        let text = serialize_skill(&doc);
        assert!(text.starts_with("---\nname: pr-review-checklist\n"));
        assert_eq!(parse_skill(&text).unwrap(), doc);
    }

    #[test]
    fn parses_contract_example_with_comments_and_block_lists() {
        let text = "---\nname: pr-review-checklist        # kebab-case\ndescription: One line, when to use it\nversion: 1.2.0                   # semver\ntags:\n  - review\n  - git\ngroups: [pr-review]\nextra: ignored\n---\nbody\n";
        let doc = parse_skill(text).unwrap();
        assert_eq!(doc.name, "pr-review-checklist");
        assert_eq!(doc.version, "1.2.0");
        assert_eq!(doc.tags, vec!["review", "git"]);
        assert_eq!(doc.groups, vec!["pr-review"]);
        assert_eq!(doc.body, "body\n");
        assert!(parse_skill("---\nname: Bad Name\nversion: 1.0.0\n---\n").is_err());
        assert!(parse_skill("---\nname: ok\nversion: one\n---\n").is_err());
    }

    #[test]
    fn bumps_follow_semver() {
        assert_eq!(bump_version("1.2.3", "patch").unwrap(), "1.2.4");
        assert_eq!(bump_version("1.2.3", "minor").unwrap(), "1.3.0");
        assert_eq!(bump_version("1.2.3", "major").unwrap(), "2.0.0");
        assert!(bump_version("1.2.3", "huge").is_err());
        let mut v = vec!["1.0.0".to_string(), "1.10.0".into(), "1.2.0".into()];
        sort_versions_desc(&mut v);
        assert_eq!(v, vec!["1.10.0", "1.2.0", "1.0.0"]);
    }
}
