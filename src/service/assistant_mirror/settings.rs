//! Typed, validated Assistant mirror config, stored as one JSON value under
//! [`SETTINGS_KEY`] in the core settings plugin store. Secrets (webhook URLs,
//! SMTP password) are write-only on the wire: [`MirrorSettings::wire`]
//! reports only whether they are set.

use std::collections::BTreeMap;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Plugin-store key under `SETTINGS_NS` / `SETTINGS_COLLECTION`.
pub const SETTINGS_KEY: &str = "assistant_mirror";

/// Env flag that also accepts `http://127.0.0.1:<port>/…` webhook URLs, so
/// integration tests can point the mirror at a local receiver. Never set in
/// production: every other host is refused (no SSRF through an admin field).
pub const TEST_ENDPOINTS_ENV: &str = "PECKBOARD_MIRROR_TEST_ENDPOINTS";

/// Field path (`"slack.webhook_url"`) → message, for per-field UI errors.
pub type FieldErrors = BTreeMap<String, String>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebhookChannel {
    pub enabled: bool,
    /// Secret; `""` = unset.
    pub webhook_url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmailChannel {
    pub enabled: bool,
    /// Recipient address; `""` = unset.
    pub to: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SmtpTls {
    #[default]
    Starttls,
    /// Implicit TLS (SMTPS, usually port 465).
    Tls,
    /// Plaintext; only allowed for a loopback host.
    None,
}

impl SmtpTls {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "starttls" => SmtpTls::Starttls,
            "tls" => SmtpTls::Tls,
            "none" => SmtpTls::None,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    pub tls: SmtpTls,
    pub username: String,
    /// Secret; `""` = unset.
    pub password: String,
    pub from: String,
}

impl Default for SmtpSettings {
    fn default() -> Self {
        SmtpSettings {
            host: String::new(),
            port: 587,
            tls: SmtpTls::Starttls,
            username: String::new(),
            password: String::new(),
            from: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MirrorSettings {
    /// Replace fenced code blocks and indented output with `[code omitted]`.
    pub redact_code_blocks: bool,
    pub slack: WebhookChannel,
    pub discord: WebhookChannel,
    pub email: EmailChannel,
    pub smtp: SmtpSettings,
}

impl Default for MirrorSettings {
    fn default() -> Self {
        MirrorSettings {
            redact_code_blocks: true,
            slack: WebhookChannel::default(),
            discord: WebhookChannel::default(),
            email: EmailChannel::default(),
            smtp: SmtpSettings::default(),
        }
    }
}

// ── PUT body ────────────────────────────────────────────────────────────

/// Partial update. Omitted fields keep their value. Secret fields
/// (`webhook_url`, `smtp.password`): omitted = keep, `""` = clear.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorPatch {
    pub redact_code_blocks: Option<bool>,
    pub slack: Option<WebhookPatch>,
    pub discord: Option<WebhookPatch>,
    pub email: Option<EmailPatch>,
    pub smtp: Option<SmtpPatch>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookPatch {
    pub enabled: Option<bool>,
    pub webhook_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailPatch {
    pub enabled: Option<bool>,
    pub to: Option<String>,
}

/// `port` and `tls` are taken loosely so a bad value is a field error, not
/// a body parse failure.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmtpPatch {
    pub host: Option<String>,
    pub port: Option<i64>,
    pub tls: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub from: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebhookKind {
    Slack,
    Discord,
}

impl MirrorSettings {
    /// Apply `patch` and validate the result. Webhook URLs are checked only
    /// when the patch sets them (they are re-checked before every send).
    pub fn apply(&self, patch: MirrorPatch) -> Result<MirrorSettings, FieldErrors> {
        let mut next = self.clone();
        let mut errors = FieldErrors::new();
        if let Some(v) = patch.redact_code_blocks {
            next.redact_code_blocks = v;
        }
        for (kind, name, p, ch) in [
            (WebhookKind::Slack, "slack", patch.slack, &mut next.slack),
            (
                WebhookKind::Discord,
                "discord",
                patch.discord,
                &mut next.discord,
            ),
        ] {
            let Some(p) = p else { continue };
            if let Some(e) = p.enabled {
                ch.enabled = e;
            }
            if let Some(url) = p.webhook_url {
                let url = url.trim().to_string();
                if !url.is_empty()
                    && let Err(e) = validate_webhook_url(kind, &url)
                {
                    errors.insert(format!("{name}.webhook_url"), e);
                }
                ch.webhook_url = url;
            }
        }
        if let Some(p) = patch.email {
            if let Some(e) = p.enabled {
                next.email.enabled = e;
            }
            if let Some(to) = p.to {
                next.email.to = to.trim().to_string();
            }
        }
        if let Some(p) = patch.smtp {
            if let Some(h) = p.host {
                next.smtp.host = h.trim().to_string();
            }
            if let Some(port) = p.port {
                match u16::try_from(port) {
                    Ok(port) if port > 0 => next.smtp.port = port,
                    _ => {
                        errors.insert("smtp.port".into(), "Port must be 1–65535".into());
                    }
                }
            }
            if let Some(tls) = p.tls {
                match SmtpTls::parse(&tls) {
                    Some(t) => next.smtp.tls = t,
                    None => {
                        errors.insert("smtp.tls".into(), "Must be starttls, tls, or none".into());
                    }
                }
            }
            if let Some(u) = p.username {
                next.smtp.username = u.trim().to_string();
            }
            if let Some(pw) = p.password {
                next.smtp.password = pw;
            }
            if let Some(f) = p.from {
                next.smtp.from = f.trim().to_string();
            }
        }
        next.check(&mut errors);
        if errors.is_empty() {
            Ok(next)
        } else {
            Err(errors)
        }
    }

    /// Whole-config rules (a channel on needs its credentials, address
    /// syntax, plaintext SMTP only to loopback). Existing errors win.
    fn check(&self, errors: &mut FieldErrors) {
        let mut err = |k: &str, m: &str| {
            errors.entry(k.to_string()).or_insert_with(|| m.to_string());
        };
        if self.slack.enabled && self.slack.webhook_url.is_empty() {
            err("slack.webhook_url", "Add a webhook URL to turn Slack on");
        }
        if self.discord.enabled && self.discord.webhook_url.is_empty() {
            err(
                "discord.webhook_url",
                "Add a webhook URL to turn Discord on",
            );
        }
        if !self.email.to.is_empty() && !valid_email(&self.email.to) {
            err("email.to", "Not a valid email address");
        }
        if !self.smtp.from.is_empty() && !valid_email(&self.smtp.from) {
            err("smtp.from", "Not a valid email address");
        }
        if !self.smtp.host.is_empty() && !valid_host(&self.smtp.host) {
            err(
                "smtp.host",
                "Enter a host name or IP address, without a scheme or port",
            );
        }
        if self.smtp.tls == SmtpTls::None
            && !self.smtp.host.is_empty()
            && !is_loopback_host(&self.smtp.host)
        {
            err(
                "smtp.tls",
                "Unencrypted SMTP is only allowed to localhost; use starttls or tls",
            );
        }
        if !self.smtp.password.is_empty() && self.smtp.username.is_empty() {
            err("smtp.username", "Required when a password is set");
        }
        if self.email.enabled {
            if self.email.to.is_empty() {
                err("email.to", "Add a recipient to turn email on");
            }
            if self.smtp.host.is_empty() {
                err("smtp.host", "Required for email");
            }
            if self.smtp.from.is_empty() {
                err("smtp.from", "Required for email");
            }
        }
    }

    /// Values the console masker must hide (see
    /// [`crate::service::secret_mask::set_extra_secrets`]).
    pub fn secret_values(&self) -> Vec<String> {
        vec![
            self.slack.webhook_url.clone(),
            self.discord.webhook_url.clone(),
            self.smtp.password.clone(),
        ]
    }

    /// The GET shape minus statuses and presence (added by the caller).
    pub fn wire(&self, status: &super::Statuses, watched: bool) -> serde_json::Value {
        serde_json::json!({
            "redact_code_blocks": self.redact_code_blocks,
            "slack": {
                "enabled": self.slack.enabled,
                "webhook_set": !self.slack.webhook_url.is_empty(),
                "status": status.slack,
            },
            "discord": {
                "enabled": self.discord.enabled,
                "webhook_set": !self.discord.webhook_url.is_empty(),
                "status": status.discord,
            },
            "email": {
                "enabled": self.email.enabled,
                "to": self.email.to,
                "status": status.email,
            },
            "smtp": {
                "host": self.smtp.host,
                "port": self.smtp.port,
                "tls": self.smtp.tls,
                "username": self.smtp.username,
                "password_set": !self.smtp.password.is_empty(),
                "from": self.smtp.from,
            },
            "watched": watched,
        })
    }
}

fn test_endpoints_allowed() -> bool {
    std::env::var(TEST_ENDPOINTS_ENV).is_ok_and(|v| v == "1")
}

/// Slack: `https://hooks.slack.com/services/…`. Discord:
/// `https://discord.com/api/webhooks/…` (or `discordapp.com`). With
/// [`TEST_ENDPOINTS_ENV`] set, `http://127.0.0.1:<port>/…` too.
pub fn validate_webhook_url(kind: WebhookKind, raw: &str) -> Result<(), String> {
    let bad = || match kind {
        WebhookKind::Slack => {
            "Must be a Slack incoming webhook (https://hooks.slack.com/services/…)".to_string()
        }
        WebhookKind::Discord => {
            "Must be a Discord webhook (https://discord.com/api/webhooks/…)".to_string()
        }
    };
    let url = reqwest::Url::parse(raw).map_err(|_| bad())?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(bad());
    }
    let host = url.host_str().unwrap_or_default();
    if test_endpoints_allowed() && url.scheme() == "http" && host == "127.0.0.1" {
        return Ok(());
    }
    if url.scheme() != "https" || url.port().is_some() {
        return Err(bad());
    }
    let ok = match kind {
        WebhookKind::Slack => host == "hooks.slack.com" && url.path().starts_with("/services/"),
        WebhookKind::Discord => {
            matches!(host, "discord.com" | "discordapp.com")
                && url.path().starts_with("/api/webhooks/")
        }
    };
    if ok { Ok(()) } else { Err(bad()) }
}

/// A bare `local@domain` address (no display name).
pub fn valid_email(s: &str) -> bool {
    !s.contains(['<', '>', ' ']) && lettre::Address::from_str(s).is_ok()
}

fn valid_host(h: &str) -> bool {
    h.len() <= 253
        && (h.parse::<std::net::IpAddr>().is_ok()
            || h.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                    && !label.starts_with('-')
                    && !label.ends_with('-')
            }))
}

pub fn is_loopback_host(h: &str) -> bool {
    h.eq_ignore_ascii_case("localhost")
        || h.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(v: serde_json::Value) -> MirrorPatch {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn webhook_urls_are_pinned_to_the_real_hosts() {
        use WebhookKind::*;
        assert!(validate_webhook_url(Slack, "https://hooks.slack.com/services/T0/B0/x").is_ok());
        assert!(validate_webhook_url(Discord, "https://discord.com/api/webhooks/1/abc").is_ok());
        assert!(validate_webhook_url(Discord, "https://discordapp.com/api/webhooks/1/abc").is_ok());
        for (kind, bad) in [
            (Slack, "http://hooks.slack.com/services/x"),
            (Slack, "https://hooks.slack.com.evil.com/services/x"),
            (Slack, "https://hooks.slack.com:8443/services/x"),
            (Slack, "https://user@hooks.slack.com/services/x"),
            (Slack, "https://hooks.slack.com/other"),
            (Discord, "https://discord.com/other/webhooks/1"),
            (Discord, "https://hooks.slack.com/services/x"),
            (Discord, "http://127.0.0.1:9/api/webhooks/1"),
            (Slack, "not a url"),
        ] {
            assert!(validate_webhook_url(kind, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn enabling_needs_credentials_and_errors_are_per_field() {
        let s = MirrorSettings::default();
        let errs = s
            .apply(patch(serde_json::json!({
                "slack": { "enabled": true },
                "discord": { "webhook_url": "https://example.com/x" },
                "email": { "enabled": true, "to": "nope" },
                "smtp": { "port": 70000, "tls": "ssl" },
            })))
            .unwrap_err();
        for key in [
            "slack.webhook_url",
            "discord.webhook_url",
            "email.to",
            "smtp.host",
            "smtp.from",
            "smtp.port",
            "smtp.tls",
        ] {
            assert!(errs.contains_key(key), "{key} missing in {errs:?}");
        }
    }

    #[test]
    fn secrets_keep_on_omit_and_clear_on_empty() {
        let s = MirrorSettings::default()
            .apply(patch(serde_json::json!({
                "slack": { "enabled": true, "webhook_url": "https://hooks.slack.com/services/a" },
                "smtp": { "host": "smtp.example.com", "username": "u", "password": "pw",
                          "from": "bot@example.com" },
                "email": { "enabled": true, "to": "me@example.com" },
            })))
            .unwrap();
        let kept = s
            .apply(patch(
                serde_json::json!({ "slack": { "enabled": true }, "smtp": {} }),
            ))
            .unwrap();
        assert_eq!(kept.slack.webhook_url, "https://hooks.slack.com/services/a");
        assert_eq!(kept.smtp.password, "pw");
        // Clearing the URL while the channel stays on is refused…
        assert!(
            s.apply(patch(serde_json::json!({ "slack": { "webhook_url": "" } })))
                .is_err()
        );
        // …but clearing it together with turning the channel off works.
        let cleared = s
            .apply(patch(serde_json::json!({
                "slack": { "enabled": false, "webhook_url": "" },
                "smtp": { "password": "" },
            })))
            .unwrap();
        assert_eq!(cleared.slack.webhook_url, "");
        assert_eq!(cleared.smtp.password, "");
        let wire = cleared.wire(&Default::default(), false);
        assert_eq!(wire["slack"]["webhook_set"], false);
        assert_eq!(wire["smtp"]["password_set"], false);
        assert!(
            !s.wire(&Default::default(), false)
                .to_string()
                .contains("pw")
        );
    }

    #[test]
    fn plaintext_smtp_only_to_loopback() {
        let base = serde_json::json!({ "smtp": { "tls": "none", "host": "smtp.example.com" } });
        let errs = MirrorSettings::default().apply(patch(base)).unwrap_err();
        assert!(errs.contains_key("smtp.tls"));
        for host in ["localhost", "127.0.0.1", "::1"] {
            let ok = serde_json::json!({ "smtp": { "tls": "none", "host": host } });
            assert!(MirrorSettings::default().apply(patch(ok)).is_ok(), "{host}");
        }
    }

    #[test]
    fn email_syntax() {
        assert!(valid_email("a.b+c@example.co"));
        for bad in ["", "a", "a@", "@b.com", "A <a@b.com>", "a b@c.com"] {
            assert!(!valid_email(bad), "{bad}");
        }
    }
}
