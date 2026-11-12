//! Porkbun DNS via the v3 JSON API (API key + secret API key; "API
//! Access" must be enabled on each domain).

use super::common::{api_error, json, longest_zone, relative_name, send};
use super::{DnsProvider, TxtRecord};
use async_trait::async_trait;
use serde_json::{Value, json};

const VENDOR: &str = "Porkbun";
pub const BASE_URL: &str = "https://api.porkbun.com/api/json/v3";

pub struct Porkbun {
    http: reqwest::Client,
    base: String,
    api_key: String,
    secret_key: String,
}

impl Porkbun {
    pub fn new(http: reqwest::Client, api_key: &str, secret_key: &str, base: &str) -> Self {
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            secret_key: secret_key.to_string(),
        }
    }

    /// Every Porkbun call is a POST carrying the keys in the body.
    async fn call(&self, path: &str, mut body: Value) -> anyhow::Result<(u16, Value, String)> {
        body["apikey"] = json!(self.api_key);
        body["secretapikey"] = json!(self.secret_key);
        let req = self.http.post(format!("{}{path}", self.base)).json(&body);
        let (status, text) = send(req, VENDOR).await?;
        let v = json(VENDOR, status, &text)?;
        if v["status"].as_str() != Some("SUCCESS") {
            return Err(api_error(VENDOR, status, &text));
        }
        Ok((status, v, text))
    }

    async fn zone(&self, fqdn: &str) -> anyhow::Result<String> {
        let (_, v, _) = self
            .call("/domain/listAll", json!({ "start": "0" }))
            .await?;
        let names = v["domains"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d["domain"].as_str());
        longest_zone(fqdn, names)
            .ok_or_else(|| anyhow::anyhow!("no Porkbun domain found for {fqdn}"))
    }
}

#[async_trait]
impl DnsProvider for Porkbun {
    async fn create_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<TxtRecord> {
        let zone = self.zone(fqdn).await?;
        let body = json!({
            "name": relative_name(fqdn, &zone), "type": "TXT", "content": value, "ttl": "600"
        });
        let (_, v, _) = self.call(&format!("/dns/create/{zone}"), body).await?;
        let id = match &v["id"] {
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            _ => anyhow::bail!("Porkbun API returned no record id"),
        };
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
            .ok_or_else(|| anyhow::anyhow!("malformed Porkbun record id '{}'", record.id))?;
        match self
            .call(&format!("/dns/delete/{zone}/{id}"), json!({}))
            .await
        {
            Ok(_) => Ok(()),
            // Porkbun reports an already-gone record as an invalid id.
            Err(e)
                if e.to_string()
                    .to_ascii_lowercase()
                    .contains("invalid record") =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::common::testsrv::serve;
    use super::*;

    #[tokio::test]
    async fn sends_keys_in_body_and_relative_name() {
        let (base, log) = serve(|r| match r.path.as_str() {
            "/domain/listAll" => (
                200,
                r#"{"status":"SUCCESS","domains":[{"domain":"example.com"}]}"#.into(),
            ),
            "/dns/create/example.com" => (200, r#"{"status":"SUCCESS","id":106926659}"#.into()),
            "/dns/delete/example.com/106926659" => (200, r#"{"status":"SUCCESS"}"#.into()),
            _ => (
                400,
                r#"{"status":"ERROR","message":"Invalid API key."}"#.into(),
            ),
        })
        .await;
        let p = Porkbun::new(crate::http_client::client(), "pk1_x", "sk1_y", &base);
        let rec = p
            .create_txt("_acme-challenge.www.example.com", "v1")
            .await
            .unwrap();
        assert_eq!(rec.id, "example.com/106926659");
        p.delete_txt(&rec).await.unwrap();

        let log = log.lock().unwrap();
        let create = log
            .iter()
            .find(|r| r.path.starts_with("/dns/create"))
            .unwrap();
        let body = create.json();
        assert_eq!(body["apikey"], "pk1_x");
        assert_eq!(body["secretapikey"], "sk1_y");
        assert_eq!(body["name"], "_acme-challenge.www");
        assert_eq!(body["content"], "v1");
        drop(log);

        let err = p
            .delete_txt(&TxtRecord {
                fqdn: "x".into(),
                value: "v".into(),
                id: "a.org/1".into(),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid API key"), "{err}");
    }
}
