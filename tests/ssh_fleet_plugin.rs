//! Integration test for the **ssh-fleet WASM plugin** against the real core
//! host functions.
//!
//! The hermetic half (always runs when the wasm is built) loads the plugin —
//! which alone proves core exposes every host import it declares, including the
//! four `peckboard_ssh_*` functions — then drives the registry tools
//! (`ssh_host_add/list/update/remove`) through the real `data_store` and
//! asserts credentials are redacted out of every tool result.
//!
//! The SSH half (gated on a local `sshd` + `ssh-keygen`) stands up a
//! self-contained OpenSSH server on an ephemeral port and drives the full
//! plugin → core → sshd chain: `ssh_probe`, `ssh_run`, and SFTP
//! write/read/edit. It skips cleanly when OpenSSH is unavailable.
//!
//! The wasm is built out-of-tree (`peck-plugins/ssh-fleet/build.sh`); this
//! test **skips** with a note when the artifact is absent.

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

use peckboard::db::Db;
use peckboard::db::models::{NewFolder, NewProject, NewSession};
use peckboard::plugin::manager::PluginManager;
use serde_json::{Value, json};

const PLUGIN_ID: &str = "ssh-fleet";

fn plugin_wasm() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../peck-plugins/ssh-fleet/dist/plugin.wasm");
    p.exists().then_some(p)
}

async fn invoke(plugins: &PluginManager, tool: &str, args: Value, ctx: &Value) -> Value {
    plugins
        .invoke_mcp_tool(tool, args, ctx.clone())
        .await
        .expect("plugin should own this tool")
        .unwrap_or_else(|e| panic!("{tool} failed: {e}"))
}

// ── optional self-contained OpenSSH server ───────────────────────────────────

struct TestSshd {
    port: u16,
    key_pem: String,
    dir_path: PathBuf,
    sftp: bool,
    _dir: tempfile::TempDir,
    child: Child,
}

impl Drop for TestSshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn first_existing(cands: &[&str]) -> Option<PathBuf> {
    cands.iter().map(PathBuf::from).find(|p| p.exists())
}

fn setup_test_sshd() -> Option<TestSshd> {
    let sshd_bin = first_existing(&["/usr/sbin/sshd", "/usr/bin/sshd", "/sbin/sshd"])?;
    let keygen = first_existing(&["/usr/bin/ssh-keygen", "/bin/ssh-keygen"])?;
    let sftp = first_existing(&[
        "/usr/lib/openssh/sftp-server",
        "/usr/libexec/openssh/sftp-server",
        "/usr/libexec/sftp-server",
        "/usr/lib/ssh/sftp-server",
    ]);

    let dir = tempfile::tempdir().ok()?;
    let dp = dir.path().to_path_buf();
    let hostkey = dp.join("hostkey");
    let clientkey = dp.join("id");
    let authkeys = dp.join("authorized_keys");
    let config = dp.join("sshd_config");
    let logfile = dp.join("sshd.log");

    for path in [&hostkey, &clientkey] {
        let ok = Command::new(&keygen)
            .args(["-t", "ed25519", "-N", "", "-q", "-f"])
            .arg(path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return None;
        }
    }
    let pubkey = std::fs::read(clientkey.with_extension("pub")).ok()?;
    std::fs::write(&authkeys, &pubkey).ok()?;
    let key_pem = std::fs::read_to_string(&clientkey).ok()?;

    let port = TcpListener::bind("127.0.0.1:0")
        .ok()?
        .local_addr()
        .ok()?
        .port();
    let mut cfg = format!(
        "Port {port}\nListenAddress 127.0.0.1\nHostKey {hk}\nAuthorizedKeysFile {ak}\n\
StrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n\
PubkeyAuthentication yes\nLogLevel ERROR\n",
        hk = hostkey.display(),
        ak = authkeys.display(),
    );
    if let Some(s) = &sftp {
        cfg.push_str(&format!("Subsystem sftp {}\n", s.display()));
    }
    std::fs::write(&config, cfg).ok()?;

    let child = Command::new(&sshd_bin)
        .arg("-D")
        .arg("-f")
        .arg(&config)
        .arg("-E")
        .arg(&logfile)
        .spawn()
        .ok()?;

    let sshd = TestSshd {
        port,
        key_pem,
        dir_path: dp,
        sftp: sftp.is_some(),
        _dir: dir,
        child,
    };
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Some(sshd);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None // never came up; dropping `sshd` kills the child
}

// ── Test ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn ssh_fleet_plugin_end_to_end() {
    let Some(wasm) = plugin_wasm() else {
        eprintln!(
            "SKIP ssh_fleet_plugin_end_to_end: plugin wasm not built \
             (run peck-plugins/ssh-fleet/build.sh)"
        );
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path();
    let plugins_dir = data_dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::copy(&wasm, plugins_dir.join(format!("{PLUGIN_ID}.wasm"))).unwrap();

    let db = Db::open(data_dir).unwrap();
    let ts = chrono::Utc::now().to_rfc3339();
    db.create_folder(NewFolder {
        id: "f1".into(),
        name: "Test folder".into(),
        path: data_dir.to_string_lossy().to_string(),
        created_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_project(NewProject {
        id: "proj-1".into(),
        name: "Test project".into(),
        context: String::new(),
        folder_id: "f1".into(),
        worker_count: 1,
        status: "active".into(),
        workflow: "fast-develop-software".into(),
        model: None,
        effort: None,
        budget_usd_cents: None,
        budget_period: None,
        worktree_isolation: false,
        parallel_instructions: false,
        auto_notify_changes: false,
        worker_communication: false,
        created_at: ts.clone(),
        last_accessed_at: ts.clone(),
    })
    .await
    .unwrap();
    db.create_session(NewSession {
        id: "caller-1".into(),
        name: "Caller".into(),
        folder_id: "f1".into(),
        project_id: Some("proj-1".into()),
        is_worker: true,
        created_at: ts.clone(),
        last_activity: ts,
        ..Default::default()
    })
    .await
    .unwrap();

    let plugins = PluginManager::new(data_dir, db.clone());
    plugins.load_all().await.unwrap();
    let info = plugins
        .decide(PLUGIN_ID, true)
        .await
        .unwrap()
        .expect("ssh-fleet plugin should be loaded");
    assert_eq!(info.status, "approved", "plugin must be active: {info:?}");

    let ctx = json!({ "sessionId": "caller-1", "projectId": "proj-1", "folderId": "f1" });

    // ── hermetic registry CRUD + redaction ───────────────────────────────────
    let res = invoke(&plugins, "ssh_host_list", json!({}), &ctx).await;
    assert_eq!(res["count"], json!(0), "empty list: {res}");

    let res = invoke(
        &plugins,
        "ssh_host_add",
        json!({"label":"box","hostname":"example.com","username":"root","password":"s3cret","tags":["prod"]}),
        &ctx,
    )
    .await;
    let host = &res["host"];
    assert_eq!(host["label"], json!("box"));
    assert_eq!(host["auth_kind"], json!("password"));
    assert_eq!(host["has_secret"], json!(true));
    assert!(
        host.get("password").is_none(),
        "redacted view must omit password: {host}"
    );
    assert!(
        !res.to_string().contains("s3cret"),
        "secret must never leak in a tool result: {res}"
    );
    let id = host["id"].as_str().unwrap().to_string();

    assert_eq!(
        invoke(&plugins, "ssh_host_list", json!({}), &ctx).await["count"],
        json!(1)
    );

    let res = invoke(
        &plugins,
        "ssh_host_update",
        json!({"id": id, "label": "renamed"}),
        &ctx,
    )
    .await;
    assert_eq!(res["host"]["label"], json!("renamed"), "update: {res}");

    let res = invoke(
        &plugins,
        "ssh_host_remove",
        json!({"host": "renamed"}),
        &ctx,
    )
    .await;
    assert_eq!(res["removed"], json!(id), "remove: {res}");
    assert_eq!(
        invoke(&plugins, "ssh_host_list", json!({}), &ctx).await["count"],
        json!(0)
    );

    // ── full SSH chain (gated on a local sshd) ────────────────────────────────
    let Some(sshd) = setup_test_sshd() else {
        eprintln!("SKIP ssh chain: no local sshd/ssh-keygen available");
        return;
    };
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default();
    if user.is_empty() {
        eprintln!("SKIP ssh chain: no USER/LOGNAME");
        return;
    }

    let add = invoke(
        &plugins,
        "ssh_host_add",
        json!({
            "label": "local",
            "hostname": "127.0.0.1",
            "port": sshd.port,
            "username": user,
            "private_key": sshd.key_pem.clone(),
        }),
        &ctx,
    )
    .await;
    assert_eq!(add["host"]["auth_kind"], json!("key"), "key host: {add}");

    let probe = invoke(&plugins, "ssh_probe", json!({"host": "local"}), &ctx).await;
    assert_eq!(probe["ok"], json!(true), "probe: {probe}");
    assert!(
        probe["server_fingerprint"]
            .as_str()
            .unwrap_or("")
            .starts_with("SHA256:"),
        "probe fingerprint: {probe}"
    );

    let run = invoke(
        &plugins,
        "ssh_run",
        json!({"host": "local", "command": "echo hi-from-fleet"}),
        &ctx,
    )
    .await;
    assert_eq!(run["exit_code"], json!(0), "run: {run}");
    assert!(
        run["stdout"].as_str().unwrap().contains("hi-from-fleet"),
        "run stdout: {run}"
    );

    // The activity log should now hold the probe + run (proves data_store logging).
    let list = invoke(&plugins, "ssh_host_list", json!({}), &ctx).await;
    assert_eq!(
        list["hosts"][0]["last_status"],
        json!("ok"),
        "host marked reachable: {list}"
    );

    if sshd.sftp {
        let remote = sshd
            .dir_path
            .join("fleet.txt")
            .to_string_lossy()
            .to_string();
        let w = invoke(
            &plugins,
            "ssh_write_file",
            json!({"host": "local", "path": remote, "content": "hello-fleet\n"}),
            &ctx,
        )
        .await;
        assert!(w.get("error").is_none(), "write: {w}");
        assert_eq!(w["bytes"], json!(12), "write bytes: {w}");

        let r = invoke(
            &plugins,
            "ssh_read_file",
            json!({"host": "local", "path": remote}),
            &ctx,
        )
        .await;
        assert_eq!(r["content"], json!("hello-fleet\n"), "read back: {r}");

        let e = invoke(
            &plugins,
            "ssh_edit_file",
            json!({"host": "local", "path": remote, "find": "hello", "replace": "goodbye"}),
            &ctx,
        )
        .await;
        assert_eq!(e["replacements"], json!(1), "edit: {e}");

        let r2 = invoke(
            &plugins,
            "ssh_read_file",
            json!({"host": "local", "path": remote}),
            &ctx,
        )
        .await;
        assert_eq!(
            r2["content"],
            json!("goodbye-fleet\n"),
            "edit applied: {r2}"
        );
    }
}
