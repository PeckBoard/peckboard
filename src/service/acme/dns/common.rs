//! Helpers shared by the DNS-01 providers: credential lookup, zone
//! matching, and turning vendor error bodies into readable errors.

use super::Credentials;
use serde_json::Value;

/// A required credential field, trimmed. The error names the field.
pub(super) fn required<'a>(creds: &'a Credentials, key: &str) -> anyhow::Result<&'a str> {
    match creds.get(key).map(|v| v.trim()) {
        Some(v) if !v.is_empty() => Ok(v),
        _ => anyhow::bail!("missing required field '{key}'"),
    }
}

/// An optional credential field; blank counts as absent.
pub(super) fn optional<'a>(creds: &'a Credentials, key: &str) -> Option<&'a str> {
    creds.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// Lowercased, without the trailing root dot.
pub(super) fn normalize(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// The longest zone among `zones` that `fqdn` equals or sits under.
pub(super) fn longest_zone<'a>(
    fqdn: &str,
    zones: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let fqdn = normalize(fqdn);
    zones
        .into_iter()
        .map(normalize)
        .filter(|z| !z.is_empty() && (fqdn == *z || fqdn.ends_with(&format!(".{z}"))))
        .max_by_key(|z| z.len())
}

/// `fqdn` relative to `zone`; empty for the apex.
pub(super) fn relative_name(fqdn: &str, zone: &str) -> String {
    let fqdn = normalize(fqdn);
    let zone = normalize(zone);
    if fqdn == zone {
        return String::new();
    }
    fqdn.strip_suffix(&format!(".{zone}"))
        .unwrap_or(&fqdn)
        .to_string()
}

/// `fqdn` and each parent with at least two labels, longest first — the
/// names to probe on APIs that look zones up by exact name.
pub(super) fn candidates(fqdn: &str) -> Vec<String> {
    let fqdn = normalize(fqdn);
    let labels: Vec<&str> = fqdn.split('.').collect();
    (0..labels.len().saturating_sub(1))
        .map(|i| labels[i..].join("."))
        .collect()
}

/// TXT values are often stored quoted; compare them bare.
pub(super) fn unquote(v: &str) -> &str {
    let v = v.trim();
    v.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(v)
}

/// Send a request; returns the status and body text. Transport errors
/// name the vendor and omit the URL (DuckDNS puts its token there).
pub(super) async fn send(
    req: reqwest::RequestBuilder,
    vendor: &str,
) -> anyhow::Result<(u16, String)> {
    let resp = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("{vendor} API request failed: {}", e.without_url()))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Ok((status, body))
}

/// An error carrying the vendor's own message when the body has one.
pub(super) fn api_error(vendor: &str, status: u16, body: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{vendor} API error (HTTP {status}): {}",
        vendor_message(body)
    )
}

/// Parse a JSON body, erroring (with the vendor message) on a non-2xx.
pub(super) fn json(vendor: &str, status: u16, body: &str) -> anyhow::Result<Value> {
    if !(200..300).contains(&status) {
        return Err(api_error(vendor, status, body));
    }
    if body.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(body).map_err(|e| {
        anyhow::anyhow!(
            "{vendor} API returned invalid JSON ({e}): {}",
            truncate(body)
        )
    })
}

/// Pull the human-readable message out of the common vendor error shapes.
fn vendor_message(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return truncate(body);
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(errs) = v.get("errors").and_then(Value::as_array) {
        for e in errs {
            if let Some(m) = e
                .get("message")
                .or_else(|| e.get("description"))
                .and_then(Value::as_str)
            {
                parts.push(m.to_string());
            } else if let Some(s) = e.as_str() {
                parts.push(s.to_string());
            }
        }
    }
    if let Some(e) = v.get("error") {
        if let Some(m) = e.get("message").and_then(Value::as_str) {
            parts.push(m.to_string());
        } else if let Some(s) = e.as_str() {
            parts.push(s.to_string());
        }
    }
    for key in ["message", "detail"] {
        if let Some(m) = v.get(key).and_then(Value::as_str) {
            parts.push(m.to_string());
        }
    }
    parts.dedup();
    if parts.is_empty() {
        truncate(body)
    } else {
        parts.join("; ")
    }
}

fn truncate(body: &str) -> String {
    let body = body.trim();
    if body.chars().count() > 300 {
        format!("{}…", body.chars().take(300).collect::<String>())
    } else {
        body.to_string()
    }
}

/// Local HTTP server standing in for a vendor API in unit tests.
#[cfg(test)]
pub(super) mod testsrv {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone)]
    pub struct Req {
        pub method: String,
        pub path: String,
        pub query: String,
        pub headers: HashMap<String, String>,
        pub body: String,
    }

    impl Req {
        pub fn header(&self, name: &str) -> &str {
            self.headers.get(name).map(String::as_str).unwrap_or("")
        }
        pub fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
        }
    }

    pub type Log = Arc<Mutex<Vec<Req>>>;

    /// Serve `respond` on 127.0.0.1:0; returns the base URL and the log of
    /// every request received.
    pub async fn serve<F>(respond: F) -> (String, Log)
    where
        F: Fn(&Req) -> (u16, String) + Send + Sync + 'static,
    {
        let log: Log = Arc::default();
        let respond = Arc::new(respond);
        let app = {
            let log = log.clone();
            axum::Router::new().fallback(move |req: axum::extract::Request| {
                let log = log.clone();
                let respond = respond.clone();
                async move {
                    let (parts, body) = req.into_parts();
                    let body = axum::body::to_bytes(body, usize::MAX)
                        .await
                        .unwrap_or_default();
                    let req = Req {
                        method: parts.method.to_string(),
                        path: parts.uri.path().to_string(),
                        query: parts.uri.query().unwrap_or("").to_string(),
                        headers: parts
                            .headers
                            .iter()
                            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                            .collect(),
                        body: String::from_utf8_lossy(&body).into_owned(),
                    };
                    let (status, body) = respond(&req);
                    log.lock().unwrap().push(req);
                    (axum::http::StatusCode::from_u16(status).unwrap(), body)
                }
            })
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), log)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_matching_prefers_longest_suffix() {
        let zones = ["example.com", "sub.example.com", "ample.com"];
        assert_eq!(
            longest_zone("_acme-challenge.www.sub.example.com", zones).as_deref(),
            Some("sub.example.com")
        );
        assert_eq!(
            longest_zone("_acme-challenge.example.com", zones).as_deref(),
            Some("example.com")
        );
        assert_eq!(longest_zone("_acme-challenge.other.org", zones), None);
        assert_eq!(
            relative_name("_acme-challenge.Example.com.", "example.com"),
            "_acme-challenge"
        );
        assert_eq!(
            candidates("_acme-challenge.a.example.com"),
            vec![
                "_acme-challenge.a.example.com",
                "a.example.com",
                "example.com"
            ]
        );
        assert!(
            vendor_message(r#"{"errors":[{"code":1,"message":"bad token"}]}"#)
                .contains("bad token")
        );
    }
}
