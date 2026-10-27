//! Walk-forward evaluation: choose parameters on a training window, then measure them only on the next,
//! unseen test window, rolling forward through the data. The concatenated test windows are the honest
//! estimate of how a parameter-choosing process would have done; in-sample results are not.

use chrono::TimeDelta;
use rust_decimal::Decimal;
use st_core::{Event, EventKind, Strategy, Timestamp};
use st_data::VecEventSource;
use st_risk::{RiskLimits, RiskManager};

use crate::backtest::{Backtest, BacktestError};
use crate::broker::{FillModel, SimulatedBroker};
use crate::metrics::Metrics;

#[derive(Clone, Debug)]
pub struct WalkForwardConfig {
    /// Window used to choose parameters.
    pub train: TimeDelta,
    /// Window each choice is then measured on.
    pub test: TimeDelta,
    /// History replayed before each window (without trading) so strategies and risk checks are warm.
    pub warmup: TimeDelta,
    pub initial_cash: Decimal,
    pub fill: FillModel,
    pub limits: RiskLimits,
}

#[derive(Clone, Debug)]
pub struct Fold<P> {
    pub train_start: Timestamp,
    pub test_start: Timestamp,
    pub test_end: Timestamp,
    pub chosen: P,
    /// In-sample: what the choice looked like on its training window.
    pub train: Metrics,
    /// Out-of-sample: what it did next.
    pub test: Metrics,
}

#[derive(Clone, Debug)]
pub struct WalkForwardReport<P> {
    pub folds: Vec<Fold<P>>,
    /// Test-window returns compounded across folds.
    pub out_of_sample_return: f64,
    /// Annualized over the combined test span.
    pub out_of_sample_cagr: f64,
}

/// Runs a walk-forward evaluation over `events` (any order). For each fold, every candidate is backtested
/// on the training window and the one with the best Sharpe ratio (ties: higher return) is then backtested
/// on the following test window. Each backtest starts from `initial_cash`. A final test window cut to less
/// than half its length by the end of the data is dropped.
///
/// Folds are laid out over the span of the price bars. Each backtest replays its warm-up and window, preceded
/// by every earlier non-bar event (filings, attention): a fresh strategy must know, say, the latest annual
/// report, which may have been filed long before the warm-up began. Those events are all public before the
/// window, so this adds no look-ahead.
pub fn walk_forward<P: Clone, S: Strategy>(
    events: &[Event],
    cfg: &WalkForwardConfig,
    candidates: &[P],
    make: impl Fn(&P) -> S,
) -> Result<WalkForwardReport<P>, BacktestError> {
    let mut events = events.to_vec();
    events.sort_by_key(|e| e.available_at);
    let is_bar = |e: &Event| matches!(e.kind, EventKind::Bar(_));
    let bar_times = || events.iter().filter(|e| is_bar(e)).map(|e| e.available_at);
    let (Some(first), Some(last)) = (bar_times().min(), bar_times().max()) else {
        return Ok(WalkForwardReport { folds: Vec::new(), out_of_sample_return: 0.0, out_of_sample_cagr: 0.0 });
    };
    assert!(!candidates.is_empty(), "walk_forward needs at least one candidate");
    let context: Vec<Event> = events.iter().filter(|e| !is_bar(e)).cloned().collect();

    let run = |strategy: S, from: Timestamp, trade_from: Timestamp, to: Timestamp| {
        let lo = events.partition_point(|e| e.available_at < from);
        let hi = events.partition_point(|e| e.available_at < to);
        let earlier = context.partition_point(|e| e.available_at < from);
        let mut replay = context[..earlier].to_vec();
        replay.extend_from_slice(&events[lo..hi]);
        Backtest::new(
            VecEventSource::new(replay),
            strategy,
            SimulatedBroker::new(cfg.fill.clone()),
            RiskManager::new(cfg.limits.clone()),
            cfg.initial_cash,
        )
        .trade_from(trade_from)
        .run()
        .map(|report| report.metrics)
    };

    let mut folds = Vec::new();
    let mut test_start = first + cfg.warmup + cfg.train;
    while test_start <= last {
        let train_start = test_start - cfg.train;
        let test_end = (test_start + cfg.test).min(last + TimeDelta::seconds(1));
        // A final fold cut short by the end of the data says little; drop it if under half a window.
        if test_end - test_start < cfg.test / 2 {
            break;
        }
        let mut best: Option<(P, Metrics)> = None;
        for candidate in candidates {
            let metrics = run(make(candidate), train_start - cfg.warmup, train_start, test_start)?;
            let better = best.as_ref().is_none_or(|(_, b)| score(&metrics) > score(b));
            if better {
                best = Some((candidate.clone(), metrics));
            }
        }
        let (chosen, train) = best.expect("at least one candidate");
        let test = run(make(&chosen), test_start - cfg.warmup, test_start, test_end)?;
        folds.push(Fold { train_start, test_start, test_end, chosen, train, test });
        test_start += cfg.test;
    }

    let out_of_sample_return =
        folds.iter().map(|f| 1.0 + finite_or_zero(f.test.total_return)).product::<f64>() - 1.0;
    let span = folds.first().zip(folds.last()).map(|(a, b)| (b.test_end - a.test_start).num_seconds() as f64);
    let years = span.unwrap_or(0.0) / (365.25 * 86_400.0);
    let out_of_sample_cagr = if years > 0.0 && out_of_sample_return > -1.0 {
        (1.0 + out_of_sample_return).powf(1.0 / years) - 1.0
    } else {
        out_of_sample_return
    };
    Ok(WalkForwardReport { folds, out_of_sample_return, out_of_sample_cagr })
}

fn finite_or_zero(x: f64) -> f64 {
    if x.is_finite() { x } else { 0.0 }
}

/// Training-window ranking: Sharpe first, total return to break ties; non-finite values rank last.
fn score(m: &Metrics) -> (f64, f64) {
    let rank = |x: f64| if x.is_finite() { x } else { f64::NEG_INFINITY };
    (rank(m.sharpe), rank(m.total_return))
}
