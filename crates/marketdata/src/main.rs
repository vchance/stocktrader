use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{Days, NaiveDate, Utc};
use chrono_tz::America::New_York;
use clap::{Parser, Subcommand};
use st_marketdata::alpaca::{DATA_URL, PAPER_TRADING_URL};
use st_marketdata::{AlpacaClient, SyncConfig, summarize, sync};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "marketdata", about = "Download the US equity universe and daily price history from Alpaca")]
struct Cli {
    /// Directory for Parquet files and asset snapshots.
    #[arg(long, env = "STOCKTRADER_MARKETDATA_DIR", default_value = "data/marketdata")]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Snapshot the asset lists and rewrite the full adjusted and raw daily history.
    Sync {
        #[arg(long, default_value = "2016-01-01")]
        start: NaiveDate,
        /// `sip` covers all US venues; `iex` is one venue with a small share of volume.
        #[arg(long, env = "ALPACA_DATA_FEED", default_value = "sip")]
        feed: String,
        #[arg(long, default_value_t = 100)]
        batch_size: usize,
        /// Abort a pass once more symbols than this have been rejected individually.
        #[arg(long, default_value_t = 50)]
        max_rejected: usize,
        /// Replace the previous files even if the new ones are much smaller (e.g. after moving --start).
        #[arg(long)]
        force: bool,
    },
    /// Print rows, symbols and date range of a bars file.
    Info { file: PathBuf },
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let cli = Cli::parse();

    match cli.command {
        Command::Sync { start, feed, batch_size, max_rejected, force } => {
            let env = |key: &str| std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
            let key_id = env("APCA_API_KEY_ID").context("APCA_API_KEY_ID is not set")?;
            let secret = env("APCA_API_SECRET_KEY").context("APCA_API_SECRET_KEY is not set")?;
            let http = reqwest::Client::builder()
                .user_agent(concat!("stocktrader-marketdata/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(60))
                .build()?;
            let mut client = AlpacaClient::new(
                http,
                env("ALPACA_DATA_URL").unwrap_or_else(|| DATA_URL.into()),
                env("ALPACA_TRADING_URL").unwrap_or_else(|| PAPER_TRADING_URL.into()),
                &key_id,
                &secret,
            )?;
            // Through yesterday's session: today's bar is incomplete until the close.
            let today = Utc::now().with_timezone(&New_York).date_naive();
            let cfg = SyncConfig {
                root: cli.data_dir,
                start,
                end: today - Days::new(1),
                today,
                feed,
                batch_size,
                max_rejected,
                force,
            };
            let report = sync(&mut client, &cfg).await?;
            println!("universe  {} symbols", report.symbols);
            for (adjustment, s) in &report.files {
                let last = s.last.map_or("-".to_string(), |d| d.to_string());
                println!("{adjustment:<9} {} rows, {} symbols, through {last}", s.rows, s.symbols);
            }
            for (adjustment, symbol) in &report.skipped {
                println!("skipped   {symbol} ({adjustment})");
            }
        }
        Command::Info { file } => {
            let s = summarize(&file)?;
            let date = |d: Option<NaiveDate>| d.map_or("-".to_string(), |d| d.to_string());
            println!("rows      {}", s.rows);
            println!("symbols   {}", s.symbols);
            println!("dates     {} .. {}", date(s.first), date(s.last));
        }
    }
    Ok(())
}
