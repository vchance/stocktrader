//! Pre-open runs against Alpaca paper accounts: `stocktrader paper` for one strategy from flags, and the
//! shared runner that `stocktrader plans paper` uses for each plan.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{Days, NaiveDate};
use clap::Args;
use rust_decimal::Decimal;
use serde_json::json;
use st_core::EventSource;
use st_engine::exchange_date;
use st_marketdata::{BarFilter, coverage};
use st_paper::alpaca::Sessions;
use st_paper::{AccountState, Journal, Outcome, PAPER_URL, TradingClient};
use st_risk::{RiskLimits, RiskManager};

use crate::{DataArgs, Params, StrategyArgs, make_strategy, prepare, report_replay};

#[derive(Args)]
pub struct PaperArgs {
    #[command(flatten)]
    strategy: StrategyArgs,
    /// Symbols the strategy trades. Positions in other symbols are reported and left alone.
    #[arg(long, value_delimiter = ',', required = true)]
    symbols: Vec<String>,
    /// Directory with bars_daily_all.parquet (and bars_daily_raw.parquet) from `marketdata sync`.
    #[arg(long, env = "STOCKTRADER_MARKETDATA_DIR", default_value = "data/marketdata")]
    marketdata_dir: PathBuf,
    /// Attention table from `signals apewisdom`, for attention strategies.
    #[arg(long)]
    attention: Option<PathBuf>,
    /// Directory from `fundamentals sync`, for fundamentals strategies.
    #[arg(long)]
    fundamentals: Option<PathBuf>,
    /// Journal and kill-switch directory. A file named KILL here blocks new exposure; exits still go out.
    #[arg(long, env = "STOCKTRADER_LIVE_DIR", default_value = "data/live")]
    live_dir: PathBuf,
    /// Calendar days of history replayed to warm up the strategy and the liquidity filter.
    #[arg(long, default_value_t = 400)]
    history_days: u64,
    /// Skip entries whose as-traded price is below this; 0 disables.
    #[arg(long, default_value = "5")]
    min_price: Decimal,
    /// Refuse the whole run if it would place more orders than this.
    #[arg(long, default_value_t = 20)]
    max_orders: usize,
    /// Send the orders. Without this the run only prints and journals its plan.
    #[arg(long)]
    submit: bool,
    /// Allow submitting while the market is open. Orders then fill at once at the current price, though
    /// they were sized on the previous close, unlike the backtest's fills at the open.
    #[arg(long)]
    allow_open: bool,
    /// Alpaca trading endpoint; anything other than the paper endpoint is refused.
    #[arg(long, default_value = PAPER_URL)]
    trading_url: String,
}

/// One paper run's settings, from flags (`stocktrader paper`) or a plan file (`stocktrader plans paper`).
pub(crate) struct Job {
    /// Shown in output and the journal: the strategy, or the plan's name.
    pub label: String,
    pub params: Params,
    pub weight: Decimal,
    /// Where bars, attention and fundamentals come from; the run sets the dates.
    pub data: DataArgs,
    /// Applied to the risk limits after the data's defaults (a plan's universe filters and risk overrides).
    pub limits: Box<dyn Fn(RiskLimits) -> RiskLimits>,
    /// Symbols traded; empty means the whole market in the data.
    pub symbols: Vec<String>,
    /// Journal directory (`<dir>/journal/<date>.jsonl`).
    pub live_dir: PathBuf,
    /// Any of these existing blocks new exposure.
    pub kill_files: Vec<PathBuf>,
    pub history_days: u64,
    pub max_orders: usize,
}

/// A trading client for the paper endpoint, with keys from `APCA_API_KEY_ID{suffix}` and
/// `APCA_API_SECRET_KEY{suffix}`. Also returns the key ID, so callers can refuse two plans sharing an account.
pub(crate) fn client_from_env(suffix: &str, trading_url: &str) -> Result<(TradingClient, String)> {
    let _ = dotenvy::dotenv();
    let env = |key: String| std::env::var(&key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).with_context(|| format!("{key} is not set"));
    let key_id = env(format!("APCA_API_KEY_ID{suffix}"))?;
    let secret = env(format!("APCA_API_SECRET_KEY{suffix}"))?;
    let http = reqwest::Client::builder()
        .user_agent(concat!("stocktrader-paper/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()?;
    Ok((TradingClient::new(http, trading_url, &key_id, &secret)?, key_id))
}

pub fn run(args: &PaperArgs) -> Result<()> {
    let (client, _) = client_from_env("", &args.trading_url)?;
    let params = args.strategy.params(args.attention.is_some(), args.fundamentals.is_some())?;
    let job = Job {
        label: params.to_string(),
        params,
        weight: args.strategy.weight,
        data: DataArgs {
            bars: None,
            parquet: Some(args.marketdata_dir.join("bars_daily_all.parquet")),
            raw: None,
            start: None,
            end: None,
            symbols: args.symbols.clone(),
            attention: args.attention.clone(),
            fundamentals: args.fundamentals.clone(),
            benchmark: "none".into(),
            synthetic_symbols: 0,
            synthetic_days: 0,
            seed: 0,
            cash: Decimal::ZERO,
            slippage_bps: Decimal::ZERO,
            min_price: args.min_price,
            attention_until: None,
        },
        limits: Box::new(|limits| limits),
        symbols: args.symbols.clone(),
        live_dir: args.live_dir.clone(),
        kill_files: vec![args.live_dir.join("KILL")],
        history_days: args.history_days,
        max_orders: args.max_orders,
    };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(run_job(&job, &client, args.submit, args.allow_open))
}

/// The first existing kill file's reason. Fails closed: if a file's presence can't be determined, the run
/// stops rather than trade.
fn kill_reason(files: &[PathBuf]) -> Result<Option<String>> {
    for file in files {
        if file.try_exists().with_context(|| format!("checking for {}", file.display()))? {
            let note = fs::read_to_string(file).unwrap_or_default();
            let note = note.lines().next().unwrap_or("").trim().to_string();
            let name = file.display();
            return Ok(Some(if note.is_empty() { format!("KILL file present ({name})") } else { format!("KILL file: {note}") }));
        }
    }
    Ok(None)
}

pub(crate) async fn run_job(job: &Job, client: &TradingClient, submit: bool, allow_open: bool) -> Result<()> {
    let clock = client.clock().await?;
    let now_date = exchange_date(clock.timestamp.to_utc());
    let calendar = client.calendar(now_date - Days::new(14), now_date).await?;
    let Some(Sessions { today, previous }) = st_paper::sessions(&clock, &calendar)? else {
        println!("no session ahead today ({now_date}); nothing to do");
        return Ok(());
    };
    if clock.is_open {
        if submit && !allow_open {
            bail!("the market is open; orders would fill now rather than at the open. Pass --allow-open to submit anyway");
        }
        eprintln!("warning: the market is open, so market orders fill now rather than at the open");
    }

    let mut data = job.data.clone();
    data.start = Some(previous - Days::new(job.history_days));
    data.attention_until = Some(clock.timestamp.to_utc());
    if let Some(parquet) = &data.parquet {
        check_freshness(parquet, &job.symbols, previous)?;
    }
    let mut prepared = prepare(&data)?;
    prepared.limits = (job.limits)(prepared.limits);
    let mut strategy = make_strategy(&job.params, job.weight, &prepared.limits)?;
    let mut risk = RiskManager::new(prepared.limits.clone());

    let account = client.account().await?;
    st_paper::check_account(&account)?;
    let positions = client.positions().await?;
    let open = client.open_orders().await?;
    let state = AccountState {
        portfolio: st_paper::portfolio_from(&account, &positions),
        open_orders: st_paper::open_requests(&open)?,
        session_start_equity: account.last_equity,
    };
    let kill = kill_reason(&job.kill_files)?;
    let mut source = prepared.source;
    let events = std::iter::from_fn(|| source.next_event());
    let plan = st_paper::plan_stream(events, &mut strategy, &mut risk, state, today, kill.as_deref())?;
    report_replay(&prepared.report)?;

    let journal = Journal::open(&job.live_dir, today)?;
    let mode = if submit { "submit" } else { "dry-run" };
    journal.record(
        "plan",
        json!({ "mode": mode, "strategy": job.params.to_string(), "label": job.label, "symbols": job.symbols,
                "kill": kill, "broker_equity": account.equity, "open_orders": open.len(), "plan": plan }),
    )?;

    println!("session           {today} (bars through {previous})");
    println!("strategy          {}", job.params);
    println!("equity            {:.2} (broker reports {:.2})", plan.equity, account.equity);
    println!("open orders       {}", open.len());
    if let Some(reason) = &kill {
        println!("kill switch       {reason}: exits only");
    }
    for (symbol, weight) in &plan.targets {
        println!("target            {symbol:<8} {:>6.2}%", weight * Decimal::ONE_HUNDRED);
    }
    for r in &plan.rejections {
        println!("rejected          {:<8} {}", r.signal.symbol, r.reason);
    }
    for symbol in &plan.untouched {
        println!("left alone        {symbol} (held, but the strategy has no current view)");
    }
    for o in &plan.orders {
        println!("order             {:<8} {:?} {} ({})", o.order.symbol, o.order.side, o.order.qty, o.order.reason);
    }
    if plan.orders.is_empty() {
        println!("no orders");
    }
    println!("journal           {}", journal.path().display());

    if plan.orders.len() > job.max_orders {
        bail!("the plan has {} orders, more than the limit of {}; nothing submitted", plan.orders.len(), job.max_orders);
    }
    if !submit {
        if !plan.orders.is_empty() {
            println!("dry run: pass --submit to send these orders");
        }
        return Ok(());
    }

    let results = st_paper::submit_plan(client, &plan, &journal).await?;
    let mut failed = 0;
    for (planned, outcome) in &results {
        let shown = match outcome {
            Outcome::Accepted { status, .. } => format!("accepted ({status})"),
            Outcome::Duplicate => {
                "already submitted today (not resent; if that order was cancelled or rejected, check the broker)"
                    .to_string()
            }
            Outcome::Failed { error } => {
                failed += 1;
                format!("FAILED: {error}")
            }
        };
        println!("submitted         {:<8} {:?} {}: {shown}", planned.order.symbol, planned.order.side, planned.order.qty);
    }
    if failed > 0 {
        bail!("{failed} of {} orders failed", results.len());
    }
    Ok(())
}

/// Orders are sized on the previous session's close, so the data must end with that session. Listed symbols
/// without a bar for it are reported; the planner leaves those alone. Reads only the symbol and date columns.
fn check_freshness(parquet: &Path, symbols: &[String], previous: NaiveDate) -> Result<()> {
    let wanted: Option<HashSet<String>> =
        (!symbols.is_empty()).then(|| symbols.iter().map(|s| s.trim().to_ascii_uppercase()).collect());
    let recent = BarFilter { symbols: wanted.clone(), start: Some(previous - Days::new(14)), end: None };
    match coverage(parquet, &recent)?.last {
        Some(d) if d == previous => {}
        Some(d) => bail!("market data ends {d} but the last session was {previous}; run `marketdata sync` first"),
        None if symbols.is_empty() => bail!("no recent bars in {}", parquet.display()),
        None => bail!("no recent bars for {}", symbols.join(",")),
    }
    if let Some(wanted) = wanted {
        let fresh = coverage(parquet, &BarFilter { start: Some(previous), end: Some(previous), ..recent })?.symbols;
        let mut stale: Vec<&str> = wanted.iter().map(String::as_str).filter(|s| !fresh.contains(*s)).collect();
        stale.sort_unstable();
        if !stale.is_empty() {
            eprintln!("warning: no {previous} bar for {}; not trading them today", stale.join(", "));
        }
    }
    Ok(())
}
