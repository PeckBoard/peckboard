pub mod crud;
pub mod docs;
pub mod events;
pub mod models;
pub mod repair;
pub mod schema;
pub mod slow_ops;
#[cfg(test)]
mod tests;

use diesel::prelude::*;
use diesel::sqlite::SqliteConnection;
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

/// Thin wrapper around a Diesel SQLite connection.
///
/// All operations clone the inner `Arc` and run on `spawn_blocking` so
/// the async runtime is never blocked by SQLite I/O.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<SqliteConnection>>,
    /// Root data directory this database was opened from — `None` for
    /// in-memory test databases. Lets the event path derive sibling blob
    /// storage (`tool-images/`) without threading config through every
    /// `emit_event` caller.
    data_dir: Option<std::path::PathBuf>,
    /// Tally of slow calls, shared by every clone. See [`slow_ops`].
    slow: Arc<slow_ops::SlowOps>,
    /// Read-only connections for [`Db::read`]. `None` for an in-memory
    /// database, which only exists on its one connection.
    readers: Option<Arc<ReadPool>>,
}

/// How many read connections stay open between calls. More are opened under
/// load and closed again afterwards.
const MAX_IDLE_READERS: usize = 4;

/// A small pool of read-only connections to the database file. WAL mode lets
/// them read while the write connection is mid-write, so a read through the
/// pool never queues behind [`Db::with_conn`].
struct ReadPool {
    url: String,
    idle: Mutex<Vec<SqliteConnection>>,
}

impl ReadPool {
    fn open(&self) -> anyhow::Result<SqliteConnection> {
        let mut conn = SqliteConnection::establish(&self.url)
            .map_err(|e| anyhow::anyhow!("failed to open database for reading: {e}"))?;
        // A write through a pooled connection is a bug: make it an error.
        diesel::sql_query("PRAGMA query_only=ON;").execute(&mut conn)?;
        for pragma in [
            "PRAGMA busy_timeout=5000;",
            "PRAGMA cache_size=-16384;",
            "PRAGMA temp_store=MEMORY;",
        ] {
            diesel::sql_query(pragma).execute(&mut conn).ok();
        }
        Ok(conn)
    }

    fn run<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&mut SqliteConnection) -> anyhow::Result<T>,
    {
        let idle = self.idle.lock().ok().and_then(|mut idle| idle.pop());
        let mut conn = match idle {
            Some(conn) => conn,
            None => self.open()?,
        };
        let out = f(&mut conn);
        if let Ok(mut idle) = self.idle.lock()
            && idle.len() < MAX_IDLE_READERS
        {
            idle.push(conn);
        }
        out
    }
}

impl Db {
    /// Open (or create) the database at `data_dir/peckboard.db` and run
    /// any pending migrations.
    pub fn open(data_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let db_path = data_dir.join("peckboard.db");
        let db_url = db_path.to_string_lossy().to_string();

        let mut conn = SqliteConnection::establish(&db_url)
            .map_err(|e| anyhow::anyhow!("failed to open database: {e}"))?;

        // Enable WAL mode for better concurrent read performance.
        diesel::sql_query("PRAGMA journal_mode=WAL;")
            .execute(&mut conn)
            .ok();
        diesel::sql_query("PRAGMA foreign_keys=ON;")
            .execute(&mut conn)
            .ok();
        // With WAL, NORMAL flushes to disk at checkpoints instead of on
        // every commit (the default, FULL, cost one fsync per event insert).
        // An application crash loses nothing; a power loss or OS crash can
        // lose the last few seconds of commits but never corrupts the file.
        // Owner-approved trade-off, 2026-11-27.
        diesel::sql_query("PRAGMA synchronous=NORMAL;")
            .execute(&mut conn)
            .ok();
        // 64 MB page cache (negative = KiB) and in-memory temp tables.
        diesel::sql_query("PRAGMA cache_size=-65536;")
            .execute(&mut conn)
            .ok();
        diesel::sql_query("PRAGMA temp_store=MEMORY;")
            .execute(&mut conn)
            .ok();
        // A side connection (`with_read_conn`, a backup) can hold the file
        // briefly; wait for it instead of failing with SQLITE_BUSY.
        diesel::sql_query("PRAGMA busy_timeout=5000;")
            .execute(&mut conn)
            .ok();

        conn.run_pending_migrations(MIGRATIONS)
            .map_err(|e| anyhow::anyhow!("migration failed: {e}"))?;

        // Heal any schema drift left over from past migration mishaps
        // (e.g. the 00000000000002_* collision that left some DBs
        // missing project columns). Runs after diesel migrations so a
        // freshly-created DB always lands here as a no-op.
        repair::ensure_schema(&mut conn)?;

        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
            data_dir: Some(data_dir.to_path_buf()),
            slow: Arc::default(),
            readers: Some(Arc::new(ReadPool {
                url: db_url,
                idle: Mutex::new(Vec::new()),
            })),
        })
    }

    /// Create an in-memory database for testing. Runs all migrations.
    pub fn in_memory() -> anyhow::Result<Self> {
        let mut conn = SqliteConnection::establish(":memory:")
            .map_err(|e| anyhow::anyhow!("failed to open in-memory database: {e}"))?;

        diesel::sql_query("PRAGMA foreign_keys=ON;")
            .execute(&mut conn)
            .ok();

        conn.run_pending_migrations(MIGRATIONS)
            .map_err(|e| anyhow::anyhow!("migration failed: {e}"))?;

        repair::ensure_schema(&mut conn)?;

        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
            data_dir: None,
            slow: Arc::default(),
            readers: None,
        })
    }

    /// The data directory this database lives in, when file-backed.
    pub fn data_dir(&self) -> Option<&Path> {
        self.data_dir.as_deref()
    }

    /// Identity of the underlying connection, shared by every clone — lets
    /// per-database caches key on "this database" (tests open many).
    pub fn instance_key(&self) -> usize {
        Arc::as_ptr(&self.conn) as usize
    }

    /// The slowest call sites since this database was opened.
    pub fn slow_ops(&self) -> &slow_ops::SlowOps {
        &self.slow
    }

    /// Run a closure with access to the underlying Diesel connection.
    ///
    /// Moves work onto a blocking thread so it is safe to call from async code.
    /// Not an `async fn` so that `#[track_caller]` can name the call site in
    /// the slow-call tally; callers `.await` it exactly as before.
    #[track_caller]
    pub fn with_conn<F, T>(
        &self,
        f: F,
    ) -> impl Future<Output = anyhow::Result<T>> + Send + use<F, T>
    where
        F: FnOnce(&mut SqliteConnection) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        self.with_conn_at(std::panic::Location::caller(), f)
    }

    /// [`Db::with_conn`] with the call site passed in, for callers that
    /// captured it themselves ([`Db::read`]'s in-memory fallback).
    fn with_conn_at<F, T>(
        &self,
        site: &'static std::panic::Location<'static>,
        f: F,
    ) -> impl Future<Output = anyhow::Result<T>> + Send + use<F, T>
    where
        F: FnOnce(&mut SqliteConnection) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        let slow = self.slow.clone();
        async move {
            let queued = std::time::Instant::now();
            tokio::task::spawn_blocking(move || {
                let mut guard = conn
                    .lock()
                    .map_err(|e| anyhow::anyhow!("db lock poisoned: {e}"))?;
                let started = std::time::Instant::now();
                let out = f(&mut *guard);
                drop(guard);
                slow.record(site, started - queued, started.elapsed());
                out
            })
            .await?
        }
    }

    /// Synchronous twin of [`Db::with_conn`]: locks the connection and runs
    /// `f` on the calling thread, with no `spawn_blocking` and no `.await`.
    ///
    /// This is the bridge for synchronous callers that cannot enter the async
    /// runtime — notably the WASM plugin host functions (`src/plugin/host.rs`),
    /// which run inside a synchronous extism `Plugin::call` on a tokio worker
    /// thread. Calling `with_conn` (or `Handle::block_on`) from there would
    /// panic ("cannot block the current thread from within a runtime"); locking
    /// the inner `std::sync::Mutex` directly does not touch the runtime at all.
    ///
    /// The closure shares the same connection mutex as every `with_conn` call,
    /// so a long query here serializes other DB users for its duration — the
    /// same cost as any normal DB access. It never reenters the runtime and
    /// never holds a plugin lock, so there is no deadlock path.
    #[track_caller]
    pub fn with_conn_blocking<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&mut SqliteConnection) -> anyhow::Result<T>,
    {
        let site = std::panic::Location::caller();
        let queued = std::time::Instant::now();
        let mut guard = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("db lock poisoned: {e}"))?;
        let started = std::time::Instant::now();
        let out = f(&mut *guard);
        drop(guard);
        self.slow.record(site, started - queued, started.elapsed());
        out
    }

    /// Run a read-only closure on a pooled read connection instead of the
    /// shared write connection: it neither waits for a write in progress nor
    /// makes anyone wait, however long it runs. It sees every write that
    /// already returned to its caller. **Only for closures that never
    /// write** — the connection is `query_only`, so an INSERT / UPDATE /
    /// DELETE through it fails. An in-memory database has no second
    /// connection to open, so there this is [`Db::with_conn`].
    #[track_caller]
    pub fn read<F, T>(&self, f: F) -> impl Future<Output = anyhow::Result<T>> + Send + use<F, T>
    where
        F: FnOnce(&mut SqliteConnection) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let site = std::panic::Location::caller();
        let readers = self.readers.clone();
        let db = self.clone();
        async move {
            match readers {
                Some(pool) => tokio::task::spawn_blocking(move || pool.run(f)).await?,
                None => db.with_conn_at(site, f).await,
            }
        }
    }

    /// Synchronous twin of [`Db::read`], for the callers [`Db::with_conn_blocking`]
    /// exists for (plugin host functions).
    #[track_caller]
    pub fn read_blocking<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&mut SqliteConnection) -> anyhow::Result<T>,
    {
        match &self.readers {
            Some(pool) => pool.run(f),
            None => self.with_conn_blocking(f),
        }
    }
}
