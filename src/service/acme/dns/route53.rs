//! Amazon Route 53 via the 2013-04-01 REST API, signed with AWS
//! Signature Version 4 (HMAC-SHA256 implemented over `sha2`). TXT values
//! live in one record set per name, so create/delete read it and UPSERT
//! it back with the value added or removed.

use super::common::{candidates, normalize, unquote};
use super::{DnsProvider, TxtRecord};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::time::Duration;

const VENDOR: &str = "Route 53";
pub const BASE_URL: &str = "https://route53.amazonaws.com";
pub const DEFAULT_REGION: &str = "us-east-1";
const API: &str = "/2013-04-01";
const TTL: u32 = 60;

pub struct Route53 {
    http: reqwest::Client,
    base: String,
    access_key: String,
    secret_key: String,
    region: String,
    /// Delay between GetChange polls while waiting for INSYNC.
    poll: Duration,
}

impl Route53 {
    pub fn new(
        http: reqwest::Client,
        access_key: &str,
        secret_key: &str,
        region: &str,
        base: &str,
    ) -> Self {
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            access_key: access_key.to_string(),
            secret_key: secret_key.to_string(),
            region: region.to_string(),
            poll: Duration::from_secs(2),
        }
    }

    /// One signed request. `query` is sorted and encoded here, so the URL
    /// query and the canonical query string are the same bytes.
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<String>,
    ) -> anyhow::Result<String> {
        let mut pairs: Vec<(String, String)> = query
            .iter()
            .map(|(k, v)| {
                (
                    urlencoding::encode(k).into_owned(),
                    urlencoding::encode(v).into_owned(),
                )
            })
            .collect();
        pairs.sort();
        let qs = pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let url = if qs.is_empty() {
            format!("{}{path}", self.base)
        } else {
            format!("{}{path}?{qs}", self.base)
        };
        let parsed = reqwest::Url::parse(&url)?;
        let host = match parsed.port() {
            Some(p) => format!("{}:{p}", parsed.host_str().unwrap_or_default()),
            None => parsed.host_str().unwrap_or_default().to_string(),
        };
        let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let payload = body.clone().unwrap_or_default();
        let auth = authorization(
            &SigningKey {
                access_key: &self.access_key,
                secret_key: &self.secret_key,
                region: &self.region,
                service: "route53",
            },
            method.as_str(),
            &host,
            path,
            &qs,
            payload.as_bytes(),
            &amz_date,
        );
        let mut req = self
            .http
            .request(method, parsed)
            .header("x-amz-date", &amz_date)
            .header("authorization", auth);
        if let Some(body) = body {
            req = req.header("content-type", "text/xml").body(body);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("{VENDOR} API request failed: {}", e.without_url()))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            let code = tag(&text, "Code").unwrap_or_default();
            let msg = tag(&text, "Message").unwrap_or_else(|| text.trim().to_string());
            anyhow::bail!("{VENDOR} API error (HTTP {status}): {code}: {msg}");
        }
        Ok(text)
    }

    /// Id (without `/hostedzone/`) of the longest public zone owning `fqdn`.
    async fn zone_id(&self, fqdn: &str) -> anyhow::Result<String> {
        for name in candidates(fqdn) {
            let dns = format!("{name}.");
            let xml = self
                .call(
                    reqwest::Method::GET,
                    &format!("{API}/hostedzonesbyname"),
                    &[("dnsname", &dns), ("maxitems", "10")],
                    None,
                )
                .await?;
            for zone in blocks(&xml, "HostedZone") {
                let zname = tag(zone, "Name").map(|n| normalize(&n));
                let private = tag(zone, "PrivateZone").as_deref() == Some("true");
                if zname.as_deref() == Some(name.as_str()) && !private {
                    let id = tag(zone, "Id").unwrap_or_default();
                    return Ok(id.trim_start_matches("/hostedzone/").to_string());
                }
            }
        }
        anyhow::bail!("no public Route 53 hosted zone found for {fqdn}")
    }

    /// Current TXT values (quoted, as stored) and TTL at `fqdn`.
    async fn current(&self, zone: &str, fqdn: &str) -> anyhow::Result<(Vec<String>, u32)> {
        let name = format!("{}.", normalize(fqdn));
        let xml = self
            .call(
                reqwest::Method::GET,
                &format!("{API}/hostedzone/{zone}/rrset"),
                &[("name", &name), ("type", "TXT"), ("maxitems", "1")],
                None,
            )
            .await?;
        for set in blocks(&xml, "ResourceRecordSet") {
            let same_name = tag(set, "Name").map(|n| normalize(&n)) == Some(normalize(fqdn));
            if same_name && tag(set, "Type").as_deref() == Some("TXT") {
                let ttl = tag(set, "TTL").and_then(|t| t.parse().ok()).unwrap_or(TTL);
                let values = blocks(set, "Value").into_iter().map(unescape).collect();
                return Ok((values, ttl));
            }
        }
        Ok((Vec::new(), TTL))
    }

    async fn change(
        &self,
        zone: &str,
        action: &str,
        fqdn: &str,
        values: &[String],
        ttl: u32,
    ) -> anyhow::Result<()> {
        let records: String = values
            .iter()
            .map(|v| {
                format!(
                    "<ResourceRecord><Value>{}</Value></ResourceRecord>",
                    escape(v)
                )
            })
            .collect();
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><ChangeResourceRecordSetsRequest xmlns="https://route53.amazonaws.com/doc/2013-04-01/"><ChangeBatch><Changes><Change><Action>{action}</Action><ResourceRecordSet><Name>{}.</Name><Type>TXT</Type><TTL>{ttl}</TTL><ResourceRecords>{records}</ResourceRecords></ResourceRecordSet></Change></Changes></ChangeBatch></ChangeResourceRecordSetsRequest>"#,
            escape(&normalize(fqdn)),
        );
        let xml = self
            .call(
                reqwest::Method::POST,
                &format!("{API}/hostedzone/{zone}/rrset"),
                &[],
                Some(body),
            )
            .await?;
        // Wait (bounded) for the change to reach every Route 53 server.
        let Some(change) = tag(&xml, "Id") else {
            return Ok(());
        };
        let change = change.trim_start_matches("/change/").to_string();
        let mut status = tag(&xml, "Status").unwrap_or_default();
        for _ in 0..30 {
            if status == "INSYNC" {
                break;
            }
            tokio::time::sleep(self.poll).await;
            let xml = self
                .call(
                    reqwest::Method::GET,
                    &format!("{API}/change/{change}"),
                    &[],
                    None,
                )
                .await?;
            status = tag(&xml, "Status").unwrap_or_default();
        }
        Ok(())
    }
}

#[async_trait]
impl DnsProvider for Route53 {
    async fn create_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<TxtRecord> {
        let zone = self.zone_id(fqdn).await?;
        let (mut values, _) = self.current(&zone, fqdn).await?;
        if !values.iter().any(|v| unquote(v) == value) {
            values.push(format!("\"{value}\""));
            self.change(&zone, "UPSERT", fqdn, &values, TTL).await?;
        }
        Ok(TxtRecord {
            fqdn: fqdn.to_string(),
            value: value.to_string(),
            id: zone,
        })
    }

    async fn delete_txt(&self, record: &TxtRecord) -> anyhow::Result<()> {
        let zone = &record.id;
        let (values, ttl) = self.current(zone, &record.fqdn).await?;
        let kept: Vec<String> = values
            .iter()
            .filter(|v| unquote(v) != record.value)
            .cloned()
            .collect();
        if kept.len() == values.len() {
            return Ok(());
        }
        if kept.is_empty() {
            // DELETE must name the record set exactly as it stands.
            self.change(zone, "DELETE", &record.fqdn, &values, ttl)
                .await
        } else {
            self.change(zone, "UPSERT", &record.fqdn, &kept, ttl).await
        }
    }
}

/// Credentials scope for one SigV4 signature.
struct SigningKey<'a> {
    access_key: &'a str,
    secret_key: &'a str,
    region: &'a str,
    service: &'a str,
}

/// The SigV4 `Authorization` header over `host` + `x-amz-date`.
/// `query` must already be the canonical (sorted, encoded) query string.
fn authorization(
    key: &SigningKey<'_>,
    method: &str,
    host: &str,
    path: &str,
    query: &str,
    payload: &[u8],
    amz_date: &str,
) -> String {
    let date = &amz_date[..8];
    let canonical = format!(
        "{method}\n{path}\n{query}\nhost:{host}\nx-amz-date:{amz_date}\n\nhost;x-amz-date\n{}",
        hex::encode(Sha256::digest(payload))
    );
    let scope = format!("{date}/{}/{}/aws4_request", key.region, key.service);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical.as_bytes()))
    );
    let k = signing_key(key.secret_key, date, key.region, key.service);
    let signature = hex::encode(hmac_sha256(&k, to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders=host;x-amz-date, Signature={signature}",
        key.access_key
    )
}

fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> [u8; 32] {
    let k = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac_sha256(&k, region.as_bytes());
    let k = hmac_sha256(&k, service.as_bytes());
    hmac_sha256(&k, b"aws4_request")
}

/// HMAC-SHA256 per RFC 2104.
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(data);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(&inner);
    let mut out = [0u8; 32];
    out.copy_from_slice(&outer.finalize());
    out
}

/// Inner text of every `<name>…</name>` in `xml` (raw, not unescaped).
fn blocks<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else {
            break;
        };
        out.push(&after[..end]);
        rest = &after[end + close.len()..];
    }
    out
}

/// First `<name>` element's text, unescaped.
fn tag(xml: &str, name: &str) -> Option<String> {
    blocks(xml, name).first().map(|s| unescape(s))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::super::common::testsrv::serve;
    use super::*;

    #[test]
    fn hmac_and_sigv4_known_answers() {
        // RFC 4231 test case 2.
        assert_eq!(
            hex::encode(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // AWS docs: deriving the signing key.
        assert_eq!(
            hex::encode(signing_key(
                "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
                "20120215",
                "us-east-1",
                "iam"
            )),
            "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d"
        );
        // AWS SigV4 test suite: get-vanilla.
        let key = SigningKey {
            access_key: "AKIDEXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
            service: "service",
        };
        assert_eq!(
            authorization(
                &key,
                "GET",
                "example.amazonaws.com",
                "/",
                "",
                b"",
                "20150830T123600Z"
            ),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[tokio::test]
    async fn upserts_preserving_other_values() {
        let (base, log) = serve(|r| match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/2013-04-01/hostedzonesbyname") if r.query.contains("dnsname=example.com.") => (
                200,
                "<ListHostedZonesByNameResponse><HostedZones>\
                 <HostedZone><Id>/hostedzone/ZPRIV</Id><Name>example.com.</Name><Config><PrivateZone>true</PrivateZone></Config></HostedZone>\
                 <HostedZone><Id>/hostedzone/Z1</Id><Name>example.com.</Name><Config><PrivateZone>false</PrivateZone></Config></HostedZone>\
                 </HostedZones></ListHostedZonesByNameResponse>"
                    .into(),
            ),
            ("GET", "/2013-04-01/hostedzonesbyname") => (
                200,
                "<ListHostedZonesByNameResponse><HostedZones><HostedZone><Id>/hostedzone/Z9</Id><Name>zzz.org.</Name></HostedZone></HostedZones></ListHostedZonesByNameResponse>".into(),
            ),
            ("GET", "/2013-04-01/hostedzone/Z1/rrset") => (
                200,
                "<ListResourceRecordSetsResponse><ResourceRecordSets><ResourceRecordSet>\
                 <Name>_acme-challenge.example.com.</Name><Type>TXT</Type><TTL>300</TTL>\
                 <ResourceRecords><ResourceRecord><Value>\"old\"</Value></ResourceRecord></ResourceRecords>\
                 </ResourceRecordSet></ResourceRecordSets></ListResourceRecordSetsResponse>"
                    .into(),
            ),
            ("POST", "/2013-04-01/hostedzone/Z1/rrset") => (
                200,
                "<ChangeResourceRecordSetsResponse><ChangeInfo><Id>/change/C1</Id><Status>PENDING</Status></ChangeInfo></ChangeResourceRecordSetsResponse>".into(),
            ),
            ("GET", "/2013-04-01/change/C1") => (
                200,
                "<GetChangeResponse><ChangeInfo><Id>/change/C1</Id><Status>INSYNC</Status></ChangeInfo></GetChangeResponse>".into(),
            ),
            _ => (
                403,
                "<ErrorResponse><Error><Code>AccessDenied</Code><Message>not allowed</Message></Error></ErrorResponse>".into(),
            ),
        })
        .await;
        let mut p = Route53::new(
            crate::http_client::client(),
            "AKID",
            "secret",
            DEFAULT_REGION,
            &base,
        );
        p.poll = Duration::from_millis(1);
        // Deeper name: zone probing walks up to example.com; no record
        // set exists at this name yet, so it UPSERTs just the new value.
        let rec = p.create_txt("_acme-challenge.www.example.com", "new").await;
        assert_eq!(rec.unwrap().id, "Z1");
        let rec = p
            .create_txt("_acme-challenge.example.com", "new")
            .await
            .unwrap();
        p.delete_txt(&TxtRecord {
            value: "old".into(),
            ..rec
        })
        .await
        .unwrap();

        let log = log.lock().unwrap();
        let first = &log[0];
        assert!(
            first
                .header("authorization")
                .starts_with("AWS4-HMAC-SHA256 Credential=AKID/")
        );
        assert!(
            first
                .header("authorization")
                .contains("/us-east-1/route53/aws4_request")
        );
        assert!(!first.header("x-amz-date").is_empty());
        let posts: Vec<_> = log.iter().filter(|r| r.method == "POST").collect();
        assert!(posts[1].body.contains("<Action>UPSERT</Action>"));
        assert!(posts[1].body.contains("<Value>\"old\"</Value>"));
        assert!(posts[1].body.contains("<Value>\"new\"</Value>"));
        assert!(
            posts[1]
                .body
                .contains("<Name>_acme-challenge.example.com.</Name>")
        );
        // Fixture still lists only "old", so removing it DELETEs the set.
        assert!(posts[2].body.contains("<Action>DELETE</Action>"));
        assert!(posts[2].body.contains("<TTL>300</TTL>"));
        assert!(log.iter().any(|r| r.path == "/2013-04-01/change/C1"));
        drop(log);

        let err = p
            .delete_txt(&TxtRecord {
                fqdn: "a.example.com".into(),
                value: "v".into(),
                id: "ZX".into(),
            })
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("AccessDenied: not allowed"),
            "{err}"
        );
    }
}
