//! Tracing subscriber setup for the binary.
//!
//! The default filter (used when `RUST_LOG` is unset or invalid) is plain
//! `info`: routine per-tick / per-poll / per-request messages log at `debug`
//! at their call sites, so `info` carries lifecycle events, warnings, and
//! errors only. `RUST_LOG`, when set, replaces the default entirely.

use std::io::IsTerminal;
use tracing_subscriber::EnvFilter;

/// Directives used when `RUST_LOG` is unset.
pub const DEFAULT_FILTER: &str = "info";

/// Build the filter from a `RUST_LOG` value: the value verbatim when set and
/// parseable, the default otherwise.
pub fn filter_from(rust_log: Option<&str>) -> EnvFilter {
    rust_log
        .filter(|v| !v.trim().is_empty())
        .and_then(|v| EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| EnvFilter::new(DEFAULT_FILTER))
}

/// ANSI colour only on an interactive terminal, and never when `NO_COLOR` is
/// set to a non-empty value (<https://no-color.org>). Under systemd stdout is
/// a pipe to journald, where escape codes are just noise.
pub fn use_ansi(stdout_is_tty: bool, no_color: Option<&str>) -> bool {
    stdout_is_tty && no_color.is_none_or(|v| v.is_empty())
}

/// Install the global subscriber. Call once, first thing in `main`.
pub fn init() {
    let rust_log = std::env::var("RUST_LOG").ok();
    let no_color = std::env::var("NO_COLOR").ok();
    tracing_subscriber::fmt()
        .with_env_filter(filter_from(rust_log.as_deref()))
        .with_ansi(use_ansi(
            std::io::stdout().is_terminal(),
            no_color.as_deref(),
        ))
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;
    use tracing_subscriber::layer::SubscriberExt;

    /// (debug, info, warn) enabled for the orchestrator target under `filter`.
    fn orchestrator_levels(filter: EnvFilter) -> (bool, bool, bool) {
        let sub = tracing_subscriber::registry().with(filter);
        tracing::subscriber::with_default(sub, || {
            (
                tracing::enabled!(target: "peckboard::worker::orchestrator", Level::DEBUG),
                tracing::enabled!(target: "peckboard::worker::orchestrator", Level::INFO),
                tracing::enabled!(target: "peckboard::worker::orchestrator", Level::WARN),
            )
        })
    }

    #[test]
    fn default_filter_drops_debug_keeps_info_and_warn() {
        for unset in [None, Some(""), Some("  ")] {
            let (debug, info, warn) = orchestrator_levels(filter_from(unset));
            assert!(!debug, "routine debug noise must be off by default");
            assert!(info && warn);
        }
    }

    #[test]
    fn rust_log_overrides_default_verbatim() {
        let (debug, _, _) = orchestrator_levels(filter_from(Some("peckboard=debug")));
        assert!(debug);
        let (_, info, warn) =
            orchestrator_levels(filter_from(Some("peckboard::worker::orchestrator=warn")));
        assert!(!info && warn);
    }

    #[test]
    fn ansi_only_on_tty_without_no_color() {
        assert!(use_ansi(true, None));
        assert!(use_ansi(true, Some("")));
        assert!(!use_ansi(true, Some("1")));
        assert!(!use_ansi(false, None));
        assert!(!use_ansi(false, Some("1")));
    }
}
