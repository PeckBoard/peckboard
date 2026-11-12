//! DNS-01 challenge providers for the Let's Encrypt client
//! (`service::acme`). Each provider publishes and removes one
//! `_acme-challenge` TXT record through its vendor's HTTP API, so
//! issuance needs no inbound port and no public exposure.
//!
//! Adding a provider: one file here implementing [`DnsProvider`], one
//! entry in [`catalog`], one arm in [`build`].

mod cloudflare;
mod common;
mod desec;
mod digitalocean;
mod duckdns;
mod gandi;
mod hetzner;
pub mod mock;
mod porkbun;
mod route53;

use async_trait::async_trait;
use serde::Serialize;
use std::collections::BTreeMap;

/// Credentials as entered in the UI: field key -> value. The keys a
/// provider reads are exactly the `key`s of its [`ProviderSpec::fields`].
pub type Credentials = BTreeMap<String, String>;

/// One input the UI renders for a provider.
#[derive(Debug, Clone, Serialize)]
pub struct CredentialField {
    pub key: &'static str,
    pub label: &'static str,
    /// Rendered as a password input and masked in API responses.
    pub secret: bool,
    pub required: bool,
    pub help: &'static str,
}

/// Static description of a provider, served to the UI so it can build
/// the "Add DNS provider" form without hard-coding vendors.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderSpec {
    /// Stable id stored in `dns_credentials.provider`.
    pub id: &'static str,
    pub name: &'static str,
    pub fields: &'static [CredentialField],
    /// Link to the vendor page where the token is created.
    pub docs_url: &'static str,
}

/// Handle to a created TXT record, returned by `create_txt` and handed
/// back to `delete_txt`. `id` is whatever the vendor needs to delete it
/// (record id, or the value itself for APIs that delete by content).
#[derive(Debug, Clone)]
pub struct TxtRecord {
    pub fqdn: String,
    pub value: String,
    pub id: String,
}

#[async_trait]
pub trait DnsProvider: Send + Sync {
    /// Publish a TXT record `fqdn` (e.g. `_acme-challenge.www.example.com`,
    /// no trailing dot) with `value`. Providers locate the owning zone
    /// themselves (longest matching suffix among the account's zones).
    /// Must not remove other TXT records at the same name — wildcard +
    /// apex certs need two values at once.
    async fn create_txt(&self, fqdn: &str, value: &str) -> anyhow::Result<TxtRecord>;

    /// Remove a record previously returned by `create_txt`. Missing
    /// record is success.
    async fn delete_txt(&self, record: &TxtRecord) -> anyhow::Result<()>;
}
const fn secret(key: &'static str, label: &'static str, help: &'static str) -> CredentialField {
    CredentialField {
        key,
        label,
        secret: true,
        required: true,
        help,
    }
}

const TOKEN_FIELDS: &[CredentialField] = &[secret(
    "api_token",
    "API token",
    "Token allowed to edit DNS records in the zone.",
)];

const CATALOG: &[ProviderSpec] = &[
    ProviderSpec {
        id: "cloudflare",
        name: "Cloudflare",
        fields: &[secret(
            "api_token",
            "API token",
            "Scoped API token with Zone:Read and DNS:Edit on the zone (not the Global API Key).",
        )],
        docs_url: "https://dash.cloudflare.com/profile/api-tokens",
    },
    ProviderSpec {
        id: "digitalocean",
        name: "DigitalOcean",
        fields: &[secret(
            "api_token",
            "API token",
            "Personal access token with write scope (or the domain:read + domain:create/delete scopes).",
        )],
        docs_url: "https://cloud.digitalocean.com/account/api/tokens",
    },
    ProviderSpec {
        id: "hetzner",
        name: "Hetzner DNS",
        fields: &[secret(
            "api_token",
            "API token",
            "Hetzner DNS Console API token (dns.hetzner.com), not a Hetzner Cloud token.",
        )],
        docs_url: "https://dns.hetzner.com/settings/api-token",
    },
    ProviderSpec {
        id: "porkbun",
        name: "Porkbun",
        fields: &[
            secret(
                "api_key",
                "API key",
                "Starts with pk1_. Enable API Access on the domain too.",
            ),
            secret("secret_api_key", "Secret API key", "Starts with sk1_."),
        ],
        docs_url: "https://porkbun.com/account/api",
    },
    ProviderSpec {
        id: "desec",
        name: "deSEC",
        fields: TOKEN_FIELDS,
        docs_url: "https://desec.io/tokens",
    },
    ProviderSpec {
        id: "gandi",
        name: "Gandi LiveDNS",
        fields: &[secret(
            "api_token",
            "Personal access token",
            "Gandi personal access token with the \"Manage domain name technical configurations\" permission.",
        )],
        docs_url: "https://admin.gandi.net/organizations/account/pat",
    },
    ProviderSpec {
        id: "duckdns",
        name: "DuckDNS",
        fields: &[secret(
            "token",
            "Token",
            "Account token. DuckDNS holds one TXT value per subdomain, so a name and its wildcard need separate certificates.",
        )],
        docs_url: "https://www.duckdns.org/",
    },
    ProviderSpec {
        id: "route53",
        name: "Amazon Route 53",
        fields: &[
            CredentialField {
                key: "access_key_id",
                label: "Access key ID",
                secret: false,
                required: true,
                help: "IAM user key with route53:ListHostedZonesByName, ListResourceRecordSets, ChangeResourceRecordSets and GetChange.",
            },
            secret(
                "secret_access_key",
                "Secret access key",
                "Secret for the access key ID.",
            ),
            CredentialField {
                key: "region",
                label: "Signing region",
                secret: false,
                required: false,
                help: "Leave blank for us-east-1 (Route 53 is global).",
            },
        ],
        docs_url: "https://console.aws.amazon.com/iam/home#/security_credentials",
    },
];

const MOCK: ProviderSpec = ProviderSpec {
    id: "mock",
    name: "Mock (test mode)",
    fields: &[secret(
        "token",
        "Token",
        "Any value; records stay in memory.",
    )],
    docs_url: "",
};

/// Every supported provider, in UI order.
pub fn catalog() -> Vec<ProviderSpec> {
    let mut out = CATALOG.to_vec();
    if mock::enabled() {
        out.push(MOCK);
    }
    out
}

/// Construct the provider `id` from stored credentials. Errors on an
/// unknown id or a missing required field (message names the field).
pub fn build(
    id: &str,
    creds: &Credentials,
    http: reqwest::Client,
) -> anyhow::Result<Box<dyn DnsProvider>> {
    use common::{optional, required};
    let provider: Box<dyn DnsProvider> = match id {
        "cloudflare" => Box::new(cloudflare::Cloudflare::new(
            http,
            required(creds, "api_token")?,
            cloudflare::BASE_URL,
        )),
        "digitalocean" => Box::new(digitalocean::DigitalOcean::new(
            http,
            required(creds, "api_token")?,
            digitalocean::BASE_URL,
        )),
        "hetzner" => Box::new(hetzner::Hetzner::new(
            http,
            required(creds, "api_token")?,
            hetzner::BASE_URL,
        )),
        "porkbun" => Box::new(porkbun::Porkbun::new(
            http,
            required(creds, "api_key")?,
            required(creds, "secret_api_key")?,
            porkbun::BASE_URL,
        )),
        "desec" => Box::new(desec::Desec::new(
            http,
            required(creds, "api_token")?,
            desec::BASE_URL,
        )),
        "gandi" => Box::new(gandi::Gandi::new(
            http,
            required(creds, "api_token")?,
            gandi::BASE_URL,
        )),
        "duckdns" => Box::new(duckdns::DuckDns::new(
            http,
            required(creds, "token")?,
            duckdns::BASE_URL,
        )),
        "route53" => Box::new(route53::Route53::new(
            http,
            required(creds, "access_key_id")?,
            required(creds, "secret_access_key")?,
            optional(creds, "region").unwrap_or(route53::DEFAULT_REGION),
            route53::BASE_URL,
        )),
        "mock" if mock::enabled() => {
            required(creds, "token")?;
            Box::new(mock::Mock)
        }
        _ => anyhow::bail!("unknown DNS provider '{id}'"),
    };
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_catalog_entry_builds_from_its_fields() {
        for spec in catalog() {
            let creds: Credentials = spec
                .fields
                .iter()
                .map(|f| (f.key.to_string(), "x".repeat(12)))
                .collect();
            assert!(
                build(spec.id, &creds, crate::http_client::client()).is_ok(),
                "{}",
                spec.id
            );
            if let Some(f) = spec.fields.iter().find(|f| f.required) {
                let mut missing = creds.clone();
                missing.remove(f.key);
                let err = build(spec.id, &missing, crate::http_client::client())
                    .err()
                    .unwrap();
                assert!(err.to_string().contains(f.key), "{err}");
            }
        }
        assert!(build("nope", &Credentials::new(), crate::http_client::client()).is_err());
    }
}
