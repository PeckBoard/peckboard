//! Nightly backups into `<data-dir>/backups`: an online SQLite backup
//! (`hub-<ts>.sqlite`) and a tarball of the repos dir taken right after it
//! (`hub-<ts>.repos.tar`), with git writes paused, so every commit the
//! database points at is in the tarball. The newest [`KEEP`] of each are
//! kept.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tracing::{info, warn};

use crate::Hub;
use crate::git::Repos;
use crate::store::Store;

pub const KEEP: usize = 7;
pub const EVERY: Duration = Duration::from_secs(24 * 60 * 60);

pub fn spawn(hub: Arc<Hub>) {
    tokio::spawn(async move {
        loop {
            let hub2 = hub.clone();
            let dir = hub.cfg.data_dir.join("backups");
            let res = tokio::task::spawn_blocking(move || {
                backup_if_due(&hub2.store, &hub2.repos, &dir, EVERY)
            })
            .await;
            match res {
                Ok(Ok(Some(p))) => info!("backup written to {}", p.display()),
                Ok(Ok(None)) => {}
                Ok(Err(e)) => warn!("backup failed: {e:#}"),
                Err(e) => warn!("backup panicked: {e}"),
            }
            // Check hourly; a backup is taken once the newest is `EVERY` old.
            tokio::time::sleep(Duration::from_secs(60 * 60)).await;
        }
    });
}

fn backups(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("hub-") && n.ends_with(suffix))
        })
        .collect();
    v.sort();
    v
}

fn prune(dir: &Path, suffix: &str) {
    let all = backups(dir, suffix);
    if all.len() > KEEP {
        for old in &all[..all.len() - KEEP] {
            let _ = std::fs::remove_file(old);
        }
    }
}

/// Tar `repos.root()` into `dest` (as `repos/…`) with git writes paused.
pub fn tar_repos(repos: &Repos, dest: &Path) -> anyhow::Result<()> {
    let _frozen = repos.freeze();
    let tmp = dest.with_extension("tar.tmp");
    let file = std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    let mut tar = tar::Builder::new(std::io::BufWriter::new(file));
    tar.follow_symlinks(false);
    tar.append_dir_all("repos", repos.root())
        .context("tar repos")?;
    tar.into_inner()
        .context("finish tar")?
        .into_inner()
        .map_err(|e| e.into_error())
        .context("flush tar")?
        .sync_all()?;
    std::fs::rename(&tmp, dest).context("rename tar")?;
    Ok(())
}

/// Back up when the newest backup is older than `every` (or none exists),
/// then prune to [`KEEP`]. Returns the SQLite backup's path.
pub fn backup_if_due(
    store: &std::sync::Mutex<Store>,
    repos: &Repos,
    dir: &Path,
    every: Duration,
) -> anyhow::Result<Option<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let existing = backups(dir, ".sqlite");
    if let Some(newest) = existing.last()
        && let Ok(age) = std::fs::metadata(newest)
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().unwrap_or_default())
        && age < every
    {
        return Ok(None);
    }
    let stem = format!("hub-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S%.3f"));
    let dest = dir.join(format!("{stem}.sqlite"));
    store
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .backup_to(&dest)?;
    tar_repos(repos, &dir.join(format!("{stem}.repos.tar")))?;
    prune(dir, ".sqlite");
    prune(dir, ".repos.tar");
    Ok(Some(dest))
}
