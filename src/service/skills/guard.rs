//! Proprietary-content guard: refuses skill / team-memory text that would
//! leak a credential or an internal detail into a shared repo.
//!
//! Applied to public publishes, team publishes and team-memory writes. The
//! credential shapes are the same ones `service::redact` masks in mirrored
//! chat (known token prefixes, PEM private keys, `secret=…` assignments,
//! high-entropy runs) plus the values of every configured secret
//! (`service::secret_mask` — Settings → Environment Variables and sensitive
//! host env). On top: emails, private / internal IPs and hostnames, and
//! absolute home-directory paths, which identify a person or a network.
//!
//! A best-effort denylist, like `redact`: a reviewer still reads what goes
//! public. Findings never echo the matched value — the excerpt redacts it.

use std::net::Ipv4Addr;
use std::sync::OnceLock;

use regex::Regex;

use crate::service::redact;
use crate::service::secret_mask::SecretMasker;

/// One reason the text was refused. `line` is 1-based.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Finding {
    pub line: usize,
    pub kind: String,
    pub excerpt: String,
}

const REDACTED: &str = "[REDACTED]";
const EXCERPT_MAX: usize = 120;

fn email_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b[A-Za-z0-9._%+-]+@((?:[A-Za-z0-9-]+\.)+[A-Za-z]{2,})\b").expect("email")
    })
}

fn ipv4_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}\b").expect("ipv4"))
}

fn internal_host_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\.)+(?:internal|local|corp|lan|intranet|home\.arpa)\b",
        )
        .expect("host")
    })
}

fn home_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:/home/|/Users/|(?i:[A-Z]:\\+Users\\+))([A-Za-z0-9._-]+)").expect("home")
    })
}

/// Domains reserved for documentation (RFC 2606 / 6761) — an address there
/// is an example, not a person.
fn is_example_domain(domain: &str) -> bool {
    let d = domain.to_ascii_lowercase();
    ["example.com", "example.org", "example.net"]
        .iter()
        .any(|e| d == *e || d.ends_with(&format!(".{e}")))
        || [".example", ".test", ".invalid", ".localhost"]
            .iter()
            .any(|t| d.ends_with(t))
}

/// Placeholder user names in documentation paths (`/home/user/...`).
fn is_placeholder_user(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "user"
            | "username"
            | "you"
            | "yourname"
            | "your-name"
            | "me"
            | "name"
            | "example"
            | "runner"
            | "ubuntu"
            | "linuxbrew"
    )
}

/// A `secret=value` whose value is a placeholder, not a credential.
fn is_placeholder_value(v: &str) -> bool {
    let v = v.trim_matches(['"', '\'']);
    let lower = v.to_ascii_lowercase();
    v.len() < 8
        || v.contains(['<', '>', '$', '{', '}', '*', '%'])
        || [
            "xxx",
            "your",
            "example",
            "changeme",
            "placeholder",
            "redacted",
            "...",
            "dummy",
        ]
        .iter()
        .any(|p| lower.contains(p))
        || !(v.bytes().any(|b| b.is_ascii_digit()) && v.bytes().any(|b| b.is_ascii_alphabetic()))
}

fn ip_is_internal(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_private()
        || ip.is_link_local()
        // Carrier-grade NAT / Tailscale tailnet addresses.
        || (o[0] == 100 && (64..=127).contains(&o[1]))
}

fn token_kind(m: &str) -> &'static str {
    if m.starts_with("gh") || m.starts_with("github_pat_") {
        "github_token"
    } else if m.starts_with("sk-ant-") {
        "anthropic_key"
    } else if m.starts_with("sk-") {
        "openai_key"
    } else if m.starts_with("xox") {
        "slack_token"
    } else if m.starts_with("AKIA") {
        "aws_access_key"
    } else {
        "api_token"
    }
}

/// The line with byte range `[start, end)` replaced, cut to a readable
/// window around it.
fn excerpt(line: &str, start: usize, end: usize) -> String {
    let red = format!("{}{REDACTED}{}", &line[..start], &line[end..]);
    window(&red, start)
}

fn window(text: &str, focus: usize) -> String {
    let text = text.trim_end();
    if text.chars().count() <= EXCERPT_MAX {
        return text.trim().to_string();
    }
    let mut from = focus.saturating_sub(EXCERPT_MAX / 2);
    while from > 0 && !text.is_char_boundary(from) {
        from -= 1;
    }
    let tail: String = text[from..].chars().take(EXCERPT_MAX).collect();
    format!("{}{}", if from > 0 { "…" } else { "" }, tail.trim())
}

/// Scan `text`. `secrets` holds the configured secret values (see
/// [`check`]); pass an empty masker to skip that pass.
pub fn scan(text: &str, secrets: &SecretMasker) -> Vec<Finding> {
    let mut findings: Vec<Finding> = Vec::new();

    // Multi-line: PEM private keys.
    for m in redact::pem_re().find_iter(text) {
        let line = text[..m.start()].matches('\n').count() + 1;
        findings.push(Finding {
            line,
            kind: "private_key".into(),
            excerpt: "-----BEGIN … PRIVATE KEY----- [REDACTED]".into(),
        });
    }

    for (idx, line) in text.lines().enumerate() {
        let n = idx + 1;
        // Ranges already reported on this line, so one value isn't counted
        // twice (a GitHub token is also a high-entropy run).
        let mut taken: Vec<(usize, usize)> = Vec::new();
        let overlaps = |taken: &[(usize, usize)], s: usize, e: usize| {
            taken.iter().any(|&(a, b)| s < b && a < e)
        };
        let mut push = |taken: &mut Vec<(usize, usize)>, kind: &str, s: usize, e: usize| {
            taken.push((s, e));
            findings.push(Finding {
                line: n,
                kind: kind.to_string(),
                excerpt: excerpt(line, s, e),
            });
        };

        if line.contains("PRIVATE KEY-----") {
            taken.push((0, line.len()));
        }
        for m in redact::token_re().find_iter(line) {
            if !overlaps(&taken, m.start(), m.end()) {
                push(&mut taken, token_kind(m.as_str()), m.start(), m.end());
            }
        }
        for m in redact::jwt_re().find_iter(line) {
            if !overlaps(&taken, m.start(), m.end()) {
                push(&mut taken, "jwt", m.start(), m.end());
            }
        }
        for c in redact::assignment_re().captures_iter(line) {
            let v = c.get(3).expect("value group");
            if !overlaps(&taken, v.start(), v.end()) && !is_placeholder_value(v.as_str()) {
                push(&mut taken, "secret_assignment", v.start(), v.end());
            }
        }
        for m in redact::long_run_re().find_iter(line) {
            if !overlaps(&taken, m.start(), m.end()) && redact::looks_random(m.as_str()) {
                push(&mut taken, "high_entropy", m.start(), m.end());
            }
        }
        for c in email_re().captures_iter(line) {
            let m = c.get(0).expect("match");
            let domain = c.get(1).map(|d| d.as_str()).unwrap_or("");
            let local = &m.as_str()[..m.as_str().find('@').unwrap_or(0)];
            if overlaps(&taken, m.start(), m.end())
                || is_example_domain(domain)
                || matches!(local, "git" | "noreply" | "no-reply")
            {
                continue;
            }
            push(&mut taken, "email", m.start(), m.end());
        }
        for m in ipv4_re().find_iter(line) {
            if overlaps(&taken, m.start(), m.end()) {
                continue;
            }
            if let Ok(ip) = m.as_str().parse::<Ipv4Addr>()
                && ip_is_internal(ip)
            {
                push(&mut taken, "private_ip", m.start(), m.end());
            }
        }
        for m in internal_host_re().find_iter(line) {
            let host = m.as_str().to_ascii_lowercase();
            if overlaps(&taken, m.start(), m.end()) || host == "host.docker.internal" {
                continue;
            }
            push(&mut taken, "internal_host", m.start(), m.end());
        }
        for c in home_path_re().captures_iter(line) {
            let m = c.get(0).expect("match");
            let user = c.get(1).map(|u| u.as_str()).unwrap_or("");
            if overlaps(&taken, m.start(), m.end()) || is_placeholder_user(user) {
                continue;
            }
            push(&mut taken, "home_path", m.start(), m.end());
        }
        if !secrets.is_empty()
            && let std::borrow::Cow::Owned(masked) = secrets.mask(line)
        {
            let focus = masked.find(crate::service::secret_mask::MASK).unwrap_or(0);
            findings.push(Finding {
                line: n,
                kind: "configured_secret".into(),
                excerpt: window(&masked, focus),
            });
        }
    }
    findings.sort_by_key(|f| f.line);
    findings
}

/// [`scan`] with the configured secrets of this instance (every env-var
/// value from every scope, sensitive host env, the server's own keys).
pub async fn check(db: &crate::db::Db, text: &str) -> Vec<Finding> {
    scan(text, &masker(db).await)
}

async fn masker(db: &crate::db::Db) -> SecretMasker {
    let db = db.clone();
    tokio::task::spawn_blocking(move || crate::service::secret_mask::masker_blocking(&db))
        .await
        .unwrap_or_else(|_| SecretMasker::new(Vec::new()))
}

/// [`check`] for a document whose `body` the user wrote and whose metadata
/// (name, description, tags, title — `meta`) rides in frontmatter. Body
/// findings keep the body's own 1-based line numbers (what the user sees in
/// the editor); metadata findings are reported as line 0.
pub async fn check_document(db: &crate::db::Db, meta: &str, body: &str) -> Vec<Finding> {
    let masker = masker(db).await;
    let mut out: Vec<Finding> = scan(meta, &masker)
        .into_iter()
        .map(|f| Finding { line: 0, ..f })
        .collect();
    out.extend(scan(body, &masker));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<String> {
        scan(text, &SecretMasker::new(Vec::new()))
            .into_iter()
            .map(|f| f.kind)
            .collect()
    }

    #[test]
    fn flags_credentials_and_internal_details() {
        let cases = [
            (
                "token ghp_abcdefghijklmnopqrstuvwxyz0123456789",
                "github_token",
            ),
            (
                "ANTHROPIC=sk-ant-api03-abcdefghijklmnop1234",
                "anthropic_key",
            ),
            ("key sk-proj-abcdefghijklmnop12345678", "openai_key"),
            ("slack xoxb-1234567890-abcdefghij", "slack_token"),
            ("aws AKIAABCDEFGHIJKLMNOP here", "aws_access_key"),
            (
                "aws_secret_access_key = wJalrXUtnFEMI7K7MDENGbPxRfiCY1",
                "secret_assignment",
            ),
            (
                "blob 9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f0a9b8c7d6e",
                "high_entropy",
            ),
            ("mail jane.doe@acme-corp.com for access", "email"),
            ("ssh into 10.20.30.40 first", "private_ip"),
            ("tailnet 100.101.102.103", "private_ip"),
            ("db at pg01.prod.internal:5432", "internal_host"),
            ("printer.lan is down", "internal_host"),
            ("open /home/alice/projects/x", "home_path"),
            ("open /Users/bob/src", "home_path"),
            (r"C:\Users\carol\repo", "home_path"),
        ];
        for (text, kind) in cases {
            let k = kinds(text);
            assert!(
                k.contains(&kind.to_string()),
                "{text:?} → {k:?}, want {kind}"
            );
        }
        let pem = "intro\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\n";
        let f = scan(pem, &SecretMasker::new(Vec::new()));
        assert_eq!(f[0].kind, "private_key");
        assert_eq!(f[0].line, 2);
    }

    #[test]
    fn spares_ordinary_documentation() {
        let ok = "# Review checklist\n\
                  Run `cargo test` in /home/user/project or ~/src.\n\
                  Email support@example.com, clone git@github.com:org/repo.git.\n\
                  Serve on 127.0.0.1:8080 or 0.0.0.0; public DNS is 8.8.8.8.\n\
                  Set API_KEY=<your-api-key> and token: $GITHUB_TOKEN.\n\
                  Commit 3f2c1a0 and uuid 123e4567-e89b-12d3-a456-426614174000.\n\
                  host.docker.internal works inside containers.\n";
        assert_eq!(kinds(ok), Vec::<String>::new());
    }

    #[test]
    fn configured_secret_values_are_flagged_without_echoing_them() {
        let masker = SecretMasker::new(vec!["hunter2-very-secret".to_string()]);
        let f = scan("password is hunter2-very-secret ok", &masker);
        let hit = f.iter().find(|f| f.kind == "configured_secret").unwrap();
        assert!(!hit.excerpt.contains("hunter2"));
        // Excerpts of pattern findings redact the value too.
        let f = scan(
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            &SecretMasker::new(vec![]),
        );
        assert!(!f[0].excerpt.contains("ghp_abc"));
        assert!(f[0].excerpt.contains(REDACTED));
    }

    /// Findings point at the body line the user wrote, not at a line of the
    /// serialized file (whose frontmatter would shift them).
    #[tokio::test]
    async fn document_findings_use_body_line_numbers_and_meta_is_line_zero() {
        let db = crate::db::Db::in_memory().unwrap();
        let body = "How we deploy:\nssh into pg01.prod.internal first\nthen export AWS_KEY=AKIAABCDEFGHIJKLMNOP\n";
        let f = check_document(&db, "notes\nmail ops@acme-corp.com", body).await;
        let lines: Vec<(usize, &str)> = f.iter().map(|x| (x.line, x.kind.as_str())).collect();
        assert!(lines.contains(&(0, "email")), "{lines:?}");
        assert!(lines.contains(&(2, "internal_host")), "{lines:?}");
        assert!(lines.contains(&(3, "aws_access_key")), "{lines:?}");
    }
}
