//! DuckDNS via its update URL with the account token.
//!
//! Limitation: DuckDNS holds exactly one TXT value per subdomain, and
//! every name under `<sub>.duckdns.org` shares it. Creating a record
//! replaces the previous value, so a certificate needing two challenges
//! at once (`foo.duckdns.org` + `*.foo.duckdns.org`) cannot validate —
//! issue those as separate certificates.

use super::common::{normalize, send};
use super::{DnsProvider, TxtRecord};
use async_trait::async_trait;

const VENDOR: &str = "DuckDNS";
pub const BASE_URL: &str = "https://www.duckdns.org";

pub struct DuckDns {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl DuckDns {
    pub fn new(http: reqwest::Client, token: &str, base: &str) -> Self {
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    async fn update(&self, fqdn: &str, txt: &str, clear: bool) -> anyhow::Result<()> {
        let sub = subdomain(fqdn)?;
        let mut url = format!(
            "{}/update?domains={}&token={}&txt={}&verbose=true",
            self.base,
            urlencoding::encode(&sub),
            urlencoding::encode(&self.token),
            urlencoding::encode(txt),
        );
        if clear {
            url.push_str("&clear=true");
        }
        let (status, body) = send(self.http.get(url), VENDOR).await?;
        if !(200..300).contains(&status) || !body.trim_start().starts_with("OK") {
            anyhow::bail!(
                "DuckDNS update failed (HTTP {status}): {} — check the token and that '{sub}' belongs to the account",
                body.trim()
            );
        }
        Ok(())
    }
}

/// The account subdomain owning `fqdn`: the label directly before
/// `duckdns.org`.
fn subdomain(fqdn: &str) -> anyhow::Result<String> {
    let name = normalize(fqdn);
    name.strip_suffix(".duckdns.org")
        .and_then(|rest| rest.rsplit('.').next())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("DuckDNS only serves names under duckdns.org, not {fqdn}"))
}

#[async_trait]
impl DnsProvider for DuckDns {
    async fn create_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<TxtRecord> {
        self.update(fqdn, value, false).await?;
        Ok(TxtRecord {
            fqdn: fqdn.to_string(),
            value: value.to_string(),
            id: value.to_string(),
        })
    }

    async fn delete_txt(&self, record: &TxtRecord) -> anyhow::Result<()> {
        self.update(&record.fqdn, "", true).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::common::testsrv::serve;
    use super::*;

    #[tokio::test]
    async fn updates_and_clears_subdomain_txt() {
        let (base, log) = serve(|r| {
            if r.query.contains("token=good") {
                (200, "OK\nv1\nUPDATED".into())
            } else {
                (200, "KO".into())
            }
        })
        .await;
        let p = DuckDns::new(reqwest::Client::new(), "good", &base);
        let rec = p
            .create_txt("_acme-challenge.myhome.duckdns.org", "v1")
            .await
            .unwrap();
        p.delete_txt(&rec).await.unwrap();
        {
            let log = log.lock().unwrap();
            assert_eq!(log[0].path, "/update");
            assert!(log[0].query.contains("domains=myhome&"), "{}", log[0].query);
            assert!(log[0].query.contains("txt=v1"));
            assert!(log[1].query.contains("clear=true"));
        }
        assert!(
            p.create_txt("_acme-challenge.example.com", "v")
                .await
                .is_err()
        );
        let bad = DuckDns::new(reqwest::Client::new(), "bad", &base);
        let err = bad
            .create_txt("_acme-challenge.myhome.duckdns.org", "v")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("KO"), "{err}");
    }
}
