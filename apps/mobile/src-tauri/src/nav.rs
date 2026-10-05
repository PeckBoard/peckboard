//! Loopback URLs for the tunnelled box UI and the WebView navigation
//! allow-list.

use peckboard_relay::tunnel::CookieGate;
use url::Url;

pub const LOOPBACK: &str = "127.0.0.1";

/// The shell UI's origin(s). Tauri serves the bundled shell from
/// `tauri://localhost` on iOS, macOS and Linux and from
/// `http://tauri.localhost` on Windows and Android (`http_scheme`). Only this
/// platform's origin counts: the other platform's is a plain web origin here
/// that nothing in the app serves, and a page there must never pass as the
/// shell. `dev` is the Vite dev server (debug builds only).
pub fn shell_origins(http_scheme: bool, dev: Option<Url>) -> Vec<Url> {
    let shell = if http_scheme {
        "http://tauri.localhost"
    } else {
        "tauri://localhost"
    };
    let mut out = vec![Url::parse(shell).expect("static shell origin")];
    out.extend(dev);
    out
}

/// Whether `url` is a shell page (same origin as one of `shell`).
pub fn is_shell(url: &Url, shell: &[Url]) -> bool {
    shell.iter().any(|s| same_origin(s, url))
}

/// `scheme://host[:port]` as `location.protocol + "//" + location.host`
/// yields it in the page (`Url::origin` serialises custom schemes as
/// "null").
pub fn origin_string(url: &Url) -> String {
    let mut s = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
    if let Some(p) = url.port() {
        s.push_str(&format!(":{p}"));
    }
    s
}

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
    if is_shell(url, shell) {
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

const BOXES_BUTTON_JS: &str = include_str!("boxes_button.js");

/// Sets the Boxes button's `data-relayed` (its "Relayed" badge); args:
/// box origin, relayed. Re-checks the origin like the button script.
const RELAY_BADGE_JS: &str = r#"(function (o, r) {
  if (location.origin !== o || window.top !== window) return;
  var b = document.getElementById("__pbm_boxes");
  if (b) b.setAttribute("data-relayed", r ? "1" : "0");
})"#;

/// A box page: the active box's loopback origin — never the shell, another
/// port or origin, nor the gate's boot page (it navigates on at once).
/// `localhost` never qualifies: [`decide`] only lets box pages load from
/// 127.0.0.1.
fn on_box_page(url: &Url, shell: &[Url], port: u16) -> bool {
    url.scheme() == "http"
        && url.host_str() == Some(LOOPBACK)
        && url.port() == Some(port)
        && !shell.iter().any(|s| same_origin(s, url))
        && url.path() != CookieGate::BOOT_PATH
}

fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// The app's "Boxes" button (back to the box list) for a page that finished
/// loading at `url`, or `None` unless it is a box page ([`on_box_page`]).
/// The script re-checks the origin when it runs and only navigates to
/// `home`; box pages get no IPC (the capabilities are `local`-only).
pub fn boxes_button_script(
    url: &Url,
    shell: &[Url],
    box_port: Option<u16>,
    home: &Url,
) -> Option<String> {
    let port = box_port?;
    on_box_page(url, shell, port).then(|| {
        BOXES_BUTTON_JS
            .replace("\"__ORIGIN__\"", &js_str(&origin(port)))
            .replace("\"__HOME__\"", &js_str(home.as_str()))
    })
}

/// Shows (`relayed`) or hides the Boxes button's "Relayed" badge on the box
/// page at `url`, or `None` unless it is one. The app evaluates it after
/// the button script and on every tunnel status, so the badge follows
/// direct ↔ relayed path changes without a reload — pushed in, never
/// asked for: box pages still get no IPC.
pub fn relay_badge_script(
    url: &Url,
    shell: &[Url],
    box_port: Option<u16>,
    relayed: bool,
) -> Option<String> {
    let port = box_port?;
    on_box_page(url, shell, port)
        .then(|| format!("{RELAY_BADGE_JS}({}, {relayed});", js_str(&origin(port))))
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

    #[test]
    fn boxes_button_only_on_the_active_box_page() {
        let shell = [u("tauri://localhost"), u("http://localhost:1420")];
        let home = u("tauri://localhost/");
        let port = Some(41000);
        let script = |url: &str, port| boxes_button_script(&u(url), &shell, port, &home);

        for page in [
            "http://127.0.0.1:41000/",
            "http://127.0.0.1:41000/sessions/abc?tab=chat#x",
        ] {
            let js = script(page, port).unwrap_or_else(|| panic!("{page}"));
            assert!(js.contains(r#"})("http://127.0.0.1:41000", "tauri://localhost/");"#));
            assert!(!js.contains("__ORIGIN__") && !js.contains("__HOME__"));
            assert!(!js.contains("__TAURI") && !js.contains("invoke"), "no IPC");
        }
        for page in [
            // the shell (incl. the dev server) and the gate's boot page
            "tauri://localhost/index.html",
            "http://localhost:1420/",
            "http://127.0.0.1:41000/__pbm/boot?k=x",
            // another port, host, scheme or origin
            "http://127.0.0.1:41001/",
            "http://localhost:41000/",
            "https://127.0.0.1:41000/",
            "http://tauri.localhost/",
            "https://example.com/",
            "file:///etc/passwd",
        ] {
            assert_eq!(script(page, port), None, "{page}");
        }
        // No tunnel, no box page.
        assert_eq!(script("http://127.0.0.1:41000/", None), None);
    }

    #[test]
    fn shell_origins_are_the_current_platforms_only() {
        // iOS / macOS / Linux: only the custom scheme is the shell.
        let apple = shell_origins(false, None);
        assert_eq!(apple, [u("tauri://localhost")]);
        assert!(is_shell(&u("tauri://localhost/index.html"), &apple));
        assert!(!is_shell(&u("http://tauri.localhost/"), &apple));
        assert!(!is_shell(&u("https://tauri.localhost/"), &apple));
        assert_eq!(
            decide(&u("http://tauri.localhost/"), &apple, Some(41000)),
            Decision::OpenExternally
        );

        // Windows / Android: only `http://tauri.localhost`.
        let http = shell_origins(true, None);
        assert_eq!(http, [u("http://tauri.localhost")]);
        assert!(is_shell(&u("http://tauri.localhost/"), &http));
        assert!(!is_shell(&u("tauri://localhost/"), &http));
        assert!(!is_shell(&u("https://tauri.localhost/"), &http));
        assert_eq!(
            decide(&u("tauri://localhost/"), &http, Some(41000)),
            Decision::Block
        );

        // Dev server joins the list; a box page never does.
        let dev = shell_origins(false, Some(u("http://localhost:1420")));
        assert!(is_shell(&u("http://localhost:1420/index.html"), &dev));
        assert!(!is_shell(&u("http://localhost:1421/"), &dev));
        assert!(!is_shell(&u("http://127.0.0.1:41000/"), &dev));

        assert_eq!(
            origin_string(&u("tauri://localhost/x")),
            "tauri://localhost"
        );
        assert_eq!(
            origin_string(&u("http://localhost:1420/")),
            "http://localhost:1420"
        );
        assert_eq!(
            origin_string(&u("http://tauri.localhost/")),
            "http://tauri.localhost"
        );
    }
}
