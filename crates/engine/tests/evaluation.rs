use chrono::{NaiveDate, TimeDelta};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use st_core::{Bar, Event};
use st_data::{SyntheticConfig, VecEventSource, generate_daily_bars};
use st_engine::metrics::{buy_and_hold, correlation};
use st_engine::{Backtest, FillModel, SimulatedBroker, WalkForwardConfig, walk_forward};
use st_risk::{RiskLimits, RiskManager};
use st_strategies::SmaCrossover;

fn bars(symbols: usize, days: usize) -> Vec<Bar> {
    generate_daily_bars(&SyntheticConfig {
        symbols,
        days,
        start: NaiveDate::from_ymd_opt(2022, 1, 3).unwrap(),
        seed: 9,
    })
}

fn limits() -> RiskLimits {
    RiskLimits { min_avg_dollar_volume: Decimal::ZERO, min_price: Decimal::ZERO, ..RiskLimits::default() }
}

#[test]
fn warm_up_never_trades_before_trade_from() {
    let bars = bars(10, 200);
    let trade_from = bars.iter().map(|b| b.end).collect::<std::collections::BTreeSet<_>>().into_iter().nth(100).unwrap();
    let report = Backtest::new(
        VecEventSource::new(bars.into_iter().map(Event::bar).collect()),
        SmaCrossover::new(10, 30, dec!(0.05)).unwrap(),
        SimulatedBroker::new(FillModel::default()),
        RiskManager::new(limits()),
        dec!(100_000),
    )
    .trade_from(trade_from)
    .run()
    .unwrap();

    assert!(report.metrics.num_fills > 0, "warmed-up SMA should trade once allowed");
    assert!(report.fills.iter().all(|f| f.at > trade_from), "no fill may precede trade_from");
    assert_eq!(report.equity_curve.first().map(|p| p.0), Some(trade_from));
    assert_eq!(report.equity_curve.len(), 100);
}

#[test]
fn benchmark_helpers() {
    let bars = bars(1, 50);
    let refs: Vec<&Bar> = bars.iter().collect();
    let curve = buy_and_hold(&refs, dec!(1000));
    assert_eq!(curve.len(), 50);
    assert_eq!(curve[0].1, dec!(1000));
    assert_eq!(curve[49].1, dec!(1000) * bars[49].close / bars[0].close);

    assert!((correlation(&curve, &curve).unwrap() - 1.0).abs() < 1e-9);
    let inverted: Vec<_> = curve.iter().map(|(t, v)| (*t, dec!(3000) - *v)).collect();
    assert!(correlation(&curve, &inverted).unwrap() < -0.9);
    assert_eq!(correlation(&curve[..2], &curve[..2]), None, "too few shared days");

    // A benchmark missing every other day still correlates perfectly with the same path, because returns
    // are only taken across dates both series have.
    let sparse: Vec<_> = curve.iter().step_by(2).copied().collect();
    assert!((correlation(&curve, &sparse).unwrap() - 1.0).abs() < 1e-9);
}

#[test]
fn walk_forward_tests_only_on_unseen_windows() {
    let events: Vec<Event> = bars(8, 400).into_iter().map(Event::bar).collect();
    let cfg = WalkForwardConfig {
        train: TimeDelta::days(120),
        test: TimeDelta::days(60),
        warmup: TimeDelta::days(60),
        initial_cash: dec!(100_000),
        fill: FillModel::default(),
        limits: limits(),
    };
    let candidates = [(5usize, 20usize), (10, 40), (20, 50)];
    let make = |&(fast, slow): &(usize, usize)| SmaCrossover::new(fast, slow, dec!(0.05)).unwrap();
    let report = walk_forward(&events, &cfg, &candidates, make).unwrap();

    assert!(report.folds.len() >= 3, "{} folds", report.folds.len());
    for pair in report.folds.windows(2) {
        assert_eq!(pair[0].test_end, pair[1].test_start, "test windows tile without overlap");
    }
    for fold in &report.folds {
        assert_eq!(fold.test_start - fold.train_start, cfg.train);
        assert!(candidates.contains(&fold.chosen));
    }
    let compounded = report.folds.iter().map(|f| 1.0 + f.test.total_return).product::<f64>() - 1.0;
    assert!((report.out_of_sample_return - compounded).abs() < 1e-12);

    let again = walk_forward(&events, &cfg, &candidates, make).unwrap();
    assert_eq!(again.out_of_sample_return, report.out_of_sample_return, "deterministic");
}

/// Holds SYN000 only once it has seen the (one, early) news item, so it trades in a fold only if that
/// item reaches the fold's fresh strategy.
struct NeedsEarlyNews {
    informed: bool,
}

impl st_core::Strategy for NeedsEarlyNews {
    fn name(&self) -> &str {
        "needs_early_news"
    }

    fn on_event(&mut self, event: &Event, _ctx: &st_core::StrategyContext<'_>) -> Vec<st_core::Signal> {
        match &event.kind {
            st_core::EventKind::News(_) => self.informed = true,
            st_core::EventKind::Bar(b) if self.informed && b.symbol.as_str() == "SYN000" => {
                return vec![st_core::Signal { symbol: b.symbol.clone(), target_weight: dec!(0.05), reason: "news".into() }];
            }
            _ => {}
        }
        Vec::new()
    }
}

#[test]
fn walk_forward_folds_follow_the_bars_and_see_earlier_non_bar_events() {
    let bars = bars(3, 400);
    let first_bar = bars.iter().map(|b| b.end).min().unwrap();
    let mut events: Vec<Event> = bars.into_iter().map(Event::bar).collect();
    // Published a year before the first bar, long before any fold's warm-up.
    events.push(Event {
        available_at: first_bar - TimeDelta::days(365),
        kind: st_core::EventKind::News(st_core::NewsItem { symbols: vec![], source: "t".into(), headline: "h".into(), url: None }),
    });
    let cfg = WalkForwardConfig {
        train: TimeDelta::days(120),
        test: TimeDelta::days(60),
        warmup: TimeDelta::days(40),
        initial_cash: dec!(100_000),
        fill: FillModel::default(),
        limits: RiskLimits { liquidity_lookback: 1, ..limits() },
    };
    let report = walk_forward(&events, &cfg, &[()], |_| NeedsEarlyNews { informed: false }).unwrap();
    assert_eq!(report.folds[0].test_start, first_bar + cfg.warmup + cfg.train, "folds start from the first bar");
    assert!(report.folds.iter().all(|f| f.test.num_fills > 0), "every fold's strategy saw the early news");
}
