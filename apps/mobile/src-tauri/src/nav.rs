//! Loopback URLs for the tunnelled box UI and the WebView navigation
//! allow-list.

use peckboard_relay::tunnel::CookieGate;
use url::Url;

pub const LOOPBACK: &str = "127.0.0.1";

/// `http://127.0.0.1:<port>` — the box UI's origin inside the app.
pub fn origin(port: u16) -> String {
    format!("http://{LOOPBACK}:{port}")
}

/// First URL the WebView loads for a box: the device-side gate answers it
/// itself, sets the `__pbm` cookie, then replaces itself with `/`.
pub fn boot_url(port: u16, gate: &CookieGate) -> String {
    format!("{}{}", origin(port), gate.boot_path())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// http(s) elsewhere: hand to the system browser, never load in-app.
    OpenExternally,
    Block,
}

/// The WebView may only show the app shell and the active box's loopback
/// origin. `shell` is the shell UI's origin(s) (`tauri://localhost`,
/// `http(s)://tauri.localhost`, the dev server in debug builds).
pub fn decide(url: &Url, shell: &[Url], box_port: Option<u16>) -> Decision {
    if shell.iter().any(|s| same_origin(s, url)) {
        return Decision::Allow;
    }
    if url.scheme() == "http"
        && url.host_str() == Some(LOOPBACK)
        && box_port.is_some()
        && url.port() == box_port
    {
        return Decision::Allow;
    }
    match url.scheme() {
        "http" | "https"
            if url.host_str() != Some(LOOPBACK) && url.host_str() != Some("localhost") =>
        {
            Decision::OpenExternally
        }
        _ => Decision::Block,
    }
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn boot_url_targets_the_gate_on_loopback() {
        let gate = CookieGate::new();
        let url = u(&boot_url(41003, &gate));
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.port(), Some(41003));
        assert_eq!(url.path(), CookieGate::BOOT_PATH);
        let k: Vec<_> = url.query_pairs().filter(|(k, _)| k == "k").collect();
        assert_eq!(k.len(), 1);
        assert_eq!(k[0].1, gate.key());
        assert_eq!(origin(41003), "http://127.0.0.1:41003");
    }

    #[test]
    fn allow_list() {
        let shell = [u("tauri://localhost"), u("http://tauri.localhost")];
        let port = Some(41000);
        assert_eq!(
            decide(&u("tauri://localhost/index.html"), &shell, port),
            Decision::Allow
        );
        assert_eq!(
            decide(&u("http://tauri.localhost/"), &shell, port),
            Decision::Allow
        );
        assert_eq!(
            decide(&u("http://127.0.0.1:41000/__pbm/boot?k=x"), &shell, port),
            Decision::Allow
        );
        assert_eq!(
            decide(&u("http://127.0.0.1:41000/ws"), &shell, port),
            Decision::Allow
        );
        assert_eq!(
            decide(&u("http://127.0.0.1:8080/"), &shell, port),
            Decision::Block
        );
        assert_eq!(
            decide(&u("http://127.0.0.1:41000/"), &shell, None),
            Decision::Block
        );
        assert_eq!(
            decide(&u("https://127.0.0.1:41000/"), &shell, port),
            Decision::Block
        );
        assert_eq!(
            decide(&u("http://localhost:41000/"), &shell, port),
            Decision::Block
        );
        assert_eq!(
            decide(&u("https://tauri.localhost/"), &shell, port),
            Decision::OpenExternally
        );
        assert_eq!(
            decide(&u("https://github.com/x"), &shell, port),
            Decision::OpenExternally
        );
        assert_eq!(
            decide(&u("file:///etc/passwd"), &shell, port),
            Decision::Block
        );
        assert_eq!(
            decide(&u("javascript:alert(1)"), &shell, port),
            Decision::Block
        );
    }
}
