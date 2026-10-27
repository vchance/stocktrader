use chrono::{NaiveDate, TimeZone, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use st_core::{Event, EventKind, EventSource, NewsItem};
use st_data::{SyntheticConfig, VecEventSource, generate_daily_bars};
use st_engine::{Backtest, BacktestError, BacktestReport, FillModel, SimulatedBroker};
use st_risk::{RiskLimits, RiskManager};
use st_strategies::SmaCrossover;

fn run_synthetic(seed: u64) -> BacktestReport {
    let bars = generate_daily_bars(&SyntheticConfig {
        symbols: 20,
        days: 300,
        start: NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(),
        seed,
    });
    let source = VecEventSource::new(bars.into_iter().map(Event::bar).collect());
    Backtest::new(
        source,
        SmaCrossover::new(10, 30, dec!(0.05)),
        SimulatedBroker::new(FillModel::default()),
        RiskManager::new(RiskLimits::default()),
        dec!(100_000),
    )
    .run()
    .unwrap()
}

#[test]
fn synthetic_backtest_trades_and_is_deterministic() {
    let a = run_synthetic(7);
    let b = run_synthetic(7);

    assert!(a.metrics.num_fills > 0, "expected the toy strategy to trade");
    assert_eq!(a.metrics.end_equity, b.metrics.end_equity);
    assert_eq!(a.equity_curve.len(), 300);
    assert!(a.final_portfolio.gross_exposure() <= a.final_portfolio.equity() * dec!(1.01));
    for fill in &a.fills {
        assert!(fill.qty > Decimal::ZERO);
    }
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
        SmaCrossover::new(2, 3, dec!(0.05)),
        SimulatedBroker::new(FillModel::default()),
        RiskManager::new(RiskLimits::default()),
        dec!(1_000),
    )
    .run();
    assert!(matches!(result, Err(BacktestError::OutOfOrder { .. })));
}
