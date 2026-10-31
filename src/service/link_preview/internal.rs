//! Links that point at this Peckboard instance. They are never fetched: the
//! preview names the entity (`{entity, id}`) and the UI loads live data with
//! the viewer's own permissions. Route shapes mirror `parseRoute` in
//! `web/src/App.tsx`.

use reqwest::Url;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InternalLink {
    /// `session` | `project` | `card` | `report` | `view` | `page`.
    pub entity: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The owning project, for `card`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

/// The authorities this instance answers on: the request's `Host`, plus
/// loopback names, on this server's ports.
#[derive(Debug, Clone, Default)]
pub struct InstanceHosts {
    names: Vec<String>,
    ports: Vec<u16>,
}

impl InstanceHosts {
    pub fn new(host_header: Option<&str>, server_ports: &[u16]) -> Self {
        let mut names: Vec<String> = ["localhost", "127.0.0.1", "[::1]"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut ports = server_ports.to_vec();
        if let Some(h) = host_header
            && let Ok(u) = Url::parse(&format!("http://{}/", h.trim()))
            && let Some(name) = u.host_str()
        {
            let name = name.to_ascii_lowercase();
            if !names.contains(&name) {
                names.push(name);
            }
            match u.port() {
                Some(p) => ports.push(p),
                // Portless Host ⇒ a reverse proxy on the default ports.
                None => ports.extend([80, 443]),
            }
        }
        Self { names, ports }
    }

    pub fn is_ours(&self, url: &Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        let host = host.to_ascii_lowercase();
        self.names.contains(&host)
            && url
                .port_or_known_default()
                .is_some_and(|p| self.ports.contains(&p))
    }
}

fn safe_id(s: &str) -> Option<String> {
    (!s.is_empty()
        && s.len() <= 200
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '%')))
    .then(|| s.to_string())
}

/// Classify a same-instance URL by its SPA route.
pub fn classify(url: &Url) -> InternalLink {
    let segs: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();
    let query_card = url
        .query_pairs()
        .find(|(k, _)| k == "card")
        .and_then(|(_, v)| safe_id(&v));
    let page = || InternalLink {
        entity: "page",
        id: None,
        project_id: None,
    };
    let one = |entity: &'static str, id: Option<&&str>| match id.and_then(|s| safe_id(s)) {
        Some(id) => InternalLink {
            entity,
            id: Some(id),
            project_id: None,
        },
        None => page(),
    };
    match segs.first().copied() {
        Some("sessions") => one("session", segs.get(1)),
        Some("projects") => {
            let pid = segs.get(1).and_then(|s| safe_id(s));
            let card = match (segs.get(2), segs.get(3)) {
                (Some(&"cards"), Some(cid)) => safe_id(cid),
                _ => query_card,
            };
            match (pid, card) {
                (Some(pid), Some(cid)) => InternalLink {
                    entity: "card",
                    id: Some(cid),
                    project_id: Some(pid),
                },
                (Some(pid), None) => InternalLink {
                    entity: "project",
                    id: Some(pid),
                    project_id: None,
                },
                _ => page(),
            }
        }
        Some("reports") => match (
            segs.get(1).and_then(|s| safe_id(s)),
            segs.get(2).and_then(|s| safe_id(s)),
        ) {
            (Some(folder), Some(file)) => InternalLink {
                entity: "report",
                id: Some(format!("{folder}/{file}")),
                project_id: None,
            },
            _ => page(),
        },
        Some("views") => one("view", segs.get(1)),
        _ => page(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn same_instance_detection_uses_host_and_ports() {
        let hosts = InstanceHosts::new(Some("board.lan:3344"), &[3344, 3345]);
        assert!(hosts.is_ours(&url("http://board.lan:3344/sessions/a")));
        assert!(hosts.is_ours(&url("https://BOARD.lan:3345/projects/p")));
        assert!(hosts.is_ours(&url("http://localhost:3344/")));
        assert!(!hosts.is_ours(&url("http://board.lan:8080/sessions/a")));
        assert!(!hosts.is_ours(&url("http://other.lan:3344/sessions/a")));
        // Behind a reverse proxy the Host has no port.
        let proxied = InstanceHosts::new(Some("peck.example.com"), &[3344, 3345]);
        assert!(proxied.is_ours(&url("https://peck.example.com/views/v1")));
    }

    #[test]
    fn routes_map_to_entities() {
        let c = |s: &str| classify(&url(s));
        assert_eq!(
            c("http://h/sessions/abc-123/todos"),
            InternalLink {
                entity: "session",
                id: Some("abc-123".into()),
                project_id: None
            }
        );
        assert_eq!(c("http://h/projects/p1").entity, "project");
        let card = c("http://h/projects/p1/cards/c9");
        assert_eq!(
            (card.entity, card.id.as_deref(), card.project_id.as_deref()),
            ("card", Some("c9"), Some("p1"))
        );
        let card = c("http://h/projects/p1?card=c7");
        assert_eq!((card.entity, card.id.as_deref()), ("card", Some("c7")));
        let report = c("http://h/reports/fold%20er/notes.md");
        assert_eq!(
            (report.entity, report.id.as_deref()),
            ("report", Some("fold%20er/notes.md"))
        );
        assert_eq!(c("http://h/views/v1").entity, "view");
        assert_eq!(c("http://h/settings/plugins").entity, "page");
        assert_eq!(c("http://h/sessions").entity, "page");
        // The URL parser leaves `'` raw in paths; ids never contain it.
        assert_eq!(c("http://h/sessions/a'b").entity, "page");
    }
}
