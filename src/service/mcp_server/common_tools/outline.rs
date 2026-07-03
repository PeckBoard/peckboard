//! Deterministic (non-AI) code outline parsers, plus the `file_outline` and
//! `read_symbol` tools built on them.
//!
//! The goal is to let an agent find the part of a file it needs — by function
//! or class name — without pulling the whole file into context. The parsers
//! are heuristic, regex + brace/indent matching (tree-sitter's C runtime
//! doesn't build for this plugin's wasm32-unknown-unknown target), which is
//! plenty for locating symbols and their line ranges:
//!
//! - **Rust** — fn, struct, enum, trait, union, mod, impl, type, const/static,
//!   macro_rules!; descends into trait/mod/impl for methods.
//! - **TypeScript/JavaScript** — function, class (+ methods), interface,
//!   type, enum, namespace, and const/let/var bound to a function or arrow.
//! - **Python** — def/class by indentation, nested defs and methods included.
//! - **Go** — func (methods get their receiver type as parent), type
//!   struct/interface/alias.
//! - **Java/Kotlin** — class/interface/enum/object/record (+ methods,
//!   constructors), Kotlin fun.
//! - **C/C++** — struct/union/enum/class/namespace, function definitions
//!   (prototypes are skipped), #define macros; class/struct/namespace are
//!   descended for C++ methods.
//!
//! Block extents are found with a small scanner that tracks braces while
//! skipping string literals and comments, so a `}` in a string doesn't end a
//! function early. Python uses indentation instead.

use super::edit::hash_text;
use super::host_bridge::{HostCtx, HostFn};
use regex::Regex;
use std::sync::LazyLock;

/// Cap on symbols returned by `file_outline` when the caller doesn't ask.
const OUTLINE_DEFAULT_MAX: usize = 200;
/// Above this many exact-name matches, `read_symbol` returns metadata only.
const READ_SYMBOL_MAX_BODIES: usize = 3;

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).unwrap());
    };
}

// ── language detection ────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Rust,
    Js,
    Python,
    Go,
    JavaLike,
    C,
}

impl Lang {
    fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Js => "typescript/javascript",
            Lang::Python => "python",
            Lang::Go => "go",
            Lang::JavaLike => "java/kotlin",
            Lang::C => "c/c++",
        }
    }
}

pub fn detect_lang(path: &str) -> Option<Lang> {
    let ext = std::path::Path::new(path)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => Some(Lang::Rust),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" => Some(Lang::Js),
        "py" | "pyi" => Some(Lang::Python),
        "go" => Some(Lang::Go),
        "java" | "kt" | "kts" => Some(Lang::JavaLike),
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "ino" => Some(Lang::C),
        _ => None,
    }
}

// ── symbols ───────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct Symbol {
    pub name: String,
    pub kind: &'static str,
    pub parent: Option<String>,
    /// 1-based, inclusive.
    pub start_line: usize,
    /// 1-based, inclusive.
    pub end_line: usize,
    pub signature: String,
}

impl Symbol {
    fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "name": self.name,
            "kind": self.kind,
            "start_line": self.start_line,
            "end_line": self.end_line,
            "signature": self.signature,
        });
        if let Some(p) = &self.parent {
            v["parent"] = serde_json::Value::String(p.clone());
        }
        v
    }
}

/// What a declaration-start line was recognized as.
struct Decl {
    name: String,
    kind: &'static str,
    /// Containers (class/impl/trait/…) are descended into for members;
    /// non-containers (functions) have their bodies skipped.
    container: bool,
    /// Parent carried by the declaration itself (a Go method's receiver
    /// type); otherwise the enclosing container from the walk is used.
    parent: Option<String>,
}

// ── outline: dispatcher ───────────────────────────────────────────────

pub fn outline(content: &str, lang: Lang) -> Vec<Symbol> {
    let lines: Vec<&str> = content.lines().collect();
    if lang == Lang::Python {
        return python_outline(&lines);
    }
    brace_outline(&lines, lang)
}

fn brace_outline(lines: &[&str], lang: Lang) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    // (name, kind, end_line 1-based) of enclosing containers.
    let mut stack: Vec<(String, &'static str, usize)> = Vec::new();
    let mut in_block_comment = false;
    let mut i = 0;
    while i < lines.len() {
        while stack.last().is_some_and(|s| s.2 < i + 1) {
            stack.pop();
        }
        let line = lines[i];
        if in_block_comment {
            if let Some(p) = line.find("*/") {
                in_block_comment = line[p + 2..].contains("/*") && !line[p + 2..].contains("*/");
            }
            i += 1;
            continue;
        }
        let t = line.trim();
        if t.is_empty() || t.starts_with("//") {
            i += 1;
            continue;
        }
        let container = stack.last().map(|(n, k, _)| (n.as_str(), *k));
        if let Some(mut d) = match_decl(lang, t, container) {
            if lang == Lang::C && d.kind == "macro" {
                // #define: extent is the line plus backslash continuations.
                let mut end = i;
                while lines[end].trim_end().ends_with('\\') && end + 1 < lines.len() {
                    end += 1;
                }
                symbols.push(Symbol {
                    name: d.name,
                    kind: d.kind,
                    parent: d
                        .parent
                        .take()
                        .or_else(|| stack.last().map(|s| s.0.clone())),
                    start_line: i + 1,
                    end_line: end + 1,
                    signature: truncate_sig(t),
                });
                i = end + 1;
                continue;
            }
            let ext = block_extent(lines, i, lang);
            // A C "function" without a body is a prototype/call — skip it.
            if lang == Lang::C && (d.kind == "function" || d.kind == "method") && !ext.has_body {
                i += 1;
                continue;
            }
            symbols.push(Symbol {
                name: d.name.clone(),
                kind: d.kind,
                parent: d
                    .parent
                    .take()
                    .or_else(|| stack.last().map(|s| s.0.clone())),
                start_line: i + 1,
                end_line: ext.end_line,
                signature: ext.signature,
            });
            if d.container && ext.has_body && ext.end_line > i + 1 {
                stack.push((d.name, d.kind, ext.end_line));
                i += 1;
            } else {
                // Skip the body: next 0-based index == 1-based end_line.
                i = ext.end_line.max(i + 1);
            }
        } else {
            if let Some(p) = line.rfind("/*")
                && !line[p + 2..].contains("*/")
            {
                in_block_comment = true;
            }
            i += 1;
        }
    }
    symbols
}

// ── block extent scanner ──────────────────────────────────────────────

struct Extent {
    /// 1-based, inclusive.
    end_line: usize,
    signature: String,
    /// Whether a `{ … }` block was actually opened (vs a `;`-terminated decl).
    has_body: bool,
}

/// From the declaration at `lines[start]`, find where its block ends by
/// counting braces — skipping strings, chars, and comments so punctuation in
/// literals doesn't confuse the count. A `;` at depth 0 before any `{` ends a
/// body-less declaration; for the semicolon-less languages (Kotlin, bare JS,
/// Go) a braceless statement ends at the first line that is complete
/// (balanced parens, no trailing operator) and isn't continued below.
fn block_extent(lines: &[&str], start: usize, lang: Lang) -> Extent {
    let mut depth: i32 = 0;
    let mut paren: i32 = 0;
    let mut opened = false;
    let mut in_block_comment = false;
    let mut sig_lines: Vec<&str> = Vec::new();

    for (li, line) in lines.iter().enumerate().skip(start) {
        if !opened && sig_lines.len() < 4 {
            sig_lines.push(line.trim());
        }
        let mut chars = line.chars().peekable();
        let mut in_str: Option<char> = None;
        while let Some(c) = chars.next() {
            if in_block_comment {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    in_block_comment = false;
                }
                continue;
            }
            if let Some(q) = in_str {
                if c == '\\' {
                    chars.next();
                } else if c == q {
                    in_str = None;
                }
                continue;
            }
            match c {
                '/' if lang != Lang::Python && chars.peek() == Some(&'/') => break,
                '/' if lang != Lang::Python && chars.peek() == Some(&'*') => {
                    chars.next();
                    in_block_comment = true;
                }
                '#' if lang == Lang::Python => break,
                '"' => in_str = Some('"'),
                '`' if lang == Lang::Js => in_str = Some('`'),
                '\'' => match lang {
                    Lang::Rust => {
                        // 'x' / '\n' is a char literal; 'a (no close) a lifetime.
                        let mut la = chars.clone();
                        match la.next() {
                            Some('\\') => {
                                la.next();
                                if la.next() == Some('\'') {
                                    chars.next();
                                    chars.next();
                                    chars.next();
                                }
                            }
                            Some(_) if la.next() == Some('\'') => {
                                chars.next();
                                chars.next();
                            }
                            _ => {}
                        }
                    }
                    _ => in_str = Some('\''),
                },
                '(' => paren += 1,
                ')' => paren -= 1,
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' => {
                    depth -= 1;
                    if opened && depth <= 0 {
                        return Extent {
                            end_line: li + 1,
                            signature: finish_sig(&sig_lines),
                            has_body: true,
                        };
                    }
                }
                ';' if !opened && depth == 0 => {
                    return Extent {
                        end_line: li + 1,
                        signature: finish_sig(&sig_lines),
                        has_body: false,
                    };
                }
                _ => {}
            }
        }
        if !opened
            && !in_block_comment
            && depth == 0
            && paren <= 0
            && matches!(lang, Lang::Js | Lang::JavaLike | Lang::Go)
            && statement_ends(line, lines, li, lang)
        {
            return Extent {
                end_line: li + 1,
                signature: finish_sig(&sig_lines),
                has_body: false,
            };
        }
    }
    Extent {
        end_line: lines.len().max(start + 1),
        signature: finish_sig(&sig_lines),
        has_body: opened,
    }
}

/// Whether a braceless, semicolon-less statement is complete at `lines[li]`:
/// the line doesn't end in something that demands continuation, and the next
/// content line doesn't continue it (an Allman-style `{`, a `throws`/
/// `extends`/`where` clause, a chained `.` or operator).
fn statement_ends(line: &str, lines: &[&str], li: usize, lang: Lang) -> bool {
    let content = strip_line_comment(line, lang).trim_end();
    if content.is_empty() {
        return false;
    }
    const CONTINUERS: &[char] = &[
        '=', ',', '(', '[', '+', '-', '*', '/', '%', '|', '&', '<', '>', ':', '.', '?', '!',
    ];
    if content.ends_with(CONTINUERS) {
        return false;
    }
    for next in lines.iter().skip(li + 1) {
        let t = next.trim();
        if t.is_empty() || t.starts_with("//") {
            continue;
        }
        return !(t.starts_with('{')
            || t.starts_with('.')
            || t.starts_with(':')
            || t.starts_with("=>")
            || t.starts_with("->")
            || t.starts_with("throws")
            || t.starts_with("extends")
            || t.starts_with("implements")
            || t.starts_with("where"));
    }
    true
}

/// The line with any trailing `//` line comment removed (quote-aware).
fn strip_line_comment(line: &str, lang: Lang) -> &str {
    let mut in_str: Option<char> = None;
    let mut skip_next = false;
    let mut prev: Option<char> = None;
    for (i, c) in line.char_indices() {
        if skip_next {
            skip_next = false;
            prev = Some(c);
            continue;
        }
        if let Some(q) = in_str {
            if c == '\\' {
                skip_next = true;
            } else if c == q {
                in_str = None;
            }
        } else {
            match c {
                '"' | '\'' => in_str = Some(c),
                '`' if lang == Lang::Js => in_str = Some('`'),
                '/' if prev == Some('/') => return &line[..i - 1],
                _ => {}
            }
        }
        prev = Some(c);
    }
    line
}

fn finish_sig(sig_lines: &[&str]) -> String {
    let joined = sig_lines.join(" ");
    let cut = joined.split('{').next().unwrap_or("").trim();
    let collapsed = cut.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_sig(&collapsed)
}

fn truncate_sig(s: &str) -> String {
    if s.chars().count() <= 200 {
        return s.to_string();
    }
    let mut out: String = s.chars().take(199).collect();
    out.push('…');
    out
}

// ── per-language declaration matchers ─────────────────────────────────

fn match_decl(lang: Lang, t: &str, container: Option<(&str, &'static str)>) -> Option<Decl> {
    match lang {
        Lang::Rust => rust_decl(t),
        Lang::Js => js_decl(t, container),
        Lang::Go => go_decl(t),
        Lang::JavaLike => jk_decl(t, container),
        Lang::C => c_decl(t, container),
        Lang::Python => unreachable!("python uses the indent parser"),
    }
}

fn rust_decl(t: &str) -> Option<Decl> {
    re!(
        FN,
        r"^(?:pub\s*(?:\([^)]*\))?\s*)?(?:default\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+\x22[^\x22]*\x22\s+)?fn\s+([A-Za-z_]\w*)"
    );
    re!(
        CONTAINER,
        r"^(?:pub\s*(?:\([^)]*\))?\s*)?(?:unsafe\s+)?(struct|enum|trait|union|mod)\s+([A-Za-z_]\w*)"
    );
    re!(TYPE, r"^(?:pub\s*(?:\([^)]*\))?\s*)?type\s+([A-Za-z_]\w*)");
    re!(
        CONST,
        r"^(?:pub\s*(?:\([^)]*\))?\s*)?(?:const|static)\s+(?:mut\s+)?([A-Za-z_]\w*)\s*:"
    );
    re!(MACRO, r"^macro_rules!\s*([A-Za-z_]\w*)");

    if let Some(c) = FN.captures(t) {
        return Some(Decl {
            name: c[1].to_string(),
            kind: "fn",
            container: false,
            parent: None,
        });
    }
    if let Some(c) = CONTAINER.captures(t) {
        let (kw, name) = (c.get(1).unwrap().as_str(), c[2].to_string());
        let kind: &'static str = match kw {
            "struct" => "struct",
            "enum" => "enum",
            "trait" => "trait",
            "union" => "union",
            _ => "mod",
        };
        return Some(Decl {
            name,
            kind,
            container: matches!(kw, "trait" | "mod"),
            parent: None,
        });
    }
    if let Some(rest) = t.strip_prefix("impl")
        && rest.starts_with(|c: char| c.is_whitespace() || c == '<')
    {
        // Strip a leading generics list, then take everything up to the body.
        let rest = rest.trim_start();
        let rest = if let Some(inner) = rest.strip_prefix('<') {
            let mut depth = 1;
            let mut idx = inner.len();
            for (i, c) in inner.char_indices() {
                match c {
                    '<' => depth += 1,
                    '>' => {
                        depth -= 1;
                        if depth == 0 {
                            idx = i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            inner[idx.min(inner.len())..].trim_start()
        } else {
            rest
        };
        let name = rest
            .split('{')
            .next()
            .unwrap_or("")
            .split(" where")
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if !name.is_empty() {
            return Some(Decl {
                name,
                kind: "impl",
                container: true,
                parent: None,
            });
        }
    }
    if let Some(c) = MACRO.captures(t) {
        return Some(Decl {
            name: c[1].to_string(),
            kind: "macro",
            container: false,
            parent: None,
        });
    }
    if let Some(c) = TYPE.captures(t) {
        return Some(Decl {
            name: c[1].to_string(),
            kind: "type",
            container: false,
            parent: None,
        });
    }
    if let Some(c) = CONST.captures(t) {
        return Some(Decl {
            name: c[1].to_string(),
            kind: "const",
            container: false,
            parent: None,
        });
    }
    None
}

fn js_decl(t: &str, container: Option<(&str, &'static str)>) -> Option<Decl> {
    re!(
        FUNC,
        r"^(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)"
    );
    re!(
        CLASS,
        r"^(?:export\s+)?(?:default\s+)?(?:declare\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)"
    );
    re!(
        IFACE,
        r"^(?:export\s+)?(?:declare\s+)?interface\s+([A-Za-z_$][\w$]*)"
    );
    re!(
        TYPEALIAS,
        r"^(?:export\s+)?type\s+([A-Za-z_$][\w$]*)(?:\s*<[^>]*>)?\s*="
    );
    re!(
        ENUM,
        r"^(?:export\s+)?(?:declare\s+)?(?:const\s+)?enum\s+([A-Za-z_$][\w$]*)"
    );
    re!(NS, r"^(?:export\s+)?(?:declare\s+)?namespace\s+([\w.$]+)");
    re!(
        VARFN,
        r"^(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?="
    );
    re!(
        METHOD,
        r"^(?:(?:public|private|protected|static|readonly|async|override|abstract|get|set)\s+)*[*#]?\s*([A-Za-z_$#][\w$]*)\s*(?:<[^>]*>)?\("
    );
    const KEYWORDS: &[&str] = &[
        "if", "for", "while", "switch", "catch", "return", "new", "typeof", "delete", "do", "else",
        "try", "throw", "yield", "await", "in", "of", "case", "default", "function", "class",
        "const", "let", "var", "import", "export", "super", "this",
    ];

    if let Some(c) = FUNC.captures(t) {
        return Some(Decl {
            name: c[1].to_string(),
            kind: "function",
            container: false,
            parent: None,
        });
    }
    if let Some(c) = CLASS.captures(t) {
        return Some(Decl {
            name: c[1].to_string(),
            kind: "class",
            container: true,
            parent: None,
        });
    }
    if let Some(c) = IFACE.captures(t) {
        return Some(Decl {
            name: c[1].to_string(),
            kind: "interface",
            container: false,
            parent: None,
        });
    }
    if let Some(c) = TYPEALIAS.captures(t) {
        return Some(Decl {
            name: c[1].to_string(),
            kind: "type",
            container: false,
            parent: None,
        });
    }
    if let Some(c) = ENUM.captures(t) {
        return Some(Decl {
            name: c[1].to_string(),
            kind: "enum",
            container: false,
            parent: None,
        });
    }
    if let Some(c) = NS.captures(t) {
        return Some(Decl {
            name: c[1].to_string(),
            kind: "namespace",
            container: true,
            parent: None,
        });
    }
    if let Some(c) = VARFN.captures(t)
        && (t.contains("=>") || t.contains("function"))
    {
        return Some(Decl {
            name: c[1].to_string(),
            kind: "function",
            container: false,
            parent: None,
        });
    }
    if let Some((_, ckind)) = container
        && ckind == "class"
        && let Some(c) = METHOD.captures(t)
    {
        let name = c[1].to_string();
        if !KEYWORDS.contains(&name.as_str()) {
            let kind = if name == "constructor" {
                "constructor"
            } else {
                "method"
            };
            return Some(Decl {
                name,
                kind,
                container: false,
                parent: None,
            });
        }
    }
    None
}

fn go_decl(t: &str) -> Option<Decl> {
    re!(
        FUNC,
        r"^func\s+(?:\(\s*[A-Za-z_]\w*\s+\*?([A-Za-z_][\w\[\]]*)\s*\)\s*)?([A-Za-z_]\w*)\s*(?:\[[^\]]*\])?\("
    );
    re!(
        TYPE,
        r"^type\s+([A-Za-z_]\w*)\s*(?:\[[^\]]*\])?\s+(struct|interface)?"
    );

    if let Some(c) = FUNC.captures(t) {
        // A method's receiver type becomes its parent.
        let (kind, parent) = match c.get(1) {
            Some(m) => ("method", Some(m.as_str().to_string())),
            None => ("func", None),
        };
        return Some(Decl {
            name: c[2].to_string(),
            kind,
            container: false,
            parent,
        });
    }
    if let Some(c) = TYPE.captures(t) {
        let kind: &'static str = match c.get(2).map(|m| m.as_str()) {
            Some("struct") => "struct",
            Some("interface") => "interface",
            _ => "type",
        };
        return Some(Decl {
            name: c[1].to_string(),
            kind,
            container: false,
            parent: None,
        });
    }
    None
}

fn jk_decl(t: &str, container: Option<(&str, &'static str)>) -> Option<Decl> {
    re!(
        CONTAINER,
        r"^(?:(?:public|private|protected|internal|abstract|final|static|open|sealed|data|inner|annotation|value)\s+)*(class|interface|object|record|enum)(?:\s+class)?\s+([A-Za-z_]\w*)"
    );
    re!(COMPANION, r"^companion\s+object\b(?:\s+([A-Za-z_]\w*))?");
    re!(
        KTFUN,
        r"^(?:(?:public|private|protected|internal|open|override|suspend|inline|operator|infix|tailrec|external|final|abstract|actual|expect)\s+)*fun\s+(?:<[^>]*>\s*)?(?:[\w<>?.]+\.)?([A-Za-z_]\w*)\s*\("
    );
    re!(
        JMETHOD,
        r"^(?:(?:public|private|protected|static|final|abstract|synchronized|native|default|strictfp)\s+)*(?:<[^>]*>\s*)?[\w<>\[\],.?&]+(?:\s+[\w<>\[\],.?&]+)*\s+([A-Za-z_]\w*)\s*\("
    );
    re!(
        JCTOR,
        r"^(?:(?:public|private|protected)\s+)*([A-Z]\w*)\s*\("
    );
    const KEYWORDS: &[&str] = &[
        "if", "for", "while", "switch", "catch", "return", "new", "throw", "else", "do", "try",
        "super", "this", "when", "assert",
    ];

    if t.starts_with('@') {
        return None; // annotation line; the decl follows on a later line
    }
    if let Some(c) = CONTAINER.captures(t) {
        let kind: &'static str = match c.get(1).unwrap().as_str() {
            "class" => "class",
            "interface" => "interface",
            "object" => "object",
            "record" => "record",
            _ => "enum",
        };
        return Some(Decl {
            name: c[2].to_string(),
            kind,
            container: true,
            parent: None,
        });
    }
    if let Some(c) = COMPANION.captures(t) {
        let name = c.get(1).map_or("companion", |m| m.as_str()).to_string();
        return Some(Decl {
            name,
            kind: "object",
            container: true,
            parent: None,
        });
    }
    if let Some(c) = KTFUN.captures(t) {
        return Some(Decl {
            name: c[1].to_string(),
            kind: "fun",
            container: false,
            parent: None,
        });
    }
    if let Some((cname, _)) = container {
        if let Some(c) = JCTOR.captures(t)
            && c[1] == *cname
        {
            return Some(Decl {
                name: c[1].to_string(),
                kind: "constructor",
                container: false,
                parent: None,
            });
        }
        if let Some(c) = JMETHOD.captures(t) {
            let name = c[1].to_string();
            if !KEYWORDS.contains(&name.as_str()) {
                return Some(Decl {
                    name,
                    kind: "method",
                    container: false,
                    parent: None,
                });
            }
        }
    }
    None
}

fn c_decl(t: &str, container: Option<(&str, &'static str)>) -> Option<Decl> {
    re!(
        CONTAINER,
        r"^(?:typedef\s+)?(struct|union|enum|class|namespace)\s+([A-Za-z_]\w*)"
    );
    re!(DEFINE, r"^#\s*define\s+([A-Za-z_]\w*)");
    re!(
        FUNC,
        r"^(?:[A-Za-z_][\w:<>~,*&\s]*[*&\s])?([A-Za-z_~][\w:~]*)\s*\("
    );
    const KEYWORDS: &[&str] = &[
        "if", "for", "while", "switch", "return", "sizeof", "else", "do", "catch", "defined",
    ];

    if let Some(c) = DEFINE.captures(t) {
        return Some(Decl {
            name: c[1].to_string(),
            kind: "macro",
            container: false,
            parent: None,
        });
    }
    if t.starts_with('#') {
        return None; // other preprocessor directives
    }
    if let Some(c) = CONTAINER.captures(t) {
        let kw = c.get(1).unwrap().as_str();
        let kind: &'static str = match kw {
            "struct" => "struct",
            "union" => "union",
            "enum" => "enum",
            "class" => "class",
            _ => "namespace",
        };
        return Some(Decl {
            name: c[2].to_string(),
            kind,
            container: matches!(kw, "class" | "struct" | "namespace"),
            parent: None,
        });
    }
    if let Some(c) = FUNC.captures(t) {
        let name = c[1].to_string();
        if !KEYWORDS.contains(&name.as_str()) {
            // The caller drops this candidate unless a body actually opens,
            // which filters prototypes and stray call-looking lines.
            let kind = if container.is_some_and(|(_, k)| matches!(k, "class" | "struct")) {
                "method"
            } else {
                "function"
            };
            return Some(Decl {
                name,
                kind,
                container: false,
                parent: None,
            });
        }
    }
    None
}

// ── python: indentation-based outline ─────────────────────────────────

fn python_outline(lines: &[&str]) -> Vec<Symbol> {
    re!(PYDEF, r"^(\s*)(?:async\s+)?def\s+([A-Za-z_]\w*)\s*\(");
    re!(PYCLASS, r"^(\s*)class\s+([A-Za-z_]\w*)");

    let mut symbols = Vec::new();
    // (indent, name) of enclosing def/class scopes.
    let mut stack: Vec<(usize, String)> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let (indent_str, name, kind) = if let Some(c) = PYDEF.captures(line) {
            (c.get(1).unwrap().as_str(), c[2].to_string(), "def")
        } else if let Some(c) = PYCLASS.captures(line) {
            (c.get(1).unwrap().as_str(), c[2].to_string(), "class")
        } else {
            continue;
        };
        let indent = indent_width(indent_str);

        while stack.last().is_some_and(|(si, _)| *si >= indent) {
            stack.pop();
        }
        let parent = stack.last().map(|(_, n)| n.clone());
        let kind: &'static str = if kind == "def" {
            if parent.is_some() { "method" } else { "def" }
        } else {
            "class"
        };

        // Extent: consume a multi-line signature (unbalanced brackets), then
        // run until the last non-blank line before dedent to <= our indent.
        let mut paren: i32 = bracket_delta(line);
        let mut j = i;
        while paren > 0 && j + 1 < lines.len() {
            j += 1;
            paren += bracket_delta(lines[j]);
        }
        let mut last = j;
        let mut k = j + 1;
        while k < lines.len() {
            let l = lines[k];
            let lt = l.trim();
            if !lt.is_empty() && !lt.starts_with('#') {
                if indent_width(&l[..l.len() - l.trim_start().len()]) <= indent {
                    break;
                }
                last = k;
            }
            k += 1;
        }

        symbols.push(Symbol {
            name: name.clone(),
            kind,
            parent,
            start_line: i + 1,
            end_line: last + 1,
            signature: truncate_sig(line.trim()),
        });
        stack.push((indent, name));
    }
    symbols
}

fn indent_width(s: &str) -> usize {
    s.chars().map(|c| if c == '\t' { 8 } else { 1 }).sum()
}

/// Net `([{` minus `)]}` on a line, ignoring everything after a `#` and the
/// contents of simple string literals.
fn bracket_delta(line: &str) -> i32 {
    let mut d = 0;
    let mut in_str: Option<char> = None;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = in_str {
            if c == '\\' {
                chars.next();
            } else if c == q {
                in_str = None;
            }
            continue;
        }
        match c {
            '#' => break,
            '"' | '\'' => in_str = Some(c),
            '(' | '[' | '{' => d += 1,
            ')' | ']' | '}' => d -= 1,
            _ => {}
        }
    }
    d
}

// ── tools ─────────────────────────────────────────────────────────────

fn read_source(ctx: &HostCtx, path: &str) -> Result<(Lang, String), String> {
    let lang = detect_lang(path).ok_or_else(|| {
        format!(
            "no outline parser for {path} — supported extensions: .rs, .ts/.tsx/.js/.jsx, .py, .go, .java/.kt, .c/.h/.cc/.cpp/.hpp. Use read_file with a line window instead."
        )
    })?;
    let resp = ctx.call_host(HostFn::ReadFile, &serde_json::json!({ "path": path }))?;
    if resp["truncated"].as_bool().unwrap_or(false) {
        return Err(format!(
            "{path} exceeds the read cap, so its outline would be incomplete; use read_file with a line window"
        ));
    }
    Ok((lang, resp["content"].as_str().unwrap_or("").to_string()))
}

pub fn file_outline_tool(
    args: serde_json::Value,
    ctx: &HostCtx,
) -> Result<serde_json::Value, String> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("`path` (project-relative) is required")?;
    let name_contains = args.get("name_contains").and_then(|v| v.as_str());
    let max = args
        .get("max")
        .and_then(|v| v.as_u64())
        .unwrap_or(OUTLINE_DEFAULT_MAX as u64)
        .clamp(1, 1000) as usize;

    let (lang, content) = read_source(ctx, path)?;
    let mut symbols = outline(&content, lang);
    if let Some(sub) = name_contains {
        let sub = sub.to_lowercase();
        symbols.retain(|s| s.name.to_lowercase().contains(&sub));
    }
    let total = symbols.len();
    symbols.truncate(max);

    Ok(serde_json::json!({
        "path": path,
        "language": lang.name(),
        "hash": hash_text(&content),
        "total_lines": content.lines().count(),
        "count": symbols.len(),
        "truncated": total > symbols.len(),
        "symbols": symbols.iter().map(Symbol::to_json).collect::<Vec<_>>(),
    }))
}

pub fn read_symbol_tool(
    args: serde_json::Value,
    ctx: &HostCtx,
) -> Result<serde_json::Value, String> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("`path` (project-relative) is required")?;
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("`name` (exact symbol name) is required")?;
    let want_kind = args.get("kind").and_then(|v| v.as_str());
    let want_parent = args.get("parent").and_then(|v| v.as_str());

    let (lang, content) = read_source(ctx, path)?;
    let all = outline(&content, lang);
    let matches: Vec<&Symbol> = all
        .iter()
        .filter(|s| s.name == name)
        .filter(|s| want_kind.is_none_or(|k| s.kind == k))
        .filter(|s| want_parent.is_none_or(|p| s.parent.as_deref() == Some(p)))
        .collect();

    if matches.is_empty() {
        let near: Vec<&str> = all
            .iter()
            .filter(|s| s.name.to_lowercase().contains(&name.to_lowercase()))
            .map(|s| s.name.as_str())
            .take(20)
            .collect();
        return Err(if near.is_empty() {
            format!(
                "no symbol named '{name}' in {path} ({} symbols found) — call file_outline to list them",
                all.len()
            )
        } else {
            format!(
                "no symbol named '{name}' in {path}. Close matches: {}",
                near.join(", ")
            )
        });
    }

    let hash = hash_text(&content);
    let lines: Vec<&str> = content.lines().collect();
    let include_bodies = matches.len() <= READ_SYMBOL_MAX_BODIES;
    let symbols: Vec<serde_json::Value> = matches
        .iter()
        .map(|s| {
            let mut v = s.to_json();
            if include_bodies {
                v["content"] = serde_json::Value::String(
                    lines[s.start_line - 1..s.end_line.min(lines.len())].join("\n"),
                );
            }
            v
        })
        .collect();

    let mut out = serde_json::json!({
        "path": path,
        "language": lang.name(),
        "hash": hash,
        "total_lines": lines.len(),
        "count": symbols.len(),
        "symbols": symbols,
    });
    if !include_bodies {
        out["note"] = serde_json::Value::String(format!(
            "{} symbols share the name '{name}' — bodies omitted; disambiguate with `kind` or `parent`",
            matches.len()
        ));
    }
    Ok(out)
}

// ── tests (pure logic, run on the host target) ────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn find<'a>(syms: &'a [Symbol], name: &str) -> &'a Symbol {
        syms.iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("symbol '{name}' not found in {syms:?}"))
    }

    #[test]
    fn rust_outline() {
        let src = r#"
pub struct Point {
    x: f64,
}

impl Point {
    pub fn new(x: f64) -> Self {
        let s = "}"; // brace in string must not end the block
        Self { x }
    }

    fn len(&self) -> f64 {
        self.x
    }
}

pub trait Shape {
    fn area(&self) -> f64;
}

pub async fn top_level(a: u32) -> u32 {
    a
}

macro_rules! my_macro {
    () => {};
}

pub const LIMIT: usize = 10;
"#;
        let syms = outline(src, Lang::Rust);
        assert_eq!(find(&syms, "Point").kind, "struct");
        let new = find(&syms, "new");
        assert_eq!(new.parent.as_deref(), Some("Point"));
        assert_eq!((new.start_line, new.end_line), (7, 10));
        assert_eq!(find(&syms, "len").parent.as_deref(), Some("Point"));
        assert_eq!(find(&syms, "area").parent.as_deref(), Some("Shape"));
        assert_eq!(find(&syms, "top_level").kind, "fn");
        assert_eq!(find(&syms, "my_macro").kind, "macro");
        assert_eq!(find(&syms, "LIMIT").kind, "const");
    }

    #[test]
    fn rust_impl_trait_for() {
        let src = "impl Display for Point {\n    fn fmt(&self) {}\n}\n";
        let syms = outline(src, Lang::Rust);
        assert_eq!(find(&syms, "Display for Point").kind, "impl");
        assert_eq!(
            find(&syms, "fmt").parent.as_deref(),
            Some("Display for Point")
        );
    }

    #[test]
    fn js_outline() {
        let src = r#"
export function greet(name) {
    return `hi ${name}`;
}

export const add = (a, b) => {
    return a + b;
};

export default class Repo {
    constructor(url) {
        this.url = url;
    }

    async fetch(path) {
        return get(`${this.url}/${path}`);
    }
}

export interface Options {
    depth: number;
}

export type Pair = [number, number];

export enum Color { Red, Green }
"#;
        let syms = outline(src, Lang::Js);
        assert_eq!(find(&syms, "greet").kind, "function");
        assert_eq!(find(&syms, "add").kind, "function");
        assert_eq!(find(&syms, "Repo").kind, "class");
        assert_eq!(find(&syms, "constructor").parent.as_deref(), Some("Repo"));
        let fetch = find(&syms, "fetch");
        assert_eq!(fetch.kind, "method");
        assert_eq!(fetch.parent.as_deref(), Some("Repo"));
        assert_eq!(find(&syms, "Options").kind, "interface");
        assert_eq!(find(&syms, "Pair").kind, "type");
        assert_eq!(find(&syms, "Color").kind, "enum");
    }

    #[test]
    fn python_outline_nesting() {
        let src = r#"
import os

class Animal:
    sound = "?"

    def speak(self):
        return self.sound

    def rename(
        self,
        name,
    ):
        self.name = name

def helper(x):
    def inner(y):
        return y * 2
    return inner(x)

async def main():
    pass
"#;
        let syms = outline(src, Lang::Python);
        let animal = find(&syms, "Animal");
        assert_eq!(animal.kind, "class");
        assert_eq!((animal.start_line, animal.end_line), (4, 14));
        assert_eq!(find(&syms, "speak").parent.as_deref(), Some("Animal"));
        let rename = find(&syms, "rename");
        assert_eq!(rename.parent.as_deref(), Some("Animal"));
        assert_eq!((rename.start_line, rename.end_line), (10, 14));
        assert_eq!(find(&syms, "helper").kind, "def");
        assert_eq!(find(&syms, "inner").parent.as_deref(), Some("helper"));
        assert_eq!(find(&syms, "main").kind, "def");
    }

    #[test]
    fn go_outline() {
        let src = r#"
package main

type Server struct {
    port int
}

type Handler interface {
    Serve() error
}

func (s *Server) Start() error {
    return nil
}

func NewServer(port int) *Server {
    return &Server{port: port}
}
"#;
        let syms = outline(src, Lang::Go);
        assert_eq!(find(&syms, "Server").kind, "struct");
        assert_eq!(find(&syms, "Handler").kind, "interface");
        let start = find(&syms, "Start");
        assert_eq!(start.kind, "method");
        assert_eq!(start.parent.as_deref(), Some("Server"));
        assert_eq!(find(&syms, "NewServer").kind, "func");
    }

    #[test]
    fn java_kotlin_outline() {
        let java = r#"
public class Account {
    private long balance;

    public Account(long balance) {
        this.balance = balance;
    }

    public synchronized void deposit(long amount) {
        if (amount > 0) {
            balance += amount;
        }
    }
}
"#;
        let syms = outline(java, Lang::JavaLike);
        assert_eq!(find(&syms, "Account").kind, "class");
        let ctor = syms.iter().find(|s| s.kind == "constructor").unwrap();
        assert_eq!(ctor.name, "Account");
        assert_eq!(find(&syms, "deposit").kind, "method");

        let kotlin = r#"
data class User(val name: String)

object Registry {
    fun register(u: User) {
        println(u)
    }
}

suspend fun fetchAll(): List<User> {
    return emptyList()
}
"#;
        let syms = outline(kotlin, Lang::JavaLike);
        let user = find(&syms, "User");
        assert_eq!(user.kind, "class");
        // Braceless declaration must end on its own line, not swallow Registry.
        assert_eq!(user.start_line, user.end_line);
        let registry = find(&syms, "Registry");
        assert_eq!(registry.kind, "object");
        assert_eq!(registry.parent, None);
        assert_eq!(find(&syms, "register").parent.as_deref(), Some("Registry"));
        assert_eq!(find(&syms, "fetchAll").kind, "fun");
    }

    #[test]
    fn c_outline() {
        let src = r#"
#include <stdio.h>
#define MAX_LEN 128

struct point {
    int x;
    int y;
};

/* a prototype must not appear as a function */
int distance(struct point a, struct point b);

int distance(struct point a, struct point b) {
    if (a.x > b.x) {
        return a.x - b.x;
    }
    return b.x - a.x;
}

static void print_point(const struct point *p) {
    printf("%d,%d\n", p->x, p->y);
}
"#;
        let syms = outline(src, Lang::C);
        assert_eq!(find(&syms, "MAX_LEN").kind, "macro");
        assert_eq!(find(&syms, "point").kind, "struct");
        let dist: Vec<_> = syms.iter().filter(|s| s.name == "distance").collect();
        assert_eq!(dist.len(), 1, "prototype must be skipped: {syms:?}");
        assert_eq!(dist[0].kind, "function");
        assert_eq!((dist[0].start_line, dist[0].end_line), (13, 18));
        assert_eq!(find(&syms, "print_point").kind, "function");
    }

    #[test]
    fn cpp_methods_in_class() {
        let src = r#"
namespace geo {

class Circle {
public:
    double area() const {
        return 3.14 * r * r;
    }
private:
    double r;
};

double scale(double x) {
    return x * 2;
}

}
"#;
        let syms = outline(src, Lang::C);
        assert_eq!(find(&syms, "geo").kind, "namespace");
        assert_eq!(find(&syms, "Circle").parent.as_deref(), Some("geo"));
        let area = find(&syms, "area");
        assert_eq!(area.kind, "method");
        assert_eq!(area.parent.as_deref(), Some("Circle"));
        assert_eq!(find(&syms, "scale").kind, "function");
    }

    #[test]
    fn comments_do_not_produce_symbols() {
        let src = r#"
// fn commented_out() {}
/* fn also_commented() {
   } */
/// fn doc_example() {}
fn real() {}
"#;
        let syms = outline(src, Lang::Rust);
        assert_eq!(syms.len(), 1, "{syms:?}");
        assert_eq!(syms[0].name, "real");
    }

    #[test]
    fn detect_lang_by_extension() {
        assert_eq!(detect_lang("src/main.rs"), Some(Lang::Rust));
        assert_eq!(detect_lang("a/b.test.tsx"), Some(Lang::Js));
        assert_eq!(detect_lang("x.py"), Some(Lang::Python));
        assert_eq!(detect_lang("x.go"), Some(Lang::Go));
        assert_eq!(detect_lang("X.kt"), Some(Lang::JavaLike));
        assert_eq!(detect_lang("x.hpp"), Some(Lang::C));
        assert_eq!(detect_lang("notes.txt"), None);
        assert_eq!(detect_lang("Makefile"), None);
    }

    #[test]
    fn signature_is_captured_and_cut_at_brace() {
        let src = "pub fn add(\n    a: u32,\n    b: u32,\n) -> u32 {\n    a + b\n}\n";
        let syms = outline(src, Lang::Rust);
        assert_eq!(syms[0].signature, "pub fn add( a: u32, b: u32, ) -> u32");
        assert_eq!((syms[0].start_line, syms[0].end_line), (1, 6));
    }
}
