//! One-click install of the provider CLIs, run by the server itself.
//!
//! The first-run wizard can't hand this to an agent session: on a new install
//! there is no working agent yet. So the server runs the vendor's official
//! installer directly and keeps the output for the UI to tail.
//!
//! The commands are a fixed table keyed by provider id, and the API only ever
//! takes that id, so a client can never choose what runs. Every installer here
//! is user-level and needs no root.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};

use super::codex_login::strip_ansi;

/// Upper bound on one install; the slowest vendor download is well under this.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(900);
/// Lines of installer output kept for the UI.
const LOG_LINES: usize = 200;

/// How a provider's subscription sign-in works, which the UI needs in order to
/// pick the right dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignIn {
    /// Browser login with a pasted code (`/api/claude-accounts/login/start`).
    Claude,
    /// Device login on a stored account (`/api/<id>-accounts/{id}/login/start`).
    DeviceAccount,
    /// The CLI's single host login (`/api/provider-setup/cursor/login/start`).
    HostCursor,
}

pub struct CliProvider {
    /// The crate plugin id, which is also the model-id prefix.
    pub id: &'static str,
    pub display_name: &'static str,
    /// The executable the provider spawns by default.
    pub binary: &'static str,
    /// Run through `bash` on Linux and macOS.
    pub install_script: &'static str,
    /// Shown for the user to run by hand on Windows, where there is no `bash`.
    pub windows_install: &'static str,
    pub sign_in: SignIn,
}

pub const CLI_PROVIDERS: &[CliProvider] = &[
    CliProvider {
        id: "claude",
        display_name: "Claude Code",
        binary: "claude",
        install_script: "curl -fsSL https://claude.ai/install.sh | bash",
        windows_install: "irm https://claude.ai/install.ps1 | iex",
        sign_in: SignIn::Claude,
    },
    CliProvider {
        id: "codex",
        display_name: "Codex",
        binary: "codex",
        install_script: "curl -fsSL https://chatgpt.com/codex/install.sh | sh",
        windows_install: "npm install -g @openai/codex",
        sign_in: SignIn::DeviceAccount,
    },
    CliProvider {
        id: "cursor",
        display_name: "Cursor",
        binary: "cursor-agent",
        install_script: "curl -fsS https://cursor.com/install | bash",
        windows_install: "irm 'https://cursor.com/install?win32=true' | iex",
        sign_in: SignIn::HostCursor,
    },
    CliProvider {
        id: "grok",
        display_name: "Grok",
        binary: "grok",
        install_script: "curl -fsSL https://x.ai/cli/install.sh | bash",
        windows_install: "irm https://x.ai/cli/install.ps1 | iex",
        sign_in: SignIn::DeviceAccount,
    },
    CliProvider {
        id: "kimi",
        display_name: "Kimi Code",
        binary: "kimi",
        install_script: "curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash",
        windows_install: "irm https://code.kimi.com/kimi-code/install.ps1 | iex",
        sign_in: SignIn::DeviceAccount,
    },
];

pub fn find_provider(id: &str) -> Option<&'static CliProvider> {
    CLI_PROVIDERS.iter().find(|p| p.id == id)
}

/// Whether the server can run the installers itself on this platform.
pub const fn auto_install_supported() -> bool {
    cfg!(unix)
}

/// The state of a provider's most recent install, for the UI to poll. Held in
/// memory only: the CLI's presence on disk is the durable record.
#[derive(Clone, Debug, Default, Serialize)]
pub struct InstallJob {
    pub running: bool,
    pub succeeded: Option<bool>,
    pub error: Option<String>,
    pub log: Vec<String>,
}

static JOBS: LazyLock<Mutex<HashMap<String, InstallJob>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn with_jobs<T>(f: impl FnOnce(&mut HashMap<String, InstallJob>) -> T) -> T {
    let mut jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut jobs)
}

pub fn job_snapshot(id: &str) -> Option<InstallJob> {
    with_jobs(|jobs| jobs.get(id).cloned())
}

/// Claim the slot for `id`. Returns false when an install is already running,
/// so a double click can't run two installers over each other.
pub fn try_begin(id: &str) -> bool {
    with_jobs(|jobs| {
        if jobs.get(id).is_some_and(|j| j.running) {
            return false;
        }
        jobs.insert(
            id.to_string(),
            InstallJob {
                running: true,
                ..InstallJob::default()
            },
        );
        true
    })
}

fn push_log(id: &str, line: &str) {
    // Progress bars redraw with `\r`, so only the final redraw is worth keeping.
    let line = strip_ansi(line);
    let Some(line) = line.rsplit('\r').map(str::trim_end).find(|s| !s.is_empty()) else {
        return;
    };
    with_jobs(|jobs| {
        if let Some(job) = jobs.get_mut(id) {
            job.log.push(line.to_string());
            if job.log.len() > LOG_LINES {
                let excess = job.log.len() - LOG_LINES;
                job.log.drain(..excess);
            }
        }
    });
}

fn finish(id: &str, result: Result<(), String>) {
    with_jobs(|jobs| {
        if let Some(job) = jobs.get_mut(id) {
            job.running = false;
            job.succeeded = Some(result.is_ok());
            job.error = result.err();
        }
    });
}

/// Run an install to completion for a slot claimed with [`try_begin`]. Success
/// means the script exited 0 AND `binary_present` now holds: installers can
/// exit 0 without producing anything usable, and a login that then can't find
/// the CLI would be a much more confusing failure. Returns whether it worked.
pub async fn run(id: &str, script: &str, binary_present: impl Fn() -> bool) -> bool {
    let result = execute(id, script).await.and_then(|()| {
        if binary_present() {
            Ok(())
        } else {
            Err("The installer finished, but the CLI still can't be found.".to_string())
        }
    });
    let ok = result.is_ok();
    finish(id, result);
    ok
}

async fn execute(id: &str, script: &str) -> Result<(), String> {
    // `pipefail` is what makes a failed download an error: without it,
    // `curl … | bash` reports the status of the empty `bash`, which is 0.
    let wrapped = format!("exec 2>&1\nset -o pipefail\n{script}");

    // Sandboxed so the data dir stays out of reach, with all of `$HOME`
    // writable because vendors choose their own install locations there.
    let mut scope = crate::sandbox::SpawnScope::none();
    if let Some(home) = std::env::var_os("HOME") {
        scope = scope.rw(home);
    }
    let mut cmd = crate::sandbox::SandboxedTokioCommand::new("bash", &scope);
    cmd.arg("-c")
        .arg(wrapped)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Could not start the installer: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "The installer has no output stream.".to_string())?;

    let run_to_end = async {
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => push_log(id, &String::from_utf8_lossy(&buf)),
            }
        }
        child.wait().await
    };

    match tokio::time::timeout(INSTALL_TIMEOUT, run_to_end).await {
        Err(_) => Err("The installer timed out.".to_string()),
        Ok(Err(e)) => Err(format!("The installer could not be waited on: {e}")),
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(match status.code() {
            Some(code) => format!("The installer failed with exit code {code}."),
            None => "The installer was stopped before it finished.".to_string(),
        }),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_failed_download_piped_into_a_shell_counts_as_a_failure() {
        let id = "test-pipefail";
        assert!(try_begin(id));
        // Without pipefail this pipeline exits 0, the status of `cat`.
        let ok = run(id, "echo fetching; false | cat", || true).await;
        assert!(!ok);
        let job = job_snapshot(id).unwrap();
        assert!(!job.running);
        assert_eq!(job.succeeded, Some(false));
        assert_eq!(job.log, vec!["fetching".to_string()]);
    }

    #[tokio::test]
    async fn success_needs_the_binary_as_well_as_exit_zero() {
        let id = "test-binary";
        assert!(try_begin(id));
        assert!(!run(id, "true", || false).await);
        assert!(
            job_snapshot(id)
                .unwrap()
                .error
                .unwrap()
                .contains("can't be found")
        );

        assert!(try_begin(id));
        assert!(run(id, "echo done >&2", || true).await);
        let job = job_snapshot(id).unwrap();
        assert_eq!(job.succeeded, Some(true));
        // stderr is captured too, and a retry starts from a clean log.
        assert_eq!(job.log, vec!["done".to_string()]);
    }

    #[test]
    fn a_running_install_cannot_be_started_twice() {
        let id = "test-double";
        assert!(try_begin(id));
        assert!(!try_begin(id));
        finish(id, Ok(()));
        assert!(try_begin(id));
    }

    #[test]
    fn progress_redraws_keep_only_the_last_frame_and_no_colour() {
        let id = "test-progress";
        assert!(try_begin(id));
        push_log(id, "\u{1b}[32m 10%\r 50%\r100%\u{1b}[0m\n");
        push_log(id, "\n");
        assert_eq!(job_snapshot(id).unwrap().log, vec!["100%".to_string()]);
    }

    #[test]
    fn every_installer_targets_https_and_a_crate_provider() {
        for p in CLI_PROVIDERS {
            assert!(
                crate::plugin::crates::is_crate_provider_id(p.id),
                "{}",
                p.id
            );
            assert!(p.install_script.contains("https://"), "{}", p.id);
            assert!(!p.install_script.contains("sudo"), "{}", p.id);
        }
    }
}
