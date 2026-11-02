//! Cheap structural checks of the web files a model just wrote, appended to
//! the write's tool result so the model fixes them while the file is fresh.
//!
//! Local models garble long writes, mostly where markup repeats (seen:
//! `<="play">`, `class="="card-body"`, a `</div>` closing an `<article>`, a
//! missing `)` in a CSS gradient), and then judge the page from a screenshot
//! that doesn't show the cause. These checks are heuristics, not parsers: they
//! report only clear breakage (unbalanced tags, braces and brackets, tags with
//! no name, attribute values starting with `=`) and stay quiet otherwise.

/// Most problems listed per file; past the first few, the rest are usually
/// the same breakage cascading.
const MAX_PROBLEMS: usize = 6;
/// A written file longer than this gets a note to split it.
const LONG_FILE_LINES: usize = 400;

/// Elements with no closing tag.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];
/// Elements whose closing tag HTML lets you omit, plus the document
/// wrappers; never reported as unclosed.
const OPTIONAL_CLOSE: &[&str] = &[
    "body", "colgroup", "dd", "dt", "head", "html", "li", "optgroup", "option", "p", "rp", "rt",
    "tbody", "td", "tfoot", "th", "thead", "tr",
];

/// Whether `path` is a file type these checks understand.
pub fn is_checked(path: &str) -> bool {
    matches!(ext(path).as_str(), "html" | "htm" | "css" | "js" | "mjs")
}

/// The note appended to a successful write of `path`, or `None` when there
/// is nothing to say.
pub fn report(path: &str, content: &str) -> Option<String> {
    let problems = problems(path, content);
    let lines = content.lines().count();
    let mut note = String::new();
    if !problems.is_empty() {
        note.push_str(&format!(
            "Structure check of {path} found problems. Fix them before anything else \
             (read those lines, then edit):\n"
        ));
        for p in &problems {
            note.push_str(&format!("- {p}\n"));
        }
    }
    if lines > LONG_FILE_LINES {
        note.push_str(&format!(
            "{path} is {lines} lines. Long files are where generated code goes wrong: split it \
             (shared CSS, one file per page, repeated items rendered from a data array in JS) and \
             keep each file under ~300 lines.\n"
        ));
    }
    (!note.is_empty()).then(|| note.trim_end().to_string())
}

/// Clear structural problems in `content`, at most `MAX_PROBLEMS`.
pub fn problems(path: &str, content: &str) -> Vec<String> {
    let mut out = match ext(path).as_str() {
        "html" | "htm" => html(content),
        "css" => css(content, 1),
        "js" | "mjs" => js(content, 1),
        _ => Vec::new(),
    };
    out.truncate(MAX_PROBLEMS);
    out
}

fn ext(path: &str) -> String {
    path.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// 1-based line of byte offset `at` in `src`, offset by `base - 1`.
fn line_at(src: &str, at: usize, base: usize) -> usize {
    base + src.as_bytes()[..at.min(src.len())]
        .iter()
        .filter(|&&c| c == b'\n')
        .count()
}

/// Up to 40 chars from byte offset `at`, on one line, for quoting.
fn snippet(src: &str, at: usize) -> String {
    let s: String = src[at..]
        .chars()
        .take_while(|&c| c != '\n')
        .take(40)
        .collect();
    s.trim_end().to_string()
}

fn tag_name(s: &str) -> String {
    s.chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == ':')
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Byte offset of the `>` ending the tag opened at `start`, skipping quoted
/// attribute values. `Err(at)` when a new tag starts inside a quote first
/// (an unbalanced quote) or the input ends.
fn tag_end(src: &str, start: usize) -> Result<usize, usize> {
    let b = src.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = start + 1;
    while i < b.len() {
        let c = b[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_)
                if c == b'<'
                    && b.get(i + 1)
                        .is_some_and(|n| n.is_ascii_alphabetic() || *n == b'/') =>
            {
                return Err(i);
            }
            Some(_) => {}
            None if c == b'"' || c == b'\'' => quote = Some(c),
            None if c == b'>' => return Ok(i),
            None if c == b'<' => return Err(i),
            None => {}
        }
        i += 1;
    }
    Err(b.len())
}

fn html(src: &str) -> Vec<String> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut stack: Vec<(String, usize)> = Vec::new();
    let mut i = 0;
    while i < b.len() && out.len() < MAX_PROBLEMS {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let rest = &src[i..];
        if rest.starts_with("<!--") {
            i = rest.find("-->").map_or(b.len(), |e| i + e + 3);
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            i = rest.find('>').map_or(b.len(), |e| i + e + 1);
            continue;
        }
        let line = line_at(src, i, 1);
        let next = b.get(i + 1).copied().unwrap_or(b' ');
        if next == b'/' {
            let name = tag_name(&src[i + 2..]);
            if name.is_empty() {
                out.push(format!(
                    "line {line}: broken closing tag `{}`",
                    snippet(src, i)
                ));
            } else {
                close(&mut stack, &name, line, &mut out);
            }
            i = rest.find('>').map_or(b.len(), |e| i + e + 1);
            continue;
        }
        if next == b'=' || next == b'"' || next == b'\'' {
            out.push(format!(
                "line {line}: broken tag `{}` (the tag name is missing)",
                snippet(src, i)
            ));
            i += 1;
            continue;
        }
        if !next.is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let name = tag_name(&src[i + 1..]);
        let end = match tag_end(src, i) {
            Ok(end) => end,
            Err(at) => {
                let what = if garbled(&src[i..at]) {
                    "has a garbled attribute and"
                } else {
                    "is"
                };
                out.push(format!(
                    "line {line}: tag `{}` {what} never closed with `>` (unbalanced quote?)",
                    snippet(src, i)
                ));
                i = at.max(i + 1);
                continue;
            }
        };
        let tag = &src[i..=end];
        if garbled(tag) {
            out.push(format!(
                "line {line}: garbled attribute in `{}`",
                snippet(src, i)
            ));
        }
        if name == "script" || name == "style" {
            let body_start = end + 1;
            let close_tag = format!("</{name}");
            let body_end = src[body_start..]
                .to_ascii_lowercase()
                .find(&close_tag)
                .map_or(src.len(), |e| body_start + e);
            let body = &src[body_start..body_end];
            let base = line_at(src, body_start, 1);
            if name == "style" {
                out.extend(css(body, base));
            } else if !tag.contains("src=") && !tag.contains("json") {
                out.extend(js(body, base));
            }
            i = src[body_end..]
                .find('>')
                .map_or(src.len(), |e| body_end + e + 1);
            continue;
        }
        if !VOID.contains(&name.as_str()) && !tag.ends_with("/>") {
            stack.push((name, line));
        }
        i = end + 1;
    }
    for (name, line) in stack.iter().rev() {
        if !OPTIONAL_CLOSE.contains(&name.as_str()) {
            out.push(format!("line {line}: `<{name}>` is never closed"));
        }
    }
    out
}
/// An attribute value that starts with `=`: `class="="card-body"`.
fn garbled(tag: &str) -> bool {
    tag.contains("=\"=") || tag.contains("='=")
}

fn close(stack: &mut Vec<(String, usize)>, name: &str, line: usize, out: &mut Vec<String>) {
    if VOID.contains(&name) {
        return;
    }
    let Some(pos) = stack.iter().rposition(|(n, _)| n == name) else {
        let inner = stack
            .last()
            .map(|(n, l)| format!(" (innermost open element: `<{n}>` from line {l})"))
            .unwrap_or_default();
        out.push(format!(
            "line {line}: stray `</{name}>` with no matching open tag{inner}"
        ));
        return;
    };
    for (n, l) in &stack[pos + 1..] {
        if !OPTIONAL_CLOSE.contains(&n.as_str()) {
            out.push(format!(
                "line {line}: `</{name}>` closes `<{name}>` while `<{n}>` from line {l} is still \
                 open"
            ));
        }
    }
    stack.truncate(pos);
}

/// Braces across the stylesheet, parentheses per declaration.
fn css(src: &str, base: usize) -> Vec<String> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut opened: Vec<usize> = Vec::new();
    let mut parens = 0i32;
    let mut decl_start = 0;
    let mut i = 0;
    while i < b.len() && out.len() < MAX_PROBLEMS {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = src[i + 2..].find("*/").map_or(b.len(), |e| i + 2 + e + 2);
                continue;
            }
            q @ (b'"' | b'\'') => {
                i += 1;
                while i < b.len() && b[i] != q && b[i] != b'\n' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'(' => parens += 1,
            b')' => parens -= 1,
            c @ (b'{' | b'}' | b';') => {
                if parens != 0 {
                    out.push(format!(
                        "line {}: unbalanced parentheses in `{}`",
                        line_at(src, decl_start, base),
                        snippet(src, decl_start).trim_start()
                    ));
                    parens = 0;
                }
                if c == b'{' {
                    opened.push(i);
                } else if c == b'}' && opened.pop().is_none() {
                    out.push(format!(
                        "line {}: `}}` with no matching `{{`",
                        line_at(src, i, base)
                    ));
                }
                decl_start = i + 1;
                while decl_start < b.len() && b[decl_start].is_ascii_whitespace() {
                    decl_start += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    if let Some(&at) = opened.last() {
        out.push(format!(
            "line {}: {} `{{` never closed (last one here)",
            line_at(src, at, base),
            opened.len()
        ));
    }
    out
}

/// Bracket balance outside strings, template literals and comments. Only
/// the first problem: later ones are the same mismatch cascading. Regex
/// literals aren't recognised, so brackets inside one can misfire; the
/// wording says "looks".
fn js(src: &str, base: usize) -> Vec<String> {
    let b = src.as_bytes();
    let mut stack: Vec<(u8, usize)> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                i = src[i..].find('\n').map_or(b.len(), |e| i + e);
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = src[i + 2..].find("*/").map_or(b.len(), |e| i + 2 + e + 2);
                continue;
            }
            q @ (b'"' | b'\'' | b'`') => {
                i += 1;
                while i < b.len() && b[i] != q && (q == b'`' || b[i] != b'\n') {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
            }
            c @ (b'(' | b'[' | b'{') => stack.push((c, i)),
            c @ (b')' | b']' | b'}') => {
                let want = match c {
                    b')' => b'(',
                    b']' => b'[',
                    _ => b'{',
                };
                match stack.pop() {
                    Some((open, _)) if open == want => {}
                    Some((open, at)) => {
                        return vec![format!(
                            "line {}: `{}` looks mismatched with `{}` from line {}",
                            line_at(src, i, base),
                            c as char,
                            open as char,
                            line_at(src, at, base)
                        )];
                    }
                    None => {
                        return vec![format!(
                            "line {}: `{}` looks unmatched",
                            line_at(src, i, base),
                            c as char
                        )];
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    match stack.last() {
        Some(&(open, at)) => vec![format!(
            "line {}: `{}` looks never closed",
            line_at(src, at, base),
            open as char
        )],
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The garble from Probe I's page, a CSS gradient missing its `)`, and
    /// a clean page that must stay quiet.
    #[test]
    fn flags_probe_garble_and_passes_clean_markup() {
        let clean = "<!doctype html>\n<html><head><style>\n.a { background: linear-gradient(red, \
                     blue); }\n</style></head>\n<body><ul><li>one<li>two</ul>\n<p>x<br><img \
                     src=\"a.png\" alt=\"\"><svg><path d=\"M0 0\"/></svg>\n<script>if (a < b) { \
                     f(\"}\"); }</script>\n</body></html>\n";
        assert_eq!(problems("index.html", clean), Vec::<String>::new());

        let broken = "<div class=\"grid\">\n<article class=\"card\">\n<div class=\"thumb\">\n\
                      <=\"play\"><span></span></div>\n</div>\n<div class=\"=\"card-body\">\n\
                      </div>\n</div>\n</article>\n</div>\n";
        let found = problems("ui/index.html", broken);
        assert!(
            found.iter().any(|p| p.starts_with("line 4: broken tag")),
            "{found:?}"
        );
        assert!(
            found.iter().any(|p| p.contains("garbled attribute")),
            "{found:?}"
        );
        assert!(
            found.iter().any(|p| p.contains("`<article>` from line 2")),
            "{found:?}"
        );

        let css_src =
            ".thumb::after {\n  background: linear-gradient(transparent, rgba(0,0,0,.6);\n}\n";
        let found = problems("style.css", css_src);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("line 2: unbalanced parentheses"));

        assert_eq!(
            problems("app.js", "function f() {\n  return [1, 2;\n}\n"),
            ["line 3: `}` looks mismatched with `[` from line 2"]
        );
        assert!(report("a.md", "x").is_none());
        let long = "<p>x</p>\n".repeat(450);
        assert!(report("a.html", &long).unwrap().contains("450 lines"));
    }
}
