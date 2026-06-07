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
        let args = CliArgs::parse();

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
