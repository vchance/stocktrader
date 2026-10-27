use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{NaiveDate, Utc};
use chrono_tz::America::New_York;
use clap::{Parser, Subcommand};
use st_fundamentals::sec::SEC_URL;
use st_fundamentals::store::summarize;
use st_fundamentals::sync::facts_path;
use st_fundamentals::tickers::{parse_tickers, primary_tickers};
use st_fundamentals::{FactFilter, SecClient, SyncConfig, SyncReport, read_facts, sync};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "fundamentals", about = "Point-in-time company fundamentals from SEC EDGAR")]
struct Cli {
    #[arg(long, env = "STOCKTRADER_FUNDAMENTALS_DIR", default_value = "data/fundamentals")]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Snapshot the ticker map and, if the SEC published new data, rebuild facts.parquet.
    /// Needs SEC_USER_AGENT ("Your Name you@example.com").
    Sync {
        /// Download even if unchanged, and accept a much smaller result.
        #[arg(long)]
        force: bool,
    },
    /// Print rows, companies and the newest filing date of facts.parquet.
    Info,
    /// Print what was known about one company as of a date.
    Show {
        ticker: String,
        /// Only facts usable at the start of this date, i.e. filed before it, matching when backtests see
        /// them (default: everything).
        #[arg(long)]
        as_of: Option<NaiveDate>,
        /// Comma-separated concepts, e.g. Revenues,NetIncomeLoss.
        #[arg(long, value_delimiter = ',')]
        concepts: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let cli = Cli::parse();

    match cli.command {
        Command::Sync { force } => {
            let user_agent = std::env::var("SEC_USER_AGENT").unwrap_or_default();
            let client = SecClient::new(SEC_URL, &user_agent)?;
            let today = Utc::now().with_timezone(&New_York).date_naive();
            match sync(&client, &SyncConfig { root: cli.data_dir, today, force }).await? {
                SyncReport::Unchanged { tickers } => {
                    println!("tickers   {tickers}");
                    println!("facts     unchanged since the last sync");
                }
                SyncReport::Updated { tickers, stats, documents, failed, malformed } => {
                    println!("tickers   {tickers}");
                    println!("documents {documents} ({failed} unreadable, {malformed} malformed entries skipped)");
                    println!("facts     {} rows, {} companies, newest filing {:?}", stats.rows, stats.companies, stats.last_filed);
                }
            }
        }
        Command::Info => {
            let s = summarize(&facts_path(&cli.data_dir))?;
            println!("rows      {}", s.rows);
            println!("companies {}", s.companies);
            println!("filed     through {}", s.last_filed.map_or("-".into(), |d| d.to_string()));
        }
        Command::Show { ticker, as_of, concepts } => {
            let ticker = ticker.to_ascii_uppercase();
            let snapshot = latest_ticker_snapshot(&cli.data_dir)?;
            let listings = parse_tickers(&std::fs::read(&snapshot)?)?;
            let ciks: HashSet<u32> = listings.iter().filter(|l| l.ticker == ticker).map(|l| l.cik).collect();
            if ciks.is_empty() {
                bail!("{ticker} is not in {}", snapshot.display());
            }
            let filter = FactFilter {
                ciks: Some(ciks),
                concepts: (!concepts.is_empty()).then(|| concepts.into_iter().collect()),
                as_of,
            };
            let names = primary_tickers(&listings);
            let mut facts = read_facts(&facts_path(&cli.data_dir), &filter)?;
            facts.sort_by(|a, b| (&a.concept, a.end, a.filed).cmp(&(&b.concept, b.end, b.filed)));
            println!("{:<8} {:<48} {:>10} {:>10} {:>20} {:<6} {:<7} filed", "ticker", "concept", "start", "end", "value", "unit", "form");
            for f in facts {
                let start = f.start.map_or("-".into(), |d| d.to_string());
                let ticker = names.get(&f.cik).map_or("?", String::as_str);
                println!("{ticker:<8} {:<48} {start:>10} {:>10} {:>20} {:<6} {:<7} {}", f.concept, f.end, f.value, f.unit, f.form, f.filed);
            }
        }
    }
    Ok(())
}

fn latest_ticker_snapshot(root: &std::path::Path) -> Result<PathBuf> {
    let dir = root.join("tickers");
    let mut snapshots: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("no ticker snapshots in {}; run `fundamentals sync` first", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    snapshots.sort();
    snapshots.pop().with_context(|| format!("no ticker snapshots in {}", dir.display()))
}
