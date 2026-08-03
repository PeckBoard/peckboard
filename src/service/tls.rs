// TLS cert management (rcgen self-signed + operator-uploaded)

use anyhow::{Context, Result};
use rcgen::{CertificateParams, DnType, KeyPair, SanType};
use rustls::sign::CertifiedKey;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::BufReader;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;

use crate::db::Db;
use crate::db::models::NewAnnouncement;

/// Which key material the HTTPS listener is actually serving.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsSource {
    SelfSigned,
    Uploaded,
}

/// The cert/key pair to serve, and where it came from.
#[derive(Debug, Clone)]
pub struct TlsMaterial {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub source: TlsSource,
}

/// Display summary of the material in use, for the settings API.
#[derive(Debug, Clone, Serialize)]
pub struct TlsMaterialInfo {
    pub source: TlsSource,
    /// SAN entries. Exact for self-signed material (read back from the
    /// sidecar); empty for uploaded material, whose extensions we don't
    /// parse.
    pub sans: Vec<String>,
    pub not_after: Option<chrono::DateTime<chrono::Utc>>,
}

/// Records which SANs the current self-signed cert was built for. We
/// track this in a sidecar rather than reading the X.509 extensions back
/// out: the hand-rolled ASN.1 parser below only walks as far as
/// `notAfter`, and extending it to the extension list isn't worth it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SelfSignedSidecar {
    sans: Vec<String>,
    generated_at: String,
}

/// One subject-alternative-name entry, in a form we can sort and compare.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SanSpec {
    Dns(String),
    Ip(IpAddr),
}

impl SanSpec {
    fn display(&self) -> String {
        match self {
            SanSpec::Dns(name) => name.clone(),
            SanSpec::Ip(ip) => ip.to_string(),
        }
    }
}

fn certs_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("certs")
}

fn sidecar_path(data_dir: &Path) -> PathBuf {
    certs_dir(data_dir).join("self-signed.json")
}

fn uploaded_cert_path(data_dir: &Path) -> PathBuf {
    certs_dir(data_dir).join("uploaded-cert.pem")
}

fn uploaded_key_path(data_dir: &Path) -> PathBuf {
    certs_dir(data_dir).join("uploaded-key.pem")
}

/// Is this address worth putting in a certificate? Loopback is added
/// unconditionally elsewhere; IPv6 link-local (`fe80::/10`) is scoped to a
/// single interface and never usable by a client.
fn is_usable_san_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_unspecified() && !v4.is_multicast(),
        IpAddr::V6(v6) => {
            let link_local = v6.segments()[0] & 0xffc0 == 0xfe80;
            !v6.is_loopback() && !v6.is_unspecified() && !v6.is_multicast() && !link_local
        }
    }
}

#[cfg(unix)]
fn system_hostname() -> Option<String> {
    let mut buf = vec![0u8; 256];
    // SAFETY: `buf` is a live allocation of `buf.len()` bytes, which is
    // exactly the capacity we hand to gethostname.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let host = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    // Non-ASCII names can't go in an IA5String SAN, and "localhost" is
    // already covered.
    if host.is_empty() || host == "localhost" || !host.is_ascii() {
        None
    } else {
        Some(host)
    }
}

#[cfg(not(unix))]
fn system_hostname() -> Option<String> {
    None
}

/// Every name and address this host can plausibly be reached at, sorted
/// and de-duplicated so the result is deterministic across calls.
fn san_specs() -> Vec<SanSpec> {
    let mut specs = vec![
        SanSpec::Dns("localhost".to_string()),
        SanSpec::Ip(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        SanSpec::Ip(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)),
    ];

    match if_addrs::get_if_addrs() {
        Ok(ifaces) => specs.extend(
            ifaces
                .into_iter()
                .map(|iface| iface.addr.ip())
                .filter(is_usable_san_ip)
                .map(SanSpec::Ip),
        ),
        Err(e) => tracing::warn!("Failed to enumerate interface addresses for TLS SANs: {e}"),
    }

    if let Some(host) = system_hostname() {
        let base = host.strip_suffix(".local").unwrap_or(&host).to_string();
        specs.push(SanSpec::Dns(format!("{base}.local")));
        specs.push(SanSpec::Dns(base));
    }

    specs.sort_by(|a, b| a.display().cmp(&b.display()));
    specs.dedup_by(|a, b| a.display() == b.display());
    specs
}

/// SAN entries for a freshly generated self-signed certificate.
pub fn san_targets() -> Vec<SanType> {
    san_specs()
        .into_iter()
        .filter_map(|spec| match spec {
            SanSpec::Dns(name) => name.as_str().try_into().ok().map(SanType::DnsName),
            SanSpec::Ip(ip) => Some(SanType::IpAddress(ip)),
        })
        .collect()
}

/// The same SAN list as display strings — what the sidecar stores and
/// what the settings API shows.
pub fn san_strings() -> Vec<String> {
    san_specs().iter().map(SanSpec::display).collect()
}

fn read_sidecar(path: &Path) -> Option<SelfSignedSidecar> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Reject anything `rustls` would reject at handshake time: a malformed
/// PEM, a missing private key, or a key that doesn't match the leaf —
/// `with_single_cert` compares the two. Built against an explicit
/// provider rather than the process default so validation never depends
/// on `main` having installed one.
///
/// Every path that puts material on disk or picks material up off disk
/// goes through here, so a mismatched pair is never installed and never
/// served: a torn write degrades to the self-signed fallback instead of
/// a certificate whose key can't sign for it.
pub fn validate_pem_pair(cert_pem: &[u8], key_pem: &[u8]) -> Result<()> {
    let certs: Vec<_> = rustls_pemfile::certs(&mut BufReader::new(cert_pem))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to parse certificate PEM")?;
    if certs.is_empty() {
        anyhow::bail!("no certificate found in the certificate PEM");
    }
    let key = rustls_pemfile::private_key(&mut BufReader::new(key_pem))
        .context("failed to parse private key PEM")?
        .context("no private key found in the key PEM")?;

    rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("failed to build a TLS config")?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .context("certificate and private key do not work together")?;

    Ok(())
}

/// Whether the pair on disk at these two paths is readable and usable.
fn pem_pair_on_disk_ok(cert_path: &Path, key_path: &Path) -> Result<()> {
    let cert_pem =
        fs::read(cert_path).with_context(|| format!("failed to read {}", cert_path.display()))?;
    let key_pem =
        fs::read(key_path).with_context(|| format!("failed to read {}", key_path.display()))?;
    validate_pem_pair(&cert_pem, &key_pem)
}

/// Operator-uploaded material, but only when both halves are present and
/// parse. Anything else falls through to the self-signed path.
fn uploaded_material(data_dir: &Path) -> Option<TlsMaterial> {
    let cert_path = uploaded_cert_path(data_dir);
    let key_path = uploaded_key_path(data_dir);
    if !cert_path.exists() || !key_path.exists() {
        return None;
    }

    if let Err(e) = pem_pair_on_disk_ok(&cert_path, &key_path) {
        tracing::warn!("Ignoring uploaded TLS material: {e:#}");
        return None;
    }

    Some(TlsMaterial {
        cert_path,
        key_path,
        source: TlsSource::Uploaded,
    })
}

/// Install operator-supplied material. Once written it takes precedence
/// over the self-signed cert on every subsequent `ensure_certs`.
pub fn install_uploaded(data_dir: &Path, cert_pem: &str, key_pem: &str) -> Result<TlsMaterial> {
    validate_pem_pair(cert_pem.as_bytes(), key_pem.as_bytes())?;

    let dir = certs_dir(data_dir);
    fs::create_dir_all(&dir).context("failed to create certs directory")?;

    let cert_path = uploaded_cert_path(data_dir);
    let key_path = uploaded_key_path(data_dir);

    // Stage both halves beside their targets and rename them into place,
    // so a failure part-way through can't leave a new certificate next to
    // the previous key. The key is staged at 0o600 like the final file,
    // and `rename` preserves the mode, so the secret is never readable by
    // anyone else even for an instant.
    let cert_tmp = dir.join("uploaded-cert.pem.tmp");
    let key_tmp = dir.join("uploaded-key.pem.tmp");
    let staged = fs::write(&cert_tmp, cert_pem)
        .and_then(|()| write_secret_pem(&key_tmp, key_pem))
        .context("failed to stage uploaded TLS material");
    if let Err(e) = staged {
        let _ = fs::remove_file(&cert_tmp);
        let _ = fs::remove_file(&key_tmp);
        return Err(e);
    }
    fs::rename(&key_tmp, &key_path).context("failed to write uploaded-key.pem")?;
    fs::rename(&cert_tmp, &cert_path).context("failed to write uploaded-cert.pem")?;

    tracing::info!("Installed operator-uploaded TLS certificate at {:?}", dir);

    Ok(TlsMaterial {
        cert_path,
        key_path,
        source: TlsSource::Uploaded,
    })
}

/// Drop operator-supplied material, reverting to the self-signed cert.
/// Removing material that isn't there is not an error.
pub fn clear_uploaded(data_dir: &Path) -> Result<()> {
    for path in [uploaded_cert_path(data_dir), uploaded_key_path(data_dir)] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).context(format!("failed to remove {}", path.display()));
            }
        }
    }
    Ok(())
}

/// Resolve the TLS material to serve. Operator-uploaded material wins
/// whenever it is present and parses; otherwise a self-signed ECDSA P-256
/// cert covering every address this host answers on is generated, and
/// regenerated when it nears expiry or when the host's addresses change.
pub fn ensure_certs(data_dir: &Path) -> Result<TlsMaterial> {
    if let Some(uploaded) = uploaded_material(data_dir) {
        tracing::info!("Using operator-uploaded TLS certificate");
        return Ok(uploaded);
    }

    let dir = certs_dir(data_dir);
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    let sidecar = sidecar_path(data_dir);
    let sans = san_strings();

    if cert_path.exists() && key_path.exists() {
        // A sidecar we can't read means we can't prove the cert covers the
        // current addresses, so treat it as drift and regenerate.
        let sans_changed = read_sidecar(&sidecar).is_none_or(|recorded| recorded.sans != sans);
        // A pair that no longer validates — a truncated write, a key that
        // doesn't match the cert beside it — is unserveable, so mint a new
        // one rather than handing the resolver material every handshake
        // will fail on.
        let pair_err = pem_pair_on_disk_ok(&cert_path, &key_path).err();
        if pair_err.is_none() && !sans_changed && !needs_renewal(&cert_path) {
            return Ok(TlsMaterial {
                cert_path,
                key_path,
                source: TlsSource::SelfSigned,
            });
        }
        if let Some(e) = pair_err {
            tracing::warn!("Self-signed TLS material is unusable, regenerating: {e:#}");
        } else if sans_changed {
            tracing::info!("TLS certificate SANs are out of date, regenerating...");
        } else {
            tracing::info!("TLS certificate needs renewal, regenerating...");
        }
    }

    regenerate_self_signed(data_dir)
}

/// Generate a fresh self-signed cert, replacing whatever self-signed
/// material is already on disk. Unlike [`ensure_certs`] this never
/// short-circuits on a cert that is still valid, so the admin regenerate
/// route picks up addresses the host has gained since boot.
pub fn regenerate_self_signed(data_dir: &Path) -> Result<TlsMaterial> {
    let dir = certs_dir(data_dir);
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    let sidecar = sidecar_path(data_dir);
    let sans = san_strings();

    fs::create_dir_all(&dir).context("failed to create certs directory")?;

    // Generate ECDSA P-256 key pair
    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .context("failed to generate ECDSA key pair")?;

    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, "Peckboard Self-Signed");
    params
        .distinguished_name
        .push(DnType::OrganizationName, "Peckboard");
    params.subject_alt_names = san_targets();

    // 365-day lifetime; `needs_renewal` does the actual expiry checking.
    let now = chrono::Utc::now();
    let expire = now + chrono::Duration::days(365);
    params.not_before = time_from_chrono(now);
    params.not_after = time_from_chrono(expire);

    let cert = params
        .self_signed(&key_pair)
        .context("failed to generate self-signed certificate")?;

    // Write cert PEM (public — default umask is fine).
    fs::write(&cert_path, cert.pem()).context("failed to write cert.pem")?;

    // Write key PEM. On Unix we open with mode 0o600 atomically via
    // OpenOptions so the file is never world-readable, not even for
    // the few microseconds between `fs::write` and `set_permissions`.
    write_secret_pem(&key_path, &key_pair.serialize_pem()).context("failed to write key.pem")?;

    let doc = SelfSignedSidecar {
        sans: sans.clone(),
        generated_at: now.to_rfc3339(),
    };
    fs::write(
        &sidecar,
        serde_json::to_string_pretty(&doc).context("failed to serialize cert sidecar")?,
    )
    .context("failed to write self-signed.json")?;

    tracing::info!("Generated self-signed TLS certificate at {dir:?} for SANs {sans:?}");

    Ok(TlsMaterial {
        cert_path,
        key_path,
        source: TlsSource::SelfSigned,
    })
}

/// Atomic-mode write for a private-key PEM. On Unix the file is
/// created with mode 0o600 in one syscall via `OpenOptions::mode` so
/// the secret never lands on disk world-readable. On other platforms
/// this is just a regular write — Windows ACLs aren't covered here.
fn write_secret_pem(path: &Path, pem: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(pem.as_bytes())?;
        f.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, pem)
    }
}

fn time_from_chrono(dt: chrono::DateTime<chrono::Utc>) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(dt.timestamp()).unwrap()
}

/// The `notAfter` of the leaf certificate at `cert_path`, if it parses.
pub fn cert_not_after(cert_path: &Path) -> Option<chrono::DateTime<chrono::Utc>> {
    let pem_data = fs::read(cert_path).ok()?;
    let certs = rustls_pemfile::certs(&mut BufReader::new(&pem_data[..]))
        .collect::<std::result::Result<Vec<_>, _>>()
        .ok()?;
    let not_after = parse_not_after(certs.first()?.as_ref())?;
    chrono::DateTime::from_timestamp(not_after, 0)
}

/// Check if cert expires within 30 days. Unreadable or unparseable
/// material counts as needing renewal.
pub fn needs_renewal(cert_path: &Path) -> bool {
    match cert_not_after(cert_path) {
        Some(not_after) => not_after - chrono::Utc::now() < chrono::Duration::days(30),
        None => true,
    }
}

/// Summarise the material in use for the settings API: which source, the
/// SANs it covers, and when it expires.
pub fn material_info(data_dir: &Path, material: &TlsMaterial) -> TlsMaterialInfo {
    let sans = match material.source {
        // The sidecar is the record of what we generated; fall back to the
        // live list if it went missing.
        TlsSource::SelfSigned => read_sidecar(&sidecar_path(data_dir))
            .map(|sidecar| sidecar.sans)
            .unwrap_or_else(san_strings),
        // We don't parse SANs out of X.509 extensions.
        TlsSource::Uploaded => Vec::new(),
    };

    TlsMaterialInfo {
        source: material.source,
        sans,
        not_after: cert_not_after(&material.cert_path),
    }
}

/// Parse the notAfter timestamp from a DER-encoded X.509 certificate.
/// Returns the Unix timestamp of the notAfter field, or None if parsing fails.
fn parse_not_after(der: &[u8]) -> Option<i64> {
    // X.509 structure: SEQUENCE { tbsCertificate, signatureAlgorithm, signatureValue }
    // tbsCertificate: SEQUENCE { version, serialNumber, signature, issuer, validity, ... }
    // validity: SEQUENCE { notBefore, notAfter }
    // We do minimal ASN.1 parsing to extract notAfter.

    let (tbs, _) = asn1_sequence(der)?;
    let (tbs_inner, _) = asn1_sequence(tbs)?;

    let mut pos = tbs_inner;

    // version (explicit tag [0], optional)
    if !pos.is_empty() && pos[0] == 0xA0 {
        let (_, rest) = asn1_skip_tlv(pos)?;
        pos = rest;
    }

    // serialNumber (INTEGER)
    let (_, rest) = asn1_skip_tlv(pos)?;
    pos = rest;

    // signature (SEQUENCE)
    let (_, rest) = asn1_skip_tlv(pos)?;
    pos = rest;

    // issuer (SEQUENCE)
    let (_, rest) = asn1_skip_tlv(pos)?;
    pos = rest;

    // validity (SEQUENCE { notBefore, notAfter })
    let (validity_content, _) = asn1_sequence(pos)?;

    // notBefore
    let (_, rest) = asn1_skip_tlv(validity_content)?;

    // notAfter
    let (not_after_bytes, _) = asn1_read_tlv(rest)?;
    parse_asn1_time(not_after_bytes)
}

fn asn1_sequence(data: &[u8]) -> Option<(&[u8], &[u8])> {
    if data.is_empty() || data[0] != 0x30 {
        return None;
    }
    let (content, rest) = asn1_read_content(&data[1..])?;
    Some((content, rest))
}

fn asn1_read_content(data: &[u8]) -> Option<(&[u8], &[u8])> {
    if data.is_empty() {
        return None;
    }
    let (len, header_size) = asn1_read_length(data)?;
    let content = data.get(header_size..header_size + len)?;
    let rest = data.get(header_size + len..)?;
    Some((content, rest))
}

fn asn1_read_length(data: &[u8]) -> Option<(usize, usize)> {
    if data.is_empty() {
        return None;
    }
    if data[0] < 0x80 {
        Some((data[0] as usize, 1))
    } else {
        let num_bytes = (data[0] & 0x7F) as usize;
        if num_bytes == 0 || num_bytes > 4 || data.len() < 1 + num_bytes {
            return None;
        }
        let mut len = 0usize;
        for i in 0..num_bytes {
            len = (len << 8) | (data[1 + i] as usize);
        }
        Some((len, 1 + num_bytes))
    }
}

fn asn1_skip_tlv(data: &[u8]) -> Option<(&[u8], &[u8])> {
    if data.is_empty() {
        return None;
    }
    let tag_len = 1; // simple tags only
    let (content_len, len_size) = asn1_read_length(&data[tag_len..])?;
    let total = tag_len + len_size + content_len;
    let value = data.get(tag_len + len_size..total)?;
    let rest = data.get(total..)?;
    Some((value, rest))
}

fn asn1_read_tlv(data: &[u8]) -> Option<(&[u8], &[u8])> {
    asn1_skip_tlv(data)
}

fn parse_asn1_time(data: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(data).ok()?;
    // UTCTime: YYMMDDHHMMSSZ (13 chars)
    // GeneralizedTime: YYYYMMDDHHMMSSZ (15 chars)
    let (year, rest) = if s.len() == 13 {
        let y: i32 = s[0..2].parse().ok()?;
        let y = if y >= 50 { 1900 + y } else { 2000 + y };
        (y, &s[2..])
    } else if s.len() >= 15 {
        let y: i32 = s[0..4].parse().ok()?;
        (y, &s[4..])
    } else {
        return None;
    };

    let month: u32 = rest[0..2].parse().ok()?;
    let day: u32 = rest[2..4].parse().ok()?;
    let hour: u32 = rest[4..6].parse().ok()?;
    let min: u32 = rest[6..8].parse().ok()?;
    let sec: u32 = rest[8..10].parse().ok()?;

    let dt = chrono::NaiveDate::from_ymd_opt(year, month, day)?
        .and_hms_opt(hour, min, sec)?
        .and_utc();
    Some(dt.timestamp())
}

/// Parse cert/key PEM into a `CertifiedKey` for the hot-swappable
/// resolver. Does not build a `ServerConfig` — the resolver is built once
/// at startup and each swap only replaces the key this returns.
pub fn certified_key(tls: &TlsMaterial) -> Result<Arc<CertifiedKey>> {
    let cert_pem = fs::read(&tls.cert_path).context("failed to read cert.pem")?;
    let key_pem = fs::read(&tls.key_path).context("failed to read key.pem")?;

    let certs: Vec<_> = rustls_pemfile::certs(&mut BufReader::new(&cert_pem[..]))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to parse certificates")?;

    let key = rustls_pemfile::private_key(&mut BufReader::new(&key_pem[..]))
        .context("failed to parse private key")?
        .context("no private key found in key.pem")?;

    let signing_key = rustls::crypto::aws_lc_rs::sign::any_supported_type(&key)
        .context("unsupported private key type")?;

    Ok(Arc::new(CertifiedKey::new(certs, signing_key)))
}

/// Build a TLS acceptor backed by a `ResolvesServerCert` (rather than a
/// fixed cert baked into the `ServerConfig`), so swapping the resolver's
/// underlying key takes effect on the next handshake with no listener
/// rebind.
pub fn acceptor_from_resolver(
    resolver: Arc<dyn rustls::server::ResolvesServerCert>,
) -> TlsAcceptor {
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(resolver);

    TlsAcceptor::from(Arc::new(config))
}

/// Fixed id for the "HTTPS is disabled" banner so restarts never
/// duplicate it — each announce call deletes any existing row first.
pub const TLS_FAILURE_ANNOUNCEMENT_ID: &str = "tls-startup-failure";

/// Record (or refresh) the HTTPS-disabled banner shown to the next user
/// who logs in. Delete-then-insert on the fixed id so it never
/// duplicates across restarts.
pub async fn announce_failure(db: &Db, detail: &str) -> Result<()> {
    db.delete_announcement(TLS_FAILURE_ANNOUNCEMENT_ID).await?;
    db.create_announcement(NewAnnouncement {
        id: TLS_FAILURE_ANNOUNCEMENT_ID.to_string(),
        kind: "tls_error".to_string(),
        title: "HTTPS is disabled".to_string(),
        message: "Peckboard is serving plain HTTP only".to_string(),
        detail: Some(detail.to_string()),
        created_at: chrono::Utc::now().to_rfc3339(),
    })
    .await?;
    Ok(())
}

/// Clear the HTTPS-disabled banner after a successful TLS load/reload.
pub async fn clear_failure_announcement(db: &Db) -> Result<()> {
    db.delete_announcement(TLS_FAILURE_ANNOUNCEMENT_ID).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A throwaway cert/key pair standing in for operator-supplied material.
    fn generate_pem_pair() -> (String, String) {
        let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut params = CertificateParams::default();
        params.subject_alt_names = vec![SanType::DnsName("uploaded.example".try_into().unwrap())];
        let cert = params.self_signed(&key_pair).unwrap();
        (cert.pem(), key_pair.serialize_pem())
    }

    #[test]
    fn test_ensure_certs_generates_files() {
        let tmp = TempDir::new().unwrap();
        let material = ensure_certs(tmp.path()).unwrap();

        assert_eq!(material.source, TlsSource::SelfSigned);
        assert!(material.cert_path.exists());
        assert!(material.key_path.exists());

        // Verify PEM content
        let cert_content = fs::read_to_string(&material.cert_path).unwrap();
        assert!(cert_content.contains("BEGIN CERTIFICATE"));

        let key_content = fs::read_to_string(&material.key_path).unwrap();
        assert!(key_content.contains("BEGIN"));
    }

    #[test]
    fn test_ensure_certs_idempotent() {
        let tmp = TempDir::new().unwrap();
        let m1 = ensure_certs(tmp.path()).unwrap();
        let cert1 = fs::read_to_string(&m1.cert_path).unwrap();
        let key1 = fs::read_to_string(&m1.key_path).unwrap();
        let sidecar1 = read_sidecar(&sidecar_path(tmp.path())).unwrap();

        // Second call should reuse existing certs (unless renewal needed)
        let m2 = ensure_certs(tmp.path()).unwrap();
        let cert2 = fs::read_to_string(&m2.cert_path).unwrap();
        let key2 = fs::read_to_string(&m2.key_path).unwrap();
        let sidecar2 = read_sidecar(&sidecar_path(tmp.path())).unwrap();

        assert_eq!(cert1, cert2);
        assert_eq!(
            key1, key2,
            "unchanged SANs + healthy expiry must not touch the key"
        );
        assert_eq!(
            sidecar1.generated_at, sidecar2.generated_at,
            "unchanged SANs + healthy expiry must not rewrite the sidecar"
        );
    }

    #[test]
    fn test_is_usable_san_ip_excludes_link_local_v6() {
        assert!(!is_usable_san_ip(&"fe80::1".parse().unwrap()));
        assert!(!is_usable_san_ip(&"fe80::abcd:1234".parse().unwrap()));
        // fec0::/10 is outside the fe80::/10 link-local range and must not
        // be excluded by the same check.
        assert!(is_usable_san_ip(&"fec0::1".parse().unwrap()));
        // A routable global-unicast address stays usable.
        assert!(is_usable_san_ip(&"2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn test_san_targets_include_a_detected_non_loopback_address() {
        let detected: Vec<_> = if_addrs::get_if_addrs()
            .unwrap()
            .into_iter()
            .map(|iface| iface.addr.ip())
            .filter(is_usable_san_ip)
            .collect();

        if detected.is_empty() {
            // No non-loopback interface on this host (e.g. a locked-down
            // container) — nothing to assert.
            return;
        }

        let targets = san_targets();
        assert!(
            detected
                .iter()
                .any(|ip| targets.contains(&SanType::IpAddress(*ip))),
            "expected at least one detected non-loopback address in san_targets(), got {targets:?}"
        );
    }

    #[test]
    fn test_san_list_is_deterministic_and_covers_loopback() {
        let sans = san_strings();

        for expected in ["localhost", "127.0.0.1", "::1"] {
            assert!(sans.iter().any(|s| s == expected), "missing SAN {expected}");
        }

        let mut sorted = sans.clone();
        sorted.sort();
        assert_eq!(sans, sorted, "SAN list must be sorted");

        let mut deduped = sans.clone();
        deduped.dedup();
        assert_eq!(sans, deduped, "SAN list must be de-duplicated");

        for san in &sans {
            if let Ok(IpAddr::V6(v6)) = san.parse::<IpAddr>() {
                assert!(
                    v6.segments()[0] & 0xffc0 != 0xfe80,
                    "link-local {san} must not be a SAN"
                );
            }
        }

        // Every spec survives the conversion to rcgen's representation.
        assert_eq!(san_targets().len(), sans.len());
    }

    #[test]
    fn test_sidecar_records_current_sans() {
        let tmp = TempDir::new().unwrap();
        ensure_certs(tmp.path()).unwrap();

        let sidecar = read_sidecar(&sidecar_path(tmp.path())).expect("sidecar written");
        assert_eq!(sidecar.sans, san_strings());
        assert!(chrono::DateTime::parse_from_rfc3339(&sidecar.generated_at).is_ok());
    }

    #[test]
    fn test_stale_sidecar_forces_regeneration() {
        let tmp = TempDir::new().unwrap();
        let m1 = ensure_certs(tmp.path()).unwrap();
        let cert1 = fs::read_to_string(&m1.cert_path).unwrap();

        // A host that has since gained an address: the sidecar no longer
        // matches what `san_targets()` would produce.
        fs::write(
            sidecar_path(tmp.path()),
            r#"{"sans":["localhost"],"generated_at":"2020-01-01T00:00:00+00:00"}"#,
        )
        .unwrap();

        let m2 = ensure_certs(tmp.path()).unwrap();
        let cert2 = fs::read_to_string(&m2.cert_path).unwrap();
        assert_ne!(cert1, cert2, "SAN drift must regenerate the cert");
        assert_eq!(
            read_sidecar(&sidecar_path(tmp.path())).unwrap().sans,
            san_strings()
        );
    }

    #[test]
    fn test_needs_renewal_missing_file() {
        assert!(needs_renewal(Path::new("/nonexistent/cert.pem")));
    }

    #[test]
    fn test_load_tls_config() {
        // Install the default crypto provider for rustls
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let tmp = TempDir::new().unwrap();
        let material = ensure_certs(tmp.path()).unwrap();
        let key = certified_key(&material).unwrap();
        let resolver: Arc<dyn rustls::server::ResolvesServerCert> = Arc::new(FixedResolver(key));
        let _acceptor = acceptor_from_resolver(resolver);
    }

    /// Test-only resolver: always returns the same key, standing in for
    /// `state::TlsCertResolver` (defined in `state.rs`, which can't be
    /// depended on from here without a cycle).
    #[derive(Debug)]
    struct FixedResolver(Arc<CertifiedKey>);

    impl rustls::server::ResolvesServerCert for FixedResolver {
        fn resolve(&self, _client_hello: rustls::server::ClientHello) -> Option<Arc<CertifiedKey>> {
            Some(self.0.clone())
        }
    }

    #[test]
    fn test_material_info_reports_sans_and_expiry() {
        let tmp = TempDir::new().unwrap();
        let material = ensure_certs(tmp.path()).unwrap();
        let info = material_info(tmp.path(), &material);

        assert_eq!(info.source, TlsSource::SelfSigned);
        assert_eq!(info.sans, san_strings());
        let not_after = info.not_after.expect("notAfter parses");
        assert!(not_after > chrono::Utc::now() + chrono::Duration::days(300));
    }

    #[test]
    fn test_uploaded_material_wins_and_survives_renewal() {
        let tmp = TempDir::new().unwrap();
        let self_signed = ensure_certs(tmp.path()).unwrap();
        assert_eq!(self_signed.source, TlsSource::SelfSigned);

        let (cert_pem, key_pem) = generate_pem_pair();
        let installed = install_uploaded(tmp.path(), &cert_pem, &key_pem).unwrap();
        assert_eq!(installed.source, TlsSource::Uploaded);

        let material = ensure_certs(tmp.path()).unwrap();
        assert_eq!(material.source, TlsSource::Uploaded);
        assert_eq!(material.cert_path, installed.cert_path);
        assert_eq!(fs::read_to_string(&material.cert_path).unwrap(), cert_pem);

        // The renewal path must not touch uploaded material.
        assert_eq!(fs::read_to_string(&material.key_path).unwrap(), key_pem);
        assert!(self_signed.cert_path.exists());

        clear_uploaded(tmp.path()).unwrap();
        assert_eq!(
            ensure_certs(tmp.path()).unwrap().source,
            TlsSource::SelfSigned
        );
        // Clearing twice is not an error.
        clear_uploaded(tmp.path()).unwrap();
    }

    #[test]
    fn test_unparseable_uploaded_material_is_ignored() {
        let tmp = TempDir::new().unwrap();
        assert!(install_uploaded(tmp.path(), "not a cert", "not a key").is_err());

        // Half-written material on disk must not shadow the self-signed cert.
        let dir = certs_dir(tmp.path());
        fs::create_dir_all(&dir).unwrap();
        fs::write(uploaded_cert_path(tmp.path()), "garbage").unwrap();
        fs::write(uploaded_key_path(tmp.path()), "garbage").unwrap();

        assert_eq!(
            ensure_certs(tmp.path()).unwrap().source,
            TlsSource::SelfSigned
        );
    }

    /// A crash between the two halves of an install leaves a new
    /// certificate next to the previous key. That pair can't complete a
    /// handshake, so it must be ignored in favour of the self-signed
    /// fallback rather than served.
    #[test]
    fn test_a_torn_uploaded_pair_is_ignored() {
        let tmp = TempDir::new().unwrap();
        let (cert_pem, key_pem) = generate_pem_pair();
        install_uploaded(tmp.path(), &cert_pem, &key_pem).unwrap();
        assert_eq!(
            ensure_certs(tmp.path()).unwrap().source,
            TlsSource::Uploaded
        );

        let (foreign_cert, _) = generate_pem_pair();
        fs::write(uploaded_cert_path(tmp.path()), &foreign_cert).unwrap();

        assert_eq!(
            ensure_certs(tmp.path()).unwrap().source,
            TlsSource::SelfSigned,
            "a certificate whose key doesn't match it must not be served"
        );
    }

    /// Same failure for the self-signed pair, except here we own both
    /// halves, so the fix is to mint a fresh pair instead of handing the
    /// resolver material every handshake would fail on.
    #[test]
    fn test_a_torn_self_signed_pair_is_regenerated() {
        let tmp = TempDir::new().unwrap();
        let material = ensure_certs(tmp.path()).unwrap();
        let (_, foreign_key) = generate_pem_pair();
        fs::write(&material.key_path, &foreign_key).unwrap();

        let healed = ensure_certs(tmp.path()).unwrap();
        assert_eq!(healed.source, TlsSource::SelfSigned);
        assert_ne!(
            fs::read_to_string(&healed.key_path).unwrap(),
            foreign_key,
            "the mismatched key must have been replaced"
        );
        validate_pem_pair(
            &fs::read(&healed.cert_path).unwrap(),
            &fs::read(&healed.key_path).unwrap(),
        )
        .unwrap();
    }

    /// The service layer rejects a mismatched pair on its own, without
    /// depending on the route in front of it having checked first.
    #[test]
    fn test_install_uploaded_rejects_a_mismatched_pair() {
        let tmp = TempDir::new().unwrap();
        let (cert_pem, _) = generate_pem_pair();
        let (_, key_pem) = generate_pem_pair();

        let err = install_uploaded(tmp.path(), &cert_pem, &key_pem).unwrap_err();
        assert!(
            format!("{err:#}").contains("do not work together"),
            "got {err:#}"
        );
        assert!(
            !uploaded_cert_path(tmp.path()).exists(),
            "a rejected pair must not leave anything on disk"
        );
        assert!(!uploaded_key_path(tmp.path()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_key_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let material = ensure_certs(tmp.path()).unwrap();
        let perms = fs::metadata(&material.key_path).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn test_uploaded_key_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let (cert_pem, key_pem) = generate_pem_pair();
        let material = install_uploaded(tmp.path(), &cert_pem, &key_pem).unwrap();
        let perms = fs::metadata(&material.key_path).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }

    #[tokio::test]
    async fn test_announce_and_clear_failure_is_idempotent() {
        let db = crate::db::Db::in_memory().unwrap();

        announce_failure(&db, "boom").await.unwrap();
        announce_failure(&db, "boom again").await.unwrap();

        let all = db.list_announcements().await.unwrap();
        assert_eq!(
            all.len(),
            1,
            "repeated failures must not duplicate the banner"
        );
        assert_eq!(all[0].id, TLS_FAILURE_ANNOUNCEMENT_ID);
        assert_eq!(all[0].kind, "tls_error");
        assert_eq!(all[0].detail.as_deref(), Some("boom again"));

        clear_failure_announcement(&db).await.unwrap();
        assert!(db.list_announcements().await.unwrap().is_empty());

        // Clearing when nothing is there is not an error.
        clear_failure_announcement(&db).await.unwrap();
    }
}
