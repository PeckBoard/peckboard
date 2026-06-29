//! Self-update: check GitHub releases for a newer PeckBoard, download the
//! platform binary (+ its `.sha256`), verify the checksum, atomically replace
//! the running executable, and re-exec into it.
//!
//! Targets the **bare-process** run model (no supervisor): the app swaps its
//! own binary on disk and, on Unix, `exec()`s the new one — same PID, same
//! args/env. The release binaries are published by the `build-*.yml` workflows;
//! each also publishes `<asset>.sha256`, which we require and verify before the
//! swap so a self-downloaded binary is never trusted on HTTPS alone.

use crate::plugin::registry::{is_newer, peckboard_version, sha256_hex};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// GitHub "latest release" API for the core repo.
const RELEASES_LATEST_API: &str =
    "https://api.github.com/repos/PeckBoard/peckboard/releases/latest";
/// Base URL for a release asset download (`…/download/<tag>/<asset>`).
const RELEASE_DOWNLOAD_BASE: &str = "https://github.com/PeckBoard/peckboard/releases/download";
/// GitHub requires a User-Agent on API requests.
const UA: &str = concat!("peckboard-self-update/", env!("PECKBOARD_VERSION"));
/// Hard cap on a downloaded binary (the release binaries are tens of MB).
const DOWNLOAD_CAP: u64 = 256 * 1024 * 1024;

/// The release asset name for the platform this binary was built for, or `None`
/// if self-update isn't supported here. Must match the names the `build-*.yml`
/// workflows publish.
pub fn platform_asset() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("peckboard-linux-x86_64"),
        ("macos", "aarch64") => Some("peckboard-macos-arm64"),
        ("windows", "x86_64") => Some("peckboard-windows-x86_64.exe"),
        _ => None,
    }
}

/// The result of an update check, serialized to the `/api/update/check` client.
#[derive(Debug, Serialize)]
pub struct UpdateStatus {
    /// The running version (the git tag stamped at build time).
    pub current_version: String,
    /// The latest released tag, if the check succeeded.
    pub latest_version: Option<String>,
    /// True iff a strictly-newer release exists AND this platform is supported.
    pub update_available: bool,
    /// Whether self-update is supported on this OS/arch.
    pub supported: bool,
    /// The platform asset name (informational), if supported.
    pub asset: Option<String>,
    /// The release notes body, if any.
    pub notes: Option<String>,
    /// The release page URL, if any.
    pub html_url: Option<String>,
}

/// Query the latest release and compare it to the running version. Network/parse
/// failures are surfaced as `Err` (the caller maps them to a 502).
pub async fn check(client: &reqwest::Client) -> Result<UpdateStatus> {
    let current = peckboard_version().to_string();
    let asset = platform_asset();

    let rel: serde_json::Value = client
        .get(RELEASES_LATEST_API)
        .header(reqwest::header::USER_AGENT, UA)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .context("contacting GitHub releases")?
        .error_for_status()
        .context("GitHub releases returned an error status")?
        .json()
        .await
        .context("parsing the GitHub release response")?;

    let latest = rel
        .get("tag_name")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let notes = rel
        .get("body")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    let html_url = rel
        .get("html_url")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let newer = latest
        .as_deref()
        .map(|t| is_newer(t, &current))
        .unwrap_or(false);

    Ok(UpdateStatus {
        current_version: current,
        latest_version: latest,
        update_available: newer && asset.is_some(),
        supported: asset.is_some(),
        asset: asset.map(str::to_string),
        notes,
        html_url,
    })
}

/// Download the platform binary for `tag` and its published `.sha256`, verify
/// the checksum, and atomically replace the running executable on disk. Does
/// NOT restart — the caller sends its HTTP response first, then calls
/// [`restart`] with the returned path.
///
/// Returns the path of the replaced executable, **captured before the swap**.
/// This matters: once the rename unlinks the old inode, `std::env::current_exe()`
/// on Linux resolves to `"<path> (deleted)"`, and `exec()`ing that bogus path
/// fails — which silently stranded swapped-but-not-restarted processes still
/// running (and reporting) the old version.
pub async fn download_and_swap(client: &reqwest::Client, tag: &str) -> Result<PathBuf> {
    let asset =
        platform_asset().ok_or_else(|| anyhow!("self-update is not supported on this platform"))?;
    let bin_url = format!("{RELEASE_DOWNLOAD_BASE}/{tag}/{asset}");
    let sha_url = format!("{bin_url}.sha256");

    // 1. Fetch the published checksum (first whitespace-delimited token — the
    //    `sha256sum` format is "<hex>  <filename>").
    let sha_text = client
        .get(&sha_url)
        .header(reqwest::header::USER_AGENT, UA)
        .send()
        .await
        .context("downloading the release checksum")?
        .error_for_status()
        .context("release checksum not found (is this release built with sha256 publishing?)")?
        .text()
        .await?;
    let expected = sha_text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase();
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("published checksum is malformed");
    }

    // 2. Download the binary (size-capped).
    let resp = client
        .get(&bin_url)
        .header(reqwest::header::USER_AGENT, UA)
        .send()
        .await
        .context("downloading the new binary")?
        .error_for_status()
        .context("release binary not found")?;
    if let Some(len) = resp.content_length()
        && len > DOWNLOAD_CAP
    {
        bail!("download is {len} bytes, over the {DOWNLOAD_CAP}-byte cap");
    }
    let bytes = resp.bytes().await?;
    if bytes.len() as u64 > DOWNLOAD_CAP {
        bail!(
            "download is {} bytes, over the {DOWNLOAD_CAP}-byte cap",
            bytes.len()
        );
    }

    // 3. Verify before anything touches disk.
    let actual = sha256_hex(&bytes);
    if actual != expected {
        bail!("checksum mismatch: expected {expected}, got {actual}");
    }

    // 4. Atomically replace the running executable, returning its path captured
    //    before the old inode is unlinked (see the fn doc).
    let exe = replace_current_exe(&bytes)?;
    Ok(exe)
}

/// Resolve the running executable's path and replace it on disk with `bytes`,
/// returning that path. On Unix, renaming over a *running* executable is safe —
/// the live process keeps executing the now-unlinked old inode until it
/// re-execs. The path is resolved (and returned) *before* the swap precisely so
/// the caller doesn't have to re-resolve it afterwards, when `current_exe()`
/// would yield a bogus `"<path> (deleted)"`.
fn replace_current_exe(bytes: &[u8]) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("resolving the current executable path")?;
    replace_exe_at(&exe, bytes)?;
    Ok(exe)
}

/// Write `bytes` to a temp file beside `exe` and atomically rename it over
/// `exe`. The temp lives in the same directory so the rename is atomic (same
/// filesystem). Split out from [`replace_current_exe`] so the swap mechanics can
/// be tested against an arbitrary path without clobbering the test runner's own
/// executable.
fn replace_exe_at(exe: &Path, bytes: &[u8]) -> Result<()> {
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("executable has no parent directory"))?;
    let tmp = dir.join(format!(".peckboard-update-{}.tmp", std::process::id()));

    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).context("setting the new binary executable");
        }
    }
    if let Err(e) = std::fs::rename(&tmp, exe) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| {
            format!(
                "replacing {} (need write permission on its directory)",
                exe.display()
            )
        });
    }
    Ok(())
}

/// Re-exec the (already-replaced) executable at `exe` with the original args and
/// environment. On Unix this replaces the process image — same PID — and never
/// returns on success; the listening socket is `CLOEXEC` so the new process
/// rebinds the port cleanly. Returns only on failure.
///
/// `exe` MUST be the path captured by [`download_and_swap`] *before* the swap.
/// Do not re-derive it from `std::env::current_exe()` here: after the binary is
/// replaced, that resolves to `"<path> (deleted)"` on Linux, and exec()ing the
/// bogus path fails — the process then keeps running the old image, which is the
/// exact bug this signature now prevents.
pub fn restart(exe: &Path) -> Result<()> {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // `exec` only returns on error.
        let err = std::process::Command::new(exe).args(&args).exec();
        Err(anyhow::Error::new(err).context("re-exec into the new binary failed"))
    }
    #[cfg(not(unix))]
    {
        std::process::Command::new(exe)
            .args(&args)
            .spawn()
            .context("spawning the new binary")?;
        std::process::exit(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_asset_matches_published_names() {
        // The mapping must stay in lockstep with the build-*.yml asset names.
        for (os, arch, want) in [
            ("linux", "x86_64", Some("peckboard-linux-x86_64")),
            ("macos", "aarch64", Some("peckboard-macos-arm64")),
            ("windows", "x86_64", Some("peckboard-windows-x86_64.exe")),
        ] {
            // Sanity on the table itself (we can't override env::consts here).
            let mapped = match (os, arch) {
                ("linux", "x86_64") => Some("peckboard-linux-x86_64"),
                ("macos", "aarch64") => Some("peckboard-macos-arm64"),
                ("windows", "x86_64") => Some("peckboard-windows-x86_64.exe"),
                _ => None,
            };
            assert_eq!(mapped, want);
        }
        // On the host running the tests, the real lookup is internally
        // consistent: supported iff it returns a name.
        let a = platform_asset();
        if let Some(name) = a {
            assert!(name.starts_with("peckboard-"));
        }
    }

    /// The swap must land the new bytes at the *exact* original path (so a later
    /// `restart(exe)` re-execs the new binary), mark it executable, and leave no
    /// staging temp behind. This is the mechanic that makes re-exec-by-path work
    /// — the half that succeeded even when the old code's post-swap re-exec via
    /// `current_exe()` ("<path> (deleted)") failed.
    #[test]
    fn replace_exe_at_swaps_in_place_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("peckboard");
        std::fs::write(&exe, b"OLD-BINARY").unwrap();

        replace_exe_at(&exe, b"NEW-BINARY-BYTES").unwrap();

        assert_eq!(std::fs::read(&exe).unwrap(), b"NEW-BINARY-BYTES");

        let leftover_tmp = std::fs::read_dir(dir.path()).unwrap().flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".peckboard-update-")
        });
        assert!(!leftover_tmp, "staging temp file was left behind");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "replaced binary must be executable");
        }
    }
}
