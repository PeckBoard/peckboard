use super::*;
use crate::db::models::NewUser;
use crate::plugin::ssh::test_support::LocalSshd;
use async_trait::async_trait;

/// Resolves every host to one fixed connection (a throwaway local sshd).
struct FixedResolver(Result<serde_json::Value, String>);

#[async_trait]
impl HostResolver for FixedResolver {
    async fn resolve(&self, _plugin_id: &str, _host_id: &str) -> Result<ResolvedHost, String> {
        self.0.clone().map(|conn| ResolvedHost {
            conn,
            label: "local".into(),
            key_ref_allowed: false,
        })
    }
    async fn list_hosts(&self) -> Vec<HostEntry> {
        Vec::new()
    }
}

async fn seed(db: &Db) -> TerminalRow {
    let now = chrono::Utc::now().to_rfc3339();
    db.create_user(NewUser {
        id: "u1".into(),
        username: "alice".into(),
        email: None,
        password_hash: "x".into(),
        role: "admin".into(),
        created_at: now.clone(),
        updated_at: now.clone(),
    })
    .await
    .unwrap();
    let id = uuid::Uuid::new_v4().simple().to_string();
    db.insert_terminal(TerminalRow {
        tmux_session: format!("peck-{id}"),
        id,
        user_id: "u1".into(),
        plugin_id: "ssh-fleet".into(),
        host_id: "h1".into(),
        name: "local".into(),
        host_label: "me@127.0.0.1:22".into(),
        persistent: false,
        created_at: now.clone(),
        last_active_at: now,
        closed_at: None,
    })
    .await
    .unwrap()
}

async fn wait_phase(term: &TermSession, phase: Phase, secs: u64) {
    let mut rx = term.status_rx();
    let ok = tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            if rx.borrow_and_update().phase == phase {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    })
    .await;
    assert!(
        ok.is_ok() && term.status().phase == phase,
        "wanted {phase:?}, status is {:?}",
        term.status()
    );
}

/// Type `command` and wait for `needle` in the output stream.
async fn run_and_expect(term: &TermSession, command: &str, needle: &str) {
    let (_, mut rx) = term.attach_output();
    assert!(
        term.input(Bytes::from(format!("{command}\r"))),
        "shell not live"
    );
    let mut seen = Vec::new();
    let found = tokio::time::timeout(Duration::from_secs(15), async {
        while let Ok(chunk) = rx.recv().await {
            seen.extend_from_slice(&chunk);
            if String::from_utf8_lossy(&seen).contains(needle) {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(
        found,
        "never saw {needle:?}; got:\n{}",
        String::from_utf8_lossy(&seen)
    );
}

#[allow(clippy::disallowed_methods)] // test probe for the tmux binary, not an agent program
fn tmux_available() -> bool {
    std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[allow(clippy::disallowed_methods)] // test cleanup of the test's own tmux server
fn tmux(socket: &str, args: &[&str]) -> bool {
    std::process::Command::new("tmux")
        .args(["-L", socket])
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn scrollback_keeps_only_the_newest_cap_bytes() {
    let mut sb = Scrollback::new(8);
    sb.push(b"abcd");
    sb.push(b"efgh");
    assert_eq!(sb.snapshot(), b"abcdefgh");
    sb.push(b"ij");
    assert_eq!(sb.snapshot(), b"cdefghij");
    sb.push(b"0123456789");
    assert_eq!(sb.snapshot(), b"23456789");
    assert_eq!(sb.len(), 8);
}

#[tokio::test]
async fn first_connect_failure_is_an_error_not_a_retry_loop() {
    let db = Db::in_memory().unwrap();
    let row = seed(&db).await;
    let dir = tempfile::tempdir().unwrap();
    let mgr = TerminalManager::new(
        db,
        dir.path().to_path_buf(),
        Arc::new(FixedResolver(Err("host 'h1' not found".into()))),
    );
    let (term, _viewer) = mgr.attach(&row, 80, 24);
    wait_phase(&term, Phase::Error, 5).await;
    assert_eq!(
        term.status().message.as_deref(),
        Some("host 'h1' not found")
    );
    assert!(
        !term.input(Bytes::from_static(b"x")),
        "no shell to type into"
    );
}

/// The whole persistence story against a real sshd + tmux: a shell
/// variable set in the terminal survives (1) a dropped connection, which
/// the driver reconnects on its own while a viewer is attached, and (2) a
/// simulated Peckboard restart — a brand-new manager over the same DB finds
/// the terminal row and reattaches the same tmux session. Closing kills it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnects_and_reattaches_tmux_across_restart() {
    let Some(sshd) = LocalSshd::spawn() else {
        return;
    };
    if !tmux_available() {
        eprintln!("SKIP: tmux not installed");
        return;
    }
    let db = Db::in_memory().unwrap();
    let row = seed(&db).await;
    let socket = format!("pecktest-{}", &row.id[..12]);
    let resolver: Arc<dyn HostResolver> = Arc::new(FixedResolver(Ok(sshd.base_conn())));
    let data_dir = sshd.dir().to_path_buf();

    let mgr =
        TerminalManager::with_tmux_socket(db.clone(), data_dir.clone(), resolver.clone(), &socket);
    let (term, viewer) = mgr.attach(&row, 100, 30);
    wait_phase(&term, Phase::Live, 20).await;
    assert_eq!(term.status().persistent, Some(true), "tmux was found");
    run_and_expect(&term, "export PECK_T=kept", "").await;
    run_and_expect(&term, "echo \"a-$((6*7))-$PECK_T\"", "a-42-kept").await;

    // (1) Network drop: the driver reconnects by itself and reattaches.
    assert!(term.send(TermCmd::DropConnection));
    wait_phase(&term, Phase::Reconnecting, 10).await;
    wait_phase(&term, Phase::Live, 30).await;
    run_and_expect(&term, "echo \"b-$((6*7))-$PECK_T\"", "b-42-kept").await;

    // (2) Restart: the process dies (connection drops, tmux stays), and a
    // fresh manager over the same DB picks the terminal back up.
    term.closing.store(true, Ordering::SeqCst);
    assert!(term.send(TermCmd::DropConnection));
    drop(viewer);
    drop(mgr);
    let rows = db.list_terminals(Some("u1")).await.unwrap();
    assert_eq!(rows.len(), 1, "the terminal row outlives the process");
    let row = rows.into_iter().next().unwrap();
    assert!(
        row.persistent,
        "persistence learned on first connect is stored"
    );

    let mgr2 = TerminalManager::with_tmux_socket(db.clone(), data_dir, resolver, &socket);
    assert_eq!(mgr2.status_of(&row).phase, Phase::Idle);
    let (term2, _viewer2) = mgr2.attach(&row, 100, 30);
    wait_phase(&term2, Phase::Live, 20).await;
    run_and_expect(&term2, "echo \"c-$((6*7))-$PECK_T\"", "c-42-kept").await;

    // Close ends the tmux session for good.
    assert!(db.close_terminal(&row.id).await.unwrap());
    mgr2.close(&row).await;
    let gone = tokio::time::timeout(Duration::from_secs(10), async {
        while tmux(&socket, &["has-session", "-t", &row.tmux_session]) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    tmux(&socket, &["kill-server"]);
    assert!(gone.is_ok(), "closing killed the tmux session");
    assert!(db.list_terminals(Some("u1")).await.unwrap().is_empty());
}
