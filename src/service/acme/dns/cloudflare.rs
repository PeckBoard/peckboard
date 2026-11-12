//! Cloudflare DNS via the v4 API with a scoped API token
//! (Zone:Read + DNS:Edit).

use super::common::{api_error, candidates, json, send};
use super::{DnsProvider, TxtRecord};
use async_trait::async_trait;
use serde_json::json;

const VENDOR: &str = "Cloudflare";
pub const BASE_URL: &str = "https://api.cloudflare.com/client/v4";

pub struct Cloudflare {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Cloudflare {
    pub fn new(http: reqwest::Client, token: &str, base: &str) -> Self {
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    /// Probe `/zones?name=` from the longest candidate down, so the
    /// first hit is the longest matching zone.
    async fn zone_id(&self, fqdn: &str) -> anyhow::Result<String> {
        for name in candidates(fqdn) {
            let req = self
                .http
                .get(format!(
                    "{}/zones?name={}",
                    self.base,
                    urlencoding::encode(&name)
                ))
                .bearer_auth(&self.token);
            let (status, body) = send(req, VENDOR).await?;
            let v = json(VENDOR, status, &body)?;
            if let Some(id) = v["result"][0]["id"].as_str() {
                return Ok(id.to_string());
            }
        }
        anyhow::bail!("no Cloudflare zone found for {fqdn} (the token needs Zone:Read on it)")
    }
}

#[async_trait]
impl DnsProvider for Cloudflare {
    async fn create_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<TxtRecord> {
        let zone = self.zone_id(fqdn).await?;
        let req = self
            .http
            .post(format!("{}/zones/{zone}/dns_records", self.base))
            .bearer_auth(&self.token)
            .json(&json!({ "type": "TXT", "name": fqdn, "content": value, "ttl": 120 }));
        let (status, body) = send(req, VENDOR).await?;
        let v = json(VENDOR, status, &body)?;
        if v["success"].as_bool() == Some(false) {
            return Err(api_error(VENDOR, status, &body));
        }
        let id = v["result"]["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Cloudflare API returned no record id"))?;
        Ok(TxtRecord {
            fqdn: fqdn.to_string(),
            value: value.to_string(),
            id: format!("{zone}/{id}"),
        })
    }

    async fn delete_txt(&self, record: &TxtRecord) -> anyhow::Result<()> {
        let (zone, id) = record
            .id
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("malformed Cloudflare record id '{}'", record.id))?;
        let req = self
            .http
            .delete(format!("{}/zones/{zone}/dns_records/{id}", self.base))
            .bearer_auth(&self.token);
        let (status, body) = send(req, VENDOR).await?;
        if status == 404 {
            return Ok(());
        }
        json(VENDOR, status, &body).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::super::common::testsrv::serve;
    use super::*;

    #[tokio::test]
    async fn creates_in_longest_zone_and_deletes() {
        let (base, log) = serve(|r| match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/zones") if r.query == "name=example.com" => {
                (200, r#"{"success":true,"result":[{"id":"z1"}]}"#.into())
            }
            ("GET", "/zones") => (200, r#"{"success":true,"result":[]}"#.into()),
            ("POST", "/zones/z1/dns_records") => {
                (200, r#"{"success":true,"result":{"id":"r1"}}"#.into())
            }
            ("DELETE", "/zones/z1/dns_records/r1") => (200, r#"{"success":true}"#.into()),
            _ => (
                400,
                r#"{"success":false,"errors":[{"code":1,"message":"nope"}]}"#.into(),
            ),
        })
        .await;
        let p = Cloudflare::new(crate::http_client::client(), "tok", &base);
        let rec = p
            .create_txt("_acme-challenge.www.example.com", "v1")
            .await
            .unwrap();
        assert_eq!(rec.id, "z1/r1");
        p.delete_txt(&rec).await.unwrap();

        let log = log.lock().unwrap();
        let post = log.iter().find(|r| r.method == "POST").unwrap();
        assert_eq!(post.header("authorization"), "Bearer tok");
        assert_eq!(post.json()["type"], "TXT");
        assert_eq!(post.json()["name"], "_acme-challenge.www.example.com");
        assert_eq!(post.json()["content"], "v1");
        assert!(log.iter().any(|r| r.method == "DELETE"));
        drop(log);

        let err = p
            .delete_txt(&TxtRecord {
                fqdn: "x".into(),
                value: "v".into(),
                id: "zz/rr".into(),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
    }
}
