use std::path::PathBuf;

use anyhow::Result;
use chrono::NaiveDate;
use clap::{Args, Parser, Subcommand};
use rust_decimal::Decimal;
use st_core::Event;
use st_data::{SyntheticConfig, VecEventSource, generate_daily_bars, load_daily_bars};
use st_engine::{Backtest, FillModel, SimulatedBroker};
use st_risk::{RiskLimits, RiskManager};
use st_strategies::SmaCrossover;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "stocktrader", about = "Automated stock trading: backtest, paper and live")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the SMA crossover strategy over historical or synthetic daily bars.
    Backtest(BacktestArgs),
}

#[derive(Args)]
struct BacktestArgs {
    /// CSV of daily bars with header symbol,date,open,high,low,close,volume. Omit to use synthetic data.
    #[arg(long)]
    bars: Option<PathBuf>,
    #[arg(long, default_value_t = 50)]
    synthetic_symbols: usize,
    #[arg(long, default_value_t = 750)]
    synthetic_days: usize,
    #[arg(long, default_value_t = 42)]
    seed: u64,
    #[arg(long, default_value = "100000")]
    cash: Decimal,
    #[arg(long, default_value_t = 20)]
    fast: usize,
    #[arg(long, default_value_t = 50)]
    slow: usize,
    /// Target weight per position when long.
    #[arg(long, default_value = "0.05")]
    weight: Decimal,
    #[arg(long, default_value = "5")]
    slippage_bps: Decimal,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();
    match Cli::parse().command {
        Command::Backtest(args) => backtest(args),
    }
}

fn backtest(args: BacktestArgs) -> Result<()> {
    let bars = match &args.bars {
        Some(path) => load_daily_bars(path)?,
        None => generate_daily_bars(&SyntheticConfig {
            symbols: args.synthetic_symbols,
            days: args.synthetic_days,
            start: NaiveDate::from_ymd_opt(2023, 1, 3).expect("valid date"),
            seed: args.seed,
        }),
    };
    let n_bars = bars.len();
    let source = VecEventSource::new(bars.into_iter().map(Event::bar).collect());

    let report = Backtest::new(
        source,
        SmaCrossover::new(args.fast, args.slow, args.weight).map_err(anyhow::Error::msg)?,
        SimulatedBroker::new(FillModel { slippage_bps: args.slippage_bps, ..FillModel::default() }),
        RiskManager::new(RiskLimits::default()),
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
