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
/// Add the closing parentheses a CSS declaration is missing right before
/// its `;`, in a `.css` file or the `<style>` blocks of an HTML file.
/// Returns the repaired text and the 1-based lines changed.
///
/// The model drops the outer `)` of nested calls before `;` in nearly every
/// stylesheet it writes (`repeat(2, minmax(0, 1fr);`,
/// `linear-gradient(transparent, rgba(0,0,0,.75);`), and when it rewrites the
/// file to fix one it drops others. Only one-line declarations that start
/// on that line (after `{`, `;`, or a line that ended a rule) and end in `;`
/// are touched, so a multi-line value is never "fixed".
pub fn repair_css_parens(path: &str, src: &str) -> (String, Vec<usize>) {
    let html = matches!(ext(path).as_str(), "html" | "htm");
    if !html && ext(path) != "css" {
        return (src.to_string(), Vec::new());
    }
    let mut fixed = Vec::new();
    let mut out = String::with_capacity(src.len() + 8);
    let mut in_style = !html;
    let mut prev_ends_rule = true;
    for (i, line) in src.split_inclusive('\n').enumerate() {
        let lower = line.to_ascii_lowercase();
        if html && lower.contains("<style") {
            in_style = true;
            prev_ends_rule = true;
        }
        if html && lower.contains("</style") {
            in_style = false;
        }
        let body = line.trim_end_matches(['\n', '\r']);
        let repaired = if in_style && prev_ends_rule && !body.contains("/*") {
            repair_line(body)
        } else {
            None
        };
        match repaired {
            Some(r) => {
                out.push_str(&r);
                out.push_str(&line[body.len()..]);
                fixed.push(i + 1);
            }
            None => out.push_str(line),
        }
        let t = body.trim_end();
        prev_ends_rule = t.is_empty()
            || t.ends_with('{')
            || t.ends_with(';')
            || t.ends_with('}')
            || t.ends_with("*/")
            || t.to_ascii_lowercase().contains("<style");
    }
    (out, fixed)
}

/// One line's declarations, each closed if it opens more `(` than it closes
/// and ends with `;`. `None` when nothing changed or the line has quotes.
fn repair_line(line: &str) -> Option<String> {
    // `url(data:…;base64,…)` legitimately holds a `;` inside parentheses.
    if line.contains(['"', '\'']) || !line.contains('(') || line.contains("url(") {
        return None;
    }
    let mut out = String::with_capacity(line.len() + 4);
    let mut seg = String::new();
    let mut changed = false;
    for c in line.chars() {
        match c {
            ';' => {
                let opens = seg.matches('(').count();
                let closes = seg.matches(')').count();
                if opens > closes {
                    seg.push_str(&")".repeat(opens - closes));
                    changed = true;
                }
                out.push_str(&seg);
                out.push(';');
                seg.clear();
            }
            '{' | '}' => {
                out.push_str(&seg);
                out.push(c);
                seg.clear();
            }
            _ => seg.push(c),
        }
    }
    out.push_str(&seg);
    changed.then_some(out)
}
/// Close the call a callback block was passed to, when the model ended the
/// block with `}` and dropped the `)` (`items.forEach(x => { … }` instead of
/// `… });`). Applies to `.js`/`.mjs` files and the inline `<script>` blocks of
/// an HTML file. Returns the repaired text and the 1-based lines changed.
///
/// Seen in Probe K: every page blank, because chrome.js, app.js and data.js
/// all failed to parse, two of them on exactly this. The rule: a `}` whose
/// `{` was opened directly inside `(`, followed (after whitespace) by
/// anything but `)`, `,` or `.`, gets a `)` right after it. Strings,
/// template literals and comments are skipped.
pub fn repair_js_parens(path: &str, src: &str) -> (String, Vec<usize>) {
    match ext(path).as_str() {
        "js" | "mjs" => repair_js_region(src, 1, true),
        "html" | "htm" => {
            let lower = src.to_ascii_lowercase();
            let mut out = String::with_capacity(src.len() + 8);
            let mut lines = Vec::new();
            let mut at = 0;
            while let Some(open) = lower[at..].find("<script").map(|o| at + o) {
                let Some(body) = lower[open..].find('>').map(|e| open + e + 1) else {
                    break;
                };
                let end = lower[body..]
                    .find("</script")
                    .map_or(src.len(), |e| body + e);
                out.push_str(&src[at..body]);
                if lower[open..body].contains("src=") || lower[open..body].contains("json") {
                    out.push_str(&src[body..end]);
                } else {
                    let (fixed, at_lines) =
                        repair_js_region(&src[body..end], line_at(src, body, 1), true);
                    out.push_str(&fixed);
                    lines.extend(at_lines);
                }
                at = end;
            }
            out.push_str(&src[at..]);
            (out, lines)
        }
        _ => (src.to_string(), Vec::new()),
    }
}

/// The text of an edit_file op on a `.js` file, repaired like a whole file
/// except that a `}` at the very end is left alone (its `)` may follow the
/// edited range).
pub fn repair_js_fragment(path: &str, text: &str) -> (String, Vec<usize>) {
    match ext(path).as_str() {
        "js" | "mjs" => repair_js_region(text, 1, false),
        _ => (text.to_string(), Vec::new()),
    }
}

fn repair_js_region(src: &str, base: usize, whole: bool) -> (String, Vec<usize>) {
    // Minified or bundled code (any line over 1,000 chars) isn't written by
    // hand; the heuristics are tuned for hand-written lines, so leave it.
    if src.lines().any(|l| l.len() > 1_000) {
        return (src.to_string(), Vec::new());
    }
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len() + 8);
    let mut lines = Vec::new();
    // Open brackets with their byte offsets; `B` marks a function body.
    let mut stack: Vec<(u8, usize)> = Vec::new();
    let mut last = 0;
    let mut i = 0;
    // The last significant byte (comments skipped), for regex detection.
    let mut prev: Option<usize> = None;
    // Where the current line starts, the stack depth there, and the last
    // significant byte before it (to tell whether a statement starts here).
    let (mut line_start, mut line_depth, mut before_line): (usize, usize, Option<u8>) =
        (0, 0, None);
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
            b'/' if let Some(end) = regex_end(b, i, prev) => {
                i = end;
            }
            b'"' | b'\'' | b'`' => i = string_end(b, i),
            b'\n' => {
                // A statement that starts on this line and ends it with `;`
                // while `(` opened on this line are still open: the model
                // dropped their `)` before the `;` (seen:
                // `Object.fromEntries(CHANNELS.map(c => [c.id, c]);`).
                let starts_statement = matches!(before_line, None | Some(b';' | b'{' | b'}'));
                let opened = &stack[line_depth.min(stack.len())..];
                if starts_statement
                    && let Some(p) = prev.filter(|p| *p >= line_start && b[*p] == b';')
                    && !opened.is_empty()
                    && opened.iter().all(|(c, _)| *c == b'(')
                    && !opened.iter().any(|&(_, at)| word_before(b, at) == b"for")
                {
                    let k = opened.len();
                    out.push_str(&src[last..p]);
                    out.push_str(&")".repeat(k));
                    last = p;
                    stack.truncate(stack.len() - k);
                    lines.push(line_at(src, p, base));
                }
                // `v8: {` as an item of an array: the key slipped in from an
                // object-shaped draft; an array item can't have one, so drop
                // it.
                if stack.last().map(|e| e.0) == Some(b'[') {
                    let rest = &src[i + 1..];
                    let indent = rest.len() - rest.trim_start_matches([' ', '\t']).len();
                    let body = &rest[indent..];
                    let key = body
                        .bytes()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'$')
                        .count();
                    let after = &body[key..];
                    let colon = after.len() - after.trim_start_matches([' ', '\t']).len();
                    let value = after[colon..]
                        .strip_prefix(':')
                        .map(|v| v.trim_start_matches([' ', '\t']));
                    if key > 0
                        && !body.as_bytes()[0].is_ascii_digit()
                        && value.is_some_and(|v| v.starts_with('{'))
                    {
                        let brace = i + 1 + rest.len() - value.unwrap_or_default().len();
                        out.push_str(&src[last..=i + indent]);
                        last = brace;
                        lines.push(line_at(src, i + 1, base));
                    }
                }
                line_start = i + 1;
                line_depth = stack.len();
                before_line = prev.map(|p| b[p]);
            }
            c @ (b'(' | b'[') => stack.push((c, i)),
            // A function body (`=> {`, `) {`) is pushed as `B`; any other
            // `{` is an object literal or destructuring pattern, which can
            // legitimately be followed by `=`, `instanceof`, … inside `(`
            // (`while (({value, done} = it.next()))`), so never repaired.
            b'{' => stack.push((if opens_body(b, i) { b'B' } else { b'{' }, i)),
            b')' | b']' => {
                stack.pop();
            }
            b'}' => {
                let open = stack.pop();
                let body_at = open.filter(|e| e.0 == b'B').map(|e| e.1);
                // Only a callback passed on the line that opens the call
                // (`list.forEach(x => {`): a `(` left open further up is a
                // different slip, and closing it here put the `)` in the
                // wrong place.
                let call_on_same_line =
                    |paren: usize, brace: usize| !src[paren..brace].contains('\n');
                if let Some(brace) = body_at
                    && let Some(&(b'(', paren)) = stack.last()
                    && call_on_same_line(paren, brace)
                {
                    let next = src[i + 1..].trim_start().bytes().next();
                    // In a fragment the `)` may sit just past its end.
                    let ends_fragment = next.is_none() && !whole;
                    // `}(` is a function body called on the spot (an IIFE).
                    // The `)` is missing only if a new statement follows: an
                    // identifier, `}`, `;` or the end. Anything else (`)`,
                    // `,`, `.`, `(` of an IIFE, `:` of a ternary) continues
                    // the call or expression.
                    let new_statement = next.is_none_or(|c| {
                        c.is_ascii_alphabetic() || matches!(c, b'_' | b'$' | b'}' | b';')
                    });
                    if !ends_fragment && new_statement {
                        out.push_str(&src[last..=i]);
                        out.push(')');
                        last = i + 1;
                        stack.pop();
                        lines.push(line_at(src, i, base));
                    }
                }
            }
            _ => {}
        }
        if i < b.len() && !b[i].is_ascii_whitespace() {
            prev = Some(i);
        }
        i += 1;
    }
    out.push_str(&src[last..]);
    (out, lines)
}
/// The index of the last byte of the regex literal starting at the `/` at
/// `at` (its flags included), or `None` when that `/` is division. A regex
/// follows an operator, `(`, `,`, `[`, `{`, `;` or nothing; its body ends
/// at an unescaped `/` outside a `[...]` class, on the same line.
fn regex_end(b: &[u8], at: usize, prev: Option<usize>) -> Option<usize> {
    if !regex_allowed(b, prev) {
        return None;
    }
    let mut i = at + 1;
    let mut class = false;
    while i < b.len() {
        match b[i] {
            b'\n' => return None,
            b'\\' => i += 1,
            b'[' => class = true,
            b']' => class = false,
            b'/' if !class => {
                while b.get(i + 1).is_some_and(u8::is_ascii_alphabetic) {
                    i += 1;
                }
                return Some(i);
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Whether a `/` can start a regex after the last significant byte `prev`
/// (comments skipped by the caller): at the start, after an operator or
/// opening bracket, or after a keyword such as `return` or `typeof`.
fn regex_allowed(b: &[u8], prev: Option<usize>) -> bool {
    let Some(p) = prev else {
        return true;
    };
    let c = b[p];
    if b"(,=:[!&|?{};+-*%<>~^".contains(&c) {
        return true;
    }
    if !(c.is_ascii_alphanumeric() || c == b'_' || c == b'$') {
        return false;
    }
    let start = b[..=p]
        .iter()
        .rposition(|c| !(c.is_ascii_alphanumeric() || *c == b'_' || *c == b'$'))
        .map_or(0, |s| s + 1);
    matches!(
        &b[start..=p],
        b"return"
            | b"typeof"
            | b"case"
            | b"do"
            | b"else"
            | b"in"
            | b"of"
            | b"void"
            | b"yield"
            | b"await"
            | b"delete"
            | b"throw"
    )
}

/// Whether the `{` at `at` opens a function or statement body: it follows
/// `=>` or a `)` (`x => {`, `function (a) {`, `if (c) {`).
fn opens_body(b: &[u8], at: usize) -> bool {
    let mut k = at;
    while k > 0 && b[k - 1].is_ascii_whitespace() {
        k -= 1;
    }
    k > 0 && (b[k - 1] == b')' || (k > 1 && &b[k - 2..k] == b"=>"))
}

/// The identifier just before byte `at` (whitespace skipped), e.g. `for`
/// for the `(` of `for (…`.
fn word_before(b: &[u8], at: usize) -> &[u8] {
    let mut end = at;
    while end > 0 && b[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && (b[start - 1].is_ascii_alphanumeric() || b[start - 1] == b'_') {
        start -= 1;
    }
    &b[start..end]
}

/// The index of the quote closing the string or template literal that opens
/// at `at` (`b.len()` if none). A template's `${…}` expressions are walked
/// with their own strings and nested templates, so
/// `` `${a ? `, ${b})` : ")"}` `` ends at its last backtick, not the second.
/// A `'`/`"` string ends at a newline too (an unterminated one).
fn string_end(b: &[u8], at: usize) -> usize {
    let q = b[at];
    let mut i = at + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            c if c == q => return i,
            b'\n' if q != b'`' => return i,
            b'$' if q == b'`' && b.get(i + 1) == Some(&b'{') => {
                let mut depth = 0;
                i += 1;
                while i < b.len() {
                    match b[i] {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        b'"' | b'\'' | b'`' => i = string_end(b, i),
                        _ => {}
                    }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    b.len()
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
            // `url(data:image/png;base64,…)`: its `;` is not a terminator.
            b'u' if src[i..].starts_with("url(") => {
                i = src[i..].find(')').map_or(b.len(), |e| i + e);
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
                // `flex-shrink: ;`, or `border:` whose next line already
                // starts another property: a value lost while writing.
                if c == b';'
                    && let Some(decl) = src.get(decl_start..i).map(str::trim)
                {
                    let next_is_property = decl.split_once('\n').is_some_and(|(first, rest)| {
                        let rest = rest.trim_start();
                        let name: String = rest
                            .chars()
                            .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                            .collect();
                        first.trim_end().ends_with(':')
                            && !name.is_empty()
                            && rest[name.len()..].trim_start().starts_with(':')
                    });
                    if decl.ends_with(':') || next_is_property {
                        let first = decl.lines().next().unwrap_or(decl).trim_end();
                        out.push(format!(
                            "line {}: `{first}` has no value",
                            line_at(src, decl_start, base)
                        ));
                    }
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
    // The last significant byte (comments skipped), for regex detection.
    let mut prev: Option<usize> = None;
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
            b'/' if let Some(end) = regex_end(b, i, prev) => {
                i = end;
            }
            b'"' | b'\'' => i = string_end(b, i),
            // An unclosed template literal swallows the rest of the file;
            // name it rather than a bracket far away (seen:
            // `` return `${m}:${s}; `` without its closing backtick).
            b'`' => {
                let end = string_end(b, i);
                if end >= b.len() {
                    return vec![format!(
                        "line {}: template literal (`) is never closed",
                        line_at(src, i, base)
                    )];
                }
                i = end;
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
            // `v8: {` as an item of `const VIDEOS = [`: a key retyped from an
            // object-shaped draft. Brackets still balance, the script dies.
            b'\n' if stack.last().is_some_and(|(c, _)| *c == b'[') => {
                let rest = src[i + 1..].trim_start_matches([' ', '\t']);
                let key: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '$')
                    .collect();
                let after = rest[key.len()..].trim_start_matches([' ', '\t']);
                if key.chars().next().is_some_and(|c| !c.is_ascii_digit())
                    && after.starts_with(':')
                    && !after.starts_with("::")
                {
                    return vec![format!(
                        "line {}: `{key}:` looks like an object key directly inside an array \
                         (`[...]` items have no keys)",
                        line_at(src, i + 1, base)
                    )];
                }
            }
            _ => {}
        }
        if i < b.len() && !b[i].is_ascii_whitespace() {
            prev = Some(i);
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

    /// Probe J's dropped `)`s are closed before the `;`; multi-line values,
    /// selectors, quoted values and HTML outside `<style>` are left alone.
    #[test]
    fn css_missing_closing_parens_are_repaired() {
        let css = ".g {\n  grid-template-columns: repeat(2, minmax(0, 1fr);\n}\n\
                   .p { background: linear-gradient(transparent, rgba(0, 0, 0, 0.75); }\n\
                   .t::after {\n  background:\n    radial-gradient(120px 80px at 75% 25%, \
                   rgba(255,255,255,0.18), transparent 70%),\n    linear-gradient(red,\n      \
                   blue);\n}\n.x:not(.y) { color: red; }\n.q { content: \"(\"; }\n";
        let (out, lines) = repair_css_parens("css/pages.css", css);
        assert_eq!(lines, [2, 4]);
        assert!(out.contains("repeat(2, minmax(0, 1fr));"));
        assert!(out.contains("rgba(0, 0, 0, 0.75)); }"));
        assert!(
            out.contains("\n      blue);\n"),
            "multi-line value untouched"
        );
        assert_eq!(problems("css/pages.css", &out), Vec::<String>::new());

        // Valid CSS stays as written: a `;` inside `url(data:…)`, and bare
        // `1fr` grid tracks (an author's choice, not a slip).
        let valid = ".i { background: url(data:image/png;base64,iVBORw0K) no-repeat; }\n\
                     .g { grid-template-columns: repeat(4, 1fr); }\n";
        let (out, lines) = repair_css_parens("base.css", valid);
        assert!(lines.is_empty() && out == valid, "{lines:?}");
        assert_eq!(problems("base.css", valid), Vec::<String>::new());

        let html = "<p>f(x;</p>\n<style>\n.a { width: calc(100% - (2 * 8px); }\n</style>\n";
        let (out, lines) = repair_css_parens("index.html", html);
        assert_eq!(lines, [3]);
        assert!(out.starts_with("<p>f(x;</p>") && out.contains("8px)); }"));
        assert!(repair_css_parens("app.js", "f(x;").1.is_empty());

        // A value lost in retyping is reported, not guessed at.
        assert_eq!(
            problems("base.css", ".s svg { fill: red; flex-shrink: ; }\n"),
            ["line 1: `flex-shrink:` has no value"]
        );
        assert_eq!(
            problems(
                "pages.css",
                ".pill {\n  background: red;\n  border: \n  border-radius: 999px;\n}\n"
            ),
            ["line 3: `border:` has no value"]
        );
    }

    /// Probe K's `v8: {` inside `const VIDEOS = [` is flagged; keys inside
    /// objects, objects inside arrays and ternaries are not.
    #[test]
    fn js_key_inside_an_array_is_flagged() {
        let bad = "const V = [\n  {\n    id: 1\n  },\n  v8: {\n    id: 8\n  }\n];\n";
        assert_eq!(
            problems("data.js", bad),
            [
                "line 5: `v8:` looks like an object key directly inside an array (`[...]` items have no keys)"
            ]
        );
        let good = "const C = {\n  ch1: { a: [1,\n    2] },\n};\nconst V = [\n  { id: 1 },\n  ok\n    ? 1\n    : 2,\n];\n";
        assert_eq!(problems("data.js", good), Vec::<String>::new());
    }

    /// Probe K's two blank-page causes get their `)`; correct code —
    /// callbacks closed with `})`, object arguments, a regex quantifier in a
    /// call, `if`/`for` blocks, IIFEs — is left alone, and so is a fragment
    /// whose `)` may sit past its end.
    #[test]
    fn js_callback_missing_paren_is_repaired() {
        let bad = "if (subBtn) {\n  list.forEach(pill => {\n    pill.addEventListener('click', () => pill.classList.toggle('active'));\n  }\n}\nsearchInput.addEventListener('keydown', e => { if (e.key === 'Enter') goSearch(); }\nrender();\n";
        let (out, lines) = repair_js_parens("js/app.js", bad);
        assert_eq!(lines, [4, 6]);
        assert!(out.contains("\n  })\n}\n"), "{out}");
        assert!(out.contains("goSearch(); })\nrender();"), "{out}");
        assert_eq!(problems("js/app.js", &out), Vec::<String>::new());

        let good = "items.forEach(x => {\n  f(x);\n});\nsetTimeout(() => { go(); }, 100);\n\
                    call({ a: 1 }, b);\nconst n = s.replace(/\\d{2}/, '');\nfor (const x of xs) {\n  if (x) { y(); }\n}\n\
                    (function () { init(); })();\np.then(() => { a(); }).catch(e => { b(e); });\n";
        let (out, lines) = repair_js_parens("app.js", good);
        assert!(lines.is_empty(), "{lines:?}");
        assert_eq!(out, good);

        let frag = "  items.forEach(x => {\n    f(x);\n  }";
        assert!(repair_js_fragment("app.js", frag).1.is_empty());
        let html = "<script>\nbtn.addEventListener('click', () => { go(); }\n</script>\n<script src=\"a.js\"></script>\n";
        let (out, lines) = repair_js_parens("index.html", html);
        assert_eq!(lines, [2]);
        assert!(out.contains("go(); })\n</script>"));
    }

    /// Probe O: a statement whose `)` was dropped before `;` gets it there,
    /// not after a later function body; `for (…;` heads, ternaries between
    /// callbacks and multi-line calls are left alone; an unclosed template
    /// literal is named by the check.
    #[test]
    fn js_statement_missing_paren_is_repaired_in_place() {
        let bad = "const MAP = Object.fromEntries(LIST.map(c => [c.id, c]);\n\
                   function get(id) { return MAP[id]; }\n";
        let (out, lines) = repair_js_parens("data.js", bad);
        assert_eq!(lines, [1]);
        assert!(
            out.starts_with("const MAP = Object.fromEntries(LIST.map(c => [c.id, c]));\n"),
            "{out}"
        );
        assert!(
            out.contains("function get(id) { return MAP[id]; }\n"),
            "{out}"
        );

        let good = "var R = 1; for ( var n = 0; n < N.length;\n  n += 2 ) {\n  f(n);\n}\n\
                    return root.each(big ? function(node) {\n  a(node);\n} : function(node) {\n  b(node);\n});\n\
                    foo(a,\n  b);\n";
        let (out, lines) = repair_js_parens("lib.js", good);
        assert!(lines.is_empty(), "{lines:?}");
        assert_eq!(out, good);

        assert_eq!(
            problems("fmt.js", "function f(m) {\n  return `${m}:00;\n}\n"),
            ["line 2: template literal (`) is never closed"]
        );
    }
}
