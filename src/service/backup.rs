use std::cmp::Reverse;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use diesel::RunQueryDsl;
use flate2::Compression;
use flate2::write::GzEncoder;

use crate::db::Db;

/// Scheduled-snapshot settings from `<data_dir>/config.json`.
pub struct BackupSettings {
    pub interval_hours: u64,
    pub dir: PathBuf,
    pub retention: usize,
}

impl BackupSettings {
    /// Load from config.json. Returns `None` when not configured or disabled.
    pub fn load(data_dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(data_dir.join("config.json")).ok()?;
        let root: serde_json::Value = serde_json::from_str(&text).ok()?;

        let interval_hours = root
            .get("backupIntervalHours")
            .and_then(|v| v.as_u64())
            .filter(|&h| h > 0)?;
        let dir = root
            .get("backupDir")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)?;
        let retention = root
            .get("backupRetention")
            .and_then(|v| v.as_u64())
            .unwrap_or(7) as usize;

        Some(BackupSettings {
            interval_hours,
            dir,
            retention,
        })
    }
}

/// Write a tar.gz archive of the data directory into `out`.
///
/// Includes: manifest.json, peckboard-backup.db (the vacuum copy),
/// config.json, vapid_keys.json, reports/, attachments/, plugins/*.wasm.
/// Excludes: live peckboard.db/-wal/-shm, certs/, worker-mcp/.
fn build_tar_gz<W: Write>(
    data_dir: &Path,
    tmp_db_path: &Path,
    app_version: &str,
    now_unix: u64,
    out: W,
) -> anyhow::Result<()> {
    let enc = GzEncoder::new(out, Compression::default());
    let mut tar = tar::Builder::new(enc);

    // manifest.json
    let manifest_bytes = serde_json::to_vec(&serde_json::json!({
        "app_version": app_version,
        "created_at": now_unix,
    }))?;
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(now_unix);
    header.set_cksum();
    tar.append_data(&mut header, "manifest.json", manifest_bytes.as_slice())?;

    // Consistent DB snapshot (produced by VACUUM INTO)
    tar.append_path_with_name(tmp_db_path, "peckboard-backup.db")?;

    // Optional single files
    let config_path = data_dir.join("config.json");
    if config_path.exists() {
        tar.append_path_with_name(&config_path, "config.json")?;
    }
    let vapid_path = data_dir.join("vapid_keys.json");
    if vapid_path.exists() {
        tar.append_path_with_name(&vapid_path, "vapid_keys.json")?;
    }

    // reports/ and attachments/ (recursive)
    let reports_dir = data_dir.join("reports");
    if reports_dir.is_dir() {
        tar.append_dir_all("reports", &reports_dir)?;
    }
    let attachments_dir = data_dir.join("attachments");
    if attachments_dir.is_dir() {
        tar.append_dir_all("attachments", &attachments_dir)?;
    }

    // plugins/*.wasm only (no config, no binaries other than the plugin blobs)
    let plugins_dir = data_dir.join("plugins");
    if plugins_dir.is_dir() {
        for entry in std::fs::read_dir(&plugins_dir)
            .with_context(|| format!("read {}", plugins_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("wasm") {
                let name = format!("plugins/{}", entry.file_name().to_string_lossy());
                tar.append_path_with_name(&path, &name)?;
            }
        }
    }

    let enc = tar.into_inner()?;
    enc.finish()?;
    Ok(())
}

/// Run `VACUUM INTO` on the live DB, returning the path of the temp copy.
/// Holds the DB mutex only for the vacuum duration.
async fn vacuum_to_tmp(db: &Db, data_dir: PathBuf) -> anyhow::Result<PathBuf> {
    db.with_conn(move |conn| {
        let tmp = data_dir.join(format!(".peckboard-backup-tmp-{}.db", uuid::Uuid::new_v4()));
        let escaped = tmp.to_string_lossy().replace('\'', "''");
        diesel::sql_query(format!("VACUUM INTO '{escaped}'"))
            .execute(conn)
            .context("VACUUM INTO failed")?;
        Ok(tmp)
    })
    .await
}

/// Build an in-memory snapshot for the HTTP download endpoint.
pub async fn create_snapshot(db: &Db, data_dir: &Path) -> anyhow::Result<Vec<u8>> {
    let data_dir = data_dir.to_path_buf();
    let tmp_db = vacuum_to_tmp(db, data_dir.clone()).await?;
    let app_version = env!("CARGO_PKG_VERSION");

    tokio::task::spawn_blocking(move || {
        let now_unix = unix_now();
        let mut buf = Vec::new();
        let result = build_tar_gz(&data_dir, &tmp_db, app_version, now_unix, &mut buf);
        let _ = std::fs::remove_file(&tmp_db);
        result.map(|_| buf)
    })
    .await?
}

/// Write a snapshot to `out_path` (crash-safe: writes to `.tmp` then renames).
pub async fn write_snapshot(db: &Db, data_dir: &Path, out_path: &Path) -> anyhow::Result<()> {
    let data_dir = data_dir.to_path_buf();
    let out_path = out_path.to_path_buf();
    let tmp_db = vacuum_to_tmp(db, data_dir.clone()).await?;
    let app_version = env!("CARGO_PKG_VERSION");

    tokio::task::spawn_blocking(move || {
        let now_unix = unix_now();
        // Crash-safe: write to .tmp then rename atomically
        let tmp_out = out_path.with_file_name(format!(
            "{}.tmp",
            out_path.file_name().unwrap_or_default().to_string_lossy()
        ));

        let result = (|| -> anyhow::Result<()> {
            let file = std::fs::File::create(&tmp_out)?;
            build_tar_gz(&data_dir, &tmp_db, app_version, now_unix, file)?;
            std::fs::rename(&tmp_out, &out_path)?;
            Ok(())
        })();

        let _ = std::fs::remove_file(&tmp_db);
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp_out);
        }
        result
    })
    .await?
}

/// Spawn the scheduled-backup loop. No-op unless both `backupIntervalHours`
/// and `backupDir` are set in `<data_dir>/config.json`.
pub fn spawn_scheduler(db: Db, data_dir: PathBuf) {
    let Some(settings) = BackupSettings::load(&data_dir) else {
        return;
    };
    let hours = settings.interval_hours;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(hours * 3600));
        // Skip catch-up bursts after a long pause.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Skip the first tick — don't run immediately on boot.
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(e) = run_scheduled_backup(&db, &data_dir, &settings).await {
                tracing::error!("scheduled backup failed: {e:#}");
            }
        }
    });
    tracing::info!("Backup scheduler started ({hours}h interval)");
}

async fn run_scheduled_backup(
    db: &Db,
    data_dir: &Path,
    settings: &BackupSettings,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(&settings.dir)
        .with_context(|| format!("cannot create backup dir {}", settings.dir.display()))?;
    let now = unix_now();
    let out_path = settings.dir.join(format!("peckboard-backup-{now}.tar.gz"));
    write_snapshot(db, data_dir, &out_path).await?;
    prune_old_backups(&settings.dir, settings.retention)?;
    tracing::info!("Backup written to {}", out_path.display());
    Ok(())
}

/// Remove oldest backup files beyond `keep` count.
/// Sorts by mtime (newest first); filename is the tiebreaker so
/// same-second writes are ordered deterministically.
pub fn prune_old_backups(dir: &Path, keep: usize) -> anyhow::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let s = name.to_string_lossy();
            s.starts_with("peckboard-backup-") && s.ends_with(".gz")
        })
        .collect();

    // Newest first: mtime desc, then filename desc as stable tiebreaker.
    entries.sort_by_key(|e| {
        let mtime = e
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        let name = e.file_name();
        Reverse((mtime, name))
    });

    for stale in entries.iter().skip(keep) {
        if let Err(e) = std::fs::remove_file(stale.path()) {
            tracing::warn!("failed to prune {}: {e}", stale.path().display());
        }
    }
    Ok(())
}

/// Restore a backup archive into `data_dir`.
///
/// Validates gzip magic and `manifest.json` presence. Refuses if
/// `peckboard.db` already exists unless `force` is `true`.
/// Rejects path-traversal entries (`..`, absolute paths).
pub fn restore_from(archive_path: &Path, data_dir: &Path, force: bool) -> anyhow::Result<()> {
    // 1. Check gzip magic bytes
    let mut f = std::fs::File::open(archive_path)
        .with_context(|| format!("cannot open {}", archive_path.display()))?;
    let mut magic = [0u8; 2];
    f.read_exact(&mut magic)
        .context("failed to read archive header")?;
    if magic != [0x1f, 0x8b] {
        bail!("not a gzip archive (wrong magic bytes)");
    }
    drop(f);

    // 2. Verify manifest.json is present
    {
        let gz = flate2::read::GzDecoder::new(std::fs::File::open(archive_path)?);
        let mut archive = tar::Archive::new(gz);
        let mut found = false;
        for entry in archive.entries()? {
            let entry = entry?;
            if entry.path()?.to_str() == Some("manifest.json") {
                found = true;
                break;
            }
        }
        if !found {
            bail!("archive does not contain manifest.json — not a valid peckboard backup");
        }
    }

    // 3. Refuse overwrite unless --force
    let db_dest = data_dir.join("peckboard.db");
    if db_dest.exists() && !force {
        bail!(
            "peckboard.db already exists at {}; pass --force to overwrite",
            db_dest.display()
        );
    }

    // 4. Unpack
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("cannot create {}", data_dir.display()))?;

    let gz = flate2::read::GzDecoder::new(std::fs::File::open(archive_path)?);
    let mut archive = tar::Archive::new(gz);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();

        if path.as_os_str() == "manifest.json" {
            continue;
        }

        // Reject unsafe paths
        if path.is_absolute()
            || path
                .components()
                .any(|c| c == std::path::Component::ParentDir)
        {
            bail!("archive contains unsafe path: {}", path.display());
        }
        // Only regular files and directories. A symlink entry could point
        // outside the data dir and redirect later entries through it; our
        // own snapshots never contain one (build_tar_gz follows symlinks).
        let kind = entry.header().entry_type();
        if !matches!(kind, tar::EntryType::Regular | tar::EntryType::Directory) {
            bail!(
                "archive contains unsupported entry type {kind:?} at {}",
                path.display()
            );
        }

        // peckboard-backup.db → peckboard.db
        let dest = if path == Path::new("peckboard-backup.db") {
            data_dir.join("peckboard.db")
        } else {
            data_dir.join(&path)
        };

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        entry
            .unpack(&dest)
            .with_context(|| format!("failed to unpack {}", path.display()))?;
    }

    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
