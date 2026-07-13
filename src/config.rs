use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "peckboard", about = "Remote Claude Code control panel")]
pub struct CliArgs {
    /// HTTP port
    #[arg(long, default_value = "3344")]
    pub port: u16,

    /// HTTPS port
    #[arg(long, default_value = "3345")]
    pub https_port: u16,

    /// Bind address
    #[arg(long, default_value = "0.0.0.0")]
    pub host: String,

    /// Data directory
    #[arg(long, env = "PECKBOARD_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Reset a user's password to a freshly-generated random value,
    /// revoke their auth sessions, print the new password, and exit.
    /// If --user is omitted and exactly one user exists, that user is
    /// reset; otherwise --user is required.
    #[arg(long)]
    pub reset_password: bool,

    /// Username for --reset-password.
    #[arg(long, requires = "reset_password")]
    pub user: Option<String>,

    /// Advertise the server on the LAN via mDNS. Off by default —
    /// publishing `_peckboard._tcp.local.` lets any host on the same
    /// network see the service exists, fingerprint the brand, and
    /// probe for credentials. Turn it on when you actively need
    /// discovery (e.g. an iPad on the same Wi-Fi).
    #[arg(long, env = "PECKBOARD_MDNS")]
    pub mdns: bool,

    /// How often (in hours) to run the provider login keep-alive, which
    /// pings each auth login (Claude/Grok per account, Cursor) with a
    /// throwaway "hi" so tokens don't go stale. `0` disables it.
    #[arg(long, env = "PECKBOARD_KEEPALIVE_HOURS", default_value = "1")]
    pub keep_alive_hours: u64,

    /// Restore a backup archive into the data directory and exit.
    /// Validate gzip magic + manifest.json, then unpack. Refuses if
    /// peckboard.db already exists unless --force is also given.
    #[arg(long, value_name = "FILE")]
    pub restore_from: Option<PathBuf>,

    /// Allow --restore-from to overwrite an existing peckboard.db.
    #[arg(long, requires = "restore_from")]
    pub force: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub https_port: u16,
    pub host: String,
    pub data_dir: PathBuf,
    pub mdns: bool,
    pub keep_alive_hours: u64,
}

impl Config {
    pub fn load() -> Self {
        Self::from_args(CliArgs::parse())
    }

    pub fn from_args(args: CliArgs) -> Self {
        let data_dir = args.data_dir.unwrap_or_else(|| {
            dirs::home_dir()
                .expect("no home directory")
                .join(".peckboard")
        });

        Config {
            port: args.port,
            https_port: args.https_port,
            host: args.host,
            data_dir,
            mdns: args.mdns,
            keep_alive_hours: args.keep_alive_hours,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bare CliArgs (no env-derived fields set) for `from_args` tests.
    /// Constructed directly so the real process environment
    /// (PECKBOARD_DATA_DIR, PECKBOARD_MDNS) can't leak into assertions.
    fn bare_args() -> CliArgs {
        CliArgs {
            port: 3344,
            https_port: 3345,
            host: "0.0.0.0".into(),
            data_dir: None,
            reset_password: false,
            user: None,
            mdns: false,
            keep_alive_hours: 1,
            restore_from: None,
            force: false,
        }
    }

    #[test]
    fn cli_defaults() {
        let args = CliArgs::try_parse_from(["peckboard"]).unwrap();
        assert_eq!(args.port, 3344);
        assert_eq!(args.https_port, 3345);
        assert_eq!(args.host, "0.0.0.0");
        assert!(!args.reset_password);
        assert!(args.user.is_none());
        assert_eq!(args.keep_alive_hours, 1);
    }

    #[test]
    fn keep_alive_hours_parses_and_disables_at_zero() {
        let on = CliArgs::try_parse_from(["peckboard", "--keep-alive-hours", "12"]).unwrap();
        assert_eq!(on.keep_alive_hours, 12);
        let off = CliArgs::try_parse_from(["peckboard", "--keep-alive-hours", "0"]).unwrap();
        assert_eq!(off.keep_alive_hours, 0);
    }

    #[test]
    fn cli_explicit_flags() {
        let args = CliArgs::try_parse_from([
            "peckboard",
            "--port",
            "8080",
            "--https-port",
            "8443",
            "--host",
            "127.0.0.1",
            "--data-dir",
            "/tmp/pb-test",
            "--mdns",
        ])
        .unwrap();
        assert_eq!(args.port, 8080);
        assert_eq!(args.https_port, 8443);
        assert_eq!(args.host, "127.0.0.1");
        assert_eq!(args.data_dir, Some(PathBuf::from("/tmp/pb-test")));
        assert!(args.mdns);
    }

    #[test]
    fn user_flag_requires_reset_password() {
        assert!(CliArgs::try_parse_from(["peckboard", "--user", "alice"]).is_err());
        assert!(
            CliArgs::try_parse_from(["peckboard", "--reset-password", "--user", "alice"]).is_ok()
        );
    }

    #[test]
    fn invalid_port_rejected() {
        assert!(CliArgs::try_parse_from(["peckboard", "--port", "70000"]).is_err());
        assert!(CliArgs::try_parse_from(["peckboard", "--port", "not-a-port"]).is_err());
    }

    #[test]
    fn from_args_defaults_data_dir_to_home_dot_peckboard() {
        let config = Config::from_args(bare_args());
        assert_eq!(
            config.data_dir,
            dirs::home_dir().unwrap().join(".peckboard")
        );
    }

    #[test]
    fn from_args_respects_explicit_data_dir() {
        let mut args = bare_args();
        args.data_dir = Some(PathBuf::from("/tmp/custom-dir"));
        let config = Config::from_args(args);
        assert_eq!(config.data_dir, PathBuf::from("/tmp/custom-dir"));
    }
}
