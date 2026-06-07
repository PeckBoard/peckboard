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
}

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub https_port: u16,
    pub host: String,
    pub data_dir: PathBuf,
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
        }
    }
}
