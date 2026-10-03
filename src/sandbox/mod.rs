//! Agent data-dir sandbox.
//!
//! Every agent CLI and everything it runs executes as the server's own OS
//! user, so without this an agent can read `<data_dir>/jwt_secret`, the DB,
//! the vault keys and other sessions' MCP tokens (and forge admin tokens —
//! this was exploited). Every agent-side spawn goes through
//! [`SandboxedCommand`] / [`SandboxedTokioCommand`], which on Linux applies a
//! Landlock domain in `pre_exec`:
//!
//! - the data dir (and Peckboard's / Tailscale's own config dirs) are
//!   unreadable and unwritable, except the session's own provider account
//!   dir and its own MCP config file;
//! - project folders, temp dirs, toolchain caches and provider homes are
//!   writable; everything else is read-only;
//! - the server binary is never writable;
//! - signals to processes outside the agent's own domain (the server) are
//!   refused (Landlock ABI ≥ 6).
//!
//! The domain also turns on `no_new_privs`, so `sudo` cannot work inside it.
//!
//! Modes ([`Mode`]): `enforce` (default on Linux), `warn` (spawn unsandboxed,
//! log once), `off`. When Landlock is unavailable an `enforce` spawn proceeds
//! unsandboxed and the Settings page shows a banner ([`status`]).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

pub mod perms;
pub mod policy;
pub mod settings;

#[cfg(target_os = "linux")]
mod linux;

/// Env var set on every sandboxed child naming the effective mode
/// (`landlock`, `warn`, `off`, `unavailable`) — for diagnostics and tests.
pub const MODE_ENV: &str = "PECKBOARD_AGENT_SANDBOX";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Enforce,
    Warn,
    Off,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "enforce" => Some(Self::Enforce),
            "warn" => Some(Self::Warn),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enforce => "enforce",
            Self::Warn => "warn",
            Self::Off => "off",
        }
    }

    /// `enforce` on Linux (where Landlock exists), `warn` elsewhere.
    pub fn platform_default() -> Self {
        if cfg!(target_os = "linux") {
            Self::Enforce
        } else {
            Self::Warn
        }
    }
}

/// Process-wide sandbox configuration, set once at boot by [`init`] and
/// updated by the settings route.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub data_dir: PathBuf,
    pub mode: Mode,
    /// Settings → "Extra paths agents may write".
    pub extra_rw: Vec<PathBuf>,
}

static GLOBAL: RwLock<Option<SandboxConfig>> = RwLock::new(None);

/// Bind the sandbox to this server's data dir. Until called, sandboxed
/// commands run unconfined (unit tests, headless tools).
pub fn init(cfg: SandboxConfig) {
    let msg = match (cfg.mode, supported()) {
        (Mode::Enforce, true) => None,
        (Mode::Enforce, false) => Some(
            "agent sandbox: Landlock is unavailable on this host — agents run UNSANDBOXED \
             and can read Peckboard's secrets",
        ),
        (Mode::Warn, _) => Some("agent sandbox in WARN mode — agents run unsandboxed"),
        (Mode::Off, _) => Some("agent sandbox is OFF — agents run unsandboxed"),
    };
    if let Some(m) = msg {
        tracing::warn!("{m}");
    } else {
        tracing::info!(abi = abi(), "agent sandbox: Landlock enforced");
    }
    *GLOBAL.write().unwrap_or_else(|p| p.into_inner()) = Some(cfg);
}

/// Change the mode / extra paths at runtime (Settings). New spawns pick it
/// up; running agents keep the domain they started with.
pub fn update(mode: Mode, extra_rw: Vec<PathBuf>) {
    let mut g = GLOBAL.write().unwrap_or_else(|p| p.into_inner());
    if let Some(cfg) = g.as_mut() {
        cfg.mode = mode;
        cfg.extra_rw = extra_rw;
    }
}

pub fn config() -> Option<SandboxConfig> {
    #[cfg(test)]
    if let Some(cfg) = TEST_CONFIG.with(|c| c.borrow().clone()) {
        return Some(cfg);
    }
    GLOBAL.read().unwrap_or_else(|p| p.into_inner()).clone()
}

#[cfg(test)]
thread_local! {
    static TEST_CONFIG: std::cell::RefCell<Option<SandboxConfig>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with `cfg` as this thread's sandbox config, leaving the
/// process-wide one (shared by every concurrently running test) alone.
#[cfg(test)]
pub(crate) fn with_test_config<R>(cfg: SandboxConfig, f: impl FnOnce() -> R) -> R {
    TEST_CONFIG.with(|c| *c.borrow_mut() = Some(cfg));
    let out = f();
    TEST_CONFIG.with(|c| *c.borrow_mut() = None);
    out
}

/// Landlock ABI version (0 = unavailable / not Linux).
pub fn abi() -> i32 {
    #[cfg(target_os = "linux")]
    {
        linux::abi()
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

pub fn supported() -> bool {
    abi() >= 1
}

/// What the Settings page shows.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Status {
    pub mode: Mode,
    pub supported: bool,
    pub abi: i32,
    /// True only when new agent spawns are actually confined.
    pub enforced: bool,
    pub platform: &'static str,
}

pub fn status() -> Status {
    let mode = config()
        .map(|c| c.mode)
        .unwrap_or_else(Mode::platform_default);
    Status {
        mode,
        supported: supported(),
        abi: abi(),
        enforced: mode == Mode::Enforce && supported(),
        platform: std::env::consts::OS,
    }
}

/// Per-spawn additions to the base policy: the folder(s) this agent works
/// in plus narrow exceptions inside the data dir. Exceptions are validated
/// against the data dir when the policy is built, so a caller can't widen
/// the sandbox to an arbitrary data-dir path by mistake.
#[derive(Debug, Default, Clone)]
pub struct SpawnScope {
    rw: Vec<PathBuf>,
    account_dirs: Vec<PathBuf>,
    mcp_configs: Vec<PathBuf>,
}

/// Env keys a provider spawn uses to point its CLI at a stored account's
/// config dir (see `plugin_provider::account_env_for`).
pub const ACCOUNT_DIR_ENV_KEYS: &[&str] = &[
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "GROK_HOME",
    "KIMI_CODE_HOME",
];

impl SpawnScope {
    /// No extra writable paths (read-only everywhere outside the defaults).
    pub fn none() -> Self {
        Self::default()
    }

    /// The agent's working folder, writable.
    pub fn folder(path: impl Into<PathBuf>) -> Self {
        Self::default().rw(path)
    }

    pub fn rw(mut self, path: impl Into<PathBuf>) -> Self {
        let p = path.into();
        if !p.as_os_str().is_empty() {
            self.rw.push(p);
        }
        self
    }

    /// A provider account dir (writable). Honoured only when it lies under
    /// `<data_dir>/<provider>-accounts/`.
    pub fn account_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.account_dirs.push(path.into());
        self
    }

    /// Pick the account dir out of a spawn's env (any of
    /// [`ACCOUNT_DIR_ENV_KEYS`]).
    pub fn account_env<'a, I>(mut self, env: I) -> Self
    where
        I: IntoIterator<Item = (&'a String, &'a String)>,
    {
        for (k, v) in env {
            if ACCOUNT_DIR_ENV_KEYS.contains(&k.as_str()) && !v.is_empty() {
                self.account_dirs.push(PathBuf::from(v));
            }
        }
        self
    }

    /// This session's own MCP config file (read-only). Honoured only under
    /// `<data_dir>/worker-mcp/`.
    pub fn mcp_config(mut self, path: impl Into<PathBuf>) -> Self {
        self.mcp_configs.push(path.into());
        self
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Writable-by-default dirs under `$HOME`: toolchain caches and provider
/// CLI homes. Everything else in `$HOME` (dotfiles, `.ssh`, other repos) is
/// read-only unless added in Settings.
const HOME_RW_DIRS: &[&str] = &[
    ".cargo",
    ".rustup",
    ".npm",
    ".cache",
    ".local",
    ".config",
    ".bun",
    ".deno",
    "go",
    ".gradle",
    ".m2",
    ".pyenv",
    ".nvm",
    ".yarn",
    ".pnpm-store",
    ".node-gyp",
    ".dotnet",
    ".nuget",
    ".claude",
    ".codex",
    ".grok",
    ".kimi",
    ".cursor",
    ".gemini",
    ".ollama",
];
const HOME_RW_FILES: &[&str] = &[".claude.json", ".ssh/known_hosts"];
/// Inside writable `~/.config`, the dirs agents must not touch.
const HOME_DENIED: &[&str] = &[".config/peckboard", ".config/tailscale", ".peckboard"];

fn is_under_canon(path: &Path, root: &Path) -> bool {
    match (std::fs::canonicalize(path), std::fs::canonicalize(root)) {
        (Ok(p), Ok(r)) => p.starts_with(&r),
        _ => false,
    }
}

/// An account dir is accepted only at `<data_dir>/<x>-accounts/<id>[/…]`.
fn valid_account_dir(data_dir: &Path, dir: &Path) -> bool {
    let (Ok(d), Ok(data)) = (std::fs::canonicalize(dir), std::fs::canonicalize(data_dir)) else {
        return false;
    };
    let Ok(rel) = d.strip_prefix(&data) else {
        return false;
    };
    let mut comps = rel.components();
    let first = comps.next().and_then(|c| c.as_os_str().to_str());
    first.is_some_and(|f| f.ends_with("-accounts")) && comps.next().is_some()
}

pub(crate) fn policy_inputs(cfg: &SandboxConfig, scope: &SpawnScope) -> policy::PolicyInputs {
    let mut inputs = policy::PolicyInputs {
        denied: vec![cfg.data_dir.clone()],
        devices: Some(PathBuf::from("/dev")),
        ..Default::default()
    };
    for p in ["/tmp", "/var/tmp", "/dev/shm"] {
        inputs.rw.push(PathBuf::from(p));
    }
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        inputs.rw.push(PathBuf::from(rt));
    }
    if let Some(home) = home_dir() {
        inputs.rw.extend(HOME_RW_DIRS.iter().map(|d| home.join(d)));
        inputs.rw.extend(HOME_RW_FILES.iter().map(|f| home.join(f)));
        inputs
            .denied
            .extend(HOME_DENIED.iter().map(|d| home.join(d)));
    }
    // A folder or extra path inside the data dir would re-open part of it;
    // only the plugin-exec scratch dir (a plugin page's exec cwd) may be.
    let plugin_exec = cfg.data_dir.join("plugin-exec");
    let allowed_rw =
        |p: &&PathBuf| !is_under_canon(p, &cfg.data_dir) || is_under_canon(p, &plugin_exec);
    inputs
        .rw
        .extend(cfg.extra_rw.iter().filter(allowed_rw).cloned());
    inputs
        .rw
        .extend(scope.rw.iter().filter(allowed_rw).cloned());
    inputs.rw.extend(
        scope
            .account_dirs
            .iter()
            .filter(|d| valid_account_dir(&cfg.data_dir, d))
            .cloned(),
    );
    let mcp_dir = cfg.data_dir.join("worker-mcp");
    inputs.ro_files.extend(
        scope
            .mcp_configs
            .iter()
            .filter(|f| is_under_canon(f, &mcp_dir))
            .cloned(),
    );
    if let Ok(exe) = std::env::current_exe() {
        inputs.protected.push(exe);
    }
    inputs
}

fn warn_once(msg: &'static str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!("{msg}");
    }
}

/// Apply the configured sandbox to `cmd`. `None` config = not initialised:
/// leave the command alone.
fn apply(cmd: &mut std::process::Command, scope: &SpawnScope, cfg: Option<&SandboxConfig>) {
    let Some(cfg) = cfg else {
        return;
    };
    match cfg.mode {
        Mode::Off => {
            cmd.env(MODE_ENV, "off");
        }
        Mode::Warn => {
            warn_once("agent sandbox in warn mode: spawning agent processes unsandboxed");
            cmd.env(MODE_ENV, "warn");
        }
        Mode::Enforce => {
            #[cfg(target_os = "linux")]
            if linux::abi() >= 1 {
                use std::os::unix::process::CommandExt;
                let rules = policy::plan(&policy_inputs(cfg, scope), &policy::read_children);
                let built = linux::build_ruleset(&rules);
                cmd.env(MODE_ENV, "landlock");
                match built {
                    Ok(fd) => {
                        let fd = std::sync::Arc::new(fd);
                        // SAFETY: the closure only makes async-signal-safe
                        // syscalls (prctl, landlock_restrict_self); the fd it
                        // uses stays open because the Arc lives in the
                        // closure, which the Command owns until spawn.
                        unsafe {
                            cmd.pre_exec(move || {
                                use std::os::fd::AsRawFd;
                                linux::restrict_self(fd.as_raw_fd())
                            });
                        }
                    }
                    Err(e) => {
                        // Fail closed: never run an agent unconfined because
                        // the ruleset couldn't be built.
                        tracing::error!("agent sandbox: building the Landlock ruleset failed: {e}");
                        let mut err = Some(std::io::Error::other(format!(
                            "agent sandbox could not be applied: {e}"
                        )));
                        // SAFETY: only moves an already-allocated error out.
                        unsafe {
                            cmd.pre_exec(move || {
                                Err(err.take().unwrap_or_else(|| {
                                    std::io::Error::from_raw_os_error(libc::EPERM)
                                }))
                            });
                        }
                    }
                }
                return;
            }
            let _ = scope;
            warn_once("agent sandbox: Landlock unavailable — spawning agent processes UNSANDBOXED");
            cmd.env(MODE_ENV, "unavailable");
        }
    }
}

/// Proof token: bearer holds a `std::process::Command` for an agent-side
/// process with the agent sandbox applied per the configured [`Mode`]. Its
/// only constructors build the sandbox; bare `Command::new` is banned by
/// `clippy.toml` (`disallowed-methods`) outside this module and a few
/// justified server-internal sites. See `PluginProviderRuntime::spawn_json`
/// for an example.
pub struct SandboxedCommand(std::process::Command);

impl SandboxedCommand {
    /// A sandboxed command under the process-wide config.
    pub fn new(program: impl AsRef<OsStr>, scope: &SpawnScope) -> Self {
        Self::with_config(program, scope, config().as_ref())
    }

    /// A sandboxed command under an explicit config (tests).
    pub fn with_config(
        program: impl AsRef<OsStr>,
        scope: &SpawnScope,
        cfg: Option<&SandboxConfig>,
    ) -> Self {
        // The one sanctioned constructor: the sandbox is applied right here.
        #[allow(clippy::disallowed_methods)]
        let mut cmd = std::process::Command::new(program);
        // Default SIGINT/SIGQUIT for the agent tree even when the server
        // inherited them ignored (see the helper).
        crate::provider::turn::reset_child_signals_std(&mut cmd);
        apply(&mut cmd, scope, cfg);
        Self(cmd)
    }

    /// The configured command (sandbox already installed).
    pub fn into_inner(self) -> std::process::Command {
        self.0
    }
}

impl std::ops::Deref for SandboxedCommand {
    type Target = std::process::Command;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for SandboxedCommand {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// [`SandboxedCommand`] for tokio. Same proof-token contract.
pub struct SandboxedTokioCommand(tokio::process::Command);

impl SandboxedTokioCommand {
    pub fn new(program: impl AsRef<OsStr>, scope: &SpawnScope) -> Self {
        Self::with_config(program, scope, config().as_ref())
    }

    pub fn with_config(
        program: impl AsRef<OsStr>,
        scope: &SpawnScope,
        cfg: Option<&SandboxConfig>,
    ) -> Self {
        Self(tokio::process::Command::from(
            SandboxedCommand::with_config(program, scope, cfg).into_inner(),
        ))
    }

    pub fn into_inner(self) -> tokio::process::Command {
        self.0
    }
}

impl std::ops::Deref for SandboxedTokioCommand {
    type Target = tokio::process::Command;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for SandboxedTokioCommand {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
/// `-c` overrides for every git run in a repo an agent can write. An
/// agent-planted `.git/config` (`core.fsmonitor`, `core.hooksPath`,
/// `core.pager`) or hook would otherwise execute whenever git runs there.
/// Diff-producing calls also pass `--no-ext-diff` / `--no-textconv`.
pub const GIT_HARDENING_ARGS: &[&str] = &[
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.pager=cat",
    "-c",
    "commit.gpgSign=false",
    "-c",
    "tag.gpgSign=false",
    "-c",
    "protocol.ext.allow=never",
];

/// Server-side `git` for a repo agents write to: [`GIT_HARDENING_ARGS`],
/// the repo-retargeting env (`GIT_DIR` & co., which git sets for hook
/// subprocesses) stripped, and the agent sandbox scoped to `repo` — so
/// whatever the repo's config still triggers (a filter or merge driver)
/// can't reach the data dir.
pub fn git_command(repo: impl AsRef<Path>) -> SandboxedCommand {
    let mut cmd = SandboxedCommand::new("git", &SpawnScope::folder(repo.as_ref()));
    cmd.args(GIT_HARDENING_ARGS)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    cmd
}

/// [`git_command`] for tokio.
pub fn git_command_tokio(repo: impl AsRef<Path>) -> tokio::process::Command {
    tokio::process::Command::from(git_command(repo).into_inner())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    struct Fixture {
        _tmp: tempfile::TempDir,
        data: PathBuf,
        project: PathBuf,
        account: PathBuf,
        cfg: SandboxConfig,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let data = root.join("data");
        let project = root.join("project");
        let account = data.join("claude-accounts/acc1");
        std::fs::create_dir_all(&account).unwrap();
        std::fs::create_dir_all(data.join("plugins")).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(data.join("jwt_secret"), "TOP-SECRET-JWT").unwrap();
        std::fs::write(account.join("creds"), "account-ok").unwrap();
        std::fs::write(project.join("readme"), "project-ok").unwrap();
        let cfg = SandboxConfig {
            data_dir: data.clone(),
            mode: Mode::Enforce,
            extra_rw: vec![],
        };
        Fixture {
            _tmp: tmp,
            data,
            project,
            account,
            cfg,
        }
    }

    fn sh(f: &Fixture, script: &str) -> std::process::Output {
        let scope = SpawnScope::folder(&f.project).account_dir(&f.account);
        let mut cmd = SandboxedCommand::with_config("sh", &scope, Some(&f.cfg));
        cmd.arg("-c").arg(script).current_dir(&f.project);
        cmd.output().expect("spawn sh")
    }

    #[test]
    fn landlock_denies_the_data_dir_but_not_the_project() {
        if !supported() {
            eprintln!("skipping: Landlock unavailable on this kernel");
            return;
        }
        let f = fixture();
        let data = f.data.display();

        let out = sh(&f, &format!("cat {data}/jwt_secret"));
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("Permission denied"), "{stderr}");
        assert!(!String::from_utf8_lossy(&out.stdout).contains("TOP-SECRET"));

        let out = sh(&f, &format!("echo x > {data}/plugins/p.wasm"));
        assert!(!out.status.success());
        assert!(!f.data.join("plugins/p.wasm").exists());

        let out = sh(&f, &format!("cat {}/creds", f.account.display()));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "account-ok");

        let out = sh(&f, "cat readme && echo w > written && cat written");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), "project-okw\n");

        let out = sh(&f, "echo $PECKBOARD_AGENT_SANDBOX");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "landlock");
    }

    #[test]
    fn landlock_blocks_signalling_and_inspecting_the_server() {
        if abi() < 6 {
            eprintln!("skipping: Landlock signal scoping needs ABI >= 6");
            return;
        }
        let f = fixture();
        let me = std::process::id();
        let out = sh(&f, &format!("kill -0 {me}"));
        assert!(!out.status.success(), "kill -0 of the server must fail");
        let out = sh(&f, &format!("cat /proc/{me}/environ"));
        assert!(!out.status.success(), "server environ must be unreadable");
    }

    #[test]
    fn off_mode_leaves_the_child_unconfined() {
        let mut f = fixture();
        f.cfg.mode = Mode::Off;
        let out = sh(&f, &format!("cat {}/jwt_secret", f.data.display()));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "TOP-SECRET-JWT");
    }

    #[test]
    fn account_dirs_outside_an_accounts_tree_are_ignored() {
        let f = fixture();
        assert!(valid_account_dir(&f.data, &f.account));
        assert!(!valid_account_dir(&f.data, &f.data.join("plugins")));
        assert!(!valid_account_dir(&f.data, &f.data));
        assert!(!valid_account_dir(&f.data, &f.project));
    }
}
