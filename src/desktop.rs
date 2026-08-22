//! Native desktop window wrapping the local HTTP server.
//!
//! The wry/tao WebView is compiled only with the `desktop` cargo feature
//! (on by default). `--no-default-features` produces today's CLI-only
//! binary, which still understands `--desktop` and errors with a rebuild
//! hint.

use crate::config::CliArgs;

const TOKEN_KEY: &str = "peckboard_token";

/// True when this process can open a GUI.
///
/// Linux needs `DISPLAY` or `WAYLAND_DISPLAY`. Other platforms assume a
/// session is available; wry will still fail later if it is not.
pub fn display_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// Navigation / new-window policy: stay on loopback, otherwise the
/// system browser. OAuth-in-WebView is a trap.
pub fn is_loopback_url(url: &str) -> bool {
    if url == "about:blank" || url.starts_with("about:") {
        return true;
    }
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .or_else(|| url.strip_prefix("ws://"))
        .or_else(|| url.strip_prefix("wss://"));
    let Some(rest) = rest else {
        return false;
    };
    let hostport = rest.split('/').next().unwrap_or(rest);
    let host = if let Some(inner) = hostport.strip_prefix('[') {
        inner.split(']').next().unwrap_or("")
    } else {
        hostport.split(':').next().unwrap_or(hostport)
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Init script injected before page JS. JWT is JSON-string encoded so a
/// token can never break out of the literal. Does not overwrite an
/// existing token (returning users keep remember-me).
pub fn bootstrap_init_script(token: &str) -> String {
    let token_js = serde_json::to_string(token).unwrap_or_else(|_| "\"\"".into());
    format!(
        "(function(){{\
           try {{\
             var k = {key};\
             if (!localStorage.getItem(k) && !sessionStorage.getItem(k)) {{\
               localStorage.setItem(k, {token});\
             }}\
           }} catch (e) {{}}\
         }})();",
        key = serde_json::to_string(TOKEN_KEY).unwrap(),
        token = token_js,
    )
}

/// Open a native window onto the server, or error if this binary was
/// built without the `desktop` feature.
pub fn run(args: CliArgs) -> anyhow::Result<()> {
    #[cfg(feature = "desktop")]
    {
        native::run(args)
    }
    #[cfg(not(feature = "desktop"))]
    {
        let _ = args;
        anyhow::bail!(
            "this binary was built without desktop support. \
             Rebuild with default features (`cargo build --release`), \
             or on Linux download the `*-desktop` release asset."
        )
    }
}

/// Write `~/.local/share/applications/peckboard.desktop` plus a 512px
/// icon so the app appears in the desktop launcher.
pub fn install_desktop_entry() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux_install_desktop_entry()
    }
    #[cfg(not(target_os = "linux"))]
    {
        anyhow::bail!(
            "--install-desktop-entry is Linux-only. On macOS/Windows run \
             `peckboard --desktop` (or a shortcut that passes that flag)."
        )
    }
}

#[cfg(target_os = "linux")]
fn linux_install_desktop_entry() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let exe_str = exe.to_string_lossy();
    let data = dirs::data_dir().ok_or_else(|| anyhow::anyhow!("no XDG data dir"))?;

    let apps = data.join("applications");
    std::fs::create_dir_all(&apps)?;
    let desktop_path = apps.join("peckboard.desktop");
    let desktop = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Peckboard\n\
         Comment=Remote control panel for Claude Code\n\
         Exec=\"{exe}\" --desktop\n\
         Icon=peckboard\n\
         Terminal=false\n\
         Categories=Development;\n\
         StartupWMClass=com.peckboard.app\n",
        exe = escape_desktop_exec(&exe_str),
    );
    std::fs::write(&desktop_path, desktop)?;

    let icons = data.join("icons/hicolor/512x512/apps");
    std::fs::create_dir_all(&icons)?;
    std::fs::write(icons.join("peckboard.png"), ICON_PNG)?;

    eprintln!("Installed desktop entry at {}", desktop_path.display());
    Ok(())
}

/// Quote a path for a `.desktop` Exec= line. `"` is the only character
/// the spec requires we escape inside quotes.
fn escape_desktop_exec(path: &str) -> String {
    path.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(any(feature = "desktop", target_os = "linux"))]
const ICON_PNG: &[u8] = include_bytes!("../web/public/icon-512.png");

#[cfg(feature = "desktop")]
mod native {
    use super::{ICON_PNG, bootstrap_init_script, display_available, is_loopback_url};
    use crate::config::CliArgs;
    use crate::server::{ServerReady, run_server};
    use tao::dpi::LogicalSize;
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::platform::run_return::EventLoopExtRunReturn;
    use tao::window::WindowBuilder;
    use wry::WebViewBuilder;

    #[derive(Debug, Clone, Copy)]
    enum DesktopEvent {
        ServerStopped,
    }

    pub fn run(args: CliArgs) -> anyhow::Result<()> {
        if !display_available() {
            anyhow::bail!(
                "--desktop needs a graphical session. On Linux install \
                 libwebkit2gtk-4.1-0 and run from a desktop (DISPLAY or \
                 WAYLAND_DISPLAY). Omit --desktop to run as a server."
            );
        }

        #[cfg(windows)]
        hide_console();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        let mut event_loop_builder = EventLoopBuilder::<DesktopEvent>::with_user_event();
        #[cfg(target_os = "linux")]
        {
            // GTK requires a reverse-DNS application id (g_application_id_is_valid).
            // Matches StartupWMClass in packaging/peckboard.desktop.
            use tao::platform::unix::EventLoopBuilderExtUnix;
            event_loop_builder.with_app_id("com.peckboard.app");
        }
        let mut event_loop = event_loop_builder.build();

        let proxy = event_loop.create_proxy();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        let server = rt.spawn(async move {
            let result = run_server(args, Some(shutdown_rx), Some(ready_tx)).await;
            let _ = proxy.send_event(DesktopEvent::ServerStopped);
            result
        });

        let ready = match ready_rx.blocking_recv() {
            Ok(ready) => ready,
            Err(_) => {
                return match rt.block_on(server) {
                    Ok(Err(e)) => Err(e),
                    Ok(Ok(())) => Err(anyhow::anyhow!("server exited before bind")),
                    Err(join) => Err(anyhow::anyhow!("server task: {join}")),
                };
            }
        };

        run_window(&mut event_loop, ready, shutdown_tx)?;

        match rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(15), server).await
        }) {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(join)) => Err(anyhow::anyhow!("server task: {join}")),
            Err(_) => {
                tracing::warn!("server did not stop within 15s after the window closed");
                Ok(())
            }
        }
    }

    fn run_window(
        event_loop: &mut tao::event_loop::EventLoop<DesktopEvent>,
        ready: ServerReady,
        shutdown_tx: tokio::sync::oneshot::Sender<()>,
    ) -> anyhow::Result<()> {
        let mut shutdown_tx = Some(shutdown_tx);
        tracing::info!(url = %ready.http_url, "Opening desktop window");
        let icon = load_window_icon();
        let window = WindowBuilder::new()
            .with_title("Peckboard")
            .with_inner_size(LogicalSize::new(1280.0, 800.0))
            .with_window_icon(icon)
            .build(event_loop)
            .map_err(|e| anyhow::anyhow!("failed to create window: {e}"))?;

        let mut web_context = wry::WebContext::new(Some(ready.data_dir.join("webview")));
        let mut builder = WebViewBuilder::new_with_web_context(&mut web_context)
            .with_url(&ready.http_url)
            .with_clipboard(true)
            .with_hotkeys_zoom(true)
            .with_navigation_handler(|url| {
                if is_loopback_url(&url) {
                    true
                } else {
                    open_external(&url);
                    false
                }
            })
            .with_new_window_req_handler(|url, _features| {
                if is_loopback_url(&url) {
                    wry::NewWindowResponse::Allow
                } else {
                    open_external(&url);
                    wry::NewWindowResponse::Deny
                }
            });

        if let Some(token) = ready.bootstrap_token.as_deref() {
            builder = builder.with_initialization_script(bootstrap_init_script(token));
        }

        // Keep the webview alive for the event loop. On Linux, wry needs
        // a GTK container (Wayland + X11); elsewhere it attaches to the
        // tao window handle.
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let _webview = builder
            .build(&window)
            .map_err(|e| anyhow::anyhow!("failed to create webview: {e}"))?;
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let _webview = {
            use tao::platform::unix::WindowExtUnix;
            use wry::WebViewBuilderExtUnix;
            let vbox = window
                .default_vbox()
                .ok_or_else(|| anyhow::anyhow!("gtk window has no default vbox"))?;
            builder
                .build_gtk(vbox)
                .map_err(|e| anyhow::anyhow!("failed to create webview: {e}"))?
        };
        tracing::info!("Desktop webview attached");

        event_loop.run_return(move |event, _, control_flow| {
            *control_flow = ControlFlow::Wait;
            match event {
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                } => {
                    if let Some(tx) = shutdown_tx.take() {
                        let _ = tx.send(());
                    }
                    *control_flow = ControlFlow::Exit;
                }
                Event::UserEvent(DesktopEvent::ServerStopped) => {
                    *control_flow = ControlFlow::Exit;
                }
                _ => {}
            }
        });
        Ok(())
    }

    fn open_external(url: &str) {
        if let Err(e) = open::that(url) {
            tracing::warn!("failed to open {url} in the system browser: {e}");
        }
    }

    fn load_window_icon() -> Option<tao::window::Icon> {
        let img = image::load_from_memory(ICON_PNG).ok()?.into_rgba8();
        let (width, height) = img.dimensions();
        tao::window::Icon::from_rgba(img.into_raw(), width, height).ok()
    }

    #[cfg(windows)]
    fn hide_console() {
        // Console-subsystem binaries flash a terminal on double-click.
        // Detach it once the GUI is taking over. CLI mode never reaches
        // this function.
        #[link(name = "kernel32")]
        extern "system" {
            fn FreeConsole() -> i32;
        }
        unsafe {
            let _ = FreeConsole();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_http_allowed() {
        assert!(is_loopback_url("http://127.0.0.1:3344/"));
        assert!(is_loopback_url("http://127.0.0.1:3344/sessions"));
        assert!(is_loopback_url("http://localhost:3344"));
        assert!(is_loopback_url("https://localhost/"));
        assert!(is_loopback_url("http://[::1]:3344/"));
        assert!(is_loopback_url("about:blank"));
    }

    #[test]
    fn foreign_hosts_rejected() {
        assert!(!is_loopback_url("https://github.com/login"));
        assert!(!is_loopback_url("http://example.com"));
        assert!(!is_loopback_url("http://0.0.0.0:3344/"));
        assert!(!is_loopback_url("file:///etc/passwd"));
    }

    #[test]
    fn bootstrap_script_json_escapes_and_is_conditional() {
        let script = bootstrap_init_script("abc.def.ghi");
        assert!(script.contains("localStorage.getItem"));
        assert!(script.contains("sessionStorage.getItem"));
        assert!(script.contains("\"abc.def.ghi\""));
        assert!(
            script.contains("if (!localStorage.getItem(k) && !sessionStorage.getItem(k))"),
            "must not overwrite an existing token: {script}"
        );
    }

    #[test]
    fn bootstrap_script_escapes_quotes() {
        let script = bootstrap_init_script("x\"y");
        assert!(script.contains("\"x\\\"y\"") || script.contains(r#""x\"y""#));
        assert!(!script.contains("setItem(k, x\"y)"));
    }

    #[test]
    fn linux_display_absent_is_false() {
        #[cfg(target_os = "linux")]
        {
            let display = std::env::var_os("DISPLAY");
            let wayland = std::env::var_os("WAYLAND_DISPLAY");
            unsafe {
                std::env::remove_var("DISPLAY");
                std::env::remove_var("WAYLAND_DISPLAY");
            }
            let available = display_available();
            if let Some(v) = display {
                unsafe { std::env::set_var("DISPLAY", v) }
            }
            if let Some(v) = wayland {
                unsafe { std::env::set_var("WAYLAND_DISPLAY", v) }
            }
            assert!(!available);
        }
    }

    #[test]
    fn escape_desktop_exec_quotes_inner_quotes() {
        assert_eq!(escape_desktop_exec(r#"C:\foo"bar"#), r#"C:\\foo\"bar"#);
    }
}
