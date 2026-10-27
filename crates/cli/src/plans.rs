//! Trading plans: named, saved descriptions of how to trade (strategy, universe, risk and capital) kept as
//! TOML files in `plans/`, so several can be backtested, compared and later paper-traded side by side.

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
    /// Backtest several plans (all, if none are named) over the same dates and write a comparison report.
    Compare {
        names: Vec<String>,
        #[command(flatten)]
        run: RunArgs,
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
    /// Directory with bars_daily_all.parquet (and the raw file) from `marketdata sync`.
    #[arg(long, env = "STOCKTRADER_MARKETDATA_DIR", default_value = "data/marketdata")]
    marketdata_dir: PathBuf,
    /// Directory from `fundamentals sync`, for fundamentals strategies.
    #[arg(long, env = "STOCKTRADER_FUNDAMENTALS_DIR", default_value = "data/fundamentals")]
    fundamentals_dir: PathBuf,
    /// Directory with apewisdom.parquet from `signals apewisdom`, for attention strategies.
    #[arg(long, env = "STOCKTRADER_SIGNALS_DIR", default_value = "data/signals")]
    signals_dir: PathBuf,
    /// Run on generated prices instead of market data, to try the plumbing before any data is downloaded
    /// (prices only: not for strategies that need fundamentals or attention).
    #[arg(long)]
    synthetic: bool,
    /// Where HTML reports are written.
    #[arg(long, env = "STOCKTRADER_REPORTS_DIR", default_value = "reports")]
    reports_dir: PathBuf,
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
    },
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
fn toml_date<'de, D: Deserializer<'de>>(d: D) -> Result<Option<NaiveDate>, D::Error> {
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
    fn params(&self) -> (Params, Decimal) {
        match *self {
            StrategySpec::Sma { fast, slow, weight } => (Params::Sma { fast, slow }, weight),
            StrategySpec::AttentionSpike { min_mentions, ratio, hold_days, weight } => {
                (Params::Spike { min_mentions, ratio, hold_days }, weight)
            }
            // Weight is 1/top_n inside the strategy.
            StrategySpec::QualityValue { top_n } => (Params::QualityValue { top_n }, Decimal::ONE),
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
            StrategySpec::QualityValue { top_n } => {
                check_top_n(*top_n, limits.max_position_weight)?;
                if limits.max_gross_exposure < Decimal::ONE {
                    bail!(
                        "quality-value invests 1/top_n in each name, so it needs risk.max_gross_exposure of 1, \
                         got {}",
                        limits.max_gross_exposure
                    );
                }
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
        let (params, weight) = self.strategy.params();
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
    fn data_args(&self, run: &RunArgs, window: (Option<NaiveDate>, Option<NaiveDate>)) -> Result<DataArgs> {
        let needs_attention = matches!(self.strategy, StrategySpec::AttentionSpike { .. });
        let needs_fundamentals = matches!(self.strategy, StrategySpec::QualityValue { .. });
        if run.synthetic && (needs_attention || needs_fundamentals) {
            bail!("plan {} needs real data (fundamentals or attention); --synthetic only generates prices", self.name);
        }
        let parquet = run.marketdata_dir.join("bars_daily_all.parquet");
        if !run.synthetic && !parquet.exists() {
            bail!("no market data at {}; run `marketdata sync`, or try the plan with --synthetic", parquet.display());
        }
        Ok(DataArgs {
            bars: None,
            parquet: (!run.synthetic).then_some(parquet),
            raw: None,
            start: if run.synthetic { None } else { window.0 },
            end: if run.synthetic { None } else { window.1 },
            symbols: if run.synthetic { Vec::new() } else { self.universe.symbols.clone() },
            attention: needs_attention.then(|| run.signals_dir.join("apewisdom.parquet")),
            fundamentals: needs_fundamentals.then(|| run.fundamentals_dir.clone()),
            benchmark: if run.synthetic { "none".into() } else { self.backtest.benchmark.clone() },
            synthetic_symbols: 50,
            synthetic_days: 750,
            seed: 42,
            cash: self.capital,
            slippage_bps: self.backtest.slippage_bps,
            min_price: self.universe.min_price,
            attention_until: None,
        })
    }

    fn run(&self, args: &RunArgs, window: (Option<NaiveDate>, Option<NaiveDate>)) -> Result<Run> {
        let data = self.data_args(args, window)?;
        let (params, weight) = self.strategy.params();
        run_backtest(&data, &params, weight, |limits| *limits = self.limits(limits.clone()))
    }
}

/// A parsed plan and the file it came from, verbatim.
type PlanFile = (Plan, String);

/// Every `*.toml` in `dir`, sorted, each parsed (or the error explaining why it isn't valid).
fn load_all(dir: &Path) -> Result<Vec<(PathBuf, Result<PlanFile>)>> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("reading plans directory {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
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

fn window(args: &RunArgs, plan: &Plan) -> (Option<NaiveDate>, Option<NaiveDate>) {
    (args.start.or(plan.backtest.start), args.end.or(plan.backtest.end))
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
            println!("{:<24} {:<9} {:<28} {:<16} notes", "name", "mode", "strategy", "universe");
            for (path, plan) in &plans {
                match plan {
                    Ok((p, _)) => {
                        let (params, _) = p.strategy.params();
                        let notes = p.notes.lines().next().unwrap_or("");
                        println!("{:<24} {:<9} {:<28} {:<16} {notes}", p.name, p.mode.to_string(), params.to_string(), p.universe_label());
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
            let result = plan.run(run, window(run, &plan))?;
            let (params, _) = plan.strategy.params();
            println!("plan              {} ({params})", plan.name);
            print_run(&result, &plan.backtest.benchmark);
            let html = report::plan_report(&plan, &source, &result, run.synthetic);
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
            let start = run.start.or_else(|| plans.iter().filter_map(|(p, _)| p.backtest.start).max());
            let end = run.end.or_else(|| plans.iter().filter_map(|(p, _)| p.backtest.end).min());
            if let (Some(s), Some(e)) = (start, end)
                && s >= e
            {
                bail!("the plans' backtest windows don't overlap ({s} to {e}); pass --start and --end");
            }
            let mut results = Vec::new();
            for (plan, _) in &plans {
                eprintln!("running {}", plan.name);
                results.push((plan, plan.run(run, (start, end))?));
            }
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
            let html = report::compare_report(&results, run.synthetic);
            let path = write_report(&run.reports_dir, "compare.html", &html)?;
            println!("report            {}", path.display());
            Ok(())
        }
    }
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
        assert_eq!(plan.strategy, StrategySpec::QualityValue { top_n: 25 });
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
