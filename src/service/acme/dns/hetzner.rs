//! Hetzner DNS (dns.hetzner.com) via its v1 API with an API token.

use super::common::{json, longest_zone, relative_name, send};
use super::{DnsProvider, TxtRecord};
use async_trait::async_trait;
use serde_json::json;

const VENDOR: &str = "Hetzner DNS";
pub const BASE_URL: &str = "https://dns.hetzner.com/api/v1";

pub struct Hetzner {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Hetzner {
    pub fn new(http: reqwest::Client, token: &str, base: &str) -> Self {
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    /// `(zone_id, zone_name)` of the longest zone owning `fqdn`.
    async fn zone(&self, fqdn: &str) -> anyhow::Result<(String, String)> {
        let mut zones: Vec<(String, String)> = Vec::new();
        let mut page = 1;
        loop {
            let req = self
                .http
                .get(format!("{}/zones?per_page=100&page={page}", self.base))
                .header("Auth-API-Token", &self.token);
            let (status, body) = send(req, VENDOR).await?;
            let v = json(VENDOR, status, &body)?;
            for z in v["zones"].as_array().into_iter().flatten() {
                if let (Some(id), Some(name)) = (z["id"].as_str(), z["name"].as_str()) {
                    zones.push((id.to_string(), name.to_string()));
                }
            }
            let last = v["meta"]["pagination"]["last_page"].as_i64().unwrap_or(1);
            if page >= last || page >= 50 {
                break;
            }
            page += 1;
        }
        let name = longest_zone(fqdn, zones.iter().map(|(_, n)| n.as_str()))
            .ok_or_else(|| anyhow::anyhow!("no Hetzner DNS zone found for {fqdn}"))?;
        let id = zones
            .into_iter()
            .find(|(_, n)| n.eq_ignore_ascii_case(&name))
            .map(|(id, _)| id)
            .unwrap_or_default();
        Ok((id, name))
    }
}

#[async_trait]
impl DnsProvider for Hetzner {
    async fn create_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<TxtRecord> {
        let (zone_id, zone) = self.zone(fqdn).await?;
        let name = relative_name(fqdn, &zone);
        let name = if name.is_empty() {
            "@".to_string()
        } else {
            name
        };
        let req = self
            .http
            .post(format!("{}/records", self.base))
            .header("Auth-API-Token", &self.token)
            .json(&json!({
                "zone_id": zone_id, "type": "TXT", "name": name, "value": value, "ttl": 60
            }));
        let (status, body) = send(req, VENDOR).await?;
        let v = json(VENDOR, status, &body)?;
        let id = v["record"]["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Hetzner DNS API returned no record id"))?;
        Ok(TxtRecord {
            fqdn: fqdn.to_string(),
            value: value.to_string(),
            id: id.to_string(),
        })
    }

    async fn delete_txt(&self, record: &TxtRecord) -> anyhow::Result<()> {
        let req = self
            .http
            .delete(format!("{}/records/{}", self.base, record.id))
            .header("Auth-API-Token", &self.token);
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
    async fn creates_with_zone_id_and_auth_header() {
        let (base, log) = serve(|r| match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/zones") => (
                200,
                r#"{"zones":[{"id":"za","name":"example.com"},{"id":"zb","name":"example.net"}],
                    "meta":{"pagination":{"last_page":1}}}"#
                    .into(),
            ),
            ("POST", "/records") => (200, r#"{"record":{"id":"rec1"}}"#.into()),
            ("DELETE", "/records/rec1") => (200, "{}".into()),
            _ => (
                422,
                r#"{"error":{"message":"invalid zone","code":422}}"#.into(),
            ),
        })
        .await;
        let p = Hetzner::new(crate::http_client::client(), "tok", &base);
        let rec = p
            .create_txt("_acme-challenge.example.com", "v1")
            .await
            .unwrap();
        assert_eq!(rec.id, "rec1");
        p.delete_txt(&rec).await.unwrap();

        let log = log.lock().unwrap();
        let post = log.iter().find(|r| r.method == "POST").unwrap();
        assert_eq!(post.header("auth-api-token"), "tok");
        assert_eq!(post.json()["zone_id"], "za");
        assert_eq!(post.json()["name"], "_acme-challenge");
        assert_eq!(post.json()["value"], "v1");
        drop(log);

        let err = p
            .delete_txt(&TxtRecord {
                fqdn: "x".into(),
                value: "v".into(),
                id: "bad".into(),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid zone"), "{err}");
    }
}
