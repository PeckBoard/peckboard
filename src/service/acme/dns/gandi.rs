//! Gandi LiveDNS via the v5 API with a personal access token (Bearer).
//! TXT values live in one RRset per name, so create/delete read it and
//! write it back with the value added or removed.

use super::common::{json, longest_zone, relative_name, send, unquote};
use super::{DnsProvider, TxtRecord};
use async_trait::async_trait;
use serde_json::json;

const VENDOR: &str = "Gandi";
pub const BASE_URL: &str = "https://api.gandi.net/v5/livedns";
const TTL: u32 = 300;

pub struct Gandi {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Gandi {
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
            .get(format!("{}/domains", self.base))
            .bearer_auth(&self.token);
        let (status, body) = send(req, VENDOR).await?;
        let v = json(VENDOR, status, &body)?;
        let names = v
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["fqdn"].as_str());
        longest_zone(fqdn, names)
            .ok_or_else(|| anyhow::anyhow!("no Gandi LiveDNS domain found for {fqdn}"))
    }

    fn url(&self, zone: &str, fqdn: &str) -> String {
        let name = relative_name(fqdn, zone);
        let name = if name.is_empty() {
            "@".to_string()
        } else {
            name
        };
        format!("{}/domains/{zone}/records/{name}/TXT", self.base)
    }

    async fn values(&self, url: &str) -> anyhow::Result<Vec<String>> {
        let (status, body) = send(self.http.get(url).bearer_auth(&self.token), VENDOR).await?;
        if status == 404 {
            return Ok(Vec::new());
        }
        let v = json(VENDOR, status, &body)?;
        Ok(v["rrset_values"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r.as_str().map(str::to_string))
            .collect())
    }

    async fn put(&self, url: &str, values: Vec<String>) -> anyhow::Result<()> {
        let req = self
            .http
            .put(url)
            .bearer_auth(&self.token)
            .json(&json!({ "rrset_values": values, "rrset_ttl": TTL }));
        let (status, body) = send(req, VENDOR).await?;
        json(VENDOR, status, &body).map(|_| ())
    }
}

#[async_trait]
impl DnsProvider for Gandi {
    async fn create_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<TxtRecord> {
        let zone = self.zone(fqdn).await?;
        let url = self.url(&zone, fqdn);
        let mut values = self.values(&url).await?;
        if !values.iter().any(|v| unquote(v) == value) {
            values.push(value.to_string());
            self.put(&url, values).await?;
        }
        Ok(TxtRecord {
            fqdn: fqdn.to_string(),
            value: value.to_string(),
            id: zone,
        })
    }

    async fn delete_txt(&self, record: &TxtRecord) -> anyhow::Result<()> {
        let url = self.url(&record.id, &record.fqdn);
        let values = self.values(&url).await?;
        let kept: Vec<String> = values
            .iter()
            .filter(|v| unquote(v) != record.value)
            .cloned()
            .collect();
        if kept.len() == values.len() {
            return Ok(());
        }
        if !kept.is_empty() {
            return self.put(&url, kept).await;
        }
        let (status, body) = send(self.http.delete(&url).bearer_auth(&self.token), VENDOR).await?;
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
    async fn appends_to_existing_rrset_with_bearer() {
        let (base, log) = serve(|r| match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/domains") => (200, r#"[{"fqdn":"example.com"}]"#.into()),
            ("GET", "/domains/example.com/records/_acme-challenge/TXT") => (
                200,
                r#"{"rrset_name":"_acme-challenge","rrset_values":["\"old\""],"rrset_ttl":300}"#
                    .into(),
            ),
            ("PUT", _) => (201, r#"{"message":"DNS Record Created"}"#.into()),
            ("DELETE", _) => (204, String::new()),
            _ => (403, r#"{"code":403,"message":"Access was denied"}"#.into()),
        })
        .await;
        let p = Gandi::new(reqwest::Client::new(), "pat", &base);
        let rec = p
            .create_txt("_acme-challenge.example.com", "new")
            .await
            .unwrap();
        {
            let log = log.lock().unwrap();
            let put = log.iter().find(|r| r.method == "PUT").unwrap();
            assert_eq!(put.header("authorization"), "Bearer pat");
            assert_eq!(put.json()["rrset_values"], json!(["\"old\"", "new"]));
        }
        // Removing the only stored value deletes the RRset.
        p.delete_txt(&TxtRecord {
            value: "old".into(),
            ..rec
        })
        .await
        .unwrap();
        assert!(log.lock().unwrap().iter().any(|r| r.method == "DELETE"));

        let err = p
            .create_txt("_acme-challenge.nope.org", "v")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no Gandi"), "{err}");
    }
}
