//! Let's Encrypt certificates over DNS-01, plus the multi-cert store the
//! HTTPS listener resolves from by SNI.
//!
//! Rows live in `tls_certificates` / `dns_credentials`; key material in
//! `<data>/certs/managed/<id>/{cert,key}.pem` (0600). Every change ends in
//! [`reload_managed`], which re-reads every cert on disk into
//! `TlsState`'s managed set — the resolver picks among them by SNI
//! (`TlsState::resolve_name`).
//!
//! Issuance never needs an inbound port: the challenge is a TXT record
//! published through the operator's DNS provider API ([`dns`]).
//!
//! Test mode (`PECKBOARD_ACME_TEST=1`, or [`AcmeRuntime::set_test_mode`])
//! publishes and removes the TXT records as usual but self-signs the
//! certificate instead of talking to an ACME directory.

pub mod dns;

use anyhow::{Context, Result};
use sha2::Digest;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::db::Db;
use crate::db::models::{DnsCredentialRow, TlsCertificateRow};
use crate::service::tls::{self, TlsMaterial, TlsSource};
use crate::state::{AppState, ManagedCert, TlsState};

/// Env flag for test mode (mock DNS provider + self-signed issuance).
pub const TEST_ENV: &str = "PECKBOARD_ACME_TEST";
/// Env override for the ACME directory URL (e.g. a local Pebble).
pub const DIRECTORY_ENV: &str = "PECKBOARD_ACME_DIRECTORY";
/// Optional PEM root to trust for the ACME directory's TLS (Pebble's
/// self-signed listener). Only read alongside [`DIRECTORY_ENV`].
pub const DIRECTORY_ROOT_ENV: &str = "PECKBOARD_ACME_DIRECTORY_ROOT";

/// Public DoH resolvers polled until a challenge TXT value is visible.
const DOH_RESOLVERS: &[&str] = &[
    "https://cloudflare-dns.com/dns-query",
    "https://dns.google/resolve",
];
const PROPAGATION_TIMEOUT: Duration = Duration::from_secs(180);
const PROPAGATION_POLL: Duration = Duration::from_secs(5);
/// Renewal sweep cadence.
const RENEW_EVERY: Duration = Duration::from_secs(12 * 3600);
/// Renew certs expiring within this window.
const RENEW_BEFORE_DAYS: i64 = 30;
/// Retry failed (or abandoned pending) certs after this long.
const RETRY_ERROR_AFTER_HOURS: i64 = 6;
const MAX_DOMAINS: usize = 100;

pub fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .is_some_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
}

/// Process-wide ACME bookkeeping, owned by `TlsState` so every `AppState`
/// constructor gets one for free.
#[derive(Default)]
pub struct AcmeRuntime {
    in_flight: Arc<Mutex<HashSet<String>>>,
    force_test_mode: AtomicBool,
}

impl AcmeRuntime {
    /// Force test mode on regardless of the env (unit tests).
    pub fn set_test_mode(&self, on: bool) {
        self.force_test_mode.store(on, Ordering::SeqCst);
    }

    pub fn test_mode(&self) -> bool {
        self.force_test_mode.load(Ordering::SeqCst) || env_flag(TEST_ENV)
    }

    pub fn is_in_flight(&self, id: &str) -> bool {
        self.in_flight.lock().unwrap().contains(id)
    }

    /// Claim the per-cert issuance slot. `None` when one is already
    /// running for `id`.
    fn claim(&self, id: &str) -> Option<IssueGuard> {
        let mut set = self.in_flight.lock().unwrap();
        if !set.insert(id.to_string()) {
            return None;
        }
        Some(IssueGuard {
            id: id.to_string(),
            set: self.in_flight.clone(),
        })
    }
}

// Proof token: bearer holds the in-flight slot for `id`, so no second
// issuance for the same cert can run. Only `AcmeRuntime::claim` builds one;
// see `spawn_issue`.
struct IssueGuard {
    id: String,
    set: Arc<Mutex<HashSet<String>>>,
}

impl Drop for IssueGuard {
    fn drop(&mut self) {
        self.set.lock().unwrap().remove(&self.id);
    }
}

// ── Domains ──────────────────────────────────────

/// Lowercase, trim, strip a trailing dot, validate, and dedupe (order
/// kept). `*.` is allowed only as the whole first label, over at least two
/// further labels. Errors name the offending entry.
pub fn normalize_domains(input: &[String]) -> std::result::Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for raw in input {
        let d = raw.trim().trim_end_matches('.').to_ascii_lowercase();
        if d.is_empty() {
            continue;
        }
        validate_domain(&d)?;
        if !out.contains(&d) {
            out.push(d);
        }
    }
    if out.is_empty() {
        return Err("at least one domain is required".into());
    }
    if out.len() > MAX_DOMAINS {
        return Err(format!("at most {MAX_DOMAINS} domains per certificate"));
    }
    Ok(out)
}

fn validate_domain(d: &str) -> std::result::Result<(), String> {
    let bad = |why: &str| Err(format!("invalid domain '{d}': {why}"));
    if d.len() > 253 {
        return bad("longer than 253 characters");
    }
    let (wildcard, rest) = match d.strip_prefix("*.") {
        Some(rest) => (true, rest),
        None => (false, d),
    };
    let labels: Vec<&str> = rest.split('.').collect();
    if labels.len() < 2 {
        return bad(if wildcard {
            "a wildcard needs at least two labels after '*.'"
        } else {
            "needs at least two labels"
        });
    }
    for label in &labels {
        if label.is_empty() || label.len() > 63 {
            return bad("each label must be 1-63 characters");
        }
        if *label == "*" || label.contains('*') {
            return bad("'*' is only allowed as the whole first label");
        }
        if !label
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return bad("only letters, digits and '-' are allowed");
        }
        if label.starts_with('-') || label.ends_with('-') {
            return bad("labels cannot start or end with '-'");
        }
    }
    Ok(())
}

// ── Paths / files ────────────────────────────────

fn managed_dir(data_dir: &Path, id: &str) -> PathBuf {
    data_dir.join("certs").join("managed").join(id)
}

fn managed_material(data_dir: &Path, id: &str) -> TlsMaterial {
    let dir = managed_dir(data_dir, id);
    TlsMaterial {
        cert_path: dir.join("cert.pem"),
        key_path: dir.join("key.pem"),
        source: TlsSource::Uploaded,
    }
}

/// Stage + rename both halves into `<data>/certs/managed/<id>/`, 0600.
fn write_managed(data_dir: &Path, id: &str, cert_pem: &str, key_pem: &str) -> Result<()> {
    tls::validate_pem_pair(cert_pem.as_bytes(), key_pem.as_bytes())?;
    let dir = managed_dir(data_dir, id);
    std::fs::create_dir_all(&dir).context("failed to create managed cert directory")?;
    let cert_tmp = dir.join("cert.pem.tmp");
    let key_tmp = dir.join("key.pem.tmp");
    let staged = tls::write_secret_pem(&cert_tmp, cert_pem)
        .and_then(|()| tls::write_secret_pem(&key_tmp, key_pem))
        .context("failed to stage certificate files");
    if let Err(e) = staged {
        let _ = std::fs::remove_file(&cert_tmp);
        let _ = std::fs::remove_file(&key_tmp);
        return Err(e);
    }
    let m = managed_material(data_dir, id);
    std::fs::rename(&key_tmp, &m.key_path).context("failed to write key.pem")?;
    std::fs::rename(&cert_tmp, &m.cert_path).context("failed to write cert.pem")?;
    Ok(())
}

pub fn remove_managed_files(data_dir: &Path, id: &str) -> Result<()> {
    match std::fs::remove_dir_all(managed_dir(data_dir, id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("failed to remove certificate files"),
    }
}

/// DNS SANs (lowercase) and `notAfter` of the leaf in `cert_pem`.
pub fn parse_leaf(cert_pem: &[u8]) -> Result<(Vec<String>, chrono::DateTime<chrono::Utc>)> {
    let der = rustls_pemfile::certs(&mut std::io::BufReader::new(cert_pem))
        .next()
        .context("no certificate found in the certificate PEM")?
        .context("failed to parse certificate PEM")?;
    let (_, cert) = x509_parser::parse_x509_certificate(der.as_ref())
        .map_err(|e| anyhow::anyhow!("failed to parse certificate: {e}"))?;
    let mut sans = Vec::new();
    if let Ok(Some(ext)) = cert.subject_alternative_name() {
        for name in &ext.value.general_names {
            if let x509_parser::extensions::GeneralName::DNSName(n) = name {
                let n = n.trim_end_matches('.').to_ascii_lowercase();
                if !sans.contains(&n) {
                    sans.push(n);
                }
            }
        }
    }
    let not_after = chrono::DateTime::from_timestamp(cert.validity().not_after.timestamp(), 0)
        .context("certificate notAfter out of range")?;
    Ok((sans, not_after))
}

// ── Cert store ───────────────────────────────────

/// Re-read every managed cert whose files are on disk into the resolver.
/// A cert whose renewal failed keeps serving its still-valid files.
pub async fn reload_managed(db: &Db, data_dir: &Path, tls_state: &TlsState) -> Result<()> {
    let rows = db.list_tls_certificates().await?;
    let mut certs = Vec::new();
    for row in rows {
        let m = managed_material(data_dir, &row.id);
        if !m.cert_path.exists() || !m.key_path.exists() {
            continue;
        }
        match tls::certified_key(&m) {
            Ok(key) => certs.push(ManagedCert {
                sans: row_domains(&row),
                id: row.id,
                key,
            }),
            Err(e) => tracing::warn!(cert_id = %row.id, "Skipping managed TLS cert: {e:#}"),
        }
    }
    tls_state.set_managed(certs);
    Ok(())
}

pub fn row_domains(row: &TlsCertificateRow) -> Vec<String> {
    serde_json::from_str(&row.domains).unwrap_or_default()
}

/// After any managed-cert change: reload the store and turn HTTPS on if the
/// listener is bound and something can now be served.
async fn reload_and_refresh(state: &AppState) {
    if let Err(e) = reload_managed(&state.db, &state.config.data_dir, &state.tls).await {
        tracing::warn!("Failed to reload managed TLS certs: {e:#}");
    }
    let bound = state.tls.snapshot().listener_bound;
    state
        .tls
        .set_https_enabled(bound && state.tls.has_any_cert());
}

// ── API views ────────────────────────────────────

pub fn certificate_json(row: &TlsCertificateRow) -> serde_json::Value {
    serde_json::json!({
        "id": row.id,
        "source": row.source,
        "domains": row_domains(row),
        "dns_credential_id": row.dns_credential_id,
        "acme_email": row.acme_email,
        "staging": row.staging,
        "status": row.status,
        "error": row.last_error,
        "not_after": row.not_after,
        "created_at": row.created_at,
        "updated_at": row.updated_at,
    })
}

/// `"••••" + last 4` for values of 8+ chars, else `"••••"`.
pub fn mask_secret(v: &str) -> String {
    let chars: Vec<char> = v.chars().collect();
    if chars.len() < 8 {
        "••••".to_string()
    } else {
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("••••{tail}")
    }
}

/// Credential view with secret fields masked. Fields the provider spec
/// doesn't know are treated as secret.
pub fn credential_json(vault_key: &[u8], row: &DnsCredentialRow) -> serde_json::Value {
    let creds = open_credentials(vault_key, &row.credentials).unwrap_or_default();
    let spec = dns::catalog().into_iter().find(|p| p.id == row.provider);
    let fields: serde_json::Map<String, serde_json::Value> = creds
        .iter()
        .map(|(k, v)| {
            let secret = spec
                .as_ref()
                .and_then(|s| s.fields.iter().find(|f| f.key == k))
                .is_none_or(|f| f.secret);
            let shown = if secret { mask_secret(v) } else { v.clone() };
            (k.clone(), serde_json::Value::String(shown))
        })
        .collect();
    serde_json::json!({
        "id": row.id,
        "provider": row.provider,
        "label": row.label,
        "fields": fields,
        "created_at": row.created_at,
    })
}

// ── Validation for the routes ────────────────────

/// Check `fields` against provider `id`'s spec: unknown provider or a
/// missing required field is an error. Returns the trimmed, spec-keyed
/// map to store.
pub fn validate_credentials(
    id: &str,
    fields: &dns::Credentials,
) -> std::result::Result<dns::Credentials, String> {
    let spec = dns::catalog()
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| format!("unknown DNS provider '{id}'"))?;
    let mut out = dns::Credentials::new();
    for f in spec.fields {
        let v = fields.get(f.key).map(|v| v.trim()).unwrap_or("");
        if v.is_empty() {
            if f.required {
                return Err(format!("{} is required", f.label));
            }
            continue;
        }
        out.insert(f.key.to_string(), v.to_string());
    }
    dns::build(id, &out, http_client()).map_err(|e| format!("{e:#}"))?;
    Ok(out)
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default()
}

fn build_provider(vault_key: &[u8], cred: &DnsCredentialRow) -> Result<Box<dyn dns::DnsProvider>> {
    let creds = open_credentials(vault_key, &cred.credentials)?;
    dns::build(&cred.provider, &creds, http_client())
}

/// Prefix of a sealed `dns_credentials.credentials` value.
const SEALED_PREFIX: &str = "v1:";

/// Encrypt credentials for storage under the instance vault key (the one
/// SSH keys use): `v1:<nonce_hex>:<ciphertext_b64>`.
pub fn seal_credentials(vault_key: &[u8], creds: &dns::Credentials) -> Result<String> {
    let json = serde_json::to_string(creds)?;
    let (ct, nonce) = crate::service::ssh_keys::encrypt(vault_key, &json)?;
    Ok(format!("{SEALED_PREFIX}{nonce}:{ct}"))
}

/// Inverse of [`seal_credentials`].
pub fn open_credentials(vault_key: &[u8], stored: &str) -> Result<dns::Credentials> {
    let (nonce, ct) = stored
        .strip_prefix(SEALED_PREFIX)
        .and_then(|rest| rest.split_once(':'))
        .context("stored DNS credentials are corrupt")?;
    let json = crate::service::ssh_keys::decrypt(vault_key, nonce, ct)
        .context("stored DNS credentials could not be decrypted")?;
    serde_json::from_str(&json).context("stored DNS credentials are corrupt")
}

/// `POST …/dns-credentials/{id}/test`: create and delete
/// `_acme-challenge.<domain>` TXT `peckboard-test`.
pub async fn test_credential(
    vault_key: &[u8],
    cred: &DnsCredentialRow,
    domain: &str,
) -> Result<()> {
    let provider = build_provider(vault_key, cred)?;
    let fqdn = format!("_acme-challenge.{domain}");
    let rec = provider
        .create_txt(&fqdn, "peckboard-test")
        .await
        .context("creating the test TXT record failed")?;
    provider
        .delete_txt(&rec)
        .await
        .context("the test TXT record was created but deleting it failed")?;
    Ok(())
}

/// Install an operator-uploaded pair as a managed cert: domains = the
/// leaf's DNS SANs, status `issued`.
pub async fn install_uploaded(
    state: &AppState,
    cert_pem: &str,
    key_pem: &str,
) -> std::result::Result<TlsCertificateRow, String> {
    tls::validate_pem_pair(cert_pem.as_bytes(), key_pem.as_bytes())
        .map_err(|e| format!("{e:#}"))?;
    let (sans, not_after) = parse_leaf(cert_pem.as_bytes()).map_err(|e| format!("{e:#}"))?;
    if sans.is_empty() {
        return Err("certificate has no DNS names (subjectAltName)".into());
    }
    let id = uuid::Uuid::new_v4().to_string();
    write_managed(&state.config.data_dir, &id, cert_pem, key_pem).map_err(|e| format!("{e:#}"))?;
    let now = chrono::Utc::now().to_rfc3339();
    let row = TlsCertificateRow {
        id: id.clone(),
        source: "uploaded".into(),
        domains: serde_json::to_string(&sans).unwrap(),
        dns_credential_id: None,
        acme_email: None,
        staging: false,
        status: "issued".into(),
        last_error: None,
        not_after: Some(not_after.to_rfc3339()),
        created_at: now.clone(),
        updated_at: now,
    };
    if let Err(e) = state.db.insert_tls_certificate(row.clone()).await {
        let _ = remove_managed_files(&state.config.data_dir, &id);
        return Err(format!("{e:#}"));
    }
    reload_and_refresh(state).await;
    Ok(row)
}

/// Delete a cert row and its files, and drop it from the resolver.
pub async fn delete_certificate(state: &AppState, id: &str) -> Result<bool> {
    let existed = state.db.delete_tls_certificate(id).await?;
    remove_managed_files(&state.config.data_dir, id)?;
    reload_and_refresh(state).await;
    Ok(existed)
}

// ── Issuance ─────────────────────────────────────

/// Mark `id` pending and run issuance in the background. `false` when an
/// issuance for this cert is already running (nothing new is started).
pub async fn spawn_issue(state: Arc<AppState>, id: String) -> Result<bool> {
    let Some(guard) = state.tls.acme.claim(&id) else {
        return Ok(false);
    };
    state
        .db
        .update_tls_certificate_status(&id, "pending", None, None)
        .await?;
    tokio::spawn(async move {
        issue_now(&state, &guard).await;
    });
    Ok(true)
}

/// Run one issuance to completion and record the outcome.
async fn issue_now(state: &Arc<AppState>, guard: &IssueGuard) {
    let id = guard.id.as_str();
    let outcome = issue_inner(state, id).await;
    let recorded = match &outcome {
        Ok(not_after) => {
            tracing::info!(cert_id = %id, "ACME certificate issued");
            state
                .db
                .update_tls_certificate_status(id, "issued", None, Some(not_after.clone()))
                .await
        }
        Err(e) => {
            tracing::warn!(cert_id = %id, "ACME issuance failed: {e:#}");
            state
                .db
                .update_tls_certificate_status(id, "error", Some(format!("{e:#}")), None)
                .await
        }
    };
    if let Err(e) = recorded {
        tracing::warn!(cert_id = %id, "Failed to record ACME outcome: {e:#}");
    }
    reload_and_refresh(state).await;
}

/// Issue (or reissue) cert `id`; returns the new `not_after` (RFC3339).
/// Every TXT record published is deleted again whatever the outcome.
async fn issue_inner(state: &Arc<AppState>, id: &str) -> Result<String> {
    let row = state
        .db
        .get_tls_certificate(id)
        .await?
        .context("certificate no longer exists")?;
    anyhow::ensure!(row.source == "acme", "only ACME certificates can be issued");
    let domains = row_domains(&row);
    anyhow::ensure!(!domains.is_empty(), "certificate has no domains");
    let cred_id = row
        .dns_credential_id
        .as_deref()
        .context("certificate has no DNS credential")?;
    let cred = state
        .db
        .get_dns_credential(cred_id)
        .await?
        .context("the certificate's DNS credential was deleted")?;
    let provider = build_provider(&state.ssh_vault_key, &cred)?;

    let mut records: Vec<dns::TxtRecord> = Vec::new();
    let result = if state.tls.acme.test_mode() {
        issue_test_mode(provider.as_ref(), &domains, &mut records).await
    } else {
        issue_acme(
            provider.as_ref(),
            &row,
            &domains,
            &state.config.data_dir,
            &mut records,
        )
        .await
    };
    for rec in &records {
        if let Err(e) = provider.delete_txt(rec).await {
            tracing::warn!(cert_id = %id, fqdn = %rec.fqdn, "Failed to delete ACME TXT record: {e:#}");
        }
    }
    let (cert_pem, key_pem) = result?;

    // Deleted while we were issuing: discard instead of resurrecting files.
    if state.db.get_tls_certificate(id).await?.is_none() {
        anyhow::bail!("certificate was deleted during issuance");
    }
    let (_, not_after) = parse_leaf(cert_pem.as_bytes())?;
    write_managed(&state.config.data_dir, id, &cert_pem, &key_pem)?;
    Ok(not_after.to_rfc3339())
}

fn challenge_fqdn(domain: &str) -> String {
    format!(
        "_acme-challenge.{}",
        domain.strip_prefix("*.").unwrap_or(domain)
    )
}

async fn issue_test_mode(
    provider: &dyn dns::DnsProvider,
    domains: &[String],
    records: &mut Vec<dns::TxtRecord>,
) -> Result<(String, String)> {
    for d in domains {
        let value = uuid::Uuid::new_v4().simple().to_string();
        records.push(provider.create_txt(&challenge_fqdn(d), &value).await?);
    }
    if let Some(d) = domains.iter().find(|d| d.contains("fail")) {
        anyhow::bail!("test-mode failure for {d}");
    }
    let key = rcgen::KeyPair::generate()?;
    let mut params = rcgen::CertificateParams::new(domains.to_vec())?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::minutes(5);
    params.not_after = now + time::Duration::days(90);
    let cert = params.self_signed(&key)?;
    Ok((cert.pem(), key.serialize_pem()))
}

fn directory_url(staging: bool) -> (String, &'static str) {
    if let Ok(url) = std::env::var(DIRECTORY_ENV)
        && !url.trim().is_empty()
    {
        return (url.trim().to_string(), "custom");
    }
    if staging {
        (
            instant_acme::LetsEncrypt::Staging.url().to_string(),
            "staging",
        )
    } else {
        (
            instant_acme::LetsEncrypt::Production.url().to_string(),
            "prod",
        )
    }
}

fn account_builder() -> Result<instant_acme::AccountBuilder> {
    if env_flag(DIRECTORY_ENV)
        && let Ok(root) = std::env::var(DIRECTORY_ROOT_ENV)
        && !root.trim().is_empty()
    {
        return Ok(instant_acme::Account::builder_with_root(root.trim())?);
    }
    Ok(instant_acme::Account::builder()?)
}

/// Load the cached ACME account for (directory, email) or register one.
/// Cached at `<data>/certs/acme/account-{prod|staging}-<email-hash>.json`.
async fn load_or_create_account(
    data_dir: &Path,
    email: Option<&str>,
    staging: bool,
) -> Result<instant_acme::Account> {
    let (url, kind) = directory_url(staging);
    let email = email.unwrap_or("").trim().to_ascii_lowercase();
    let mut hasher_input = email.clone();
    if kind == "custom" {
        hasher_input.push('|');
        hasher_input.push_str(&url);
    }
    let hash = hex::encode(sha2::Sha256::digest(hasher_input.as_bytes()));
    let dir = data_dir.join("certs").join("acme");
    let path = dir.join(format!("account-{kind}-{}.json", &hash[..16]));

    if let Ok(raw) = std::fs::read_to_string(&path) {
        match serde_json::from_str::<instant_acme::AccountCredentials>(&raw) {
            Ok(creds) => return Ok(account_builder()?.from_credentials(creds).await?),
            Err(e) => tracing::warn!("Ignoring unreadable ACME account cache: {e}"),
        }
    }

    let contact = format!("mailto:{email}");
    let contacts: Vec<&str> = if email.is_empty() {
        Vec::new()
    } else {
        vec![contact.as_str()]
    };
    let (account, creds) = account_builder()?
        .create(
            &instant_acme::NewAccount {
                contact: &contacts,
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            url,
            None,
        )
        .await
        .context("ACME account registration failed")?;
    std::fs::create_dir_all(&dir).context("failed to create ACME account directory")?;
    tls::write_secret_pem(&path, &serde_json::to_string(&creds)?)
        .context("failed to cache ACME account")?;
    Ok(account)
}

async fn issue_acme(
    provider: &dyn dns::DnsProvider,
    row: &TlsCertificateRow,
    domains: &[String],
    data_dir: &Path,
    records: &mut Vec<dns::TxtRecord>,
) -> Result<(String, String)> {
    use instant_acme::{
        AuthorizationStatus, ChallengeType, Identifier, NewOrder, OrderStatus, RetryPolicy,
    };

    let account = load_or_create_account(data_dir, row.acme_email.as_deref(), row.staging).await?;
    let identifiers: Vec<Identifier> = domains.iter().map(|d| Identifier::Dns(d.clone())).collect();
    let mut order = account
        .new_order(&NewOrder::new(&identifiers))
        .await
        .context("ACME new order failed")?;

    // Pass 1: collect the TXT value each pending authorization wants.
    let mut wanted: Vec<(String, String)> = Vec::new();
    {
        let mut authzs = order.authorizations();
        while let Some(authz) = authzs.next().await {
            let mut authz = authz?;
            match authz.status {
                AuthorizationStatus::Pending => {}
                AuthorizationStatus::Valid => continue,
                other => anyhow::bail!("authorization for {} is {other:?}", authz.identifier()),
            }
            let domain = match authz.identifier().identifier {
                Identifier::Dns(d) => d.clone(),
                other => anyhow::bail!("unsupported identifier {other:?}"),
            };
            let challenge = authz
                .challenge(ChallengeType::Dns01)
                .with_context(|| format!("no dns-01 challenge offered for {domain}"))?;
            wanted.push((
                format!("_acme-challenge.{domain}"),
                challenge.key_authorization().dns_value(),
            ));
        }
    }

    for (fqdn, value) in &wanted {
        records.push(
            provider
                .create_txt(fqdn, value)
                .await
                .with_context(|| format!("creating TXT record {fqdn} failed"))?,
        );
    }
    let http = http_client();
    for (fqdn, value) in &wanted {
        wait_for_txt(&http, fqdn, value).await?;
    }

    // Pass 2: tell the CA each challenge is ready.
    {
        let mut authzs = order.authorizations();
        while let Some(authz) = authzs.next().await {
            let mut authz = authz?;
            if !matches!(authz.status, AuthorizationStatus::Pending) {
                continue;
            }
            let mut challenge = authz
                .challenge(ChallengeType::Dns01)
                .context("dns-01 challenge disappeared")?;
            challenge.set_ready().await?;
        }
    }

    let retry = RetryPolicy::new()
        .initial_delay(Duration::from_secs(2))
        .timeout(Duration::from_secs(300));
    let status = order.poll_ready(&retry).await?;
    if status != OrderStatus::Ready {
        let detail = order
            .state()
            .error
            .as_ref()
            .map(|p| format!(": {p:?}"))
            .unwrap_or_default();
        anyhow::bail!("ACME order is {status:?}{detail}");
    }

    let key = rcgen::KeyPair::generate()?;
    let mut params = rcgen::CertificateParams::new(domains.to_vec())?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    let csr = params.serialize_request(&key)?;
    order.finalize_csr(csr.der()).await?;
    let chain = order.poll_certificate(&retry).await?;
    Ok((chain, key.serialize_pem()))
}

/// Poll public DoH resolvers until `value` shows up at `fqdn`.
async fn wait_for_txt(http: &reqwest::Client, fqdn: &str, value: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + PROPAGATION_TIMEOUT;
    loop {
        for base in DOH_RESOLVERS {
            if txt_visible(http, base, fqdn, value).await {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "TXT record {fqdn} was not visible in public DNS after {}s",
                PROPAGATION_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(PROPAGATION_POLL).await;
    }
}

async fn txt_visible(http: &reqwest::Client, base: &str, fqdn: &str, value: &str) -> bool {
    let url = format!("{base}?name={}&type=TXT", urlencoding::encode(fqdn));
    let Ok(resp) = http
        .get(url)
        .header("accept", "application/dns-json")
        .send()
        .await
    else {
        return false;
    };
    let Ok(body) = resp.json::<serde_json::Value>().await else {
        return false;
    };
    body.get("Answer")
        .and_then(|a| a.as_array())
        .is_some_and(|answers| {
            answers.iter().any(|a| {
                a.get("data")
                    .and_then(|d| d.as_str())
                    .is_some_and(|d| d.replace("\" \"", "").trim_matches('"') == value)
            })
        })
}

// ── Renewal ──────────────────────────────────────

/// Whether `row` is due for (re)issuance at `now`.
fn due_for_renewal(row: &TlsCertificateRow, now: chrono::DateTime<chrono::Utc>) -> bool {
    if row.source != "acme" {
        return false;
    }
    let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok();
    let stale = || {
        parse(&row.updated_at)
            .is_none_or(|t| now - t.to_utc() > chrono::Duration::hours(RETRY_ERROR_AFTER_HOURS))
    };
    match row.status.as_str() {
        "error" | "pending" => stale(),
        _ => row
            .not_after
            .as_deref()
            .and_then(parse)
            .is_none_or(|t| t.to_utc() - now < chrono::Duration::days(RENEW_BEFORE_DAYS)),
    }
}

/// One renewal sweep: start issuance for every due cert.
pub async fn renew_due(state: &Arc<AppState>) -> Result<usize> {
    let now = chrono::Utc::now();
    let mut started = 0;
    for row in state.db.list_tls_certificates().await? {
        if due_for_renewal(&row, now) && spawn_issue(state.clone(), row.id.clone()).await? {
            tracing::info!(cert_id = %row.id, "Renewing ACME certificate");
            started += 1;
        }
    }
    Ok(started)
}

/// Background renewal loop: a first sweep shortly after boot, then every
/// 12h. Not started under `PECKBOARD_NO_RESUME` (see `server::run_server`).
pub fn spawn_renewal_task(state: Arc<AppState>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            if let Err(e) = renew_due(&state).await {
                tracing::warn!("ACME renewal sweep failed: {e:#}");
            }
            tokio::time::sleep(RENEW_EVERY).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn normalize_domains_lowercases_trims_and_dedupes() {
        let out =
            normalize_domains(&s(&[" Example.COM. ", "*.example.com", "example.com", ""])).unwrap();
        assert_eq!(out, s(&["example.com", "*.example.com"]));
    }

    #[test]
    fn normalize_domains_rejects_bad_input() {
        assert!(normalize_domains(&[]).is_err());
        assert!(normalize_domains(&s(&["  "])).is_err());
        assert!(normalize_domains(&s(&["a.*.example.com"])).is_err());
        assert!(normalize_domains(&s(&["*.com"])).is_err());
        assert!(normalize_domains(&s(&["localhost"])).is_err());
        assert!(normalize_domains(&s(&["-bad.example.com"])).is_err());
        assert!(normalize_domains(&s(&["under_score.example.com"])).is_err());
    }

    #[test]
    fn mask_secret_keeps_last_four_of_long_values() {
        assert_eq!(mask_secret("abcdefgh1234"), "••••1234");
        assert_eq!(mask_secret("short"), "••••");
    }
    #[test]
    fn credentials_are_sealed_at_rest() {
        let key = [7u8; 32];
        let mut creds = dns::Credentials::new();
        creds.insert("api_token".into(), "supersecrettoken".into());
        let sealed = seal_credentials(&key, &creds).unwrap();
        assert!(!sealed.contains("supersecrettoken"));
        assert_eq!(open_credentials(&key, &sealed).unwrap(), creds);
        assert!(open_credentials(&[8u8; 32], &sealed).is_err());
    }

    #[test]
    fn renewal_picks_expiring_and_stale_failed_certs() {
        let now = chrono::Utc::now();
        let base = TlsCertificateRow {
            id: "c".into(),
            source: "acme".into(),
            domains: "[\"a.example.com\"]".into(),
            dns_credential_id: Some("d".into()),
            acme_email: None,
            staging: false,
            status: "issued".into(),
            last_error: None,
            not_after: Some((now + chrono::Duration::days(60)).to_rfc3339()),
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
        };
        assert!(!due_for_renewal(&base, now));
        let expiring = TlsCertificateRow {
            not_after: Some((now + chrono::Duration::days(10)).to_rfc3339()),
            ..base.clone()
        };
        assert!(due_for_renewal(&expiring, now));
        let fresh_error = TlsCertificateRow {
            status: "error".into(),
            ..base.clone()
        };
        assert!(!due_for_renewal(&fresh_error, now));
        let old_error = TlsCertificateRow {
            status: "error".into(),
            updated_at: (now - chrono::Duration::hours(7)).to_rfc3339(),
            ..base.clone()
        };
        assert!(due_for_renewal(&old_error, now));
        let uploaded = TlsCertificateRow {
            source: "uploaded".into(),
            ..expiring
        };
        assert!(!due_for_renewal(&uploaded, now));
    }
}
