use chrono::{NaiveDate, TimeZone, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use st_core::{Event, EventKind, EventSource, NewsItem, Signal, Strategy, StrategyContext};
use st_data::{SyntheticConfig, VecEventSource, generate_daily_bars};
use st_engine::{Backtest, BacktestError, BacktestReport, FillModel, SimulatedBroker};
use st_risk::{RiskLimits, RiskManager};
use st_strategies::SmaCrossover;

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
    // The budget fits roughly ten 10% positions, not forty.
    assert!(p.positions().count() < 20, "held {} positions", p.positions().count());
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
