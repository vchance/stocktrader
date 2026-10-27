use rust_decimal::Decimal;
use st_core::{
    Clock, EventKind, EventSource, ExecutionAdapter, Fill, OrderRequest, OrderType, Portfolio, Side, Signal,
    Strategy, StrategyContext, Timestamp,
};
use st_risk::{RiskDecision, RiskManager};

use crate::clock::SimClock;
use crate::metrics::{self, Metrics};

#[derive(Debug, thiserror::Error)]
pub enum BacktestError {
    #[error("event source went backwards in time: {next} after {prev}")]
    OutOfOrder { prev: Timestamp, next: Timestamp },
}

#[derive(Clone, Debug)]
pub struct Rejection {
    pub at: Timestamp,
    pub signal: Signal,
    pub reason: String,
}

#[derive(Debug)]
pub struct BacktestReport {
    /// Equity at the end of each distinct event timestamp.
    pub equity_curve: Vec<(Timestamp, Decimal)>,
    pub fills: Vec<Fill>,
    pub rejections: Vec<Rejection>,
    pub final_portfolio: Portfolio,
    pub metrics: Metrics,
}

/// Event-driven backtest. For each event, in order: fills from the broker, mark-to-market, risk
/// bookkeeping, strategy signals, risk checks, then order placement.
pub struct Backtest<E, S, B> {
    source: E,
    strategy: S,
    broker: B,
    risk: RiskManager,
    portfolio: Portfolio,
    clock: SimClock,
}

impl<E: EventSource, S: Strategy, B: ExecutionAdapter> Backtest<E, S, B> {
    pub fn new(source: E, strategy: S, broker: B, risk: RiskManager, initial_cash: Decimal) -> Self {
        Self { source, strategy, broker, risk, portfolio: Portfolio::new(initial_cash), clock: SimClock::new() }
    }

    pub fn run(mut self) -> Result<BacktestReport, BacktestError> {
        let mut equity_curve = Vec::new();
        let mut fills = Vec::new();
        let mut rejections = Vec::new();
        let mut last: Option<Timestamp> = None;

        while let Some(event) = self.source.next_event() {
            if let Some(prev) = last {
                if event.available_at < prev {
                    return Err(BacktestError::OutOfOrder { prev, next: event.available_at });
                }
                if event.available_at > prev {
                    equity_curve.push((prev, self.portfolio.equity()));
                }
            }
            last = Some(event.available_at);
            self.clock.set(event.available_at);
            let now = self.clock.now();

            for fill in self.broker.on_market_event(&event) {
                self.portfolio.apply_fill(&fill);
                fills.push(fill);
            }
            if let EventKind::Bar(bar) = &event.kind {
                self.portfolio.mark(&bar.symbol, bar.close);
            }
            self.risk.on_market_event(&event);

            let ctx = StrategyContext { now, portfolio: &self.portfolio };
            let signals = self.strategy.on_event(&event, &ctx);
            for signal in signals {
                match self.risk.check(signal, &self.portfolio, now) {
                    RiskDecision::Approve(signal) => self.place(signal, now),
                    RiskDecision::Reject { signal, reason } => {
                        tracing::debug!(%signal.symbol, %reason, "signal rejected");
                        rejections.push(Rejection { at: now, signal, reason });
                    }
                }
            }
        }
        if let Some(prev) = last {
            equity_curve.push((prev, self.portfolio.equity()));
        }

        let metrics = metrics::compute(&equity_curve, &fills);
        Ok(BacktestReport { equity_curve, fills, rejections, final_portfolio: self.portfolio, metrics })
    }

    /// Converts a target weight into a whole-share market order for the difference from the current position.
    fn place(&mut self, signal: Signal, now: Timestamp) {
        // One working order per symbol keeps sizing simple; the next signal re-targets after it fills.
        if self.broker.open_orders().iter().any(|(_, o)| o.symbol == signal.symbol) {
            return;
        }
        let Some(price) = self.portfolio.last_price(&signal.symbol).filter(|p| *p > Decimal::ZERO) else {
            return;
        };
        let target_qty = (self.portfolio.equity() * signal.target_weight / price).trunc();
        let delta = target_qty - self.portfolio.qty(&signal.symbol);
        if delta.is_zero() {
            return;
        }
        let side = if delta > Decimal::ZERO { Side::Buy } else { Side::Sell };
        let order = OrderRequest {
            symbol: signal.symbol,
            side,
            qty: delta.abs(),
            order_type: OrderType::Market,
            reason: signal.reason,
        };
        if let Err(e) = self.broker.submit(order, now) {
            tracing::warn!(error = %e, "order submission failed");
        }
    }
}
