//! Deciding today's orders: replay history through the strategy, take its current targets, and size them
//! against the broker's real positions with the same risk checks and sizing the backtester uses.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Serialize;
use st_core::{
    Event, EventKind, OrderId, OrderRequest, OrderType, Portfolio, Position, Side, Signal, Strategy, StrategyContext,
    Symbol, Timestamp,
};
use st_engine::size_order;
use st_risk::{RiskDecision, RiskManager};

use crate::alpaca::{self, Account};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PlannedOrder {
    /// `st-<date>-<symbol>-<side>`: one order per symbol and side per day, however often the runner is started.
    pub client_order_id: String,
    pub order: OrderRequest,
    /// The approved (possibly clamped) weight the order moves towards.
    pub target_weight: Decimal,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Rejected {
    pub signal: Signal,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Plan {
    /// Close of the latest bar batch; only signals from then on count as current.
    pub as_of: Option<Timestamp>,
    pub equity: Decimal,
    /// Current target per symbol, before risk checks.
    pub targets: BTreeMap<Symbol, Decimal>,
    pub orders: Vec<PlannedOrder>,
    pub rejections: Vec<Rejected>,
    /// Held positions the strategy has no current view on (not in the data, or no bar in the latest
    /// batch); they are left alone.
    pub untouched: Vec<Symbol>,
}

/// What the broker reports, as the planner needs it.
#[derive(Clone, Debug)]
pub struct AccountState {
    pub portfolio: Portfolio,
    pub open_orders: Vec<(OrderId, OrderRequest)>,
    /// Equity at the previous close, for the daily loss limit.
    pub session_start_equity: Decimal,
}

pub fn client_order_id(date: NaiveDate, symbol: &Symbol, side: Side) -> String {
    let side = match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    };
    format!("st-{date}-{symbol}-{side}")
}

/// Fails unless the account can trade.
pub fn check_account(account: &Account) -> Result<()> {
    if account.status != "ACTIVE" {
        bail!("account status is {}, not ACTIVE", account.status);
    }
    if account.trading_blocked || account.account_blocked {
        bail!("account is blocked from trading");
    }
    if account.currency != "USD" {
        bail!("account currency is {}, expected USD", account.currency);
    }
    Ok(())
}

/// The broker's cash and positions, each position marked at the broker's current price (or its cost).
pub fn portfolio_from(account: &Account, positions: &[alpaca::Position]) -> Portfolio {
    let mut portfolio = Portfolio::new(account.cash);
    for p in positions {
        let symbol = Symbol::new(&p.symbol);
        portfolio.set_position(symbol.clone(), Position { qty: p.qty, avg_cost: p.avg_entry_price });
        portfolio.mark(&symbol, p.current_price.unwrap_or(p.avg_entry_price));
    }
    portfolio
}

/// Open orders as the unfilled remainder of each, so sizing and risk see them as pending. Orders placed
/// by dollar amount have no share quantity to count, so they stop the run rather than be guessed at.
pub fn open_requests(orders: &[alpaca::Order]) -> Result<Vec<(OrderId, OrderRequest)>> {
    let mut requests = Vec::new();
    for (i, o) in orders.iter().enumerate() {
        let Some(qty) = o.qty else {
            bail!("open order {} for {} is by dollar amount; cancel it or let it fill first", o.client_order_id, o.symbol);
        };
        let side = match o.side.as_str() {
            "buy" => Side::Buy,
            "sell" => Side::Sell,
            other => bail!("open order {} has unknown side {other}", o.client_order_id),
        };
        let order_type = o.limit_price.map_or(OrderType::Market, OrderType::Limit);
        let request = OrderRequest {
            symbol: Symbol::new(&o.symbol),
            side,
            qty: (qty - o.filled_qty).max(Decimal::ZERO),
            order_type,
            reason: format!("open order {}", o.client_order_id),
        };
        requests.push((OrderId(i as u64 + 1), request));
    }
    Ok(requests)
}

/// Replays `events` (in any order; they are sorted by `available_at`) through `strategy` and `risk`, then
/// turns the strategy's current targets into orders against the account's positions and open orders.
///
/// Strategies restate their full target on every relevant event, so the targets are the last signal per
/// symbol from the latest bar batch onward (attention that arrived overnight counts). `kill` blocks
/// anything that adds exposure; exits still go through.
pub fn plan<S: Strategy>(
    mut events: Vec<Event>,
    strategy: &mut S,
    risk: &mut RiskManager,
    account: AccountState,
    date: NaiveDate,
    kill: Option<&str>,
) -> Plan {
    let AccountState { mut portfolio, mut open_orders, session_start_equity } = account;
    events.sort_by_key(|e| e.available_at);
    let as_of = events.iter().filter(|e| matches!(e.kind, EventKind::Bar(_))).map(|e| e.available_at).max();
    let Some(as_of) = as_of else {
        return Plan { equity: portfolio.equity(), ..Plan::default() };
    };

    // Events sharing a timestamp form a batch, as in the backtester: each goes to the strategy, then
    // `after_batch` once.
    let mut targets: BTreeMap<Symbol, Signal> = BTreeMap::new();
    for (i, event) in events.iter().enumerate() {
        risk.on_market_event(event);
        if let EventKind::Bar(bar) = &event.kind {
            portfolio.mark(&bar.symbol, bar.close);
        }
        let ctx = StrategyContext { now: event.available_at, portfolio: &portfolio };
        let mut signals = strategy.on_event(event, &ctx);
        if events.get(i + 1).is_none_or(|next| next.available_at != event.available_at) {
            signals.extend(strategy.after_batch(&ctx));
        }
        if event.available_at >= as_of {
            for signal in signals {
                targets.insert(signal.symbol.clone(), signal);
            }
        }
    }

    risk.start_session(session_start_equity);
    if let Some(reason) = kill {
        risk.kill(reason);
    }
    let held: BTreeSet<Symbol> = portfolio.positions().map(|(s, _)| s.clone()).collect();
    let mut plan = Plan {
        as_of: Some(as_of),
        equity: portfolio.equity(),
        targets: targets.iter().map(|(s, sig)| (s.clone(), sig.target_weight)).collect(),
        untouched: held.into_iter().filter(|s| !targets.contains_key(s)).collect(),
        ..Plan::default()
    };
    let mut next_id = open_orders.len() as u64 + 1;
    for signal in targets.into_values() {
        match risk.check(signal, &portfolio, &open_orders) {
            RiskDecision::Approve(signal) => {
                let target_weight = signal.target_weight;
                if let Some(order) = size_order(signal, &portfolio, &open_orders) {
                    let client_order_id = client_order_id(date, &order.symbol, order.side);
                    open_orders.push((OrderId(next_id), order.clone()));
                    next_id += 1;
                    plan.orders.push(PlannedOrder { client_order_id, order, target_weight });
                }
            }
            RiskDecision::Reject { signal, reason } => plan.rejections.push(Rejected { signal, reason }),
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use chrono::{Days, TimeZone, Utc};
    use rust_decimal_macros::dec;
    use st_core::AttentionObservation;
    use st_risk::RiskLimits;
    use st_strategies::{AttentionSpike, SmaCrossover};

    use super::*;

    fn day(n: u64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, 5).unwrap() + Days::new(n)
    }

    /// `n` daily bars, rising or falling by 1 a day, with plenty of volume.
    fn bars(symbol: &str, n: u64, start_price: i64, step: i64) -> Vec<Event> {
        (0..n)
            .map(|i| {
                let close = Decimal::from(start_price + step * i as i64);
                let mut bar = st_data::daily_bar(Symbol::new(symbol), day(i), close, close, close, close, dec!(1_000_000));
                bar.traded_close = Some(close);
                Event::bar(bar)
            })
            .collect()
    }

    fn sma() -> SmaCrossover {
        SmaCrossover::new(2, 5, dec!(0.10)).unwrap()
    }

    fn risk() -> RiskManager {
        RiskManager::new(RiskLimits { liquidity_lookback: 5, require_traded_price: true, ..RiskLimits::default() })
    }

    fn today() -> NaiveDate {
        day(30)
    }

    fn state(portfolio: Portfolio, open_orders: Vec<(OrderId, OrderRequest)>, start: Decimal) -> AccountState {
        AccountState { portfolio, open_orders, session_start_equity: start }
    }

    fn holding(symbol: &str, qty: Decimal, cash: Decimal) -> Portfolio {
        let mut p = Portfolio::new(cash);
        p.set_position(Symbol::new(symbol), Position { qty, avg_cost: dec!(50) });
        p
    }

    #[test]
    fn buys_current_targets_from_an_empty_account() {
        let mut events = bars("UP", 20, 100, 1);
        events.extend(bars("DOWN", 20, 100, -1));
        let p = plan(events, &mut sma(), &mut risk(), state(Portfolio::new(dec!(100_000)), vec![], dec!(100_000)), today(), None);

        assert_eq!(p.targets, BTreeMap::from([(Symbol::new("DOWN"), dec!(0)), (Symbol::new("UP"), dec!(0.10))]));
        assert_eq!(p.orders.len(), 1);
        let o = &p.orders[0];
        // 10% of $100,000 at the last close of 119.
        assert_eq!((o.order.symbol.as_str(), o.order.side, o.order.qty), ("UP", Side::Buy, dec!(84)));
        assert_eq!(o.client_order_id, "st-2026-02-04-UP-buy");
    }

    #[test]
    fn sells_a_holding_the_strategy_wants_flat_and_leaves_others_alone() {
        let mut events = bars("DOWN", 20, 100, -1);
        events.extend(bars("OLD", 10, 100, 0));
        let mut portfolio = holding("DOWN", dec!(30), dec!(50_000));
        portfolio.set_position(Symbol::new("OTHER"), Position { qty: dec!(5), avg_cost: dec!(10) });
        let p = plan(events, &mut sma(), &mut risk(), state(portfolio, vec![], dec!(50_000)), today(), None);

        assert_eq!(p.orders.len(), 1);
        assert_eq!((p.orders[0].order.side, p.orders[0].order.qty), (Side::Sell, dec!(30)));
        assert_eq!(p.orders[0].client_order_id, "st-2026-02-04-DOWN-sell");
        // OTHER isn't in the data; OLD's last bar is older than the latest batch.
        assert_eq!(p.untouched, vec![Symbol::new("OTHER")]);
        assert!(!p.targets.contains_key(&Symbol::new("OLD")));
    }

    #[test]
    fn kill_switch_allows_exits_only() {
        let mut events = bars("UP", 20, 100, 1);
        events.extend(bars("DOWN", 20, 100, -1));
        let portfolio = holding("DOWN", dec!(30), dec!(50_000));
        let p = plan(events, &mut sma(), &mut risk(), state(portfolio, vec![], dec!(50_000)), today(), Some("KILL file"));

        assert_eq!(p.orders.len(), 1);
        assert_eq!((p.orders[0].order.symbol.as_str(), p.orders[0].order.side), ("DOWN", Side::Sell));
        assert_eq!(p.rejections.len(), 1);
        assert!(p.rejections[0].reason.contains("kill switch"), "{}", p.rejections[0].reason);
    }

    #[test]
    fn open_orders_count_so_a_rerun_places_nothing_new() {
        let events = bars("UP", 20, 100, 1);
        let first = plan(events.clone(), &mut sma(), &mut risk(), state(Portfolio::new(dec!(100_000)), vec![], dec!(100_000)), today(), None);
        assert_eq!(first.orders.len(), 1);

        // The first run's order is now open at the broker.
        let open = vec![(OrderId(1), first.orders[0].order.clone())];
        let second = plan(events, &mut sma(), &mut risk(), state(Portfolio::new(dec!(100_000)), open, dec!(100_000)), today(), None);
        assert!(second.orders.is_empty(), "{:?}", second.orders);
    }

    #[test]
    fn daily_loss_limit_blocks_entries() {
        let events = bars("UP", 20, 100, 1);
        // Equity is 100,000 now but was 110,000 at the last close: down 9%, past the 3% limit.
        let p = plan(events, &mut sma(), &mut risk(), state(Portfolio::new(dec!(100_000)), vec![], dec!(110_000)), today(), None);
        assert!(p.orders.is_empty());
        assert_eq!(p.rejections[0].reason, "daily loss limit reached");
    }

    #[test]
    fn overnight_attention_counts_as_current() {
        let mut events = bars("MEME", 10, 20, 0);
        let last_close = events.last().unwrap().available_at;
        events.push(Event {
            available_at: last_close + chrono::TimeDelta::hours(10),
            kind: EventKind::Attention(AttentionObservation {
                symbol: Symbol::new("MEME"),
                source: "apewisdom".into(),
                window_hours: 24,
                mentions: 300,
                mentions_prev: Some(50),
                rank: Some(3),
                rank_prev: Some(40),
                upvotes: None,
            }),
        });
        let mut strategy = AttentionSpike::new(100, 3.0, 5, dec!(0.05)).unwrap();
        let p = plan(events, &mut strategy, &mut risk(), state(Portfolio::new(dec!(10_000)), vec![], dec!(10_000)), today(), None);
        assert_eq!(p.orders.len(), 1);
        assert_eq!((p.orders[0].order.side, p.orders[0].order.qty), (Side::Buy, dec!(25)));
    }

    /// Targets UP only from `after_batch`, and only once it has seen both of a session's bars.
    struct PerSession {
        seen: usize,
    }

    impl Strategy for PerSession {
        fn name(&self) -> &str {
            "per_session"
        }

        fn on_event(&mut self, _event: &Event, _ctx: &StrategyContext<'_>) -> Vec<Signal> {
            self.seen += 1;
            Vec::new()
        }

        fn after_batch(&mut self, _ctx: &StrategyContext<'_>) -> Vec<Signal> {
            let complete = std::mem::take(&mut self.seen) == 2;
            let weight = if complete { dec!(0.10) } else { dec!(0) };
            vec![Signal { symbol: Symbol::new("UP"), target_weight: weight, reason: format!("complete: {complete}") }]
        }
    }

    #[test]
    fn after_batch_signals_from_the_latest_session_become_targets() {
        let mut events = bars("UP", 20, 100, 1);
        events.extend(bars("FLAT", 20, 100, 0));
        let mut strategy = PerSession { seen: 0 };
        let p = plan(events, &mut strategy, &mut risk(), state(Portfolio::new(dec!(100_000)), vec![], dec!(100_000)), today(), None);
        assert_eq!(p.targets, BTreeMap::from([(Symbol::new("UP"), dec!(0.10))]));
        assert_eq!((p.orders[0].order.symbol.as_str(), p.orders[0].order.side), ("UP", Side::Buy));
    }

    #[test]
    fn no_bars_means_no_plan() {
        let p = plan(vec![], &mut sma(), &mut risk(), state(Portfolio::new(dec!(1_000)), vec![], dec!(1_000)), today(), None);
        assert_eq!((p.as_of, p.orders.len()), (None, 0));
    }

    #[test]
    fn broker_state_becomes_a_portfolio_and_pending_orders() {
        let account = Account {
            status: "ACTIVE".into(),
            currency: "USD".into(),
            cash: dec!(1_000),
            equity: dec!(2_000),
            last_equity: dec!(2_000),
            trading_blocked: false,
            account_blocked: false,
        };
        check_account(&account).unwrap();
        let positions = vec![alpaca::Position {
            symbol: "AAPL".into(),
            qty: dec!(5),
            avg_entry_price: dec!(150),
            current_price: Some(dec!(200)),
        }];
        let portfolio = portfolio_from(&account, &positions);
        assert_eq!(portfolio.equity(), dec!(2_000));

        let order = |qty: Option<Decimal>| alpaca::Order {
            id: "o1".into(),
            client_order_id: "c1".into(),
            symbol: "msft".into(),
            side: "buy".into(),
            order_type: "limit".into(),
            qty,
            filled_qty: dec!(4),
            notional: None,
            limit_price: Some(dec!(300)),
            status: "partially_filled".into(),
        };
        let open = open_requests(&[order(Some(dec!(10)))]).unwrap();
        assert_eq!((open[0].1.symbol.as_str(), open[0].1.qty), ("MSFT", dec!(6)));
        assert!(open_requests(&[order(None)]).is_err());

        let blocked = Account { trading_blocked: true, ..account };
        assert!(check_account(&blocked).is_err());
    }

    #[test]
    fn order_ids_are_stable_per_day_symbol_and_side() {
        let date = Utc.with_ymd_and_hms(2026, 10, 27, 12, 0, 0).unwrap().date_naive();
        assert_eq!(client_order_id(date, &Symbol::new("brk.b"), Side::Sell), "st-2026-10-27-BRK.B-sell");
    }
}
