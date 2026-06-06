// TLS cert management (rcgen self-signed + file-based)

use anyhow::{Context, Result};
use rcgen::{CertificateParams, DnType, KeyPair, SanType};
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;

#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

/// Check if certs exist; generate self-signed ECDSA P-256 certs if not.
pub fn ensure_certs(data_dir: &Path) -> Result<TlsConfig> {
    let certs_dir = data_dir.join("certs");
    let cert_path = certs_dir.join("cert.pem");
    let key_path = certs_dir.join("key.pem");

    if cert_path.exists() && key_path.exists() {
        if !needs_renewal(&cert_path) {
            return Ok(TlsConfig {
                cert_path,
                key_path,
            });
        }
        tracing::info!("TLS certificate needs renewal, regenerating...");
    }

    fs::create_dir_all(&certs_dir).context("failed to create certs directory")?;

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
    params.subject_alt_names = vec![
        SanType::DnsName("localhost".try_into().unwrap()),
        SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        SanType::IpAddress(std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)),
    ];
    params.not_before = rcgen::date_time_ymd(2024, 1, 1);
    params.not_after = rcgen::date_time_ymd(2026, 1, 1);
    // 365 days is approximate; we use a fixed range for simplicity in generation,
    // and rely on needs_renewal for actual expiry checking.
    let now = chrono::Utc::now();
    let expire = now + chrono::Duration::days(365);
    params.not_before = time_from_chrono(now);
    params.not_after = time_from_chrono(expire);

    let cert = params
        .self_signed(&key_pair)
        .context("failed to generate self-signed certificate")?;

    // Write cert PEM
    fs::write(&cert_path, cert.pem()).context("failed to write cert.pem")?;

    // Write key PEM
    fs::write(&key_path, key_pair.serialize_pem()).context("failed to write key.pem")?;

    // Set key permissions to 0o600
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))
            .context("failed to set key.pem permissions")?;
    }

    tracing::info!("Generated self-signed TLS certificate at {:?}", certs_dir);

    Ok(TlsConfig {
        cert_path,
        key_path,
    })
}

fn time_from_chrono(dt: chrono::DateTime<chrono::Utc>) -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(dt.timestamp()).unwrap()
}

/// Check if cert expires within 30 days.
pub fn needs_renewal(cert_path: &Path) -> bool {
    let pem_data = match fs::read(cert_path) {
        Ok(data) => data,
        Err(_) => return true,
    };

    let mut reader = BufReader::new(&pem_data[..]);
    let certs = match rustls_pemfile::certs(&mut reader).collect::<std::result::Result<Vec<_>, _>>()
    {
        Ok(c) => c,
        Err(_) => return true,
    };

    let cert = match certs.first() {
        Some(c) => c,
        None => return true,
    };

    // Parse the X.509 certificate to check validity
    // We use a simple ASN.1 approach: check the notAfter field
    // For robustness, try to parse with x509-parser logic or just check file age
    // Since we don't have x509-parser, use a heuristic: check the file modification time
    // and compare against certificate lifetime.
    //
    // A more robust approach: parse the DER and extract notAfter.
    // For now, use the DER bytes directly.
    match parse_not_after(cert.as_ref()) {
        Some(not_after) => {
            let now = chrono::Utc::now().timestamp();
            let thirty_days = 30 * 24 * 60 * 60;
            not_after - now < thirty_days
        }
        None => true, // If we can't parse, assume renewal needed
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

/// Load cert/key into a TLS acceptor.
pub fn load_tls_config(tls: &TlsConfig) -> Result<TlsAcceptor> {
    let cert_pem = fs::read(&tls.cert_path).context("failed to read cert.pem")?;
    let key_pem = fs::read(&tls.key_path).context("failed to read key.pem")?;

    let certs: Vec<_> = rustls_pemfile::certs(&mut BufReader::new(&cert_pem[..]))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to parse certificates")?;

    let key = rustls_pemfile::private_key(&mut BufReader::new(&key_pem[..]))
        .context("failed to parse private key")?
        .context("no private key found in key.pem")?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("failed to build TLS server config")?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_ensure_certs_generates_files() {
        let tmp = TempDir::new().unwrap();
        let tls_config = ensure_certs(tmp.path()).unwrap();

        assert!(tls_config.cert_path.exists());
        assert!(tls_config.key_path.exists());

        // Verify PEM content
        let cert_content = fs::read_to_string(&tls_config.cert_path).unwrap();
        assert!(cert_content.contains("BEGIN CERTIFICATE"));

        let key_content = fs::read_to_string(&tls_config.key_path).unwrap();
        assert!(key_content.contains("BEGIN"));
    }

    #[test]
    fn test_ensure_certs_idempotent() {
        let tmp = TempDir::new().unwrap();
        let config1 = ensure_certs(tmp.path()).unwrap();
        let cert1 = fs::read_to_string(&config1.cert_path).unwrap();

        // Second call should reuse existing certs (unless renewal needed)
        let config2 = ensure_certs(tmp.path()).unwrap();
        let cert2 = fs::read_to_string(&config2.cert_path).unwrap();

        assert_eq!(cert1, cert2);
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
        let tls_config = ensure_certs(tmp.path()).unwrap();
        let _acceptor = load_tls_config(&tls_config).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_key_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let tls_config = ensure_certs(tmp.path()).unwrap();
        let perms = fs::metadata(&tls_config.key_path).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }
}
