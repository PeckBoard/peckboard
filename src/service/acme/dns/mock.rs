//! In-process DNS provider for tests. Only offered when
//! `PECKBOARD_ACME_TEST=1`; records live in a process-global list that
//! [`records`] exposes.

use super::{DnsProvider, TxtRecord};
use async_trait::async_trait;
use std::sync::Mutex;

static RECORDS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Whether test mode is on, which is what makes `mock` selectable.
pub fn enabled() -> bool {
    std::env::var("PECKBOARD_ACME_TEST").as_deref() == Ok("1")
}

/// Every TXT record currently published, as `(fqdn, value)`.
pub fn records() -> Vec<(String, String)> {
    RECORDS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub struct Mock;

#[async_trait]
impl DnsProvider for Mock {
    async fn create_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<TxtRecord> {
        let mut records = RECORDS.lock().unwrap_or_else(|e| e.into_inner());
        let entry = (fqdn.to_string(), value.to_string());
        if !records.contains(&entry) {
            records.push(entry);
        }
        Ok(TxtRecord {
            fqdn: fqdn.to_string(),
            value: value.to_string(),
            id: value.to_string(),
        })
    }

    async fn delete_txt(&self, record: &TxtRecord) -> anyhow::Result<()> {
        RECORDS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(f, v)| !(f == &record.fqdn && v == &record.value));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publishes_and_removes() {
        let fqdn = "_acme-challenge.mock-unit.example";
        let mine = || -> Vec<String> {
            records()
                .into_iter()
                .filter(|(f, _)| f == fqdn)
                .map(|(_, v)| v)
                .collect()
        };
        let a = Mock.create_txt(fqdn, "a").await.unwrap();
        Mock.create_txt(fqdn, "b").await.unwrap();
        assert_eq!(mine(), ["a", "b"]);
        Mock.delete_txt(&a).await.unwrap();
        assert_eq!(mine(), ["b"]);
    }
}
