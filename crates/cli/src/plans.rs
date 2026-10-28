//! Trading plans: named, saved descriptions of how to trade (strategy, universe, risk and capital) kept as
//! TOML files in `plans/`, so several can be backtested, compared and later paper-traded side by side.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use clap::{Args, Subcommand};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Deserializer};
use st_risk::RiskLimits;

use crate::research::{self, Research, Window};
use crate::{DataArgs, Params, Run, check_top_n, print_run, report, run_backtest};

#[derive(Args)]
pub struct PlansArgs {
    /// Directory of plan files (`<name>.toml`).
    #[arg(long, env = "STOCKTRADER_PLANS_DIR", default_value = "plans")]
    plans_dir: PathBuf,
    #[command(subcommand)]
    command: PlansCommand,
}

#[derive(Subcommand)]
enum PlansCommand {
    /// List every plan, and check that each file is valid.
    List,
    /// Backtest one plan against its benchmark and write an HTML report.
    Backtest {
        name: String,
        #[command(flatten)]
        run: RunArgs,
    },
    /// Show how many experiments have been run, per plan, including looks at the held-back period.
    Log {
        /// How many recent runs to list.
        #[arg(long, default_value_t = 20)]
        last: usize,
        /// Defaults to research/experiments.jsonl beside the plans directory.
        #[arg(long, env = "STOCKTRADER_RESEARCH_LOG")]
        research_log: Option<PathBuf>,
    },
    /// Backtest several plans (all, if none are named) over the same dates and write a comparison report.
    Compare {
        names: Vec<String>,
        #[command(flatten)]
        run: RunArgs,
    },
    /// Before the open, trade each plan in paper mode (or the plans named) in its own Alpaca paper account,
    /// with keys from APCA_API_KEY_ID_<PLAN> and APCA_API_SECRET_KEY_<PLAN>. Prints the plans only, unless
    /// --submit.
    Paper {
        names: Vec<String>,
        #[command(flatten)]
        run: PaperRunArgs,
    },
}

#[derive(Args)]
struct RunArgs {
    /// First session (overrides the plan's `[backtest] start`).
    #[arg(long)]
    start: Option<NaiveDate>,
    /// Last session (overrides the plan's `[backtest] end`).
    #[arg(long)]
    end: Option<NaiveDate>,
    #[command(flatten)]
    dirs: DataDirs,
    /// Evaluate the held-back period set in plans/_research.toml (warming up on the history before it).
    /// Ordinary runs never include it; look at it once a plan is final, and expect each look to be logged.
    #[arg(long)]
    holdout: bool,
    /// Every run is appended here, so the number of experiments behind a result stays visible. Defaults to
    /// research/experiments.jsonl beside the plans directory, wherever the command is run from.
    #[arg(long, env = "STOCKTRADER_RESEARCH_LOG")]
    research_log: Option<PathBuf>,
    /// Run on generated prices instead of market data, to try the plumbing before any data is downloaded
    /// (prices only: not for strategies that need fundamentals or attention).
    #[arg(long)]
    synthetic: bool,
    /// Where HTML reports are written.
    #[arg(long, env = "STOCKTRADER_REPORTS_DIR", default_value = "reports")]
    reports_dir: PathBuf,
}

#[derive(Args)]
struct DataDirs {
    /// Directory with bars_daily_all.parquet (and the raw file) from `marketdata sync`.
    #[arg(long, env = "STOCKTRADER_MARKETDATA_DIR", default_value = "data/marketdata")]
    marketdata_dir: PathBuf,
    /// Directory from `fundamentals sync`, for fundamentals strategies.
    #[arg(long, env = "STOCKTRADER_FUNDAMENTALS_DIR", default_value = "data/fundamentals")]
    fundamentals_dir: PathBuf,
    /// Directory with apewisdom.parquet from `signals apewisdom`, for attention strategies.
    #[arg(long, env = "STOCKTRADER_SIGNALS_DIR", default_value = "data/signals")]
    signals_dir: PathBuf,
}

#[derive(Args)]
struct PaperRunArgs {
    #[command(flatten)]
    dirs: DataDirs,
    /// Send the orders. Without this each plan's orders are only printed and journaled.
    #[arg(long)]
    submit: bool,
    /// Allow submitting while the market is open (orders then fill at once, not at the open).
    #[arg(long)]
    allow_open: bool,
    /// Journals go to `<dir>/<plan>/journal/`. A KILL file in `<dir>` stops new exposure for every plan,
    /// one in `<dir>/<plan>` for that plan; exits still go out.
    #[arg(long, env = "STOCKTRADER_LIVE_DIR", default_value = "data/live")]
    live_dir: PathBuf,
    /// Alpaca trading endpoint; anything other than the paper endpoint is refused.
    #[arg(long, default_value = st_paper::PAPER_URL)]
    trading_url: String,
}

/// A plan file. Unknown keys are errors, so a typo can't silently fall back to a default.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// Must match the file name (`<name>.toml`).
    pub name: String,
    /// What this plan is for, in your own words.
    #[serde(default)]
    pub notes: String,
    /// `backtest` (the default) or `paper`.
    #[serde(default)]
    pub mode: Mode,
    /// Starting cash for backtests.
    #[serde(default = "default_capital")]
    pub capital: Decimal,
    pub strategy: StrategySpec,
    #[serde(default)]
    pub universe: Universe,
    #[serde(default)]
    pub risk: Risk,
    #[serde(default)]
    pub backtest: BacktestWindow,
    #[serde(default)]
    pub paper: PaperSettings,
}

/// Settings for paper runs (`mode = "paper"`).
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PaperSettings {
    /// Refuse a run that would place more orders than this.
    #[serde(default = "default_max_orders")]
    pub max_orders: usize,
    /// Calendar days of history replayed before each run to warm up the strategy.
    #[serde(default = "default_history_days")]
    pub history_days: u64,
}

impl Default for PaperSettings {
    fn default() -> Self {
        Self { max_orders: default_max_orders(), history_days: default_history_days() }
    }
}

fn default_max_orders() -> usize {
    20
}
fn default_history_days() -> u64 {
    400
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    #[default]
    Backtest,
    Paper,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Backtest => "backtest",
            Mode::Paper => "paper",
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum StrategySpec {
    /// Long `weight` while the fast moving average is above the slow one.
    Sma {
        fast: usize,
        slow: usize,
        #[serde(default = "default_weight")]
        weight: Decimal,
    },
    /// Buy on a jump in social-media mentions; needs the attention table.
    AttentionSpike {
        #[serde(default = "default_min_mentions")]
        min_mentions: u32,
        #[serde(default = "default_ratio")]
        ratio: f64,
        #[serde(default = "default_hold_days")]
        hold_days: u32,
        #[serde(default = "default_weight")]
        weight: Decimal,
    },
    /// Monthly ranking on earnings yield plus return on assets; needs fundamentals.
    QualityValue {
        #[serde(default = "default_top_n")]
        top_n: usize,
        /// Rank only the largest this many eligible companies by market value.
        largest: Option<usize>,
    },
    /// Every symbol in `[universe] symbols` at the same weight, re-decided monthly; with `trend`, only those
    /// whose close is above their `trend`-session average.
    EqualWeight {
        trend: Option<usize>,
    },
    /// Monthly ranking on the return from `lookback` to `skip` sessions ago.
    Momentum {
        #[serde(default = "default_top_n")]
        top_n: usize,
        #[serde(default = "default_lookback")]
        lookback: usize,
        #[serde(default = "default_skip")]
        skip: usize,
        /// Rank only the most heavily traded this many eligible stocks.
        largest: Option<usize>,
    },
}

fn default_lookback() -> usize {
    252
}
fn default_skip() -> usize {
    21
}

/// Which stocks the plan may trade.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Universe {
    /// Only these symbols; empty means the whole market in the data.
    #[serde(default)]
    pub symbols: Vec<String>,
    /// Skip stocks trading below this (as traded, not split-adjusted).
    #[serde(default = "default_min_price")]
    pub min_price: Decimal,
    /// Skip stocks averaging less than this in daily dollar volume over 20 sessions.
    #[serde(default = "default_min_dollar_volume")]
    pub min_dollar_volume: Decimal,
}

impl Default for Universe {
    fn default() -> Self {
        Self { symbols: Vec::new(), min_price: default_min_price(), min_dollar_volume: default_min_dollar_volume() }
    }
}

/// Overrides of the risk manager's limits, each a fraction of equity.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Risk {
    /// Largest single position (default 0.10).
    pub max_position: Option<Decimal>,
    /// Sum of all positions; 1.0 means fully invested, no leverage (default 1.0).
    pub max_gross_exposure: Option<Decimal>,
    /// Stop adding exposure for the day after losing this much since the previous close (default 0.03).
    pub max_daily_loss: Option<Decimal>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BacktestWindow {
    #[serde(default, deserialize_with = "toml_date")]
    pub start: Option<NaiveDate>,
    #[serde(default, deserialize_with = "toml_date")]
    pub end: Option<NaiveDate>,
    /// Buy-and-hold comparison symbol, or "none".
    #[serde(default = "default_benchmark")]
    pub benchmark: String,
    #[serde(default = "default_slippage")]
    pub slippage_bps: Decimal,
}

impl Default for BacktestWindow {
    fn default() -> Self {
        Self { start: None, end: None, benchmark: default_benchmark(), slippage_bps: default_slippage() }
    }
}

fn default_capital() -> Decimal {
    dec!(100_000)
}
fn default_weight() -> Decimal {
    dec!(0.05)
}
fn default_min_mentions() -> u32 {
    50
}
fn default_ratio() -> f64 {
    3.0
}
fn default_hold_days() -> u32 {
    5
}
fn default_top_n() -> usize {
    20
}
fn default_min_price() -> Decimal {
    dec!(5)
}
fn default_min_dollar_volume() -> Decimal {
    dec!(5_000_000)
}
fn default_benchmark() -> String {
    "SPY".into()
}
fn default_slippage() -> Decimal {
    dec!(5)
}

/// Accepts a bare TOML date (`start = 2018-01-01`).
pub(crate) fn toml_date<'de, D: Deserializer<'de>>(d: D) -> Result<Option<NaiveDate>, D::Error> {
    let Some(dt) = Option::<toml::value::Datetime>::deserialize(d)? else {
        return Ok(None);
    };
    match (dt.date, dt.time) {
        (Some(date), None) => NaiveDate::from_ymd_opt(i32::from(date.year), u32::from(date.month), u32::from(date.day))
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid date {dt}"))),
        _ => Err(serde::de::Error::custom(format!("expected a date like 2018-01-01, got {dt}"))),
    }
}

impl StrategySpec {
    fn kind(&self) -> &'static str {
        match self {
            StrategySpec::EqualWeight { .. } => "equal-weight",
            StrategySpec::Sma { .. } => "sma",
            StrategySpec::AttentionSpike { .. } => "attention-spike",
            StrategySpec::QualityValue { .. } => "quality-value",
            StrategySpec::Momentum { .. } => "momentum",
        }
    }

    /// `slots` is the number of symbols in the plan's universe (for equal weight).
    fn params(&self, slots: usize) -> (Params, Decimal) {
        match *self {
            StrategySpec::EqualWeight { trend } => (Params::EqualWeight { slots, trend }, Decimal::ONE),
            StrategySpec::Sma { fast, slow, weight } => (Params::Sma { fast, slow }, weight),
            StrategySpec::AttentionSpike { min_mentions, ratio, hold_days, weight } => {
                (Params::Spike { min_mentions, ratio, hold_days }, weight)
            }
            // Weight is 1/top_n inside these strategies.
            StrategySpec::QualityValue { top_n, largest } => (Params::QualityValue { top_n, largest }, Decimal::ONE),
            StrategySpec::Momentum { top_n, lookback, skip, largest } => {
                (Params::Momentum { top_n, lookback, skip, largest }, Decimal::ONE)
            }
        }
    }
}

impl Plan {
    /// Parses and checks a plan file; its `name` must match the file name.
    pub fn load(path: &Path) -> Result<(Self, String)> {
        let source = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let plan: Plan = toml::from_str(&source).with_context(|| format!("parsing {}", path.display()))?;
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        if plan.name != stem {
            bail!("{}: name \"{}\" must match the file name \"{stem}\"", path.display(), plan.name);
        }
        plan.validate().with_context(|| format!("checking {}", path.display()))?;
        Ok((plan, source))
    }

    fn validate(&self) -> Result<()> {
        if self.name.is_empty() || !self.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            bail!("name may only use lowercase letters, digits and dashes");
        }
        // Each plan's paper files live in data/live/<name>/, beside the shared KILL file and journal.
        if matches!(self.name.as_str(), "kill" | "journal") {
            bail!("\"{}\" is reserved; choose another name", self.name);
        }
        if self.capital <= Decimal::ZERO {
            bail!("capital must be positive");
        }
        let fraction = |what: &str, v: Option<Decimal>, max: Decimal| match v {
            Some(v) if v <= Decimal::ZERO || v > max => bail!("{what} must be above 0 and at most {max}, got {v}"),
            _ => Ok(()),
        };
        fraction("risk.max_position", self.risk.max_position, Decimal::ONE)?;
        fraction("risk.max_gross_exposure", self.risk.max_gross_exposure, Decimal::ONE)?;
        fraction("risk.max_daily_loss", self.risk.max_daily_loss, Decimal::ONE)?;
        let mut seen = std::collections::HashSet::new();
        if let Some(dup) = self.universe.symbols.iter().find(|s| !seen.insert(s.trim().to_ascii_uppercase())) {
            bail!("universe.symbols lists {dup} more than once");
        }
        if self.universe.min_price < Decimal::ZERO || self.universe.min_dollar_volume < Decimal::ZERO {
            bail!("universe filters can't be negative");
        }
        if let (Some(s), Some(e)) = (self.backtest.start, self.backtest.end)
            && s >= e
        {
            bail!("backtest.start must be before backtest.end");
        }
        let limits = self.limits(RiskLimits::default());
        match &self.strategy {
            StrategySpec::EqualWeight { .. } => {
                if self.universe.symbols.is_empty() {
                    bail!("equal-weight needs [universe] symbols: the basket to hold");
                }
                check_top_n(self.universe.symbols.len(), limits.max_position_weight)?;
                check_full_exposure(self.strategy.kind(), &limits)?;
            }
            StrategySpec::QualityValue { top_n, largest } | StrategySpec::Momentum { top_n, largest, .. } => {
                check_top_n(*top_n, limits.max_position_weight)?;
                crate::check_largest(*top_n, *largest)?;
                check_full_exposure(self.strategy.kind(), &limits)?;
            }
            StrategySpec::Sma { weight, .. } | StrategySpec::AttentionSpike { weight, .. } => {
                if *weight <= Decimal::ZERO || *weight > Decimal::ONE {
                    bail!("strategy.weight must be above 0 and at most 1");
                }
                // Above the cap, every entry would be clamped and every later signal rejected.
                if *weight > limits.max_position_weight {
                    bail!("strategy.weight {weight} is above risk.max_position {}", limits.max_position_weight);
                }
            }
        }
        let (params, weight) = self.strategy.params(self.universe.symbols.len());
        crate::make_strategy(&params, weight, &limits)?;
        Ok(())
    }

    /// `base` with this plan's universe filters and risk overrides applied.
    pub fn limits(&self, base: RiskLimits) -> RiskLimits {
        RiskLimits {
            min_price: self.universe.min_price,
            min_avg_dollar_volume: self.universe.min_dollar_volume,
            max_position_weight: self.risk.max_position.unwrap_or(base.max_position_weight),
            max_gross_exposure: self.risk.max_gross_exposure.unwrap_or(base.max_gross_exposure),
            max_daily_loss: self.risk.max_daily_loss.unwrap_or(base.max_daily_loss),
            ..base
        }
    }

    fn universe_label(&self) -> String {
        if self.universe.symbols.is_empty() {
            "whole market".into()
        } else if self.universe.symbols.len() <= 4 {
            self.universe.symbols.join(",")
        } else {
            format!("{} symbols", self.universe.symbols.len())
        }
    }

    /// The replay inputs for this plan over `window`.
    fn data_args(&self, dirs: &DataDirs, synthetic: bool, window: (Option<NaiveDate>, Option<NaiveDate>)) -> Result<DataArgs> {
        if synthetic && matches!(self.strategy, StrategySpec::EqualWeight { .. }) {
            bail!("plan {} holds a list of real symbols; --synthetic generates other ones", self.name);
        }
        let needs_attention = matches!(self.strategy, StrategySpec::AttentionSpike { .. });
        let needs_fundamentals = matches!(self.strategy, StrategySpec::QualityValue { .. });
        if synthetic && (needs_attention || needs_fundamentals) {
            bail!("plan {} needs real data (fundamentals or attention); --synthetic only generates prices", self.name);
        }
        let parquet = dirs.marketdata_dir.join("bars_daily_all.parquet");
        if !synthetic && !parquet.exists() {
            bail!("no market data at {}; run `marketdata sync` first (backtests can also try --synthetic)", parquet.display());
        }
        Ok(DataArgs {
            bars: None,
            parquet: (!synthetic).then_some(parquet),
            raw: None,
            start: if synthetic { None } else { window.0 },
            end: if synthetic { None } else { window.1 },
            symbols: if synthetic { Vec::new() } else { self.universe.symbols.clone() },
            attention: needs_attention.then(|| dirs.signals_dir.join("apewisdom.parquet")),
            fundamentals: needs_fundamentals.then(|| dirs.fundamentals_dir.clone()),
            benchmark: if synthetic { "none".into() } else { self.backtest.benchmark.clone() },
            synthetic_symbols: 50,
            synthetic_days: 750,
            seed: 42,
            cash: self.capital,
            slippage_bps: self.backtest.slippage_bps,
            min_price: self.universe.min_price,
            attention_until: None,
        })
    }

    /// The paper run for this plan: its universe, risk and strategy, its own journal and kill file.
    fn paper_job(&self, dirs: &DataDirs, live_dir: &Path) -> Result<crate::paper::Job> {
        let (params, weight) = self.strategy.params(self.universe.symbols.len());
        let mut data = self.data_args(dirs, false, (None, None))?;
        data.benchmark = "none".into();
        let plan = self.clone();
        let plan_dir = live_dir.join(&self.name);
        Ok(crate::paper::Job {
            label: self.name.clone(),
            params,
            weight,
            data,
            limits: Box::new(move |limits| plan.limits(limits)),
            symbols: self.universe.symbols.clone(),
            kill_files: vec![live_dir.join("KILL"), plan_dir.join("KILL")],
            live_dir: plan_dir,
            history_days: self.paper.history_days,
            max_orders: self.paper.max_orders,
        })
    }

    fn run(&self, args: &RunArgs, window: &Window) -> Result<Run> {
        let data = self.data_args(&args.dirs, args.synthetic, (window.start, window.end))?;
        let (params, weight) = self.strategy.params(self.universe.symbols.len());
        let trade_from = if args.synthetic { None } else { window.trade_from.map(st_data::session_open) };
        let run = run_backtest(&data, &params, weight, trade_from, |limits| *limits = self.limits(limits.clone()))?;
        if window.holdout && run.report.equity_curve.is_empty() {
            bail!("no market data in the held-back period for plan {}; nothing to evaluate (not logged)", self.name);
        }
        Ok(run)
    }

    fn result(&self, source: &str, run: &Run) -> research::PlanResult {
        let m = &run.report.metrics;
        research::PlanResult {
            name: self.name.clone(),
            source: source.to_string(),
            total_return: m.total_return,
            cagr: m.cagr,
            max_drawdown: m.max_drawdown,
            sharpe: m.sharpe,
            fills: m.num_fills,
            benchmark: run.benchmark.as_ref().map(|_| self.backtest.benchmark.to_ascii_uppercase()),
            benchmark_cagr: run.benchmark.as_ref().map(|(_, b)| b.cagr),
        }
    }
}

/// A parsed plan and the file it came from, verbatim.
type PlanFile = (Plan, String);

/// Strategies that put 1/N in each of N names fill the whole account; a lower gross cap would fill only
/// the first names alphabetically.
fn check_full_exposure(kind: &str, limits: &RiskLimits) -> Result<()> {
    if limits.max_gross_exposure < Decimal::ONE {
        bail!("{kind} invests 1/N in each of N names, so it needs risk.max_gross_exposure of 1, got {}", limits.max_gross_exposure);
    }
    Ok(())
}

/// Every `*.toml` in `dir`, sorted, each parsed (or the error explaining why it isn't valid).
fn load_all(dir: &Path) -> Result<Vec<(PathBuf, Result<PlanFile>)>> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("reading plans directory {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        // `_research.toml` and other `_` files are settings, not plans.
        .filter(|p| !p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('_')))
        .collect();
    paths.sort();
    Ok(paths.into_iter().map(|p| (p.clone(), Plan::load(&p))).collect())
}

fn load_named(dir: &Path, name: &str) -> Result<(Plan, String)> {
    let path = dir.join(format!("{name}.toml"));
    if !path.exists() {
        bail!("no plan {name} ({} does not exist); see `stocktrader plans list`", path.display());
    }
    Plan::load(&path)
}

/// The experiment log: as given, or research/experiments.jsonl beside the plans directory.
fn research_log(plans_dir: &Path, given: Option<&PathBuf>) -> PathBuf {
    given.cloned().unwrap_or_else(|| {
        let base = plans_dir.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        base.join("research").join("experiments.jsonl")
    })
}

/// The run's window under the research settings. Synthetic data has its own dates, so it ignores them.
fn window(dir: &Path, args: &RunArgs, planned: (Option<NaiveDate>, Option<NaiveDate>)) -> Result<Window> {
    if args.synthetic {
        if args.holdout {
            bail!("--holdout needs real data; synthetic prices have no held-back period");
        }
        return Ok(Window::default());
    }
    research::resolve(&Research::load(dir)?, (args.start, args.end), planned, args.holdout)
}

/// Logs a run, before any of its results are shown, so a look at the held-back period can't go unrecorded;
/// for held-back runs, says how many times each plan's held-back period has now been seen.
fn log_run(plans_dir: &Path, args: &RunArgs, command: &str, window: &Window, results: Vec<research::PlanResult>) -> Result<()> {
    let log = research_log(plans_dir, args.research_log.as_ref());
    if !log.exists() {
        eprintln!("starting a new experiment log at {}", log.display());
    }
    let names: Vec<String> = results.iter().map(|r| r.name.clone()).collect();
    let entry = research::Entry { at: chrono::Utc::now(), command: command.into(), synthetic: args.synthetic, window: *window, plans: results };
    research::append(&log, &entry).context("the run could not be logged, so its results are not shown")?;
    if window.holdout && !args.synthetic {
        let (entries, bad) = research::read(&log)?;
        if bad > 0 {
            eprintln!("warning: {bad} unreadable line(s) in {}; the look counts below may be low", log.display());
        }
        for name in names {
            let looks = research::holdout_looks(&entries, &name);
            let note = if looks > 1 { ": results seen more than once are no longer unbiased" } else { "" };
            println!("held-back looks   {name}: {looks}{note}");
        }
    }
    Ok(())
}

fn write_report(dir: &Path, file: &str, html: &str) -> Result<PathBuf> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(file);
    fs::write(&path, html).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

pub fn run(args: &PlansArgs) -> Result<()> {
    match &args.command {
        PlansCommand::List => {
            let plans = load_all(&args.plans_dir)?;
            if plans.is_empty() {
                println!("no plans in {}", args.plans_dir.display());
            }
            let mut invalid = 0;
            println!("{:<24} {:<9} {:<44} {:<16} notes", "name", "mode", "strategy", "universe");
            for (path, plan) in &plans {
                match plan {
                    Ok((p, _)) => {
                        let (params, _) = p.strategy.params(p.universe.symbols.len());
                        let notes = p.notes.lines().next().unwrap_or("");
                        println!("{:<24} {:<9} {:<44} {:<16} {notes}", p.name, p.mode.to_string(), params.to_string(), p.universe_label());
                    }
                    Err(e) => {
                        invalid += 1;
                        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
                        println!("{name:<24} INVALID   {e:#}");
                    }
                }
            }
            if invalid > 0 {
                bail!("{invalid} invalid plan file(s)");
            }
            Ok(())
        }
        PlansCommand::Backtest { name, run } => {
            let (plan, source) = load_named(&args.plans_dir, name)?;
            let window = window(&args.plans_dir, run, (plan.backtest.start, plan.backtest.end))?;
            let result = plan.run(run, &window)?;
            let (params, _) = plan.strategy.params(plan.universe.symbols.len());
            println!("plan              {} ({params})", plan.name);
            if !run.synthetic {
                println!("window            {}", window.describe());
            }
            log_run(&args.plans_dir, run, "backtest", &window, vec![plan.result(&source, &result)])?;
            print_run(&result, &plan.backtest.benchmark);
            let html = report::plan_report(&plan, &source, &result, run.synthetic, &window);
            let path = write_report(&run.reports_dir, &format!("{}.html", plan.name), &html)?;
            println!("report            {}", path.display());
            Ok(())
        }
        PlansCommand::Compare { names, run } => {
            let plans: Vec<(Plan, String)> = if names.is_empty() {
                load_all(&args.plans_dir)?.into_iter().map(|(_, p)| p).collect::<Result<_>>()?
            } else {
                names.iter().map(|n| load_named(&args.plans_dir, n)).collect::<Result<_>>()?
            };
            if plans.is_empty() {
                bail!("no plans to compare in {}", args.plans_dir.display());
            }
            // One window for all: the latest start and the earliest end any plan asks for.
            let start = plans.iter().filter_map(|(p, _)| p.backtest.start).max();
            let end = plans.iter().filter_map(|(p, _)| p.backtest.end).min();
            let window = window(&args.plans_dir, run, (start, end))?;
            if let (Some(s), Some(e)) = (window.trade_from.or(window.start), window.end)
                && s >= e
            {
                bail!("the plans' backtest windows don't overlap ({s} to {e}); pass --start and --end");
            }
            if !run.synthetic {
                println!("window            {}", window.describe());
            }
            let mut results = Vec::new();
            for (plan, _) in &plans {
                eprintln!("running {}", plan.name);
                results.push((plan, plan.run(run, &window)?));
            }
            let logged = results.iter().zip(&plans).map(|((plan, r), (_, source))| plan.result(source, r)).collect();
            log_run(&args.plans_dir, run, "compare", &window, logged)?;
            println!("{:<24} {:>9} {:>9} {:>8} {:>8} {:>10} {:>7}", "plan", "return", "CAGR", "max DD", "Sharpe", "vs bench", "fills");
            for (plan, r) in &results {
                let m = &r.report.metrics;
                let excess = r.benchmark.as_ref().map_or("-".to_string(), |(_, b)| format!("{:+.2}%", (m.cagr - b.cagr) * 100.0));
                println!(
                    "{:<24} {:>8.2}% {:>8.2}% {:>7.2}% {:>8.2} {:>10} {:>7}",
                    plan.name,
                    m.total_return * 100.0,
                    m.cagr * 100.0,
                    m.max_drawdown * 100.0,
                    m.sharpe,
                    excess,
                    m.num_fills
                );
            }
            let html = report::compare_report(&results, run.synthetic, &window);
            let path = write_report(&run.reports_dir, "compare.html", &html)?;
            println!("report            {}", path.display());
            Ok(())
        }
        PlansCommand::Paper { names, run } => paper(&args.plans_dir, names, run),
        PlansCommand::Log { last, research_log: log } => research::print_log(&research_log(&args.plans_dir, log.as_ref()), *last),
    }
}

/// `_QUALITY_VALUE_CORE` for `quality-value-core`: the suffix of the plan's Alpaca key variables.
pub fn account_suffix(name: &str) -> String {
    format!("_{}", name.to_ascii_uppercase().replace('-', "_"))
}

fn paper(dir: &Path, names: &[String], run: &PaperRunArgs) -> Result<()> {
    let plans: Vec<Plan> = if names.is_empty() {
        load_all(dir)?
            .into_iter()
            .map(|(_, p)| p.map(|(plan, _)| plan))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|p| p.mode == Mode::Paper)
            .collect()
    } else {
        let named: Vec<Plan> = names.iter().map(|n| load_named(dir, n).map(|(p, _)| p)).collect::<Result<_>>()?;
        if let Some(p) = named.iter().find(|p| p.mode != Mode::Paper) {
            bail!("plan {} is in {} mode; set mode = \"paper\" to paper-trade it", p.name, p.mode);
        }
        named
    };
    if plans.is_empty() {
        println!("no plans in paper mode in {}", dir.display());
        return Ok(());
    }
    // Every plan needs its own account: two plans sharing one would trade against each other's positions.
    // Checked by key and then by the account each key opens (one account can have several key pairs), and
    // against the shared keys used by `stocktrader paper`, all before anything trades. `.env` is loaded first
    // (it never overrides the environment) so a shared key kept only there is seen too.
    let _ = dotenvy::dotenv();
    let shared_key = std::env::var("APCA_API_KEY_ID").ok().map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
    let mut clients = Vec::new();
    let mut by_key: HashMap<String, &str> = HashMap::new();
    for plan in &plans {
        let (client, key_id) = crate::paper::client_from_env(&account_suffix(&plan.name), &run.trading_url)
            .with_context(|| format!("plan {} needs its own Alpaca paper account", plan.name))?;
        if shared_key.as_deref() == Some(key_id.as_str()) {
            bail!("plan {} uses the shared APCA_API_KEY_ID account; give each plan its own paper account", plan.name);
        }
        if let Some(other) = by_key.insert(key_id, &plan.name) {
            bail!("plans {other} and {} use the same Alpaca account; give each plan its own paper account", plan.name);
        }
        clients.push(client);
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let mut by_account: HashMap<String, &str> = HashMap::new();
    for (plan, client) in plans.iter().zip(&clients) {
        let account = rt.block_on(client.account()).with_context(|| format!("reading plan {}'s Alpaca account", plan.name))?;
        if account.id.is_empty() {
            bail!("Alpaca returned no account ID for plan {}; can't confirm it has its own account", plan.name);
        }
        if let Some(other) = by_account.insert(account.id, &plan.name) {
            bail!("plans {other} and {} use keys for the same Alpaca account; give each plan its own paper account", plan.name);
        }
    }

    let mut failed = Vec::new();
    for (plan, client) in plans.iter().zip(&clients) {
        println!();
        println!("plan              {}", plan.name);
        let result = plan.paper_job(&run.dirs, &run.live_dir).and_then(|job| {
            rt.block_on(crate::paper::run_job(&job, client, run.submit, run.allow_open))
        });
        if let Err(e) = result {
            println!("Error: {e:#}");
            failed.push(plan.name.clone());
        }
    }
    println!();
    for plan in &plans {
        let state = if failed.contains(&plan.name) { "FAILED" } else { "ok" };
        println!("paper plan        {:<24} {state}", plan.name);
    }
    if !failed.is_empty() {
        bail!("{} of {} plans failed: {}", failed.len(), plans.len(), failed.join(", "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Result<Plan> {
        let plan: Plan = toml::from_str(toml)?;
        plan.validate()?;
        Ok(plan)
    }

    #[test]
    fn parses_a_full_plan_with_defaults_filled_in() {
        let plan = parse(
            r#"
            name = "core"
            notes = "Cheap, profitable companies."
            mode = "paper"
            capital = 50000
            [strategy]
            kind = "quality-value"
            top_n = 25
            [universe]
            min_price = 10
            [risk]
            max_position = 0.06
            [backtest]
            start = 2018-01-01
            benchmark = "QQQ"
            "#,
        )
        .unwrap();
        assert_eq!(plan.mode, Mode::Paper);
        assert_eq!(plan.strategy, StrategySpec::QualityValue { top_n: 25, largest: None });
        assert_eq!(plan.backtest.start, NaiveDate::from_ymd_opt(2018, 1, 1));
        let limits = plan.limits(RiskLimits::default());
        assert_eq!((limits.min_price, limits.max_position_weight, limits.max_daily_loss), (dec!(10), dec!(0.06), dec!(0.03)));
        assert_eq!(limits.min_avg_dollar_volume, dec!(5_000_000));
    }

    #[test]
    fn rejects_typos_and_inconsistent_settings() {
        let base = "name = \"p\"\n[strategy]\nkind = \"sma\"\nfast = 5\nslow = 20\n";
        assert!(parse(base).is_ok());
        assert!(parse(&format!("{base}wieght = 0.1\n")).is_err(), "unknown key in a section");
        assert!(parse(&base.replace("kind = \"sma\"", "kind = \"smaa\"")).is_err(), "unknown strategy");
        assert!(parse(&base.replace("fast = 5", "fast = 30")).is_err(), "fast must be below slow");
        assert!(parse(&format!("{base}[risk]\nmax_position = 1.5\n")).is_err());
        assert!(parse(&format!("{base}[backtest]\nstart = 2020-01-01\nend = 2019-01-01\n")).is_err());
        assert!(parse(&format!("{base}[backtest]\nstart = \"2020-01-01\"\n")).is_err(), "dates are TOML dates");
        let qv = "name = \"q\"\n[strategy]\nkind = \"quality-value\"\ntop_n = 12\n[risk]\nmax_position = 0.05\n";
        assert!(parse(qv).is_err(), "1/12 is above the 5% position cap");
        assert!(parse(&format!("{base}weight = 0.2\n[risk]\nmax_position = 0.1\n")).is_err(), "weight above the cap");
        let half = "name = \"q\"\n[strategy]\nkind = \"quality-value\"\n[risk]\nmax_gross_exposure = 0.5\n";
        assert!(parse(half).is_err(), "quality-value can't fit 1/N weights under half exposure");
        assert!(parse("name = \"Bad Name\"\n[strategy]\nkind = \"quality-value\"\n").is_err());
        assert!(parse("name = \"kill\"\n[strategy]\nkind = \"quality-value\"\n").is_err(), "reserved name");
        assert!(parse("name = \"m\"\n[strategy]\nkind = \"momentum\"\ntop_n = 20\nlargest = 10\n").is_err(), "largest below top_n");
        assert!(parse("name = \"e\"\n[strategy]\nkind = \"equal-weight\"\n").is_err(), "equal weight needs symbols");
        let ew = "name = \"e\"\n[strategy]\nkind = \"equal-weight\"\ntrend = 200\n[universe]\nsymbols = [\"A\", \"B\"]\n[risk]\nmax_position = 0.5\n";
        assert_eq!(parse(ew).unwrap().strategy, StrategySpec::EqualWeight { trend: Some(200) });
        assert!(parse(&ew.replace("max_position = 0.5", "max_position = 0.4")).is_err(), "1/2 above the cap");
        assert!(parse(&format!("{ew}max_gross_exposure = 0.5\n")).is_err(), "equal weight needs full exposure");
        assert!(parse(&ew.replace("\"B\"", "\"a\"")).is_err(), "duplicate symbol, any case");
        assert!(parse("name = \"m\"\n[strategy]\nkind = \"momentum\"\nlookback = 21\nskip = 21\n").is_err(), "skip >= lookback");
    }

    #[test]
    fn paper_settings_and_account_suffix() {
        let plan = parse("name = \"p\"\nmode = \"paper\"\n[strategy]\nkind = \"sma\"\nfast = 5\nslow = 20\n[paper]\nmax_orders = 5\n").unwrap();
        assert_eq!(plan.paper, PaperSettings { max_orders: 5, history_days: 400 });
        assert!(parse("name = \"p\"\n[strategy]\nkind = \"sma\"\nfast = 5\nslow = 20\n[paper]\nmax_order = 5\n").is_err());
        assert_eq!(account_suffix("quality-value-core"), "_QUALITY_VALUE_CORE");
    }

    #[test]
    fn name_must_match_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("other.toml");
        fs::write(&path, "name = \"p\"\n[strategy]\nkind = \"sma\"\nfast = 5\nslow = 20\n").unwrap();
        let err = Plan::load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("must match the file name"));
    }
}
