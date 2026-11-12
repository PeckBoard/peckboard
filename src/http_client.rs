//! Outgoing HTTP clients and the process-wide rustls crypto provider.
//!
//! ring is the only rustls backend linked into the binary (see the `# TLS`
//! block in `Cargo.toml`). reqwest is built with `rustls-no-provider`, so
//! building a `reqwest::Client` panics unless a process-default
//! `CryptoProvider` is installed first. Every client is built through
//! [`builder`] / [`client`], which install it; `clippy.toml` disallows the
//! bare reqwest constructors so a new call site can't skip this.

/// Install ring as the process-default rustls `CryptoProvider`. Idempotent
/// and cheap after the first call; safe to call from anywhere.
pub fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// `reqwest::Client::builder()`, with the crypto provider installed.
#[allow(clippy::disallowed_methods)] // the one sanctioned constructor
pub fn builder() -> reqwest::ClientBuilder {
    install_crypto_provider();
    reqwest::Client::builder()
}

/// `reqwest::Client::new()`, with the crypto provider installed. Panics on
/// the same (TLS backend / resolver init) failures `Client::new` does.
pub fn client() -> reqwest::Client {
    builder()
        .build()
        .expect("reqwest client with default settings")
}

#[cfg(test)]
mod tests {
    #[test]
    fn client_builds_without_a_preinstalled_provider() {
        let _ = super::client();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
}
