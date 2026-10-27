use chrono::NaiveDate;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use st_core::{
    Clock, Event, EventKind, EventSource, ExecutionAdapter, Fill, OrderRequest, OrderType, Portfolio, Side,
    Signal, Strategy, StrategyContext, Timestamp,
};
use st_risk::{RiskDecision, RiskManager};

use crate::clock::SimClock;
use crate::exchange_date;
use crate::metrics::{self, Metrics};

/// Skip resizing an existing position by less than this fraction of equity, so equity drift doesn't
/// churn small trades. Entries and exits to flat always go through.
const REBALANCE_BAND: Decimal = dec!(0.01);
/// Buys smaller than this fraction of equity are skipped, so target weights below it never trade.
const MIN_BUY: Decimal = dec!(0.0025);
/// Headroom kept when sizing buys from cash, since a market order fills at the next open, not the last close.
const CASH_BUFFER: Decimal = dec!(0.02);

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
}

impl<E: EventSource, S: Strategy, B: ExecutionAdapter> Backtest<E, S, B> {
    pub fn new(source: E, strategy: S, broker: B, risk: RiskManager, initial_cash: Decimal) -> Self {
        Self { source, strategy, broker, risk, portfolio: Portfolio::new(initial_cash), clock: SimClock::new() }
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
        signals
    }

    /// Converts a target weight into a whole-share market order for the difference between the target
    /// and the projected position (current position plus unfilled orders). Buys are capped by cash not
    /// already committed to other pending buys.
    fn place(&mut self, signal: Signal, now: Timestamp) {
        let Some(price) = self.portfolio.last_price(&signal.symbol).filter(|p| *p > Decimal::ZERO) else {
            return;
        };
        let open = self.broker.open_orders();
        let equity = self.portfolio.equity();
        let target_qty = (equity * signal.target_weight / price).trunc().max(Decimal::ZERO);
        let projected = self.portfolio.projected_qty(&signal.symbol, &open);
        let mut delta = target_qty - projected;

        let resize = !target_qty.is_zero() && !projected.is_zero();
        if resize && (delta * price).abs() < equity * REBALANCE_BAND {
            return;
        }
        if delta > Decimal::ZERO {
            let reserved: Decimal = open
                .iter()
                .filter(|(_, o)| o.side == Side::Buy)
                .map(|(_, o)| o.qty * self.portfolio.last_price(&o.symbol).unwrap_or(price))
                .sum();
            let available = self.portfolio.cash() - reserved * (Decimal::ONE + CASH_BUFFER);
            let affordable = (available / (price * (Decimal::ONE + CASH_BUFFER))).trunc().max(Decimal::ZERO);
            delta = delta.min(affordable);
            // Cash or gross-exposure headroom can shrink an order to a sliver; skip those.
            if delta * price < equity * MIN_BUY {
                return;
            }
        }
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
