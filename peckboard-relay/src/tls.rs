//! TLS 1.3-only server configs: ACME (TLS-ALPN-01 on the same :443
//! listener) for production, or a throwaway self-signed cert for tests and
//! local runs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use futures_util::StreamExt;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls_acme::caches::DirCache;
use rustls_acme::{AcmeConfig, is_tls_alpn_challenge};
use tracing::{info, warn};

use crate::proto::ALPN;

pub const ACME_TLS_ALPN: &[u8] = b"acme-tls/1";

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn builder() -> anyhow::Result<rustls::ConfigBuilder<ServerConfig, rustls::WantsVerifier>> {
    Ok(ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?)
}

fn finish(mut cfg: ServerConfig, with_acme: bool) -> Arc<ServerConfig> {
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    if with_acme {
        cfg.alpn_protocols.push(ACME_TLS_ALPN.to_vec());
    }
    // No session tickets/resumption: sessions are long-lived and few.
    cfg.send_tls13_tickets = 0;
    Arc::new(cfg)
}

/// Self-signed cert for `names`. Returns the config and the cert DER so a
/// test/dev client can pin it.
pub fn self_signed(
    names: &[String],
) -> anyhow::Result<(Arc<ServerConfig>, CertificateDer<'static>)> {
    let ck = rcgen::generate_simple_self_signed(names.to_vec())?;
    let cert = CertificateDer::from(ck.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.key_pair.serialize_der()));
    let cfg = builder()?
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key)?;
    Ok((finish(cfg, false), cert))
}

/// ACME-managed cert for `domain`, cached under `state_dir/acme` (dir 0700,
/// files 0600). Spawns the renewal driver on the current runtime.
pub fn acme(
    domain: &str,
    contact: Option<&str>,
    state_dir: &Path,
    production: bool,
) -> anyhow::Result<Arc<ServerConfig>> {
    let cache_dir = state_dir.join("acme");
    std::fs::create_dir_all(&cache_dir).context("create ACME cache dir")?;
    restrict_perms(state_dir);
    restrict_perms(&cache_dir);
    let mut cfg = AcmeConfig::new([domain])
        .cache(DirCache::new(cache_dir.clone()))
        .directory_lets_encrypt(production);
    if let Some(c) = contact {
        cfg = cfg.contact_push(format!("mailto:{c}"));
    }
    let mut state = cfg.state();
    let resolver = state.resolver();
    tokio::spawn(async move {
        while let Some(ev) = state.next().await {
            match ev {
                Ok(ok) => {
                    info!("acme: {ok:?}");
                    restrict_tree(&cache_dir);
                }
                Err(err) => warn!("acme: {err}"),
            }
        }
    });
    let server = builder()?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    Ok(finish(server, true))
}

/// True if this ClientHello is an ACME TLS-ALPN-01 validation probe.
pub fn is_acme_probe(hello: &rustls::server::ClientHello<'_>) -> bool {
    is_tls_alpn_challenge(hello)
}

#[cfg(unix)]
fn restrict_perms(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = if p.is_dir() { 0o700 } else { 0o600 };
    if let Err(e) = std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)) {
        warn!("chmod {}: {e}", p.display());
    }
}

#[cfg(not(unix))]
fn restrict_perms(_p: &Path) {}

fn restrict_tree(dir: &PathBuf) {
    restrict_perms(dir);
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            restrict_perms(&e.path());
        }
    }
}
