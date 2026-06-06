pub mod docs;
pub mod events;

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

        conn.run_pending_migrations(MIGRATIONS)
            .map_err(|e| anyhow::anyhow!("migration failed: {e}"))?;

        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run a closure with access to the underlying Diesel connection.
    ///
    /// Moves work onto a blocking thread so it is safe to call from async code.
    pub async fn with_conn<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&mut SqliteConnection) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("db lock poisoned: {e}"))?;
            f(&mut *guard)
        })
        .await?
    }
}
