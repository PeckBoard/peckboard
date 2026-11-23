//! HTTPS serving: TLS 1.3 with an ACME (Let's Encrypt, TLS-ALPN-01 on the
//! same :443 listener) certificate, as in `peckboard-relay`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use futures_util::StreamExt;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use rustls::ServerConfig;
use rustls_acme::AcmeConfig;
use rustls_acme::caches::DirCache;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

pub const ACME_TLS_ALPN: &[u8] = b"acme-tls/1";
/// Concurrent connections served at once.
pub const MAX_CONNECTIONS: usize = 2048;

/// ACME-managed cert for `domain`, cached under `data_dir/acme` (0700).
/// Spawns the renewal driver on the current runtime.
pub fn acme(
    domain: &str,
    contact: Option<&str>,
    data_dir: &Path,
    production: bool,
) -> anyhow::Result<Arc<ServerConfig>> {
    let cache_dir = data_dir.join("acme");
    std::fs::create_dir_all(&cache_dir).context("create ACME cache dir")?;
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
    let mut server =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_no_client_auth()
            .with_cert_resolver(resolver);
    server.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec(), ACME_TLS_ALPN.to_vec()];
    Ok(Arc::new(server))
}

/// Accept TLS connections forever and serve `app` on them. ACME
/// validation handshakes are answered by the resolver and then closed.
pub async fn serve(listener: TcpListener, cfg: Arc<ServerConfig>, app: Router) {
    let acceptor = TlsAcceptor::from(cfg);
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (tcp, _) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                warn!("accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            continue; // At capacity: drop the connection.
        };
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let tls =
                match tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp)).await {
                    Ok(Ok(s)) => s,
                    _ => return,
                };
            if tls.get_ref().1.alpn_protocol() == Some(ACME_TLS_ALPN) {
                return;
            }
            let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
            builder
                .http1()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(15));
            let _ = builder
                .serve_connection(TokioIo::new(tls), TowerToHyperService::new(app))
                .await;
        });
    }
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
