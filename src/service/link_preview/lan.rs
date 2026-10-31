//! Self-hosted / LAN apps (reachable only once the admin trusts the host):
//! recognise the app from its HTML so the card names it even when the page
//! carries no Open Graph tags (login screens, SPAs).
//!
//! Always a card + Open link, never an iframe: `frame-src` is one static
//! policy derived from the provider table (`security.rs`), and widening it
//! per response for admin-trusted hosts (which may be bare IPs / CIDRs CSP
//! can't express) would loosen it for every page of the app. A Grafana
//! `d-solo` panel link is therefore a card too.

/// `(id, display name, needles)`: any needle in the lower-cased head of the
/// page identifies the app. Most specific first.
const APPS: &[(&str, &str, &[&str])] = &[
    (
        "forgejo",
        "Forgejo",
        &["content=\"forgejo", "powered by forgejo"],
    ),
    ("gitea", "Gitea", &["powered by gitea", "self-hosted,gitea"]),
    (
        "jellyfin",
        "Jellyfin",
        &["<title>jellyfin</title>", "content=\"jellyfin\""],
    ),
    ("plex", "Plex", &["<title>plex</title>", "plex web"]),
    (
        "immich",
        "Immich",
        &["<title>immich</title>", "content=\"immich\""],
    ),
    (
        "nextcloud",
        "Nextcloud",
        &[
            "data-requesttoken",
            "oc_requesttoken",
            "- nextcloud</title>",
        ],
    ),
    (
        "grafana",
        "Grafana",
        &["grafanabootdata", "<title>grafana</title>"],
    ),
    (
        "home-assistant",
        "Home Assistant",
        &["<title>home assistant</title>", "home-assistant-main"],
    ),
    ("uptime-kuma", "Uptime Kuma", &["uptime kuma"]),
    (
        "proxmox",
        "Proxmox VE",
        &["proxmox virtual environment", "pve-manager"],
    ),
];

/// The self-hosted app serving `html`, as `(id, name)`.
pub(super) fn fingerprint(html: &str) -> Option<(&'static str, &'static str)> {
    let mut end = html.len().min(64 * 1024);
    while !html.is_char_boundary(end) {
        end -= 1;
    }
    let head = html[..end].to_ascii_lowercase();
    APPS.iter()
        .find(|(_, _, needles)| needles.iter().any(|n| head.contains(n)))
        .map(|(id, name, _)| (*id, *name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apps_are_recognised() {
        assert_eq!(
            fingerprint("<html><head><title>Jellyfin</title>"),
            Some(("jellyfin", "Jellyfin"))
        );
        assert_eq!(
            fingerprint("<script>window.grafanaBootData = {}</script>"),
            Some(("grafana", "Grafana"))
        );
        assert_eq!(
            fingerprint("<footer>Powered by Forgejo</footer> gitea"),
            Some(("forgejo", "Forgejo"))
        );
        assert_eq!(fingerprint("<title>My blog</title>"), None);
    }
}
