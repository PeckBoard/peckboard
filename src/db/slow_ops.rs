//! Which database calls are slow, and which ones wait behind them.
//!
//! Every [`super::Db`] call goes through one shared connection, so a single
//! slow call stalls all the others. This keeps a small in-memory tally per
//! call site (the `file:line` that called `with_conn`), filled only by calls
//! whose wait plus run time reaches [`SLOW_MS`]. Nothing is persisted: it
//! starts empty on every boot and is read by Settings → Data → Database.

use std::collections::HashMap;
use std::panic::Location;
use std::sync::Mutex;
use std::time::Duration;

/// A call is recorded once waiting for the connection plus running took at
/// least this long.
pub const SLOW_MS: u64 = 100;
/// At or above this a call is also logged as a warning.
const WARN_MS: u64 = 1000;

#[derive(Default, Clone, Copy)]
struct Tally {
    count: u64,
    total_run_ms: u64,
    max_run_ms: u64,
    max_wait_ms: u64,
    last_at: i64,
}

/// One call site's slow calls since the server started.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SlowOp {
    /// `src/db/crud/sessions.rs:150`: where `with_conn` was called from.
    pub site: String,
    /// How many calls from this site were slow.
    pub count: u64,
    /// Time those calls held the connection, summed.
    pub total_run_ms: u64,
    /// Longest single hold.
    pub max_run_ms: u64,
    /// Longest time a call from this site waited for the connection.
    pub max_wait_ms: u64,
    /// Unix seconds of the latest slow call.
    pub last_at: i64,
}

pub struct SlowOps {
    started_at: i64,
    by_site: Mutex<HashMap<(&'static str, u32), Tally>>,
}

impl Default for SlowOps {
    fn default() -> Self {
        Self {
            started_at: chrono::Utc::now().timestamp(),
            by_site: Mutex::new(HashMap::new()),
        }
    }
}

impl SlowOps {
    /// Unix seconds this tally started at (server start).
    pub fn started_at(&self) -> i64 {
        self.started_at
    }

    /// Note one finished call. Cheap for a fast call: two comparisons.
    pub fn record(&self, site: &'static Location<'static>, wait: Duration, run: Duration) {
        let (wait_ms, run_ms) = (wait.as_millis() as u64, run.as_millis() as u64);
        if wait_ms + run_ms < SLOW_MS {
            return;
        }
        if run_ms >= WARN_MS {
            tracing::warn!(
                site = %format_args!("{}:{}", site.file(), site.line()),
                run_ms,
                wait_ms,
                "Slow database call held the shared connection"
            );
        }
        let Ok(mut map) = self.by_site.lock() else {
            return;
        };
        let tally = map.entry((site.file(), site.line())).or_default();
        tally.count += 1;
        tally.total_run_ms += run_ms;
        tally.max_run_ms = tally.max_run_ms.max(run_ms);
        tally.max_wait_ms = tally.max_wait_ms.max(wait_ms);
        tally.last_at = chrono::Utc::now().timestamp();
    }

    /// The `limit` call sites that held the connection longest in total,
    /// then those that waited longest.
    pub fn top(&self, limit: usize) -> Vec<SlowOp> {
        let Ok(map) = self.by_site.lock() else {
            return Vec::new();
        };
        let mut ops: Vec<SlowOp> = map
            .iter()
            .map(|((file, line), t)| SlowOp {
                site: format!("{file}:{line}"),
                count: t.count,
                total_run_ms: t.total_run_ms,
                max_run_ms: t.max_run_ms,
                max_wait_ms: t.max_wait_ms,
                last_at: t.last_at,
            })
            .collect();
        ops.sort_by(|a, b| {
            b.total_run_ms
                .cmp(&a.total_run_ms)
                .then(b.max_wait_ms.cmp(&a.max_wait_ms))
                .then(a.site.cmp(&b.site))
        });
        ops.truncate(limit);
        ops
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    /// A call that holds the connection is recorded under the line that
    /// called `with_conn`, and a call queued behind it records its wait.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_slow_call_and_the_call_waiting_behind_it_are_recorded() {
        let db = Db::in_memory().unwrap();
        db.with_conn(|_| Ok(())).await.unwrap();
        assert!(
            db.slow_ops().top(10).is_empty(),
            "a fast call leaves no trace"
        );

        let holder = db.with_conn(|_| {
            std::thread::sleep(Duration::from_millis(250));
            Ok(())
        });
        let holder = tokio::spawn(holder);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let waiter = db.with_conn_blocking(|_| Ok(()));
        holder.await.unwrap().unwrap();
        waiter.unwrap();

        let ops = db.slow_ops().top(10);
        assert_eq!(ops.len(), 2, "{ops:?}");
        assert!(ops.iter().all(|o| o.site.starts_with(file!())), "{ops:?}");
        // Sorted by time held: the holder first, then the one that waited.
        assert!(ops[0].max_run_ms >= 250 && ops[0].count == 1, "{ops:?}");
        assert!(
            ops[1].max_wait_ms >= SLOW_MS && ops[1].max_run_ms < SLOW_MS,
            "{ops:?}"
        );
    }

    /// On a file database a pooled read runs while the write connection is
    /// held, sees writes that already returned, and cannot write.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pooled_read_does_not_wait_behind_a_write() {
        use crate::db::models::NewFolder;
        use diesel::prelude::*;

        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open(tmp.path()).unwrap();
        db.create_folder(NewFolder {
            id: "f".into(),
            name: "f".into(),
            path: ".".into(),
            created_at: "now".into(),
        })
        .await
        .unwrap();

        let holder = tokio::spawn(db.with_conn(|_| {
            std::thread::sleep(Duration::from_millis(600));
            Ok(())
        }));
        tokio::time::sleep(Duration::from_millis(50)).await;
        let started = std::time::Instant::now();
        let folders: i64 = db
            .read(|conn| {
                crate::db::schema::folders::table
                    .count()
                    .get_result(conn)
                    .map_err(Into::into)
            })
            .await
            .unwrap();
        assert_eq!(folders, 1, "the read sees the committed folder");
        assert!(db.list_sessions().await.unwrap().is_empty());
        assert!(db.get_session_blocking("nope").unwrap().is_none());
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "reads waited for the write connection: {:?}",
            started.elapsed()
        );
        holder.await.unwrap().unwrap();

        let write = db
            .read(|conn| {
                diesel::sql_query("DELETE FROM folders")
                    .execute(conn)
                    .map_err(Into::into)
            })
            .await;
        assert!(write.is_err(), "a pooled connection must refuse writes");
        assert_eq!(db.list_folders().await.unwrap().len(), 1);
    }
}
