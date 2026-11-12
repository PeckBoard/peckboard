//! DigitalOcean DNS via the v2 API with a personal access token
//! (write scope).

use super::common::{json, longest_zone, relative_name, send};
use super::{DnsProvider, TxtRecord};
use async_trait::async_trait;
use serde_json::json;

const VENDOR: &str = "DigitalOcean";
pub const BASE_URL: &str = "https://api.digitalocean.com/v2";

pub struct DigitalOcean {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl DigitalOcean {
    pub fn new(http: reqwest::Client, token: &str, base: &str) -> Self {
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    async fn zone(&self, fqdn: &str) -> anyhow::Result<String> {
        let req = self
            .http
            .get(format!("{}/domains?per_page=200", self.base))
            .bearer_auth(&self.token);
        let (status, body) = send(req, VENDOR).await?;
        let v = json(VENDOR, status, &body)?;
        let names = v["domains"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["name"].as_str());
        longest_zone(fqdn, names)
            .ok_or_else(|| anyhow::anyhow!("no DigitalOcean domain found for {fqdn}"))
    }
}

#[async_trait]
impl DnsProvider for DigitalOcean {
    async fn create_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<TxtRecord> {
        let zone = self.zone(fqdn).await?;
        let name = relative_name(fqdn, &zone);
        let name = if name.is_empty() {
            "@".to_string()
        } else {
            name
        };
        let req = self
            .http
            .post(format!("{}/domains/{zone}/records", self.base))
            .bearer_auth(&self.token)
            .json(&json!({ "type": "TXT", "name": name, "data": value, "ttl": 30 }));
        let (status, body) = send(req, VENDOR).await?;
        let v = json(VENDOR, status, &body)?;
        let id = v["domain_record"]["id"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("DigitalOcean API returned no record id"))?;
        Ok(TxtRecord {
            fqdn: fqdn.to_string(),
            value: value.to_string(),
            id: format!("{zone}/{id}"),
        })
    }

    async fn delete_txt(&self, record: &TxtRecord) -> anyhow::Result<()> {
        let (zone, id) = record
            .id
            .rsplit_once('/')
            .ok_or_else(|| anyhow::anyhow!("malformed DigitalOcean record id '{}'", record.id))?;
        let req = self
            .http
            .delete(format!("{}/domains/{zone}/records/{id}", self.base))
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
    async fn creates_relative_record_in_longest_zone() {
        let (base, log) = serve(|r| match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/domains") => (
                200,
                r#"{"domains":[{"name":"example.com"},{"name":"sub.example.com"}]}"#.into(),
            ),
            ("POST", "/domains/sub.example.com/records") => {
                (201, r#"{"domain_record":{"id":42}}"#.into())
            }
            ("DELETE", "/domains/sub.example.com/records/42") => (204, String::new()),
            _ => (401, r#"{"id":"unauthorized","message":"bad token"}"#.into()),
        })
        .await;
        let p = DigitalOcean::new(crate::http_client::client(), "tok", &base);
        let rec = p
            .create_txt("_acme-challenge.sub.example.com", "v1")
            .await
            .unwrap();
        assert_eq!(rec.id, "sub.example.com/42");
        p.delete_txt(&rec).await.unwrap();

        let log = log.lock().unwrap();
        let post = log.iter().find(|r| r.method == "POST").unwrap();
        assert_eq!(post.header("authorization"), "Bearer tok");
        assert_eq!(post.json()["name"], "_acme-challenge");
        assert_eq!(post.json()["data"], "v1");
        drop(log);

        let err = p
            .create_txt("_acme-challenge.other.org", "v")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no DigitalOcean domain"), "{err}");
    }
}
