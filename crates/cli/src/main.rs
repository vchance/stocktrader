use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Result, bail};
use chrono::NaiveDate;
use clap::{Args, Parser, Subcommand, ValueEnum};
use rust_decimal::Decimal;
use st_core::{Event, Strategy};
use st_data::{SyntheticConfig, VecEventSource, generate_daily_bars, load_daily_bars};
use st_engine::{Backtest, FillModel, SimulatedBroker};
use st_marketdata::{BarFilter, read_bars, to_core_bar};
use st_risk::{RiskLimits, RiskManager};
use st_strategies::{AttentionSpike, SmaCrossover};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "stocktrader", about = "Automated stock trading: backtest, paper and live")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the SMA crossover strategy over Parquet, CSV or synthetic daily bars.
    Backtest(BacktestArgs),
}

#[derive(Args)]
struct BacktestArgs {
    /// CSV of daily bars with header symbol,date,open,high,low,close,volume.
    #[arg(long, conflicts_with = "parquet")]
    bars: Option<PathBuf>,
    /// Parquet bars from `marketdata sync`, e.g. data/marketdata/bars_daily_all.parquet. Every matching bar
    /// is held in memory, so narrow large files with --start/--end/--symbols.
    #[arg(long)]
    parquet: Option<PathBuf>,
    /// First session date to load (Parquet only).
    #[arg(long, requires = "parquet")]
    start: Option<NaiveDate>,
    /// Last session date to load (Parquet only).
    #[arg(long, requires = "parquet")]
    end: Option<NaiveDate>,
    /// Comma-separated symbols to load (Parquet only).
    #[arg(long, requires = "parquet", value_delimiter = ',')]
    symbols: Vec<String>,
    /// Used when neither --bars nor --parquet is given.
    #[arg(long, default_value_t = 50)]
    synthetic_symbols: usize,
    #[arg(long, default_value_t = 750)]
    synthetic_days: usize,
    #[arg(long, default_value_t = 42)]
    seed: u64,
    #[arg(long, default_value = "100000")]
    cash: Decimal,
    #[arg(long, value_enum, default_value_t = StrategyName::Sma)]
    strategy: StrategyName,
    /// SMA crossover: fast window.
    #[arg(long, default_value_t = 20)]
    fast: usize,
    /// SMA crossover: slow window.
    #[arg(long, default_value_t = 50)]
    slow: usize,
    /// Attention table from `signals apewisdom` (data/signals/apewisdom.parquet), merged into the replay.
    #[arg(long)]
    attention: Option<PathBuf>,
    /// Attention spike: minimum mentions in the last 24h.
    #[arg(long, default_value_t = 50)]
    spike_min_mentions: u32,
    /// Attention spike: minimum ratio of mentions to the previous 24h.
    #[arg(long, default_value_t = 3.0)]
    spike_ratio: f64,
    /// Attention spike: days to hold after the latest spike.
    #[arg(long, default_value_t = 5)]
    spike_hold_days: u32,
    /// Target weight per position when long.
    #[arg(long, default_value = "0.05")]
    weight: Decimal,
    #[arg(long, default_value = "5")]
    slippage_bps: Decimal,
    /// Risk filter: skip entries below this price. On split-adjusted data this sees adjusted, not traded,
    /// prices; set 0 to disable.
    #[arg(long, default_value = "5")]
    min_price: Decimal,
}

#[derive(Clone, Copy, ValueEnum)]
enum StrategyName {
    /// Moving-average crossover on prices.
    Sma,
    /// Buy on a jump in social-media mentions (needs --attention).
    AttentionSpike,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();
    match Cli::parse().command {
        Command::Backtest(args) => backtest(args),
    }
}

fn backtest(args: BacktestArgs) -> Result<()> {
    let bars = match (&args.bars, &args.parquet) {
        (Some(path), _) => load_daily_bars(path)?,
        (None, Some(path)) => {
            let filter = BarFilter {
                symbols: (!args.symbols.is_empty())
                    .then(|| args.symbols.iter().map(|s| s.trim().to_ascii_uppercase()).collect()),
                start: args.start,
                end: args.end,
            };
            if args.min_price > Decimal::ZERO && !path.to_string_lossy().contains("_raw") {
                eprintln!(
                    "warning: --min-price {} is applied to adjusted prices, which understate pre-split prices; \
                     the universe will be biased (use --min-price 0, or the raw file)",
                    args.min_price
                );
            }
            let raw = read_bars(path, &filter)?;
            let bars: Vec<_> = raw.iter().filter_map(to_core_bar).collect();
            if bars.len() < raw.len() {
                eprintln!("skipped {} bars with invalid prices", raw.len() - bars.len());
            }
            if let Some(wanted) = &filter.symbols {
                let found: HashSet<&str> = raw.iter().map(|b| b.symbol.as_str()).collect();
                let missing: Vec<&str> = wanted.iter().map(String::as_str).filter(|s| !found.contains(s)).collect();
                if !missing.is_empty() {
                    eprintln!("no bars for: {}", missing.join(", "));
                }
            }
            bars
        }
        (None, None) => generate_daily_bars(&SyntheticConfig {
            symbols: args.synthetic_symbols,
            days: args.synthetic_days,
            start: NaiveDate::from_ymd_opt(2023, 1, 3).expect("valid date"),
            seed: args.seed,
        }),
    };
    let n_bars = bars.len();
    // Attention only matters for symbols we have prices for, within the replayed period; anything else
    // would cost memory and stretch the metrics' time span.
    let bar_symbols: HashSet<String> = bars.iter().map(|b| b.symbol.as_str().to_string()).collect();
    let first_bar = bars.iter().map(|b| b.start).min();
    let last_bar = bars.iter().map(|b| b.end).max();
    let mut events: Vec<Event> = bars.into_iter().map(Event::bar).collect();
    if let Some(path) = &args.attention {
        let filter = st_signals::RowFilter { tickers: Some(bar_symbols), from: first_bar, to: last_bar };
        let rows = st_signals::read_rows(path, &filter)?;
        println!("attention events  {}", rows.len());
        let first_attention = rows.iter().map(|r| r.observed_at).min();
        match (first_bar, first_attention) {
            (_, None) => eprintln!("warning: no attention data for these symbols and dates"),
            (Some(bar), Some(att)) if att - bar > chrono::TimeDelta::days(30) => eprintln!(
                "warning: attention data starts {} but prices start {}; metrics cover the whole price \
                 period, so consider --start {}",
                att.date_naive(),
                bar.date_naive(),
                att.date_naive()
            ),
            _ => {}
        }
        events.extend(st_signals::to_events(&rows));
    }
    let source = VecEventSource::new(events);

    let strategy: Box<dyn Strategy> = match args.strategy {
        StrategyName::Sma => Box::new(SmaCrossover::new(args.fast, args.slow, args.weight).map_err(anyhow::Error::msg)?),
        StrategyName::AttentionSpike => {
            if args.attention.is_none() {
                bail!("--strategy attention-spike needs --attention data/signals/apewisdom.parquet");
            }
            Box::new(
                AttentionSpike::new(args.spike_min_mentions, args.spike_ratio, args.spike_hold_days, args.weight)
                    .map_err(anyhow::Error::msg)?,
            )
        }
    };

    let report = Backtest::new(
        source,
        strategy,
        SimulatedBroker::new(FillModel { slippage_bps: args.slippage_bps, ..FillModel::default() }),
        RiskManager::new(RiskLimits { min_price: args.min_price, ..RiskLimits::default() }),
        args.cash,
    )
    .run()?;

    let m = &report.metrics;
    println!("bars replayed     {n_bars}");
    println!("start equity      {:.2}", m.start_equity);
    println!("end equity        {:.2}", m.end_equity);
    println!("total return      {:>7.2}%", m.total_return * 100.0);
    println!("CAGR              {:>7.2}%", m.cagr * 100.0);
    println!("max drawdown      {:>7.2}%", m.max_drawdown * 100.0);
    println!("Sharpe            {:>7.2}", m.sharpe);
    println!("fills             {}", m.num_fills);
    println!("commissions       {:.2}", m.commissions);
    println!("rejected signals  {}", report.rejections.len());
    println!("open positions    {}", report.final_portfolio.positions().count());
    Ok(())
}
