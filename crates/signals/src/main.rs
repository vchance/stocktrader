use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use st_collector::read_source;
use st_signals::apewisdom::SOURCE;
use st_signals::{build_rows, read_rows, write_rows};

#[derive(Parser)]
#[command(name = "signals", about = "Derive trading signals from the raw collector archive")]
struct Cli {
    /// Root of the raw archive written by the collector.
    #[arg(long, env = "STOCKTRADER_DATA_DIR", default_value = "data/raw")]
    archive: PathBuf,
    /// Where derived signal tables are written.
    #[arg(long, env = "STOCKTRADER_SIGNALS_DIR", default_value = "data/signals")]
    out_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Rebuild apewisdom.parquet (one row per ticker per snapshot) from the whole archive.
    Apewisdom,
    /// Print a ticker's attention history.
    Show {
        ticker: String,
        /// Only the most recent N snapshots.
        #[arg(long, default_value_t = 24)]
        last: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let table = cli.out_dir.join("apewisdom.parquet");
    match cli.command {
        Command::Apewisdom => {
            let mut records = Vec::new();
            let read = read_source(&cli.archive, SOURCE, |r| records.push(r))?;
            if read.records == 0 {
                // A fresh install: nothing to derive yet, and any existing table is left as it is.
                println!("no {SOURCE} records under {} yet; nothing to build", cli.archive.display());
                return Ok(());
            }
            let build = build_rows(records);
            write_rows(&table, &build.rows)?;
            println!(
                "{} records ({} unreadable lines) -> {} snapshots, {} rows ({} malformed) -> {}",
                read.records,
                read.skipped,
                build.snapshots,
                build.rows.len(),
                build.malformed,
                table.display()
            );
        }
        Command::Show { ticker, last } => {
            let rows = read_rows(&table, Some(&ticker.to_ascii_uppercase()))?;
            println!("{:<20} {:>5} {:>9} {:>8} {:>8} {:>9}", "observed_at", "rank", "mentions", "prev", "change", "upvotes");
            for r in rows.iter().rev().take(last).rev() {
                let change = match r.mentions_24h_ago {
                    Some(p) if p > 0 => format!("{:.1}x", f64::from(r.mentions) / f64::from(p)),
                    _ => "-".into(),
                };
                let opt = |v: Option<u32>| v.map_or("-".into(), |v| v.to_string());
                let upvotes = r.upvotes.map_or("-".into(), |v| v.to_string());
                println!(
                    "{:<20} {:>5} {:>9} {:>8} {:>8} {:>9}",
                    r.observed_at.format("%Y-%m-%d %H:%M"),
                    opt(r.rank),
                    r.mentions,
                    opt(r.mentions_24h_ago),
                    change,
                    upvotes
                );
            }
        }
    }
    Ok(())
}
