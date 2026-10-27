use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand};
use st_collector::runner::{poll, run_forever};
use st_collector::{Archive, Config, Source};
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinSet;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "collector", about = "Archive raw news and social-media data that cannot be backfilled later")]
struct Cli {
    /// Root directory of the raw archive.
    #[arg(long, env = "STOCKTRADER_DATA_DIR", default_value = "data/raw")]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Poll every enabled source until interrupted (Ctrl-C or SIGTERM).
    Run,
    /// Fetch every enabled source once and exit; non-zero exit if any source failed.
    Once,
    /// Show which sources are enabled and which environment variables the others need.
    Sources,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Credentials come from the environment or a git-ignored .env file.
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cli = Cli::parse();
    let config = Config::from_env();
    let archive = Archive::new(cli.data_dir);
    let http = reqwest::Client::builder()
        .user_agent(concat!("stocktrader-collector/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()?;

    match cli.command {
        Command::Sources => {
            for (name, missing) in config.describe() {
                match missing {
                    None => println!("{name:<14} enabled"),
                    Some(vars) => println!("{name:<14} disabled (set {vars})"),
                }
            }
        }
        Command::Once => {
            let mut failed = 0;
            for mut source in config.sources() {
                let name = source.name();
                source.restore(&archive)?;
                match poll(&mut source, &http, &archive).await {
                    Ok(result) => {
                        println!("{name:<14} {} records", result.records);
                        for e in &result.errors {
                            failed += 1;
                            println!("{name:<14} FAILED: {e:#}");
                        }
                    }
                    Err(e) => {
                        failed += 1;
                        println!("{name:<14} ARCHIVE WRITE FAILED: {e:#}");
                    }
                }
            }
            println!("archive: {}", archive.root().display());
            if failed > 0 {
                bail!("{failed} request(s) failed");
            }
        }
        Command::Run => {
            let mut tasks = JoinSet::new();
            for source in config.sources() {
                tracing::info!(source = source.name(), every = ?source.interval(), "starting");
                tasks.spawn(run_forever(source, http.clone(), archive.clone()));
            }
            let mut terminate = signal(SignalKind::terminate())?;
            let outcome = tokio::select! {
                _ = tokio::signal::ctrl_c() => Ok(()),
                _ = terminate.recv() => Ok(()),
                // Source tasks loop forever, so any task ending means it panicked.
                Some(ended) = tasks.join_next() => Err(anyhow!("a source task stopped unexpectedly: {ended:?}")),
            };
            tracing::info!("shutting down");
            // Archive writes are synchronous, so aborting can only interrupt a task between writes.
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
            return outcome;
        }
    }
    Ok(())
}
