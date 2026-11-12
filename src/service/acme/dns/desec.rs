//! deSEC (desec.io) via its v1 API with an API token. TXT values live in
//! one RRset per name, so create/delete read it and write it back with
//! the value added or removed.

use super::common::{json, longest_zone, relative_name, send, unquote};
use super::{DnsProvider, TxtRecord};
use async_trait::async_trait;
use serde_json::json;

const VENDOR: &str = "deSEC";
pub const BASE_URL: &str = "https://desec.io/api/v1";
/// deSEC's minimum TTL.
const TTL: u32 = 3600;

pub struct Desec {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Desec {
    pub fn new(http: reqwest::Client, token: &str, base: &str) -> Self {
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("Authorization", format!("Token {}", self.token))
    }

    async fn zone(&self, fqdn: &str) -> anyhow::Result<String> {
        let req = self.auth(self.http.get(format!("{}/domains/", self.base)));
        let (status, body) = send(req, VENDOR).await?;
        let v = json(VENDOR, status, &body)?;
        let names = v
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["name"].as_str());
        longest_zone(fqdn, names).ok_or_else(|| anyhow::anyhow!("no deSEC domain found for {fqdn}"))
    }

    /// Current TXT values (quoted, as stored) at `subname`.
    async fn values(&self, zone: &str, subname: &str) -> anyhow::Result<Vec<String>> {
        let sub = if subname.is_empty() { "@" } else { subname };
        let req = self.auth(
            self.http
                .get(format!("{}/domains/{zone}/rrsets/{sub}/TXT/", self.base)),
        );
        let (status, body) = send(req, VENDOR).await?;
        if status == 404 {
            return Ok(Vec::new());
        }
        let v = json(VENDOR, status, &body)?;
        Ok(v["records"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r.as_str().map(str::to_string))
            .collect())
    }

    /// Bulk PATCH creates, replaces, or (empty `records`) deletes the RRset.
    async fn write(&self, zone: &str, subname: &str, records: Vec<String>) -> anyhow::Result<()> {
        let req = self.auth(
            self.http
                .patch(format!("{}/domains/{zone}/rrsets/", self.base))
                .json(
                    &json!([{ "subname": subname, "type": "TXT", "ttl": TTL, "records": records }]),
                ),
        );
        let (status, body) = send(req, VENDOR).await?;
        json(VENDOR, status, &body).map(|_| ())
    }
}

#[async_trait]
impl DnsProvider for Desec {
    async fn create_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<TxtRecord> {
        let zone = self.zone(fqdn).await?;
        let sub = relative_name(fqdn, &zone);
        let mut records = self.values(&zone, &sub).await?;
        if !records.iter().any(|r| unquote(r) == value) {
            records.push(format!("\"{value}\""));
            self.write(&zone, &sub, records).await?;
        }
        Ok(TxtRecord {
            fqdn: fqdn.to_string(),
            value: value.to_string(),
            id: zone,
        })
    }

    async fn delete_txt(&self, record: &TxtRecord) -> anyhow::Result<()> {
        let zone = &record.id;
        let sub = relative_name(&record.fqdn, zone);
        let records = self.values(zone, &sub).await?;
        let kept: Vec<String> = records
            .iter()
            .filter(|r| unquote(r) != record.value)
            .cloned()
            .collect();
        if kept.len() == records.len() {
            return Ok(());
        }
        self.write(zone, &sub, kept).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::common::testsrv::serve;
    use super::*;

    #[tokio::test]
    async fn keeps_existing_values_in_rrset() {
        let (base, log) = serve(|r| match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/domains/") => (
                200,
                r#"[{"name":"example.com"},{"name":"dedyn.io"}]"#.into(),
            ),
            ("GET", "/domains/example.com/rrsets/_acme-challenge/TXT/") => (
                200,
                r#"{"subname":"_acme-challenge","type":"TXT","records":["\"old\""],"ttl":3600}"#
                    .into(),
            ),
            ("PATCH", "/domains/example.com/rrsets/") => (200, "[]".into()),
            _ => (403, r#"{"detail":"Invalid token."}"#.into()),
        })
        .await;
        let p = Desec::new(crate::http_client::client(), "tok", &base);
        let rec = p
            .create_txt("_acme-challenge.example.com", "new")
            .await
            .unwrap();
        assert_eq!(rec.id, "example.com");

        {
            let log = log.lock().unwrap();
            let patch = log.iter().find(|r| r.method == "PATCH").unwrap();
            assert_eq!(patch.header("authorization"), "Token tok");
            assert_eq!(patch.json()[0]["subname"], "_acme-challenge");
            assert_eq!(patch.json()[0]["records"], json!(["\"old\"", "\"new\""]));
        }

        // The mock still reports only "old", so removing "new" is a no-op
        // and removing "old" writes an empty RRset.
        p.delete_txt(&TxtRecord {
            value: "old".into(),
            ..rec
        })
        .await
        .unwrap();
        let log = log.lock().unwrap();
        let last = log.iter().rev().find(|r| r.method == "PATCH").unwrap();
        assert_eq!(last.json()[0]["records"], json!([]));
    }
}
