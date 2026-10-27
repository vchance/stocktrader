//! `stocktrader paper`: one pre-open run against the Alpaca paper account.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{Days, NaiveDate};
use clap::Args;
use rust_decimal::Decimal;
use serde_json::json;
use st_core::EventKind;
use st_engine::exchange_date;
use st_paper::alpaca::Sessions;
use st_paper::{AccountState, Journal, Outcome, PAPER_URL, TradingClient};
use st_risk::RiskManager;

use crate::{DataArgs, StrategyArgs, load, make_strategy};

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

pub fn run(args: &PaperArgs) -> Result<()> {
    let _ = dotenvy::dotenv();
    let env = |key: &str| std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let key_id = env("APCA_API_KEY_ID").context("APCA_API_KEY_ID is not set")?;
    let secret = env("APCA_API_SECRET_KEY").context("APCA_API_SECRET_KEY is not set")?;
    let http = reqwest::Client::builder()
        .user_agent(concat!("stocktrader-paper/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()?;
    let client = TradingClient::new(http, &args.trading_url, &key_id, &secret)?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(run_with(args, &client))
}

async fn run_with(args: &PaperArgs, client: &TradingClient) -> Result<()> {
    let clock = client.clock().await?;
    let now_date = exchange_date(clock.timestamp.to_utc());
    let calendar = client.calendar(now_date - Days::new(14), now_date).await?;
    let Some(Sessions { today, previous }) = st_paper::sessions(&clock, &calendar)? else {
        println!("no session ahead today ({now_date}); nothing to do");
        return Ok(());
    };
    if clock.is_open {
        if args.submit && !args.allow_open {
            bail!("the market is open; orders would fill now rather than at the open. Pass --allow-open to submit anyway");
        }
        eprintln!("warning: the market is open, so market orders fill now rather than at the open");
    }

    let params = args.strategy.params(args.attention.is_some(), args.fundamentals.is_some())?;
    let mut data = data_args(args, previous - Days::new(args.history_days));
    data.attention_until = Some(clock.timestamp.to_utc());
    let loaded = load(&data)?;
    check_freshness(&loaded.events, previous, &args.symbols)?;
    let mut strategy = make_strategy(&params, args.strategy.weight, &loaded.limits)?;

    let account = client.account().await?;
    st_paper::check_account(&account)?;
    let positions = client.positions().await?;
    let open = client.open_orders().await?;
    let state = AccountState {
        portfolio: st_paper::portfolio_from(&account, &positions),
        open_orders: st_paper::open_requests(&open)?,
        session_start_equity: account.last_equity,
    };

    let kill_file = args.live_dir.join("KILL");
    // Fails closed: if the kill file's presence can't be determined, nothing is traded.
    let kill_present = kill_file.try_exists().with_context(|| format!("checking for {}", kill_file.display()))?;
    let kill = kill_present.then(|| {
        let note = fs::read_to_string(&kill_file).unwrap_or_default();
        let note = note.lines().next().unwrap_or("").trim();
        if note.is_empty() { "KILL file present".to_string() } else { format!("KILL file: {note}") }
    });
    let mut risk = RiskManager::new(loaded.limits);
    let plan = st_paper::plan(loaded.events, &mut strategy, &mut risk, state, today, kill.as_deref());

    let journal = Journal::open(&args.live_dir, today)?;
    let mode = if args.submit { "submit" } else { "dry-run" };
    journal.record(
        "plan",
        json!({ "mode": mode, "strategy": params.to_string(), "symbols": args.symbols, "kill": kill,
                "broker_equity": account.equity, "open_orders": open.len(), "plan": plan }),
    )?;

    println!("session           {today} (bars through {previous})");
    println!("strategy          {params}");
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

    if plan.orders.len() > args.max_orders {
        bail!("the plan has {} orders, more than --max-orders {}; nothing submitted", plan.orders.len(), args.max_orders);
    }
    if !args.submit {
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

fn data_args(args: &PaperArgs, start: NaiveDate) -> DataArgs {
    DataArgs {
        bars: None,
        parquet: Some(args.marketdata_dir.join("bars_daily_all.parquet")),
        raw: None,
        start: Some(start),
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
    }
}

/// Orders are sized on the previous session's close, so that session's bars must be present. Symbols
/// missing them are reported; the planner leaves those alone.
fn check_freshness(events: &[st_core::Event], previous: NaiveDate, symbols: &[String]) -> Result<()> {
    let latest = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Bar(bar) => Some(exchange_date(bar.end)),
            _ => None,
        })
        .max();
    match latest {
        Some(d) if d == previous => {}
        Some(d) => bail!("market data ends {d} but the last session was {previous}; run `marketdata sync` first"),
        None => bail!("no bars found for {}", symbols.join(",")),
    }
    let fresh: BTreeSet<&str> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Bar(bar) if exchange_date(bar.end) == previous => Some(bar.symbol.as_str()),
            _ => None,
        })
        .collect();
    let stale: Vec<String> = symbols
        .iter()
        .map(|s| s.trim().to_ascii_uppercase())
        .filter(|s| !fresh.contains(s.as_str()))
        .collect();
    if !stale.is_empty() {
        eprintln!("warning: no {previous} bar for {}; not trading them today", stale.join(", "));
    }
    Ok(())
}
