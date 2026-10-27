use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use chrono::{Days, Utc};
use clap::{Parser, Subcommand};
use st_collector::maintenance::{Status, compact, disabled_check, marketdata_check, source_check, stale_after};
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
    /// Gzip day files older than yesterday (UTC); the file being written is never touched.
    /// Exits 1 if any file could not be compacted (the rest still are).
    Compact,
    /// Report how recently each source (and the market data sync) produced data, one line each.
    /// Exits 1 if anything is stale, missing, or has data but is no longer configured.
    Health {
        /// Market data directory, checked if it holds a completed sync.
        #[arg(long, env = "STOCKTRADER_MARKETDATA_DIR", default_value = "data/marketdata")]
        marketdata_dir: PathBuf,
        /// Allowed days since the last market data sync. The sync runs Tue-Sat, so a normal weekend
        /// gap (Sat to Tue) is 3 days.
        #[arg(long, default_value_t = 3)]
        marketdata_max_days: u64,
    },
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
        Command::Compact => {
            let before = Utc::now().date_naive() - Days::new(1);
            let report = compact(archive.root(), before)?;
            println!(
                "compressed {} file(s) dated before {before}; removed {} leftover duplicate(s)",
                report.compressed.len(),
                report.recovered.len()
            );
            for (path, reason) in &report.errors {
                println!("NOT COMPACTED {}: {reason}", path.display());
            }
            if !report.errors.is_empty() {
                std::process::exit(1);
            }
        }
        Command::Health { marketdata_dir, marketdata_max_days } => {
            let now = Utc::now();
            let mut checks = Vec::new();
            for source in config.sources() {
                let limit = stale_after(source.interval(), source.max_backoff());
                checks.push(source_check(archive.root(), source.name(), limit, now)?);
            }
            for (name, missing) in config.describe() {
                if missing.is_some() {
                    checks.extend(disabled_check(archive.root(), name));
                }
            }
            // The sync runs in New York time; its manifest is dated in New York.
            let today = now.with_timezone(&chrono_tz::America::New_York).date_naive();
            checks.extend(marketdata_check(&marketdata_dir, marketdata_max_days, today)?);
            let mut problems = 0;
            for c in &checks {
                problems += usize::from(c.status != Status::Ok);
                println!("{:<14} {:<8} {}", c.name, c.status.label(), c.detail);
            }
            if problems > 0 {
                std::process::exit(1);
            }
        }
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
