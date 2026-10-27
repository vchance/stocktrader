use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use st_collector::runner::{run_forever, run_once};
use st_collector::{Archive, Config, Source};
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
    /// Poll every enabled source until interrupted (Ctrl-C).
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
                match run_once(&mut source, &http, &archive).await {
                    Ok(n) => println!("{:<14} {n} records", source.name()),
                    Err(e) => {
                        failed += 1;
                        println!("{:<14} FAILED: {e:#}", source.name());
                    }
                }
            }
            println!("archive: {}", archive.root().display());
            if failed > 0 {
                bail!("{failed} source(s) failed");
            }
        }
        Command::Run => {
            let mut tasks = JoinSet::new();
            for source in config.sources() {
                tracing::info!(source = source.name(), every = ?source.interval(), "starting");
                tasks.spawn(run_forever(source, http.clone(), archive.clone()));
            }
            tokio::signal::ctrl_c().await?;
            tracing::info!("shutting down");
            tasks.abort_all();
        }
    }
    Ok(())
}
