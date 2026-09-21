//! `server` capability: start / stop / restart / inspect an owner-defined
//! managed process.
//!
//! This builds on the same shell-spawn primitive as [`crate::terminal`],
//! but — crucially — the server can only control processes the machine
//! owner pre-declared in [`Config::servers`] by name. A remote request
//! names a *server key*, never a command line, so `server` can't be turned
//! into an arbitrary-exec `terminal` back door.
//!
//! Each running process gets its stdout+stderr tailed into a bounded log
//! ring so `logs` can return recent output without a live stream. State
//! (which servers are running, their rings) lives in the single shared
//! [`ServerExecutor`] instance held by the registry.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;

use crate::config::{Config, ManagedServer};
use crate::executor::{CapabilityExecutor, ExecContext};
use crate::terminal::shell_command;
use std::collections::HashMap;

/// Trailing log lines kept per running server.
const LOG_RING: usize = 500;

#[derive(Debug, Deserialize)]
struct ServerArgs {
    /// One of start / stop / restart / status / logs / health.
    action: String,
    /// The managed-server key from [`Config::servers`]. Required for every
    /// action except a fleet-wide `status`.
    #[serde(default)]
    name: Option<String>,
    /// For `logs`: how many trailing lines to return (default all kept).
    #[serde(default)]
    lines: Option<usize>,
}

/// A process this daemon started and still tracks.
struct RunningServer {
    child: Child,
    logs: Arc<Mutex<VecDeque<String>>>,
}

/// Controls the owner-configured managed processes. One instance is shared
/// across all `server` dispatches so process state persists between
/// requests.
pub struct ServerExecutor {
    config: Arc<Config>,
    running: Mutex<HashMap<String, RunningServer>>,
}

impl ServerExecutor {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            running: Mutex::new(HashMap::new()),
        }
    }

    /// Look up a configured server by key, or a clear error if unknown.
    fn configured(&self, name: &str) -> Result<ManagedServer, String> {
        self.config
            .servers
            .get(name)
            .cloned()
            .ok_or_else(|| format!("no managed server named '{name}' is configured"))
    }

    /// Whether `name` is currently tracked as running (reaping it first if
    /// it has already exited).
    fn is_running(&self, name: &str) -> bool {
        let mut map = self.running.lock().unwrap();
        if let Some(rs) = map.get_mut(name) {
            match rs.child.try_wait() {
                Ok(Some(_)) => {
                    // Exited on its own; forget it so start works again.
                    map.remove(name);
                    false
                }
                Ok(None) => true,
                Err(_) => true,
            }
        } else {
            false
        }
    }

    async fn start(&self, name: &str) -> Result<Value, String> {
        let cfg = self.configured(name)?;
        if self.is_running(name) {
            return Err(format!("server '{name}' is already running"));
        }
        let mut child = shell_command(&cfg.command, cfg.cwd.as_deref())
            .spawn()
            .map_err(|e| format!("failed to start server '{name}': {e}"))?;

        let logs = Arc::new(Mutex::new(VecDeque::with_capacity(LOG_RING)));
        if let Some(stdout) = child.stdout.take() {
            spawn_log_reader(stdout, logs.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_log_reader(stderr, logs.clone());
        }
        let pid = child.id();
        self.running
            .lock()
            .unwrap()
            .insert(name.to_string(), RunningServer { child, logs });
        Ok(json!({"action": "start", "name": name, "running": true, "pid": pid}))
    }

    async fn stop(&self, name: &str) -> Result<Value, String> {
        // Ensure it's a known key even when not running, for a clear error.
        self.configured(name)?;
        let entry = self.running.lock().unwrap().remove(name);
        match entry {
            Some(mut rs) => {
                let _ = rs.child.start_kill();
                let _ = rs.child.wait().await;
                Ok(json!({"action": "stop", "name": name, "running": false}))
            }
            None => Err(format!("server '{name}' is not running")),
        }
    }

    async fn restart(&self, name: &str) -> Result<Value, String> {
        // Stop is a no-op-safe best effort; a not-running server still restarts.
        if self.is_running(name) {
            self.stop(name).await?;
        } else {
            self.configured(name)?;
        }
        let mut out = self.start(name).await?;
        if let Some(o) = out.as_object_mut() {
            o.insert("action".to_string(), json!("restart"));
        }
        Ok(out)
    }

    fn status(&self, name: Option<&str>) -> Result<Value, String> {
        match name {
            Some(name) => {
                self.configured(name)?;
                Ok(json!({"action": "status", "name": name, "running": self.is_running(name)}))
            }
            None => {
                // Fleet status across every configured server.
                let servers: Vec<Value> = self
                    .config
                    .servers
                    .keys()
                    .map(|k| json!({"name": k, "running": self.is_running(k)}))
                    .collect();
                Ok(json!({"action": "status", "servers": servers}))
            }
        }
    }

    fn logs(&self, name: &str, lines: Option<usize>) -> Result<Value, String> {
        self.configured(name)?;
        let map = self.running.lock().unwrap();
        let rs = map
            .get(name)
            .ok_or_else(|| format!("server '{name}' is not running"))?;
        let ring = rs.logs.lock().unwrap();
        let take = lines.unwrap_or(ring.len()).min(ring.len());
        let start = ring.len() - take;
        let tail: Vec<&String> = ring.iter().skip(start).collect();
        Ok(json!({
            "action": "logs",
            "name": name,
            "lines": tail,
        }))
    }

    async fn health(&self, name: &str) -> Result<Value, String> {
        let cfg = self.configured(name)?;
        match cfg.health_command {
            Some(cmd) => {
                // Exit 0 ⇒ healthy. Output is discarded; only the code matters.
                let status = shell_command(&cmd, cfg.cwd.as_deref())
                    .status()
                    .await
                    .map_err(|e| format!("health command for '{name}' failed to run: {e}"))?;
                Ok(json!({
                    "action": "health",
                    "name": name,
                    "healthy": status.success(),
                    "exit_code": status.code(),
                    "checked_via": "health_command",
                }))
            }
            None => {
                // No explicit probe: healthy iff the managed process is up.
                Ok(json!({
                    "action": "health",
                    "name": name,
                    "healthy": self.is_running(name),
                    "checked_via": "running_state",
                }))
            }
        }
    }
}

#[async_trait]
impl CapabilityExecutor for ServerExecutor {
    async fn execute(&self, _ctx: &ExecContext, args: Value) -> Result<Value, String> {
        let args: ServerArgs =
            serde_json::from_value(args).map_err(|e| format!("invalid server args: {e}"))?;
        let need_name = || {
            args.name
                .clone()
                .ok_or_else(|| format!("action '{}' requires a server name", args.action))
        };
        match args.action.as_str() {
            "start" => self.start(&need_name()?).await,
            "stop" => self.stop(&need_name()?).await,
            "restart" => self.restart(&need_name()?).await,
            "status" => self.status(args.name.as_deref()),
            "logs" => self.logs(&need_name()?, args.lines),
            "health" => self.health(&need_name()?).await,
            other => Err(format!(
                "unknown server action '{other}' (want start/stop/restart/status/logs/health)"
            )),
        }
    }
}

/// Tail a child stream into a bounded log ring, dropping the oldest line
/// when full. Ends when the pipe closes.
fn spawn_log_reader<R>(reader: R, ring: Arc<Mutex<VecDeque<String>>>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut r = ring.lock().unwrap();
            if r.len() == LOG_RING {
                r.pop_front();
            }
            r.push_back(line);
        }
    });
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::executor::tests_support::collecting_ctx;
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn config_with(name: &str, server: ManagedServer) -> Arc<Config> {
        let mut servers = BTreeMap::new();
        servers.insert(name.to_string(), server);
        Arc::new(Config {
            servers,
            ..Default::default()
        })
    }

    async fn call(exec: &ServerExecutor, args: Value) -> Result<Value, String> {
        let (ctx, _rx) = collecting_ctx();
        exec.execute(&ctx, args).await
    }

    #[tokio::test]
    async fn start_status_logs_stop_round_trip() {
        let cfg = config_with(
            "web",
            ManagedServer {
                // Emit a line, then idle so it stays "running".
                command: "echo booted; sleep 30".to_string(),
                cwd: None,
                health_command: None,
            },
        );
        let exec = ServerExecutor::new(cfg);

        let started = call(&exec, json!({"action": "start", "name": "web"}))
            .await
            .unwrap();
        assert_eq!(started["running"], true);

        let status = call(&exec, json!({"action": "status", "name": "web"}))
            .await
            .unwrap();
        assert_eq!(status["running"], true);

        // Give the reader a moment to capture the first line.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let logs = call(&exec, json!({"action": "logs", "name": "web"}))
            .await
            .unwrap();
        let lines = logs["lines"].as_array().unwrap();
        assert!(
            lines.iter().any(|l| l == "booted"),
            "expected 'booted' in {lines:?}"
        );

        let stopped = call(&exec, json!({"action": "stop", "name": "web"}))
            .await
            .unwrap();
        assert_eq!(stopped["running"], false);

        let after = call(&exec, json!({"action": "status", "name": "web"}))
            .await
            .unwrap();
        assert_eq!(after["running"], false);
    }

    #[tokio::test]
    async fn double_start_is_rejected() {
        let cfg = config_with(
            "svc",
            ManagedServer {
                command: "sleep 30".to_string(),
                cwd: None,
                health_command: None,
            },
        );
        let exec = ServerExecutor::new(cfg);
        call(&exec, json!({"action": "start", "name": "svc"}))
            .await
            .unwrap();
        let err = call(&exec, json!({"action": "start", "name": "svc"}))
            .await
            .unwrap_err();
        assert!(err.contains("already running"), "got: {err}");
        let _ = call(&exec, json!({"action": "stop", "name": "svc"})).await;
    }

    #[tokio::test]
    async fn unknown_server_name_errors() {
        let exec = ServerExecutor::new(Arc::new(Config::default()));
        let err = call(&exec, json!({"action": "start", "name": "ghost"}))
            .await
            .unwrap_err();
        assert!(
            err.contains("no managed server named 'ghost'"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn health_command_reports_exit_code() {
        let cfg = config_with(
            "db",
            ManagedServer {
                command: "sleep 30".to_string(),
                cwd: None,
                health_command: Some("true".to_string()),
            },
        );
        let exec = ServerExecutor::new(cfg);
        let health = call(&exec, json!({"action": "health", "name": "db"}))
            .await
            .unwrap();
        assert_eq!(health["healthy"], true);
        assert_eq!(health["checked_via"], "health_command");
    }

    #[tokio::test]
    async fn health_without_command_uses_running_state() {
        let cfg = config_with(
            "cache",
            ManagedServer {
                command: "sleep 30".to_string(),
                cwd: None,
                health_command: None,
            },
        );
        let exec = ServerExecutor::new(cfg);
        // Not started yet ⇒ unhealthy.
        let before = call(&exec, json!({"action": "health", "name": "cache"}))
            .await
            .unwrap();
        assert_eq!(before["healthy"], false);
        assert_eq!(before["checked_via"], "running_state");
    }

    #[tokio::test]
    async fn unknown_action_errors() {
        let exec = ServerExecutor::new(Arc::new(Config::default()));
        let err = call(&exec, json!({"action": "frobnicate", "name": "x"}))
            .await
            .unwrap_err();
        assert!(err.contains("unknown server action"), "got: {err}");
    }
}
