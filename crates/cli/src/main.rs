use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use chrono::{NaiveDate, TimeDelta};
use clap::{Args, Parser, Subcommand, ValueEnum};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use st_core::{Bar, Event, Strategy, Timestamp};
use st_data::{SyntheticConfig, VecEventSource, generate_daily_bars, load_daily_bars};
use st_engine::metrics::{self, buy_and_hold, correlation};
use st_engine::{Backtest, FillModel, Metrics, SimulatedBroker, WalkForwardConfig, walk_forward};
use st_marketdata::{BarFilter, DailyBar, read_bars, to_core_bar};
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
    /// Run one strategy with fixed parameters over Parquet, CSV or synthetic daily bars.
    Backtest {
        #[command(flatten)]
        data: DataArgs,
        #[command(flatten)]
        strategy: StrategyArgs,
    },
    /// Choose parameters on rolling training windows and report results only on the unseen test window
    /// after each: the honest estimate of a strategy-plus-tuning process.
    WalkForward {
        #[command(flatten)]
        data: DataArgs,
        #[arg(long, value_enum, default_value_t = StrategyName::Sma)]
        strategy: StrategyName,
        /// Target weight per position when long.
        #[arg(long, default_value = "0.05")]
        weight: Decimal,
        /// Attention spike: minimum mentions (the grid varies ratio and hold).
        #[arg(long, default_value_t = 50)]
        spike_min_mentions: u32,
        #[arg(long, default_value_t = 365)]
        train_days: i64,
        #[arg(long, default_value_t = 91)]
        test_days: i64,
        /// History replayed before each window without trading, so indicators are warm.
        #[arg(long, default_value_t = 300)]
        warmup_days: i64,
    },
}

#[derive(Args)]
struct DataArgs {
    /// CSV of daily bars with header symbol,date,open,high,low,close,volume.
    #[arg(long, conflicts_with = "parquet")]
    bars: Option<PathBuf>,
    /// Parquet bars from `marketdata sync`, e.g. data/marketdata/bars_daily_all.parquet. Every matching bar
    /// is held in memory, so narrow large files with --start/--end/--symbols.
    #[arg(long)]
    parquet: Option<PathBuf>,
    /// As-traded bars for the price-level filter. Defaults to bars_daily_raw.parquet beside --parquet.
    #[arg(long, requires = "parquet")]
    raw: Option<PathBuf>,
    /// First session date to load (Parquet only).
    #[arg(long, requires = "parquet")]
    start: Option<NaiveDate>,
    /// Last session date to load (Parquet only).
    #[arg(long, requires = "parquet")]
    end: Option<NaiveDate>,
    /// Comma-separated symbols to load (Parquet only).
    #[arg(long, requires = "parquet", value_delimiter = ',')]
    symbols: Vec<String>,
    /// Attention table from `signals apewisdom` (data/signals/apewisdom.parquet), merged into the replay.
    #[arg(long)]
    attention: Option<PathBuf>,
    /// Buy-and-hold comparison symbol, or `none`.
    #[arg(long, default_value = "SPY")]
    benchmark: String,
    /// Used when neither --bars nor --parquet is given.
    #[arg(long, default_value_t = 50)]
    synthetic_symbols: usize,
    #[arg(long, default_value_t = 750)]
    synthetic_days: usize,
    #[arg(long, default_value_t = 42)]
    seed: u64,
    #[arg(long, default_value = "100000")]
    cash: Decimal,
    #[arg(long, default_value = "5")]
    slippage_bps: Decimal,
    /// Risk filter: skip entries whose as-traded price is below this; 0 disables.
    #[arg(long, default_value = "5")]
    min_price: Decimal,
}

#[derive(Args)]
struct StrategyArgs {
    #[arg(long, value_enum, default_value_t = StrategyName::Sma)]
    strategy: StrategyName,
    /// SMA crossover: fast window.
    #[arg(long, default_value_t = 20)]
    fast: usize,
    /// SMA crossover: slow window.
    #[arg(long, default_value_t = 50)]
    slow: usize,
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
}

#[derive(Clone, Copy, ValueEnum)]
enum StrategyName {
    /// Moving-average crossover on prices.
    Sma,
    /// Buy on a jump in social-media mentions (needs --attention).
    AttentionSpike,
}

/// One point in a strategy's parameter space.
#[derive(Clone, Debug, PartialEq)]
enum Params {
    Sma { fast: usize, slow: usize },
    Spike { min_mentions: u32, ratio: f64, hold_days: u32 },
}

impl fmt::Display for Params {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Params::Sma { fast, slow } => write!(f, "sma {fast}/{slow}"),
            Params::Spike { min_mentions, ratio, hold_days } => {
                write!(f, "spike >={min_mentions} x{ratio} {hold_days}d")
            }
        }
    }
}

fn make_strategy(params: &Params, weight: Decimal) -> Result<Box<dyn Strategy>> {
    Ok(match *params {
        Params::Sma { fast, slow } => Box::new(SmaCrossover::new(fast, slow, weight).map_err(anyhow::Error::msg)?),
        Params::Spike { min_mentions, ratio, hold_days } => {
            Box::new(AttentionSpike::new(min_mentions, ratio, hold_days, weight).map_err(anyhow::Error::msg)?)
        }
    })
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();
    match Cli::parse().command {
        Command::Backtest { data, strategy } => backtest(&data, &strategy),
        Command::WalkForward { data, strategy, weight, spike_min_mentions, train_days, test_days, warmup_days } => {
            let grid: Vec<Params> = match strategy {
                StrategyName::Sma => [(5, 20), (10, 30), (10, 50), (20, 50), (20, 100), (50, 200)]
                    .into_iter()
                    .map(|(fast, slow)| Params::Sma { fast, slow })
                    .collect(),
                StrategyName::AttentionSpike => {
                    require_attention(&data)?;
                    let mut grid = Vec::new();
                    for ratio in [2.0, 3.0, 5.0] {
                        for hold_days in [3, 5, 10] {
                            grid.push(Params::Spike { min_mentions: spike_min_mentions, ratio, hold_days });
                        }
                    }
                    grid
                }
            };
            let windows = (TimeDelta::days(train_days), TimeDelta::days(test_days), TimeDelta::days(warmup_days));
            walk(&data, &grid, weight, windows)
        }
    }
}

fn require_attention(data: &DataArgs) -> Result<()> {
    if data.attention.is_none() {
        bail!("attention-spike needs --attention data/signals/apewisdom.parquet");
    }
    Ok(())
}

struct Loaded {
    events: Vec<Event>,
    bars: usize,
    /// Buy-and-hold comparison bars, if the benchmark symbol was found.
    benchmark: Option<Vec<Bar>>,
}

fn backtest(data: &DataArgs, args: &StrategyArgs) -> Result<()> {
    let params = match args.strategy {
        StrategyName::Sma => Params::Sma { fast: args.fast, slow: args.slow },
        StrategyName::AttentionSpike => {
            require_attention(data)?;
            Params::Spike { min_mentions: args.spike_min_mentions, ratio: args.spike_ratio, hold_days: args.spike_hold_days }
        }
    };
    let strategy = make_strategy(&params, args.weight)?;
    let loaded = load(data)?;
    let report = Backtest::new(
        VecEventSource::new(loaded.events),
        strategy,
        SimulatedBroker::new(fill_model(data)),
        RiskManager::new(limits(data)),
        data.cash,
    )
    .run()?;

    println!("strategy          {params}");
    println!("bars replayed     {}", loaded.bars);
    print_metrics("", &report.metrics);
    println!("fills             {}", report.metrics.num_fills);
    println!("commissions       {:.2}", report.metrics.commissions);
    println!("rejected signals  {}", report.rejections.len());
    println!("open positions    {}", report.final_portfolio.positions().count());

    if let Some(bench) = &loaded.benchmark {
        let refs: Vec<&Bar> = bench.iter().collect();
        let curve = buy_and_hold(&refs, data.cash);
        let m = metrics::compute(&curve, &[]);
        println!();
        print_metrics(&format!("{} buy & hold ", data.benchmark.to_ascii_uppercase()), &m);
        println!("excess CAGR       {:>7.2}%", (report.metrics.cagr - m.cagr) * 100.0);
        if let Some(c) = correlation(&report.equity_curve, &curve) {
            println!("daily correlation {c:>7.2}");
        }
    }
    Ok(())
}

fn walk(data: &DataArgs, grid: &[Params], weight: Decimal, (train, test, warmup): (TimeDelta, TimeDelta, TimeDelta)) -> Result<()> {
    // Validate every candidate once, so a bad grid fails before hours of backtests.
    for p in grid {
        make_strategy(p, weight)?;
    }
    let loaded = load(data)?;
    let cfg = WalkForwardConfig {
        train,
        test,
        warmup,
        initial_cash: data.cash,
        fill: fill_model(data),
        limits: limits(data),
    };
    let report = walk_forward(&loaded.events, &cfg, grid, |p| make_strategy(p, weight).expect("validated above"))?;
    if report.folds.is_empty() {
        bail!("not enough data for one fold: need warm-up + training + some test period");
    }

    println!("{:<12} {:<12} {:<22} {:>9} {:>9} {:>9} {:>7}", "test from", "test to", "chosen", "train ret", "test ret", "test DD", "fills");
    for f in &report.folds {
        println!(
            "{:<12} {:<12} {:<22} {:>8.2}% {:>8.2}% {:>8.2}% {:>7}",
            f.test_start.date_naive(),
            f.test_end.date_naive(),
            f.chosen.to_string(),
            f.train.total_return * 100.0,
            f.test.total_return * 100.0,
            f.test.max_drawdown * 100.0,
            f.test.num_fills
        );
    }
    println!();
    println!("out-of-sample return {:>7.2}%", report.out_of_sample_return * 100.0);
    println!("out-of-sample CAGR   {:>7.2}%", report.out_of_sample_cagr * 100.0);

    if let Some(bench) = &loaded.benchmark {
        let (from, to) = (report.folds[0].test_start, report.folds[report.folds.len() - 1].test_end);
        let refs: Vec<&Bar> = bench.iter().filter(|b| b.end >= from && b.end < to).collect();
        let m = metrics::compute(&buy_and_hold(&refs, data.cash), &[]);
        println!("{} buy & hold CAGR   {:>7.2}% (same test span)", data.benchmark.to_ascii_uppercase(), m.cagr * 100.0);
    }
    Ok(())
}

fn print_metrics(prefix: &str, m: &Metrics) {
    println!("{prefix}start equity      {:.2}", m.start_equity);
    println!("{prefix}end equity        {:.2}", m.end_equity);
    println!("{prefix}total return      {:>7.2}%", m.total_return * 100.0);
    println!("{prefix}CAGR              {:>7.2}%", m.cagr * 100.0);
    println!("{prefix}max drawdown      {:>7.2}%", m.max_drawdown * 100.0);
    println!("{prefix}Sharpe            {:>7.2}", m.sharpe);
}

fn fill_model(data: &DataArgs) -> FillModel {
    FillModel { slippage_bps: data.slippage_bps, ..FillModel::default() }
}

fn limits(data: &DataArgs) -> RiskLimits {
    RiskLimits { min_price: data.min_price, ..RiskLimits::default() }
}

/// `bars_daily_all.parquet` -> `bars_daily_raw.parquet` in the same directory.
fn raw_sibling(adjusted: &Path) -> Option<PathBuf> {
    let name = adjusted.file_name()?.to_str()?;
    name.contains("_all").then(|| adjusted.with_file_name(name.replacen("_all", "_raw", 1)))
}

fn load(data: &DataArgs) -> Result<Loaded> {
    let benchmark_symbol = (!data.benchmark.eq_ignore_ascii_case("none")).then(|| data.benchmark.to_ascii_uppercase());
    let (bars, benchmark) = match (&data.bars, &data.parquet) {
        (Some(path), _) => {
            let bars = load_daily_bars(path)?;
            let bench = benchmark_symbol.as_ref().map(|s| bars.iter().filter(|b| b.symbol.as_str() == s).cloned().collect());
            (bars, bench)
        }
        (None, Some(path)) => load_parquet(data, path, benchmark_symbol.as_deref())?,
        (None, None) => {
            let bars = generate_daily_bars(&SyntheticConfig {
                symbols: data.synthetic_symbols,
                days: data.synthetic_days,
                start: NaiveDate::from_ymd_opt(2023, 1, 3).expect("valid date"),
                seed: data.seed,
            });
            let bench = benchmark_symbol.as_ref().map(|s| bars.iter().filter(|b| b.symbol.as_str() == s).cloned().collect());
            (bars, bench)
        }
    };
    let benchmark = match (benchmark, &benchmark_symbol) {
        (Some(b), _) if !b.is_empty() => Some(b),
        (_, Some(symbol)) => {
            eprintln!("benchmark {symbol}: no bars in this data (pass --benchmark none or a symbol that is present)");
            None
        }
        _ => None,
    };

    let n_bars = bars.len();
    // Attention only matters for symbols we have prices for, within the replayed period; anything else
    // would cost memory and stretch the metrics' time span.
    let bar_symbols: HashSet<String> = bars.iter().map(|b| b.symbol.as_str().to_string()).collect();
    let first_bar = bars.iter().map(|b| b.start).min();
    let last_bar = bars.iter().map(|b| b.end).max();
    let mut events: Vec<Event> = bars.into_iter().map(Event::bar).collect();
    if let Some(path) = &data.attention {
        let filter = st_signals::RowFilter { tickers: Some(bar_symbols), from: first_bar, to: last_bar };
        let rows = st_signals::read_rows(path, &filter)?;
        println!("attention events  {}", rows.len());
        warn_on_coverage(first_bar, rows.iter().map(|r| r.observed_at).min());
        events.extend(st_signals::to_events(&rows));
    }
    Ok(Loaded { events, bars: n_bars, benchmark })
}

fn warn_on_coverage(first_bar: Option<Timestamp>, first_attention: Option<Timestamp>) {
    match (first_bar, first_attention) {
        (_, None) => eprintln!("warning: no attention data for these symbols and dates"),
        (Some(bar), Some(att)) if att - bar > TimeDelta::days(30) => eprintln!(
            "warning: attention data starts {} but prices start {}; metrics cover the whole price period, \
             so consider --start {}",
            att.date_naive(),
            bar.date_naive(),
            att.date_naive()
        ),
        _ => {}
    }
}

/// Adjusted bars for the replay, each carrying its as-traded close from the raw file when available, plus
/// the benchmark's bars (read separately, so `--symbols` needn't include it).
fn load_parquet(data: &DataArgs, path: &Path, benchmark: Option<&str>) -> Result<(Vec<Bar>, Option<Vec<Bar>>)> {
    let symbols = (!data.symbols.is_empty()).then(|| data.symbols.iter().map(|s| s.trim().to_ascii_uppercase()).collect());
    let filter = BarFilter { symbols, start: data.start, end: data.end };
    let raw_path = data.raw.clone().or_else(|| raw_sibling(path)).filter(|p| p.exists());
    let traded: HashMap<(String, NaiveDate), Decimal> = match &raw_path {
        Some(raw) => read_bars(raw, &filter)?
            .into_iter()
            .filter_map(|b| Some(((b.symbol, b.date), Decimal::from_f64(b.close)?.round_dp(6))))
            .collect(),
        None => {
            if data.min_price > Decimal::ZERO {
                eprintln!(
                    "warning: no raw bars found beside {}; --min-price {} will see split-adjusted prices, which \
                     understate pre-split prices and bias the universe (pass --raw, or --min-price 0)",
                    path.display(),
                    data.min_price
                );
            }
            HashMap::new()
        }
    };
    let to_bar = |b: &DailyBar| {
        to_core_bar(b).map(|mut bar| {
            bar.traded_close = traded.get(&(b.symbol.clone(), b.date)).copied();
            bar
        })
    };

    let raw = read_bars(path, &filter)?;
    let bars: Vec<Bar> = raw.iter().filter_map(to_bar).collect();
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
    let bench = match benchmark {
        Some(symbol) => {
            let filter = BarFilter { symbols: Some(HashSet::from([symbol.to_string()])), ..filter.clone() };
            Some(read_bars(path, &filter)?.iter().filter_map(to_core_bar).collect())
        }
        None => None,
    };
    Ok((bars, bench))
}
