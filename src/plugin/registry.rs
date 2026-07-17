//! The plugin registry client: fetch the static index of installable
//! WASM plugins and download a chosen one with SHA-256 verification.
//!
//! The registry is a single static `registry.json` (repo
//! `PeckBoard/plugins`) — core never talks to a service. It fetches the
//! index, and on install downloads the plugin's `url` and checks the bytes
//! against the index's `sha256` before the [`super::manager::PluginManager`]
//! persists and loads them. The downloaded plugin still loads **inert** and
//! goes through the per-plugin hook-approval gate, so the checksum only
//! guards integrity-against-the-index; capability is guarded by approval.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Refuse to buffer a plugin download larger than this. Mirrors the cap in
/// the registry's own validator. Real plugins are well under a megabyte.
const DOWNLOAD_CAP: u64 = 64 * 1024 * 1024;

/// An extra registry source injected via `PECKBOARD_PLUGIN_REGISTRY_URL`.
/// Always included in the aggregate alongside the operator's configured
/// repositories, and not removable through the UI — it's a dev/ops
/// override (e.g. pointing at a local registry server). `None` when unset.
pub fn env_repository() -> Option<(String, String)> {
    std::env::var("PECKBOARD_PLUGIN_REGISTRY_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .map(|url| ("(environment)".to_string(), url))
}

/// Resolve operator input on the Repositories tab to a `(label, url)`:
/// a bare `owner/repo` slug becomes the GitHub raw `registry.json` on the
/// default branch; an `http(s)://` value is used verbatim. Anything else
/// is rejected. `label` preserves what the operator typed.
pub fn resolve_repo_input(input: &str) -> anyhow::Result<(String, String)> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        anyhow::bail!("empty repository");
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Ok((trimmed.to_string(), trimmed.to_string()));
    }
    // owner/repo — exactly one slash, each side a safe slug.
    let parts: Vec<&str> = trimmed.split('/').collect();
    let is_slug = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    };
    if parts.len() == 2 && is_slug(parts[0]) && is_slug(parts[1]) {
        let url = format!(
            "https://raw.githubusercontent.com/{}/{}/main/registry.json",
            parts[0], parts[1]
        );
        return Ok((trimmed.to_string(), url));
    }
    anyhow::bail!("expected an `owner/repo` slug or an https:// URL")
}

/// The parsed `registry.json` index.
#[derive(Debug, Clone, Deserialize)]
pub struct RegistryIndex {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub plugins: Vec<RegistryEntry>,
    /// Installable MCP server templates (Settings → MCP Servers entries with
    /// one-click add). Older cores ignore this field entirely.
    #[serde(default)]
    pub mcp_servers: Vec<McpRegistryEntry>,
}

/// One installable plugin in the index. Mirrors the registry schema; extra
/// fields are ignored so the index can add metadata without breaking older
/// clients.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RegistryEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub author: String,
    #[serde(default)]
    pub homepage: Option<String>,
    pub version: String,
    pub hooks: Vec<String>,
    pub url: String,
    pub sha256: String,
    /// Optional minimum Peckboard version this plugin supports (semver). The
    /// install/upgrade is refused, and the UI gates the button, when the
    /// running Peckboard is older. Absent ⇒ no floor declared ⇒ compatible.
    #[serde(default)]
    pub min_peckboard: Option<String>,
    /// Freeform discovery tags (kebab-case) for registry search.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Curated category (e.g. `dev-tools`, `infrastructure`); optional.
    #[serde(default)]
    pub category: Option<String>,
}

/// One key/value row in an MCP server template (env var or header). Secret
/// values ship empty — the editor prompts the user to fill them.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RegistryKv {
    pub key: String,
    #[serde(default)]
    pub value: String,
}

/// One installable MCP server template in the index. Mirrors the
/// Settings → MCP Servers editor shape so the UI can prefill the add-server
/// modal directly; nothing is downloaded — "installing" just saves a user
/// MCP server. `id` doubles as the suggested `mcpServers` key.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpRegistryEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub homepage: Option<String>,
    /// `stdio` | `http` | `sse` — same values the editor accepts.
    pub transport: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<RegistryKv>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub headers: Vec<RegistryKv>,
    /// Preset URL choices (regions/variants) rendered as a dropdown in the
    /// editor (e.g. Datadog sites); picking one sets `url`.
    #[serde(default)]
    pub url_options: Vec<crate::service::mcp_server::user_servers::UrlOption>,
    /// Editor hint shown on add (e.g. where to create the API key).
    #[serde(default)]
    pub setup_note: Option<String>,
    /// OAuth sign-in template (http/sse transports). Present — even empty
    /// `{}` — means the server authenticates with OAuth: the UI offers
    /// "Sign in" instead of a manual Authorization header, and any missing
    /// endpoint/client data is discovered from the server's `.well-known`
    /// metadata at login (see `service::mcp_server::oauth`).
    #[serde(default)]
    pub oauth: Option<crate::service::mcp_server::user_servers::McpOauthConfig>,
    /// Optional human install steps for the host binary, shown when the
    /// `command` is missing on the Peckboard host (stdio transport only).
    #[serde(default)]
    pub install: Vec<String>,
    pub tags: Vec<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub min_peckboard: Option<String>,
}

/// The running Peckboard version — the real release tag (e.g. `0.0.19`),
/// stamped at build time by `build.rs` from `git describe`. NOT
/// `CARGO_PKG_VERSION`: `Cargo.toml` (`0.1.0`) drifted from the git-tag
/// release line (`0.0.x`), and the compatibility check must compare against
/// the version users actually run. See `build.rs::stamp_version`.
pub fn peckboard_version() -> &'static str {
    env!("PECKBOARD_VERSION")
}

/// Whether the running Peckboard satisfies a registry entry's `min_peckboard`
/// floor. No floor (or a blank one) ⇒ always compatible. Fail-open on an
/// unparseable version on *either* side: the floor is advisory metadata, and
/// the sha256 + hook-approval gates are the real guards — a typo in the index
/// must not be able to brick an otherwise-valid install.
pub fn is_compatible(running: &str, min_peckboard: Option<&str>) -> bool {
    let Some(min) = min_peckboard.map(str::trim).filter(|s| !s.is_empty()) else {
        return true;
    };
    match (semver::Version::parse(running), semver::Version::parse(min)) {
        (Ok(run), Ok(floor)) => run >= floor,
        _ => true,
    }
}

/// Whether `candidate` is a strictly newer semver than `installed` — i.e. an
/// upgrade is on offer. Unparseable on either side ⇒ no upgrade offered.
pub fn is_newer(candidate: &str, installed: &str) -> bool {
    match (
        semver::Version::parse(candidate),
        semver::Version::parse(installed),
    ) {
        (Ok(c), Ok(i)) => c > i,
        _ => false,
    }
}

/// Fetch and parse the registry index from `url`.
pub async fn fetch_index(client: &reqwest::Client, url: &str) -> anyhow::Result<RegistryIndex> {
    let resp = client.get(url).send().await?.error_for_status()?;
    let index: RegistryIndex = resp.json().await?;
    Ok(index)
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Whether `bytes` hash to `expected` (case-insensitive hex). This is an
/// integrity check, not an authentication check, so a plain compare is
/// fine — there is no secret to leak via timing.
pub fn checksum_matches(bytes: &[u8], expected: &str) -> bool {
    sha256_hex(bytes).eq_ignore_ascii_case(expected.trim())
}

/// Download `url` and verify it against `expected_sha256`. Rejects a
/// download whose advertised or actual size exceeds [`DOWNLOAD_CAP`], or
/// whose checksum doesn't match. Returns the verified bytes.
pub async fn download_and_verify(
    client: &reqwest::Client,
    url: &str,
    expected_sha256: &str,
) -> anyhow::Result<Vec<u8>> {
    let resp = client.get(url).send().await?.error_for_status()?;
    if let Some(len) = resp.content_length()
        && len > DOWNLOAD_CAP
    {
        anyhow::bail!("plugin download is {len} bytes, over the {DOWNLOAD_CAP}-byte cap");
    }
    let bytes = resp.bytes().await?;
    if bytes.len() as u64 > DOWNLOAD_CAP {
        anyhow::bail!(
            "plugin download is {} bytes, over the {DOWNLOAD_CAP}-byte cap",
            bytes.len()
        );
    }
    if !checksum_matches(&bytes, expected_sha256) {
        anyhow::bail!(
            "checksum mismatch: expected {expected_sha256}, got {}",
            sha256_hex(&bytes)
        );
    }
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_matches_is_case_insensitive_and_exact() {
        // sha256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        let bytes = b"hello";
        let upper = "2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824";
        assert!(checksum_matches(bytes, &sha256_hex(bytes)));
        assert!(checksum_matches(bytes, upper)); // case-insensitive
        assert!(!checksum_matches(bytes, "deadbeef")); // wrong
        assert!(!checksum_matches(b"world", &sha256_hex(bytes))); // wrong bytes
    }

    #[test]
    fn peckboard_version_is_stamped_not_blank() {
        // build.rs stamps PECKBOARD_VERSION from the git tag; it must never be
        // empty (env! would fail to compile) and, in this repo, must not be the
        // drifted Cargo.toml value that the bug reported.
        let v = peckboard_version();
        assert!(!v.is_empty());
        assert_ne!(
            v, "0.1.0",
            "peckboard_version must come from the git tag, not Cargo.toml"
        );
    }

    #[test]
    fn is_compatible_respects_min_floor_and_fails_open() {
        // No floor declared → always compatible.
        assert!(is_compatible("0.1.0", None));
        assert!(is_compatible("0.1.0", Some("   ")));
        // Running meets or exceeds the floor.
        assert!(is_compatible("0.2.0", Some("0.2.0")));
        assert!(is_compatible("0.3.0", Some("0.2.0")));
        // Running is below the floor → incompatible.
        assert!(!is_compatible("0.1.0", Some("0.2.0")));
        // Fail-open: an unparseable floor (or running version) is ignored
        // rather than bricking an otherwise-valid install.
        assert!(is_compatible("0.1.0", Some("not-a-version")));
        assert!(is_compatible("dev", Some("0.2.0")));
    }

    #[test]
    fn is_newer_only_on_strict_semver_increase() {
        assert!(is_newer("0.2.1", "0.2.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.2.0", "0.2.0")); // same version → no upgrade
        assert!(!is_newer("0.1.0", "0.2.0")); // older → no downgrade offer
        assert!(!is_newer("garbage", "0.2.0")); // unparseable → no offer
    }

    #[test]
    fn min_peckboard_is_optional_in_the_index() {
        // An entry without `min_peckboard` parses (backward compatible) and
        // reads as `None`; a present value round-trips.
        let without: RegistryEntry = serde_json::from_value(serde_json::json!({
            "id": "x", "name": "X", "description": "d", "author": "a",
            "version": "1.0.0", "hooks": ["h"], "url": "https://e/x.wasm",
            "sha256": "00",
        }))
        .unwrap();
        assert_eq!(without.min_peckboard, None);

        let with: RegistryEntry = serde_json::from_value(serde_json::json!({
            "id": "x", "name": "X", "description": "d", "author": "a",
            "version": "1.0.0", "hooks": ["h"], "url": "https://e/x.wasm",
            "sha256": "00", "min_peckboard": "0.2.0",
        }))
        .unwrap();
        assert_eq!(with.min_peckboard.as_deref(), Some("0.2.0"));
    }

    #[test]
    fn resolve_repo_input_handles_slug_and_url() {
        // owner/repo slug → GitHub raw registry.json on default branch.
        let (label, url) = resolve_repo_input("PeckBoard/plugins").unwrap();
        assert_eq!(label, "PeckBoard/plugins");
        assert_eq!(
            url,
            "https://raw.githubusercontent.com/PeckBoard/plugins/main/registry.json"
        );

        // Full URL used verbatim (http allowed for local dev servers).
        let (label, url) = resolve_repo_input("  https://example.com/r.json  ").unwrap();
        assert_eq!(label, "https://example.com/r.json");
        assert_eq!(url, "https://example.com/r.json");
        assert_eq!(
            resolve_repo_input("http://127.0.0.1:3398/registry.json")
                .unwrap()
                .1,
            "http://127.0.0.1:3398/registry.json"
        );

        // Rejected: empty, bare word, too many path parts, ftp scheme.
        assert!(resolve_repo_input("").is_err());
        assert!(resolve_repo_input("notaslug").is_err());
        assert!(resolve_repo_input("a/b/c").is_err());
        assert!(resolve_repo_input("ftp://x/y").is_err());
    }

    #[test]
    fn index_parse_ignores_unknown_fields() {
        let json = r#"{
            "schema_version": 1,
            "extra_top": true,
            "plugins": [{
                "id": "api", "name": "API", "description": "d", "author": "PeckBoard",
                "version": "0.2.0", "hooks": ["http.request.before"],
                "url": "https://example.com/api.wasm",
                "sha256": "4ecb2ee49c3d85c323556f191f6d7fa3a5a0ec8ea9371daa952f17d577c86df2",
                "future_field": "ignored"
            }]
        }"#;
        let index: RegistryIndex = serde_json::from_str(json).unwrap();
        assert_eq!(index.schema_version, 1);
        assert_eq!(index.plugins.len(), 1);
        let e = &index.plugins[0];
        assert_eq!(e.id, "api");
        assert_eq!(e.hooks, vec!["http.request.before"]);
        assert!(e.homepage.is_none());
    }

    #[test]
    fn mcp_servers_and_tags_parse_and_default() {
        let json = r#"{
            "schema_version": 1,
            "plugins": [{
                "id": "api", "name": "API", "description": "d", "author": "PeckBoard",
                "version": "0.2.0", "hooks": [], "url": "https://e/a.wasm", "sha256": "00",
                "tags": ["http-api", "integrations"], "category": "integrations"
            }],
            "mcp_servers": [{
                "id": "playwright", "name": "Playwright", "description": "Browser automation",
                "transport": "stdio", "command": "npx",
                "args": ["-y", "@playwright/mcp@latest"],
                "tags": ["browser", "testing"], "category": "dev-tools"
            }]
        }"#;
        let index: RegistryIndex = serde_json::from_str(json).unwrap();
        assert_eq!(index.plugins[0].tags, vec!["http-api", "integrations"]);
        assert_eq!(index.plugins[0].category.as_deref(), Some("integrations"));
        let m = &index.mcp_servers[0];
        assert_eq!(m.id, "playwright");
        assert_eq!(m.transport, "stdio");
        assert_eq!(m.command, "npx");
        assert_eq!(m.args[1], "@playwright/mcp@latest");
        assert!(m.env.is_empty() && m.headers.is_empty());
        assert_eq!(m.category.as_deref(), Some("dev-tools"));

        // An index without the new fields still parses (older registries).
        let bare: RegistryIndex =
            serde_json::from_str(r#"{"schema_version":1,"plugins":[]}"#).unwrap();
        assert!(bare.mcp_servers.is_empty());
    }
}
