//! Thread-local host-function injection for in-process crate plugins.
//!
//! First-party provider crates (`peck-plugins/{claude,mock,…}`) keep a
//! single `send` loop that talks to the host through `call_host`. On
//! wasm32 that is Extism FFI. On native, Peckboard installs a dispatcher
//! for the duration of one `spawn_blocking` turn via [`with_host`].

use std::cell::RefCell;
use std::sync::Arc;

/// JSON-string-in / JSON-string-out host function, same contract as the
/// Extism `host_fn!` wrappers in `src/plugin/host.rs`.
pub type HostFn = Arc<dyn Fn(&str, &str) -> String + Send + Sync>;

thread_local! {
    /// Stack of installed hosts; nested installs push, drop pops.
    static HOST: RefCell<Vec<HostFn>> = const { RefCell::new(Vec::new()) };
}

/// Run `f` with `host` answering [`call_raw`] / [`call_json`] on this
/// thread. Nested installs stack; the previous host is restored on drop.
pub fn with_host<R>(host: HostFn, f: impl FnOnce() -> R) -> R {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            HOST.with(|slot| {
                slot.borrow_mut().pop();
            });
        }
    }
    HOST.with(|slot| slot.borrow_mut().push(host));
    let _guard = Guard;
    f()
}

/// Invoke the installed host by Extism function name. Returns the raw JSON
/// string (including `{"error": …}` replies).
pub fn call_raw(name: &str, input: &str) -> Result<String, String> {
    HOST.with(|slot| {
        slot.borrow()
            .last()
            .cloned()
            .ok_or_else(|| "native host not installed on this thread".to_string())
            .map(|host| host(name, input))
    })
}

/// [`call_raw`] plus the WASM plugin convention: parse JSON and surface an
/// `error` field as `Err`.
pub fn call_json(name: &str, input: &serde_json::Value) -> Result<serde_json::Value, String> {
    let out = call_raw(name, &input.to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| format!("host returned invalid json: {e}"))?;
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        return Err(err.to_string());
    }
    Ok(v)
}
