use chrono::{NaiveDate, TimeZone, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use st_core::{AttentionObservation, Event, EventKind, EventSource, NewsItem, Signal, Strategy, StrategyContext, Symbol};
use st_data::{SyntheticConfig, VecEventSource, generate_daily_bars};
use st_engine::{Backtest, BacktestError, BacktestReport, FillModel, SimulatedBroker};
use st_risk::{RiskLimits, RiskManager};
use st_strategies::{AttentionSpike, SmaCrossover};

fn synthetic_source(symbols: usize, days: usize, seed: u64) -> VecEventSource {
    let bars = generate_daily_bars(&SyntheticConfig {
        symbols,
        days,
        start: NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(),
        seed,
    });
    VecEventSource::new(bars.into_iter().map(Event::bar).collect())
}

fn run<S: Strategy>(source: VecEventSource, strategy: S) -> BacktestReport {
    Backtest::new(
        source,
        strategy,
        SimulatedBroker::new(FillModel::default()),
        RiskManager::new(RiskLimits { min_avg_dollar_volume: Decimal::ZERO, ..RiskLimits::default() }),
        dec!(100_000),
    )
    .run()
    .unwrap()
}

#[test]
fn synthetic_backtest_trades_and_is_deterministic() {
    let a = run(synthetic_source(20, 300, 7), SmaCrossover::new(10, 30, dec!(0.05)).unwrap());
    let b = run(synthetic_source(20, 300, 7), SmaCrossover::new(10, 30, dec!(0.05)).unwrap());

    assert!(a.metrics.num_fills > 0, "expected the toy strategy to trade");
    assert_eq!(a.metrics.end_equity, b.metrics.end_equity);
    assert_eq!(a.equity_curve.len(), 300);
    for fill in &a.fills {
        assert!(fill.qty > Decimal::ZERO);
    }
}

#[test]
fn weights_below_rebalance_band_still_enter_positions() {
    let report = run(synthetic_source(20, 300, 7), SmaCrossover::new(10, 30, dec!(0.005)).unwrap());
    assert!(report.metrics.num_fills > 0);
}

#[test]
fn attention_spike_buys_after_the_spike_and_exits_after_the_hold() {
    let bars = generate_daily_bars(&SyntheticConfig {
        symbols: 3,
        days: 60,
        start: NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(),
        seed: 5,
    });
    // A spike for SYN001 observed overnight before the 31st session, plus a non-spike for SYN002.
    let spike_at = bars.iter().filter(|b| b.symbol.as_str() == "SYN001").nth(30).unwrap().start
        - chrono::TimeDelta::hours(8);
    let attention = |symbol: &str, mentions, prev| Event {
        available_at: spike_at,
        kind: EventKind::Attention(AttentionObservation {
            symbol: Symbol::new(symbol),
            source: "test".into(),
            window_hours: 24,
            mentions,
            mentions_prev: Some(prev),
            rank: Some(1),
            rank_prev: Some(40),
            upvotes: Some(10),
        }),
    };
    let mut events: Vec<Event> = bars.into_iter().map(Event::bar).collect();
    events.push(attention("SYN001", 200, 20));
    events.push(attention("SYN002", 30, 25));

    let report = run(VecEventSource::new(events), AttentionSpike::new(50, 3.0, 5, dec!(0.10)).unwrap());
    let traded: Vec<(&str, st_core::Side)> = report.fills.iter().map(|f| (f.symbol.as_str(), f.side)).collect();
    assert_eq!(traded, vec![("SYN001", st_core::Side::Buy), ("SYN001", st_core::Side::Sell)]);
    assert!(report.fills[0].at > spike_at, "fills only after the spike was observed");
    let held = report.fills[1].at - report.fills[0].at;
    assert!(held >= chrono::TimeDelta::days(5) && held <= chrono::TimeDelta::days(9), "held {held}");
    assert_eq!(report.final_portfolio.positions().count(), 0);
}

/// Asks for 10% in every symbol on every bar: far more than the 100% gross budget.
struct WantsEverything;

impl Strategy for WantsEverything {
    fn name(&self) -> &str {
        "wants_everything"
    }

    fn on_event(&mut self, event: &Event, _ctx: &StrategyContext<'_>) -> Vec<Signal> {
        match &event.kind {
            EventKind::Bar(bar) => {
                vec![Signal { symbol: bar.symbol.clone(), target_weight: dec!(0.10), reason: "all in".into() }]
            }
            _ => Vec::new(),
        }
    }
}

#[test]
fn same_bar_signals_cannot_exceed_gross_exposure_or_cash() {
    let report = run(synthetic_source(40, 60, 3), WantsEverything);
    let p = &report.final_portfolio;

    assert!(report.metrics.num_fills > 0);
    assert!(p.cash() >= Decimal::ZERO, "cash went negative: {}", p.cash());
    assert!(
        p.gross_exposure() <= p.equity() * dec!(1.05),
        "gross {} vs equity {}",
        p.gross_exposure(),
        p.equity()
    );
    // Partial entries are allowed when headroom is short, but the budget can't hold all forty.
    assert!(p.positions().count() < 40, "held {} positions", p.positions().count());
}

struct Backwards(Vec<Event>);

impl EventSource for Backwards {
    fn next_event(&mut self) -> Option<Event> {
        self.0.pop()
    }
}

#[test]
fn rejects_sources_that_go_back_in_time() {
    let news = |hour| Event {
        available_at: Utc.with_ymd_and_hms(2026, 1, 5, hour, 0, 0).unwrap(),
        kind: EventKind::News(NewsItem { symbols: vec![], source: "test".into(), headline: "h".into(), url: None }),
    };
    // pop() yields the 15:00 event first, then 14:00.
    let source = Backwards(vec![news(14), news(15)]);
    let result = Backtest::new(
        source,
        SmaCrossover::new(2, 3, dec!(0.05)).unwrap(),
        SimulatedBroker::new(FillModel::default()),
        RiskManager::new(RiskLimits::default()),
        dec!(1_000),
    )
    .run();
    assert!(matches!(result, Err(BacktestError::OutOfOrder { .. })));
}
