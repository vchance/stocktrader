use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Result, bail};
use chrono::{NaiveDate, TimeDelta};
use clap::{Args, Parser, Subcommand, ValueEnum};
use rust_decimal::Decimal;
use st_core::{Bar, Event, EventSource, Strategy, Timestamp};
use st_data::{
    MergeSource, SyntheticConfig, VecEventSource, generate_daily_bars, load_daily_bars, session_close, session_open,
};
use st_engine::metrics::{self, buy_and_hold, correlation};
use st_engine::{Backtest, FillModel, Metrics, SimulatedBroker, WalkForwardConfig, walk_forward};
use st_marketdata::{BarEventSource, BarFilter, ReplayStats, date_range, read_bars, to_core_bar};
use st_risk::{RiskLimits, RiskManager};
use st_strategies::{AttentionSpike, SmaCrossover};
use tracing_subscriber::EnvFilter;

mod paper;

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
        /// History replayed before each window without trading, so indicators are warm. The default covers
        /// the slowest grid window (200 sessions) plus the 20-session liquidity lookback.
        #[arg(long, default_value_t = 330)]
        warmup_days: i64,
    },
    /// Before the open, trade the Alpaca paper account towards the strategy's current targets, sized and
    /// risk-checked exactly as in a backtest. Prints the plan only, unless --submit.
    Paper(Box<paper::PaperArgs>),
}

#[derive(Args)]
struct DataArgs {
    /// CSV of daily bars with header symbol,date,open,high,low,close,volume.
    #[arg(long, conflicts_with = "parquet")]
    bars: Option<PathBuf>,
    /// Parquet bars from `marketdata sync`, e.g. data/marketdata/bars_daily_all.parquet. `backtest` streams
    /// them from disk; `walk-forward` holds every matching bar in memory, so narrow it with
    /// --start/--end/--symbols.
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
    /// Latest attention to read. Replays stop at the last bar; the paper runner reads up to the present,
    /// since attention that arrived overnight is part of what it acts on.
    #[arg(skip)]
    attention_until: Option<Timestamp>,
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
        Command::Paper(args) => paper::run(&args),
    }
}

fn require_attention(data: &DataArgs) -> Result<()> {
    if data.attention.is_none() {
        bail!("attention-spike needs --attention data/signals/apewisdom.parquet");
    }
    Ok(())
}

impl StrategyArgs {
    fn params(&self, have_attention: bool) -> Result<Params> {
        Ok(match self.strategy {
            StrategyName::Sma => Params::Sma { fast: self.fast, slow: self.slow },
            StrategyName::AttentionSpike => {
                if !have_attention {
                    bail!("attention-spike needs --attention data/signals/apewisdom.parquet");
                }
                Params::Spike { min_mentions: self.spike_min_mentions, ratio: self.spike_ratio, hold_days: self.spike_hold_days }
            }
        })
    }
}

struct Loaded {
    events: Vec<Event>,
    /// Buy-and-hold comparison bars, if the benchmark symbol was found.
    benchmark: Option<Vec<Bar>>,
    /// Risk limits for this data: strict about traded prices when a raw file supplied them.
    limits: RiskLimits,
}

/// A replay that hasn't run yet, for a single pass over data that may not fit in memory.
struct Prepared {
    source: Box<dyn EventSource>,
    report: ReplayReport,
    benchmark: Option<Vec<Bar>>,
    limits: RiskLimits,
}

/// What a replay saw, to report once it has run.
struct ReplayReport {
    stats: Rc<RefCell<ReplayStats>>,
    /// The symbols asked for with --symbols, if any.
    wanted: Option<HashSet<String>>,
    /// Bars carry as-traded closes from a raw file.
    traded_prices: bool,
}

fn backtest(data: &DataArgs, args: &StrategyArgs) -> Result<()> {
    let params = args.params(data.attention.is_some())?;
    let strategy = make_strategy(&params, args.weight)?;
    let prepared = prepare(data)?;
    let report = Backtest::new(
        prepared.source,
        strategy,
        SimulatedBroker::new(fill_model(data)),
        RiskManager::new(prepared.limits),
        data.cash,
    )
    .run()?;
    report_replay(&prepared.report)?;

    println!("strategy          {params}");
    println!("bars replayed     {}", prepared.report.stats.borrow().bars);
    print_metrics("", &report.metrics);
    println!("fills             {}", report.metrics.num_fills);
    println!("commissions       {:.2}", report.metrics.commissions);
    println!("rejected signals  {}", report.rejections.len());
    println!("open positions    {}", report.final_portfolio.positions().count());

    if let Some(bench) = &prepared.benchmark {
        let refs: Vec<&Bar> = bench.iter().collect();
        let curve = buy_and_hold(&refs, data.cash);
        let m = metrics::compute(&curve, &[]);
        warn_on_span(&report.equity_curve, &curve);
        println!();
        println!("benchmark: {} buy & hold, 100% invested", data.benchmark.to_ascii_uppercase());
        print_metrics("  ", &m);
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
    // A candidate that can't warm up never trades, scores Sharpe 0, and so beats every losing candidate.
    let needed_sessions = grid
        .iter()
        .map(|p| match p {
            Params::Sma { slow, .. } => *slow,
            Params::Spike { .. } => 0,
        })
        .max()
        .unwrap_or(0)
        + RiskLimits::default().liquidity_lookback;
    let warmup_sessions = warmup.num_days() * 252 / 365;
    if (warmup_sessions as usize) < needed_sessions {
        bail!(
            "--warmup-days {} gives about {warmup_sessions} sessions, but the grid needs {needed_sessions} \
             (slowest window plus the liquidity lookback); raise --warmup-days",
            warmup.num_days()
        );
    }
    let loaded = load(data)?;
    let cfg = WalkForwardConfig {
        train,
        test,
        warmup,
        initial_cash: data.cash,
        fill: fill_model(data),
        limits: loaded.limits.clone(),
    };
    let report = walk_forward(&loaded.events, &cfg, grid, |p| make_strategy(p, weight).expect("validated above"))?;
    if report.folds.is_empty() {
        bail!("not enough data for one fold: need warm-up + training + some test period");
    }

    println!("{:<12} {:<12} {:<22} {:>9} {:>9} {:>9} {:>7}", "test from", "test to", "chosen", "train ret", "test ret", "test DD", "fills");
    for f in &report.folds {
        // A window with no bars returns 0% only because nothing happened; say so instead.
        let test_ret = if f.test.start_equity.is_zero() {
            "no data".to_string()
        } else {
            format!("{:.2}%", f.test.total_return * 100.0)
        };
        println!(
            "{:<12} {:<12} {:<22} {:>8.2}% {:>9} {:>8.2}% {:>7}",
            f.test_start.date_naive(),
            f.test_end.date_naive(),
            f.chosen.to_string(),
            f.train.total_return * 100.0,
            test_ret,
            f.test.max_drawdown * 100.0,
            f.test.num_fills
        );
    }
    println!();
    println!("out-of-sample return {:>7.2}%", report.out_of_sample_return * 100.0);
    println!("out-of-sample CAGR   {:>7.2}%", report.out_of_sample_cagr * 100.0);
    println!("(each test window starts flat from {} and ends with positions open, unliquidated)", data.cash);

    if let Some(bench) = &loaded.benchmark {
        let (from, to) = (report.folds[0].test_start, report.folds[report.folds.len() - 1].test_end);
        let refs: Vec<&Bar> = bench.iter().filter(|b| b.end >= from && b.end < to).collect();
        let m = metrics::compute(&buy_and_hold(&refs, data.cash), &[]);
        println!(
            "{} buy & hold CAGR   {:>7.2}% (same test span, 100% invested)",
            data.benchmark.to_ascii_uppercase(),
            m.cagr * 100.0
        );
    }
    Ok(())
}

/// Excess CAGR only means something if both curves cover the same period.
fn warn_on_span(strategy: &[(Timestamp, Decimal)], benchmark: &[(Timestamp, Decimal)]) {
    let span = |c: &[(Timestamp, Decimal)]| c.first().zip(c.last()).map(|(a, b)| (a.0.date_naive(), b.0.date_naive()));
    if let (Some((s0, s1)), Some((b0, b1))) = (span(strategy), span(benchmark))
        && ((s0 - b0).num_days().abs() > 3 || (s1 - b1).num_days().abs() > 3)
    {
        eprintln!("warning: strategy covers {s0}..{s1} but the benchmark covers {b0}..{b1}; excess CAGR compares different periods");
    }
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

/// `bars_daily_all.parquet` -> `bars_daily_raw.parquet` in the same directory.
fn raw_sibling(adjusted: &Path) -> Option<PathBuf> {
    let name = adjusted.file_name()?.to_str()?;
    name.contains("_all").then(|| adjusted.with_file_name(name.replacen("_all", "_raw", 1)))
}

/// Everything in memory, for runs that replay the data more than once (walk-forward) or need it as a whole
/// (paper trading's freshness check).
fn load(data: &DataArgs) -> Result<Loaded> {
    let prepared = prepare(data)?;
    let mut source = prepared.source;
    let events: Vec<Event> = std::iter::from_fn(|| source.next_event()).collect();
    report_replay(&prepared.report)?;
    Ok(Loaded { events, benchmark: prepared.benchmark, limits: prepared.limits })
}

/// The replay for `data`: Parquet bars are streamed from disk (with as-traded closes merged in from the raw
/// file), CSV and synthetic bars come from memory, and attention events are interleaved by time. Nothing is
/// checked until the source is drained; call [`report_replay`] afterwards.
fn prepare(data: &DataArgs) -> Result<Prepared> {
    let benchmark_symbol = (!data.benchmark.eq_ignore_ascii_case("none")).then(|| data.benchmark.to_ascii_uppercase());
    let (bars, benchmark, report, span, tickers): (Box<dyn EventSource>, _, _, _, _) = match (&data.bars, &data.parquet) {
        (None, Some(path)) => {
            let p = open_parquet(data, path, benchmark_symbol.as_deref())?;
            let tickers = p.report.wanted.clone();
            (Box::new(p.source), p.benchmark, p.report, p.span, tickers)
        }
        (bars_path, _) => {
            let bars = match bars_path {
                Some(path) => load_daily_bars(path)?,
                None => generate_daily_bars(&SyntheticConfig {
                    symbols: data.synthetic_symbols,
                    days: data.synthetic_days,
                    start: NaiveDate::from_ymd_opt(2023, 1, 3).expect("valid date"),
                    seed: data.seed,
                }),
            };
            let bench = benchmark_symbol.as_ref().map(|s| bars.iter().filter(|b| b.symbol.as_str() == s).cloned().collect());
            let symbols: HashSet<String> = bars.iter().map(|b| b.symbol.as_str().to_string()).collect();
            let span = bars.iter().map(|b| b.start).min().zip(bars.iter().map(|b| b.end).max());
            let stats = ReplayStats { bars: bars.len(), symbols: symbols.iter().cloned().collect(), ..ReplayStats::default() };
            let report = ReplayReport { stats: Rc::new(RefCell::new(stats)), wanted: None, traded_prices: false };
            let source = VecEventSource::new(bars.into_iter().map(Event::bar).collect());
            (Box::new(source), bench, report, span, Some(symbols))
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

    // Attention only matters for symbols we have prices for, within the replayed period; anything else
    // would cost memory and stretch the metrics' time span.
    let mut attention = Vec::new();
    if let Some(path) = &data.attention {
        let (first, last) = (span.map(|s| s.0), span.map(|s| s.1));
        let filter = st_signals::RowFilter { tickers, from: first, to: data.attention_until.or(last) };
        let rows = st_signals::read_rows(path, &filter)?;
        println!("attention events  {}", rows.len());
        warn_on_coverage(first, rows.iter().map(|r| r.observed_at).min());
        attention = st_signals::to_events(&rows);
    }
    let source: Box<dyn EventSource> = Box::new(MergeSource::new(bars, VecEventSource::new(attention)));
    let limits =
        RiskLimits { min_price: data.min_price, require_traded_price: report.traded_prices, ..RiskLimits::default() };
    Ok(Prepared { source, report, benchmark, limits })
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

struct ParquetReplay {
    /// Adjusted bars, each carrying its as-traded close when the raw file is there.
    source: BarEventSource,
    report: ReplayReport,
    /// Read separately, so `--symbols` needn't include it.
    benchmark: Option<Vec<Bar>>,
    /// From the first session's open to the last session's close.
    span: Option<(Timestamp, Timestamp)>,
}

fn open_parquet(data: &DataArgs, path: &Path, benchmark: Option<&str>) -> Result<ParquetReplay> {
    let symbols: Option<HashSet<String>> =
        (!data.symbols.is_empty()).then(|| data.symbols.iter().map(|s| s.trim().to_ascii_uppercase()).collect());
    let filter = BarFilter { symbols: symbols.clone(), start: data.start, end: data.end };
    let raw_path = match &data.raw {
        Some(raw) if !raw.exists() => bail!("--raw {} does not exist", raw.display()),
        Some(raw) => Some(raw.clone()),
        None => raw_sibling(path).filter(|p| p.exists()),
    };
    if raw_path.is_none() && data.min_price > Decimal::ZERO {
        eprintln!(
            "warning: no raw bars found beside {}; --min-price {} will see split-adjusted prices, which \
             understate pre-split prices and bias the universe (pass --raw, or --min-price 0)",
            path.display(),
            data.min_price
        );
    }
    let (source, stats) = BarEventSource::open(path, raw_path.as_deref(), &filter)?;
    let span = date_range(path)?.map(|(first, last)| {
        let first = data.start.map_or(first, |s| s.max(first));
        let last = data.end.map_or(last, |e| e.min(last));
        (session_open(first), session_close(last))
    });
    let bench = match benchmark {
        Some(symbol) => {
            let filter = BarFilter { symbols: Some(HashSet::from([symbol.to_string()])), ..filter };
            Some(read_bars(path, &filter)?.iter().filter_map(to_core_bar).collect())
        }
        None => None,
    };
    let report = ReplayReport { stats, wanted: symbols, traded_prices: raw_path.is_some() };
    Ok(ParquetReplay { source, report, benchmark: bench, span })
}

/// After a replay: fails on a read error that cut it short, and reports skipped bars, symbols that were
/// asked for but never seen, and bars lacking an as-traded price (which fail closed in the risk check).
fn report_replay(report: &ReplayReport) -> Result<()> {
    let stats = report.stats.borrow();
    if let Some(error) = &stats.error {
        bail!("reading bars failed partway, so results are incomplete: {error}");
    }
    if stats.invalid > 0 {
        eprintln!("skipped {} bars with invalid prices", stats.invalid);
    }
    if let Some(wanted) = &report.wanted {
        let mut missing: Vec<&str> = wanted.iter().map(String::as_str).filter(|s| !stats.symbols.contains(*s)).collect();
        missing.sort_unstable();
        if !missing.is_empty() {
            eprintln!("no bars for: {}", missing.join(", "));
        }
    }
    if !report.traded_prices || stats.missing_traded.is_empty() {
        return Ok(());
    }
    let missing: usize = stats.missing_traded.values().sum();
    let mut worst: Vec<(&String, &usize)> = stats.missing_traded.iter().collect();
    worst.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let names: Vec<String> = worst.iter().take(10).map(|(s, n)| format!("{s} ({n})")).collect();
    eprintln!(
        "warning: {} of {} bars ({:.1}%) have no as-traded price; entries on those days are rejected rather \
         than judged on adjusted prices. Most affected: {}",
        missing,
        stats.bars,
        100.0 * missing as f64 / stats.bars.max(1) as f64,
        names.join(", ")
    );
    Ok(())
}
