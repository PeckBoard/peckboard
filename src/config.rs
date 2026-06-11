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
}

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub https_port: u16,
    pub host: String,
    pub data_dir: PathBuf,
    pub mdns: bool,
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
