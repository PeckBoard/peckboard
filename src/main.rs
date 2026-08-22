use clap::Parser;
use peckboard::auth::reset::reset_user_password;
use peckboard::config::{CliArgs, Config};
use peckboard::db::Db;
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let args = CliArgs::parse();

    // Short-circuit CLI maintenance flows before any server startup.
    if let Some(archive_path) = args.restore_from.clone() {
        let force = args.force;
        let config = Config::from_args(args);
        peckboard::service::backup::restore_from(&archive_path, &config.data_dir, force)?;
        eprintln!(
            "Restored backup from {} into {}",
            archive_path.display(),
            config.data_dir.display()
        );
        return Ok(());
    }

    if args.reset_password {
        let username = args.user.clone();
        let config = Config::from_args(args);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        return rt.block_on(async {
            let db = Db::open(&config.data_dir)?;
            let outcome = reset_user_password(&db, username.as_deref()).await?;
            // stderr for the human note, stdout for just the credentials so
            // it's easy to pipe `peckboard --reset-password | tail -1`.
            eprintln!(
                "Reset password for '{}' and revoked {} auth session(s).",
                outcome.username, outcome.sessions_revoked,
            );
            println!("{}:{}", outcome.username, outcome.new_password);
            Ok(())
        });
    }

    if args.install_desktop_entry {
        return peckboard::desktop::install_desktop_entry();
    }

    if args.desktop {
        return peckboard::desktop::run(args);
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(peckboard::server::run_server(args, None, None))
}
