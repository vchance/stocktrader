use chrono::NaiveDate;
use rust_decimal::Decimal;
use st_core::{
    Clock, Event, EventKind, EventSource, ExecutionAdapter, Fill, Portfolio, Signal, Strategy, StrategyContext,
    Timestamp,
};
use st_risk::{RiskDecision, RiskManager};

use crate::clock::SimClock;
use crate::exchange_date;
use crate::metrics::{self, Metrics};
use crate::sizing::size_order;

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
    /// Equity after each distinct timestamp that carried price bars. Timestamps with only other events
    /// (overnight or weekend attention polls, filings) leave equity unchanged and would otherwise add
    /// zero-return "days" that distort daily statistics.
    pub equity_curve: Vec<(Timestamp, Decimal)>,
    pub fills: Vec<Fill>,
    pub rejections: Vec<Rejection>,
    pub final_portfolio: Portfolio,
    pub metrics: Metrics,
}

/// Event-driven backtest. Events sharing a timestamp are processed as one batch: first every fill and
/// mark-to-market, then every strategy callback, then risk checks and order placement. That keeps
/// equity and weights identical for every symbol in the batch, whatever order the events arrived in.
pub struct Backtest<E, S, B> {
    source: E,
    strategy: S,
    broker: B,
    risk: RiskManager,
    portfolio: Portfolio,
    clock: SimClock,
    trade_from: Option<Timestamp>,
}

impl<E: EventSource, S: Strategy, B: ExecutionAdapter> Backtest<E, S, B> {
    pub fn new(source: E, strategy: S, broker: B, risk: RiskManager, initial_cash: Decimal) -> Self {
        Self {
            source,
            strategy,
            broker,
            risk,
            portfolio: Portfolio::new(initial_cash),
            clock: SimClock::new(),
            trade_from: None,
        }
    }

    /// Warm-up: events before `t` still reach the strategy and risk manager (so moving averages, liquidity
    /// windows and the like are ready), but their signals are discarded and no equity is recorded. Trading
    /// and the reported results start at `t`.
    pub fn trade_from(mut self, t: Timestamp) -> Self {
        self.trade_from = Some(t);
        self
    }

    pub fn run(mut self) -> Result<BacktestReport, BacktestError> {
        let mut equity_curve = Vec::new();
        let mut fills = Vec::new();
        let mut rejections = Vec::new();
        let mut session: Option<NaiveDate> = None;
        let mut next = self.source.next_event();

        while let Some(first) = next.take() {
            let now = first.available_at;
            let mut batch = vec![first];
            loop {
                next = self.source.next_event();
                match &next {
                    Some(e) if e.available_at == now => batch.extend(next.take()),
                    Some(e) if e.available_at < now => {
                        return Err(BacktestError::OutOfOrder { prev: now, next: e.available_at });
                    }
                    _ => break,
                }
            }
            self.clock.set(now);
            let now = self.clock.now();

            let day = exchange_date(now);
            if session != Some(day) {
                session = Some(day);
                self.risk.start_session(self.portfolio.equity());
            }

            for event in &batch {
                for fill in self.broker.on_market_event(event) {
                    self.portfolio.apply_fill(&fill);
                    fills.push(fill);
                }
                if let EventKind::Bar(bar) = &event.kind {
                    self.portfolio.mark(&bar.symbol, bar.close);
                }
                self.risk.on_market_event(event);
            }

            let signals = self.collect_signals(&batch, now);
            if self.trade_from.is_some_and(|t| now < t) {
                continue;
            }
            for signal in signals {
                let open = self.broker.open_orders();
                match self.risk.check(signal, &self.portfolio, &open) {
                    RiskDecision::Approve(signal) => self.place(signal, now),
                    RiskDecision::Reject { signal, reason } => {
                        tracing::debug!(%signal.symbol, %reason, "signal rejected");
                        rejections.push(Rejection { at: now, signal, reason });
                    }
                }
            }
            if batch.iter().any(|e| matches!(e.kind, EventKind::Bar(_))) {
                equity_curve.push((now, self.portfolio.equity()));
            }
        }

        let metrics = metrics::compute(&equity_curve, &fills);
        Ok(BacktestReport { equity_curve, fills, rejections, final_portfolio: self.portfolio, metrics })
    }

    fn collect_signals(&mut self, batch: &[Event], now: Timestamp) -> Vec<Signal> {
        let ctx = StrategyContext { now, portfolio: &self.portfolio };
        let mut signals = Vec::new();
        for event in batch {
            signals.extend(self.strategy.on_event(event, &ctx));
        }
        signals.extend(self.strategy.after_batch(&ctx));
        signals
    }

    fn place(&mut self, signal: Signal, now: Timestamp) {
        let open = self.broker.open_orders();
        let Some(order) = size_order(signal, &self.portfolio, &open) else {
            return;
        };
        if let Err(e) = self.broker.submit(order, now) {
            tracing::warn!(error = %e, "order submission failed");
        }
    }
}
