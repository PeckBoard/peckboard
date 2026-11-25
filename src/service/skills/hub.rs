//! Client for the hosted public skills registry (`skills.peckboard.com`,
//! or whatever hub a public source's index URL lives on — see
//! [`sources::hub_base_url`](super::sources::hub_base_url)).
//!
//! Reads are plain static files (`index.json`, `skills/<name>/<version>/
//! SKILL.md`, `groups/<slug>.json`) and go through `sources`. Publishing is
//! `GET /api/challenge` → optional proof of work → sign → `POST
//! /api/publish`. The signing key is this install's permanent Ed25519
//! identity (`<data_dir>/remote_access_identity`, shared with remote access):
//! the hub binds a skill name to the first key that publishes it, so the
//! same install keeps owning its names across restarts and upgrades.
//!
//! Every other write (handle, fork, sync, merge requests) goes through
//! [`HubClient`] with the `X-Peck-*` signed headers ([`signed_headers`]):
//! the same challenge, then a signature over method, path, nonce and the
//! body's digest. `collab` builds forks and merge requests on top of it.
//!
//! Hub errors come back as `{error, code, findings?}` and are mapped to
//! plain-words [`SkillError`]s; `content_findings` becomes the same
//! [`SkillError::Proprietary`] the local content check raises, so the UI
//! lists the findings the same way.

use std::path::Path;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::guard::Finding;
use super::{SkillError, SkillResult};
use crate::db::Db;
use crate::service::remote_access::IDENTITY_FILE;
use crate::service::remote_access::tunnel::BoxIdentity;

/// What the UI calls the hosted registry.
pub const HUB_LABEL: &str = "Peckboard Public";
const TIMEOUT: Duration = Duration::from_secs(30);
/// The most proof of work we will do (2^28 hashes is already minutes);
/// a hub asking for more is refused rather than pinning a CPU.
const MAX_POW_BITS: u32 = 28;

/// A skill version the hub accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub name: String,
    pub version: String,
    pub url: String,
}

#[derive(Deserialize)]
struct Challenge {
    nonce: String,
    #[serde(default)]
    pow_difficulty: u32,
}

#[derive(Deserialize)]
struct PublishOk {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    url: String,
}

#[derive(Deserialize, Default)]
struct HubErrorBody {
    #[serde(default)]
    error: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    findings: Vec<HubFinding>,
}

#[derive(Deserialize)]
struct HubFinding {
    #[serde(default)]
    line: usize,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    excerpt: String,
}

/// This install's identity key: loaded, or created when nothing pins it
/// yet — the same rule as remote access (a new key would lock out every
/// enrolled device, so it is never regenerated once one exists).
pub async fn identity(db: &Db, data_dir: &Path) -> SkillResult<BoxIdentity> {
    let path = data_dir.join(IDENTITY_FILE);
    let create = matches!(db.list_remote_device_enrollments().await, Ok(e) if e.is_empty());
    tokio::task::spawn_blocking(move || BoxIdentity::load_or_create_with(&path, create))
        .await
        .map_err(|e| SkillError::Internal(e.into()))?
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                SkillError::Refused {
                    status: 500,
                    code: "identity_missing".into(),
                    message: format!(
                        "This PeckBoard's identity key ({IDENTITY_FILE} in its data folder) is \
                         missing, so it can't sign the publish. Restore the file, or revoke every \
                         remote-access device so a new key can be made."
                    ),
                }
            } else {
                SkillError::Internal(anyhow::anyhow!("identity key: {e}"))
            }
        })
}

/// Publish one `SKILL.md` (`name`@`version`) to the hub at `base`.
pub async fn publish(
    base: &str,
    identity: &BoxIdentity,
    name: &str,
    version: &str,
    skill_md: &str,
) -> SkillResult<Published> {
    let hub = HubClient::new(base)?;
    let public_key = STANDARD.encode(identity.public_key());
    let on_http = |status: u16, text: &str| hub_error(status, text, name, version);
    let (nonce, pow) = hub.challenge(&public_key, &on_http).await?;
    let digest = hex::encode(Sha256::digest(skill_md.as_bytes()));
    let signature = STANDARD.encode(identity.sign_skills_publish(&nonce, &digest));

    let mut body = serde_json::json!({
        "skill_md": skill_md,
        "public_key": public_key,
        "nonce": nonce,
        "signature": signature,
    });
    if let Some(pow) = pow {
        body["pow"] = serde_json::Value::String(pow);
    }
    let resp = hub
        .client
        .post(format!("{}/api/publish", hub.base))
        .json(&body)
        .send()
        .await
        .map_err(unreachable)?;
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(unreachable)?;
    if !(200..300).contains(&status) {
        return Err(hub_error(status, &text, name, version));
    }
    let ok: PublishOk = serde_json::from_str(&text).unwrap_or(PublishOk {
        name: String::new(),
        version: String::new(),
        url: String::new(),
    });
    let name = if ok.name.is_empty() {
        name.into()
    } else {
        ok.name
    };
    let version = if ok.version.is_empty() {
        version.into()
    } else {
        ok.version
    };
    // Only an http(s) link is handed to the UI as a link.
    let url = if ok.url.starts_with("https://") || ok.url.starts_with("http://") {
        ok.url
    } else {
        format!("{base}/skills/{name}/{version}/SKILL.md")
    };
    Ok(Published { name, version, url })
}

fn unreachable(e: reqwest::Error) -> SkillError {
    SkillError::Upstream(format!("Couldn't reach {HUB_LABEL}: {e}"))
}

// ── Signed-header requests: handles, forks, merge requests ──────────────

/// A connection to one hub (`base` = `https://skills.peckboard.com`).
/// Reads are plain GETs; every write other than publish / yank carries the
/// `X-Peck-*` signed headers ([`signed_headers`]).
pub struct HubClient {
    base: String,
    client: reqwest::Client,
}

/// A skill or fork name (`name@handle`) safe to put in a URL path as is —
/// signing covers the exact path, so it is never percent-encoded.
pub fn path_name(name: &str) -> SkillResult<&str> {
    let ok = !name.is_empty()
        && name.len() <= 100
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'@');
    if ok {
        Ok(name)
    } else {
        Err(SkillError::BadRequest(format!(
            "'{name}' is not a skill or fork name"
        )))
    }
}

/// The `X-Peck-*` headers of one signed request: the signature covers
/// `METHOD`, the URL `path` (no query), the challenge `nonce` and
/// `sha256(body)` (`""` when there is no body).
pub fn signed_headers(
    identity: &BoxIdentity,
    method: &str,
    path: &str,
    nonce: &str,
    pow: Option<&str>,
    body: &[u8],
) -> Vec<(&'static str, String)> {
    let digest = hex::encode(Sha256::digest(body));
    let signature = identity.sign_request(method, path, nonce, &digest);
    let mut headers = vec![
        ("X-Peck-Key", STANDARD.encode(identity.public_key())),
        ("X-Peck-Nonce", nonce.to_string()),
        ("X-Peck-Signature", STANDARD.encode(signature)),
    ];
    if let Some(pow) = pow {
        headers.push(("X-Peck-Pow", pow.to_string()));
    }
    headers
}

impl HubClient {
    pub fn new(base: &str) -> SkillResult<Self> {
        let base = base.trim_end_matches('/');
        if !(base.starts_with("https://") || base.starts_with("http://")) {
            return Err(SkillError::BadRequest(format!(
                "the public source's URL must be http(s), got {base}"
            )));
        }
        let client = crate::http_client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| SkillError::Internal(e.into()))?;
        Ok(Self {
            base: base.to_string(),
            client,
        })
    }

    /// `GET /api/challenge` → `(nonce, pow)`, solving the proof of work
    /// when the hub asks for one.
    async fn challenge(
        &self,
        public_key: &str,
        on_http: &(dyn Fn(u16, &str) -> SkillError + Sync),
    ) -> SkillResult<(String, Option<String>)> {
        let resp = self
            .client
            .get(format!(
                "{}/api/challenge?public_key={}",
                self.base,
                urlencoding::encode(public_key)
            ))
            .send()
            .await
            .map_err(unreachable)?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(unreachable)?;
        if !(200..300).contains(&status) {
            return Err(on_http(status, &text));
        }
        let challenge: Challenge = serde_json::from_str(&text).map_err(|e| {
            SkillError::Upstream(format!("{HUB_LABEL} sent an unreadable challenge: {e}"))
        })?;
        if challenge.pow_difficulty > MAX_POW_BITS {
            return Err(SkillError::Upstream(format!(
                "{HUB_LABEL} asked for more proof of work than this PeckBoard will do ({} bits). \
                 Try again later.",
                challenge.pow_difficulty
            )));
        }
        let pow = if challenge.pow_difficulty > 0 {
            let (nonce, bits) = (challenge.nonce.clone(), challenge.pow_difficulty);
            Some(
                tokio::task::spawn_blocking(move || solve_pow(&nonce, bits))
                    .await
                    .map_err(|e| SkillError::Internal(e.into()))?,
            )
        } else {
            None
        };
        Ok((challenge.nonce, pow))
    }

    async fn read(resp: reqwest::Response, op: &str) -> SkillResult<Value> {
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(unreachable)?;
        if !(200..300).contains(&status) {
            return Err(request_error(status, &text, op));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| {
            SkillError::Upstream(format!("{HUB_LABEL} sent an unreadable answer: {e}"))
        })
    }

    async fn get_json(&self, path: &str, op: &str) -> SkillResult<Value> {
        let resp = self
            .client
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .map_err(unreachable)?;
        Self::read(resp, op).await
    }

    /// A signed write. `path` is relative to the hub base; the signature
    /// covers the full URL path the hub sees.
    async fn signed(
        &self,
        identity: &BoxIdentity,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        op: &str,
    ) -> SkillResult<Value> {
        let url = reqwest::Url::parse(&format!("{}{path}", self.base))
            .map_err(|e| SkillError::BadRequest(format!("bad hub URL: {e}")))?;
        let public_key = STANDARD.encode(identity.public_key());
        let on_http = |status: u16, text: &str| request_error(status, text, op);
        let (nonce, pow) = self.challenge(&public_key, &on_http).await?;
        let raw = match body {
            Some(b) => serde_json::to_vec(b).map_err(|e| SkillError::Internal(e.into()))?,
            None => Vec::new(),
        };
        let headers = signed_headers(
            identity,
            method.as_str(),
            url.path(),
            &nonce,
            pow.as_deref(),
            &raw,
        );
        let mut req = self.client.request(method, url);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        if body.is_some() {
            req = req.header("Content-Type", "application/json").body(raw);
        }
        let resp = req.send().await.map_err(unreachable)?;
        Self::read(resp, op).await
    }

    /// `GET /api/publishers/<key>` → `{public_key, handle}`.
    pub async fn publisher(&self, public_key: &str) -> SkillResult<Value> {
        let path = format!("/api/publishers/{}", urlencoding::encode(public_key));
        self.get_json(&path, "look up this PeckBoard's handle")
            .await
    }

    /// `POST /api/handle` → `{handle}`.
    pub async fn set_handle(&self, identity: &BoxIdentity, handle: &str) -> SkillResult<Value> {
        let body = serde_json::json!({ "handle": handle });
        self.signed(
            identity,
            reqwest::Method::POST,
            "/api/handle",
            Some(&body),
            "change the handle",
        )
        .await
    }

    /// `GET /api/skills/<name>` — a skill's or fork's owner, parent and
    /// versions.
    pub async fn skill(&self, name: &str) -> SkillResult<Value> {
        let path = format!("/api/skills/{}", path_name(name)?);
        self.get_json(&path, "read the skill").await
    }

    /// `GET /api/skills/<name>/history`, newest first.
    pub async fn history(&self, name: &str) -> SkillResult<Value> {
        let path = format!("/api/skills/{}/history", path_name(name)?);
        self.get_json(&path, "read the skill's history").await
    }

    /// `GET /api/skills/<name>/forks`.
    pub async fn forks(&self, name: &str) -> SkillResult<Value> {
        let path = format!("/api/skills/{}/forks", path_name(name)?);
        self.get_json(&path, "list the skill's forks").await
    }

    /// `GET /api/skills/<name>/diff?from=&to=` → `{from, to, diff}`.
    pub async fn diff(&self, name: &str, from: &str, to: &str) -> SkillResult<Value> {
        let path = format!(
            "/api/skills/{}/diff?from={}&to={}",
            path_name(name)?,
            urlencoding::encode(from),
            urlencoding::encode(to)
        );
        self.get_json(&path, "compare the versions").await
    }

    /// One published `SKILL.md`, as text.
    pub async fn skill_md(&self, name: &str, version: &str) -> SkillResult<String> {
        if semver::Version::parse(version).is_err() {
            return Err(SkillError::BadRequest(format!(
                "'{version}' is not a version"
            )));
        }
        let resp = self
            .client
            .get(format!(
                "{}/skills/{}/{version}/SKILL.md",
                self.base,
                path_name(name)?
            ))
            .send()
            .await
            .map_err(unreachable)?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(unreachable)?;
        if !(200..300).contains(&status) {
            return Err(request_error(status, &text, "read the skill"));
        }
        Ok(text)
    }

    /// `POST /api/fork` → `{fork, url}`.
    pub async fn fork(&self, identity: &BoxIdentity, name: &str) -> SkillResult<Value> {
        let body = serde_json::json!({ "name": path_name(name)? });
        self.signed(
            identity,
            reqwest::Method::POST,
            "/api/fork",
            Some(&body),
            "fork the skill",
        )
        .await
    }

    /// `POST /api/forks/<fork>/sync` → `{status}`.
    pub async fn sync_fork(&self, identity: &BoxIdentity, fork: &str) -> SkillResult<Value> {
        let path = format!("/api/forks/{}/sync", path_name(fork)?);
        self.signed(
            identity,
            reqwest::Method::POST,
            &path,
            None,
            "sync the fork",
        )
        .await
    }

    /// `POST /api/merge-requests` → the new merge request.
    pub async fn create_merge_request(
        &self,
        identity: &BoxIdentity,
        fork: &str,
        title: &str,
        description: &str,
    ) -> SkillResult<Value> {
        let body = serde_json::json!({
            "fork": path_name(fork)?,
            "title": title,
            "description": description,
        });
        self.signed(
            identity,
            reqwest::Method::POST,
            "/api/merge-requests",
            Some(&body),
            "open the merge request",
        )
        .await
    }

    /// `GET /api/merge-requests?<query>` (`target=` or `key=` + `role=`,
    /// and `state=`).
    pub async fn list_merge_requests(&self, query: &[(&str, &str)]) -> SkillResult<Value> {
        let qs: Vec<String> = query
            .iter()
            .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
            .collect();
        let path = format!("/api/merge-requests?{}", qs.join("&"));
        self.get_json(&path, "list merge requests").await
    }

    /// `GET /api/merge-requests/<id>` — with its diff and comments.
    pub async fn merge_request(&self, id: u64) -> SkillResult<Value> {
        self.get_json(
            &format!("/api/merge-requests/{id}"),
            "read the merge request",
        )
        .await
    }

    /// `POST /api/merge-requests/<id>/comments` → the comment.
    pub async fn comment_merge_request(
        &self,
        identity: &BoxIdentity,
        id: u64,
        body: &str,
    ) -> SkillResult<Value> {
        let payload = serde_json::json!({ "body": body });
        self.signed(
            identity,
            reqwest::Method::POST,
            &format!("/api/merge-requests/{id}/comments"),
            Some(&payload),
            "comment on the merge request",
        )
        .await
    }

    /// `POST /api/merge-requests/<id>/merge` → the merged merge request.
    pub async fn merge_merge_request(
        &self,
        identity: &BoxIdentity,
        id: u64,
        publish: bool,
        bump: Option<&str>,
        version: Option<&str>,
    ) -> SkillResult<Value> {
        let mut payload = serde_json::json!({ "publish": publish });
        if let Some(v) = version {
            payload["version"] = Value::String(v.to_string());
        } else if let Some(b) = bump {
            payload["bump"] = Value::String(b.to_string());
        }
        self.signed(
            identity,
            reqwest::Method::POST,
            &format!("/api/merge-requests/{id}/merge"),
            Some(&payload),
            "merge the merge request",
        )
        .await
    }

    /// `POST /api/merge-requests/<id>/close` → the closed merge request.
    pub async fn close_merge_request(&self, identity: &BoxIdentity, id: u64) -> SkillResult<Value> {
        self.signed(
            identity,
            reqwest::Method::POST,
            &format!("/api/merge-requests/{id}/close"),
            None,
            "close the merge request",
        )
        .await
    }
}

/// A hub error response to any request but a publish, in plain words. `op`
/// finishes "couldn't …" (`"fork the skill"`).
pub(crate) fn request_error(status: u16, text: &str, op: &str) -> SkillError {
    let body: HubErrorBody = serde_json::from_str(text).unwrap_or_default();
    let code = if body.code.is_empty() {
        match status {
            429 => "rate_limited",
            413 => "too_large",
            404 => "not_found",
            _ => "",
        }
        .to_string()
    } else {
        body.code.clone()
    };
    let detail = if body.error.is_empty() {
        format!("HTTP {status}")
    } else {
        body.error.clone()
    };
    let refused = |status: u16, message: String| SkillError::Refused {
        status,
        code: code.clone(),
        message,
    };
    match code.as_str() {
        "content_findings" if !body.findings.is_empty() => SkillError::Proprietary(
            body.findings
                .into_iter()
                .map(|f| Finding {
                    line: f.line,
                    kind: f.kind,
                    excerpt: f.excerpt,
                })
                .collect(),
        ),
        "content_findings" => refused(
            422,
            format!("{HUB_LABEL}'s content check refused it: {detail}"),
        ),
        "not_owner" => refused(
            403,
            format!("Only the owner can {op}, and this PeckBoard isn't the owner."),
        ),
        "not_found" => refused(
            404,
            format!("{HUB_LABEL} couldn't find that, so it couldn't {op} ({detail})."),
        ),
        "handle_taken" => refused(
            409,
            "Another PeckBoard already uses that handle. Pick a different one.".into(),
        ),
        "already_open" => refused(409, "This fork already has an open merge request.".into()),
        "already_forked" => refused(
            409,
            "This PeckBoard already has a fork of that skill.".into(),
        ),
        "not_mergeable" => refused(
            409,
            "The fork no longer merges cleanly into the original. It needs an update from \
             the fork: sync the fork from the original, fix what clashes, and publish it again."
                .into(),
        ),
        "name_taken" => refused(
            409,
            format!("{HUB_LABEL} already has something by that name: {detail}"),
        ),
        "version_exists" => refused(
            409,
            format!(
                "That version is already on {HUB_LABEL}, and published versions can't change. \
                 Pick a newer version."
            ),
        ),
        "rate_limited" => refused(
            429,
            format!(
                "{HUB_LABEL} has had too many requests from this PeckBoard for now. Wait a \
                 while and try again."
            ),
        ),
        "too_large" => refused(
            413,
            format!("That is too large for {HUB_LABEL}. Shorten it and try again."),
        ),
        "banned" => refused(
            403,
            format!("{HUB_LABEL} no longer accepts changes from this PeckBoard."),
        ),
        "nonce_invalid" => refused(
            502,
            "The request took too long and its one-time code expired. Try again.".into(),
        ),
        "pow_required" => refused(
            502,
            format!("{HUB_LABEL} wanted a proof of work it didn't get. Try again."),
        ),
        "bad_signature" => refused(
            502,
            format!(
                "{HUB_LABEL} couldn't verify this PeckBoard's signature. Try again; if it keeps \
                 happening, this PeckBoard's identity key may be damaged."
            ),
        ),
        "bad_request" => refused(400, format!("{HUB_LABEL} couldn't {op}: {detail}")),
        _ => refused(
            502,
            format!("{HUB_LABEL} couldn't {op} ({status}): {detail}"),
        ),
    }
}

/// A decimal counter `pow` such that `sha256(nonce + pow)` starts with at
/// least `bits` zero bits.
pub(crate) fn solve_pow(nonce: &str, bits: u32) -> String {
    let mut buf = String::with_capacity(nonce.len() + 20);
    for i in 0u64.. {
        buf.clear();
        buf.push_str(nonce);
        buf.push_str(&i.to_string());
        if leading_zero_bits(&Sha256::digest(buf.as_bytes())) >= bits {
            return i.to_string();
        }
    }
    unreachable!("u64 counter exhausted")
}

fn leading_zero_bits(hash: &[u8]) -> u32 {
    let mut n = 0;
    for b in hash {
        if *b == 0 {
            n += 8;
        } else {
            return n + b.leading_zeros();
        }
    }
    n
}

/// A hub error response, in plain words.
fn hub_error(status: u16, text: &str, name: &str, version: &str) -> SkillError {
    let body: HubErrorBody = serde_json::from_str(text).unwrap_or_default();
    let code = if body.code.is_empty() {
        match status {
            429 => "rate_limited",
            413 => "too_large",
            _ => "",
        }
        .to_string()
    } else {
        body.code.clone()
    };
    let detail = if body.error.is_empty() {
        format!("HTTP {status}")
    } else {
        body.error.clone()
    };
    let refused = |status: u16, message: String| SkillError::Refused {
        status,
        code: code.clone(),
        message,
    };
    match code.as_str() {
        "content_findings" if !body.findings.is_empty() => SkillError::Proprietary(
            body.findings
                .into_iter()
                .map(|f| Finding {
                    line: f.line,
                    kind: f.kind,
                    excerpt: f.excerpt,
                })
                .collect(),
        ),
        "content_findings" => refused(
            422,
            format!("{HUB_LABEL}'s content check refused this skill: {detail}"),
        ),
        "name_taken" => refused(
            409,
            format!(
                "Another PeckBoard already published a skill called {name}. Rename yours to \
                 publish it."
            ),
        ),
        "version_exists" => refused(
            409,
            format!(
                "Version {version} of {name} is already on {HUB_LABEL}, and published versions \
                 can't change. Edit the skill to make a new version, then publish that."
            ),
        ),
        "rate_limited" => refused(
            429,
            format!(
                "{HUB_LABEL} has had too many publishes from this PeckBoard for now. Wait a \
                 while and try again."
            ),
        ),
        "too_large" => refused(
            413,
            format!("This skill is too large for {HUB_LABEL}. Shorten it and try again."),
        ),
        "banned" => refused(
            403,
            format!("{HUB_LABEL} no longer accepts skills from this PeckBoard."),
        ),
        "nonce_invalid" => refused(
            502,
            "The publish took too long and its one-time code expired. Try again.".into(),
        ),
        "pow_required" => refused(
            502,
            format!("{HUB_LABEL} wanted a proof of work it didn't get. Try again."),
        ),
        "bad_signature" => refused(
            502,
            format!(
                "{HUB_LABEL} couldn't verify this PeckBoard's signature. Try again; if it keeps \
                 happening, this PeckBoard's identity key may be damaged."
            ),
        ),
        "bad_request" => refused(400, format!("{HUB_LABEL} refused the skill: {detail}")),
        _ => refused(
            502,
            format!("{HUB_LABEL} couldn't publish the skill ({status}): {detail}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::extract::{Query, State};
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use peckboard_relay::identity::verify_skills_publish;

    use super::*;
    use crate::service::skills::{self, Actor, CreateSkill, SourceInput};

    const POW_BITS: u32 = 8;

    #[derive(Default)]
    struct Hub {
        nonce: Option<String>,
        /// name → owner public key.
        owners: Vec<(String, String)>,
    }

    type Shared = Arc<Mutex<Hub>>;

    async fn challenge(
        State(hub): State<Shared>,
        Query(q): Query<std::collections::HashMap<String, String>>,
    ) -> Json<serde_json::Value> {
        assert_eq!(STANDARD.decode(&q["public_key"]).unwrap().len(), 32);
        let nonce = STANDARD.encode(rand::random::<[u8; 16]>());
        hub.lock().unwrap().nonce = Some(nonce.clone());
        Json(serde_json::json!({"nonce": nonce, "expires_in": 60, "pow_difficulty": POW_BITS}))
    }

    async fn publish_route(
        State(hub): State<Shared>,
        Json(b): Json<serde_json::Value>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let s = |k: &str| b[k].as_str().unwrap_or_default().to_string();
        let mut hub = hub.lock().unwrap();
        let nonce = s("nonce");
        if hub.nonce.take().as_deref() != Some(nonce.as_str()) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "nonce", "code": "nonce_invalid"})),
            );
        }
        let pow_ok = leading_zero_bits(&Sha256::digest(format!("{nonce}{}", s("pow")))) >= POW_BITS;
        let key: [u8; 32] = STANDARD
            .decode(s("public_key"))
            .unwrap()
            .try_into()
            .unwrap();
        let sig: [u8; 64] = STANDARD.decode(s("signature")).unwrap().try_into().unwrap();
        let digest = hex::encode(Sha256::digest(s("skill_md").as_bytes()));
        if !pow_ok || !verify_skills_publish(&key, &nonce, &digest, &sig) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "bad sig", "code": "bad_signature"})),
            );
        }
        let doc = skills::format::parse_skill(&s("skill_md")).unwrap();
        match hub.owners.iter().find(|(n, _)| *n == doc.name) {
            Some((_, owner)) if *owner != s("public_key") => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": "taken", "code": "name_taken"})),
            ),
            _ => {
                hub.owners.push((doc.name.clone(), s("public_key")));
                (
                    StatusCode::CREATED,
                    Json(serde_json::json!({
                        "name": doc.name,
                        "version": doc.version,
                        "url": format!("https://skills.example.test/s/{}", doc.name),
                    })),
                )
            }
        }
    }

    #[tokio::test]
    async fn publish_signs_for_the_hub_and_maps_name_taken() {
        let hub: Shared = Arc::default();
        // Another install already owns this name.
        hub.lock()
            .unwrap()
            .owners
            .push(("taken-skill".into(), STANDARD.encode([7u8; 32])));
        let app = Router::new()
            .route("/api/challenge", get(challenge))
            .route("/api/publish", post(publish_route))
            .with_state(hub.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let db = Db::in_memory().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let admin = Actor {
            user_id: Some("u1".into()),
            is_admin: true,
        };
        let source = skills::create_source(
            &db,
            &admin,
            SourceInput {
                kind: "public".into(),
                name: "stub hub".into(),
                url: format!("http://{addr}/index.json"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mk = |name: &str| CreateSkill {
            name: name.into(),
            description: "Write a friendly changelog entry".into(),
            tags: vec![],
            groups: vec![],
            body: "# Changelog\n\nOne line per change.\n".into(),
            version: None,
        };
        let mine = skills::create(&db, &admin, mk("my-skill")).await.unwrap();
        let taken = skills::create(&db, &admin, mk("taken-skill"))
            .await
            .unwrap();

        let ok = skills::publish(&db, data_dir.path(), &admin, &mine.id.unwrap(), &source.id)
            .await
            .unwrap();
        assert_eq!(ok["published"], true);
        assert_eq!(ok["name"], "my-skill");
        assert_eq!(ok["version"], "1.0.0");
        assert_eq!(ok["url"], "https://skills.example.test/s/my-skill");
        // Signed with the install's identity, created on first use.
        let id = identity(&db, data_dir.path()).await.unwrap();
        assert_eq!(
            hub.lock().unwrap().owners[1].1,
            STANDARD.encode(id.public_key())
        );
        assert!(!data_dir.path().join("skills/public-outbox").exists());

        let err = skills::publish(&db, data_dir.path(), &admin, &taken.id.unwrap(), &source.id)
            .await
            .unwrap_err();
        assert_eq!(err.status(), 409);
        assert_eq!(err.body()["code"], "name_taken");
        assert!(
            err.to_string()
                .contains("Another PeckBoard already published a skill called taken-skill"),
            "{err}"
        );
    }

    #[test]
    fn pow_meets_the_difficulty_and_findings_map_to_the_local_shape() {
        let pow = solve_pow("abc", 10);
        assert!(leading_zero_bits(&Sha256::digest(format!("abc{pow}"))) >= 10);
        let err = hub_error(
            422,
            r#"{"error":"x","code":"content_findings","findings":[{"line":3,"kind":"github_token","excerpt":"[REDACTED]"}]}"#,
            "n",
            "1.0.0",
        );
        assert!(matches!(&err, SkillError::Proprietary(f) if f[0].line == 3));
        assert_eq!(hub_error(429, "", "n", "1.0.0").status(), 429);
    }
}
