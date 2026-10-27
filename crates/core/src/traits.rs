use crate::event::Event;
use crate::order::{Fill, OrderId, OrderRequest, Signal};
use crate::portfolio::Portfolio;
use crate::types::Timestamp;

/// Wall clock in live trading, simulated clock in backtests.
pub trait Clock {
    fn now(&self) -> Timestamp;
}

/// A time-ordered stream of events. Implementations must yield events in non-decreasing
/// `available_at` order; the backtester rejects a source that goes backwards.
pub trait EventSource {
    fn next_event(&mut self) -> Option<Event>;
}

/// Lets a source chosen at runtime (in-memory or streamed from disk) run through the same generic engine.
impl<E: EventSource + ?Sized> EventSource for Box<E> {
    fn next_event(&mut self) -> Option<Event> {
        (**self).next_event()
    }
}

pub struct StrategyContext<'a> {
    pub now: Timestamp,
    pub portfolio: &'a Portfolio,
}

/// Turns events into target-weight signals. Strategies never place orders directly.
pub trait Strategy {
    fn name(&self) -> &str;
    fn on_event(&mut self, event: &Event, ctx: &StrategyContext<'_>) -> Vec<Signal>;

    /// Called once after every event sharing a timestamp has been passed to [`Strategy::on_event`], so a
    /// strategy that compares symbols (say, ranking the whole market at the close) acts on a complete set.
    fn after_batch(&mut self, _ctx: &StrategyContext<'_>) -> Vec<Signal> {
        Vec::new()
    }
}

/// Lets a strategy chosen at runtime (e.g. from a CLI flag) run through the same generic engine.
impl<S: Strategy + ?Sized> Strategy for Box<S> {
    fn name(&self) -> &str {
        (**self).name()
    }

    fn on_event(&mut self, event: &Event, ctx: &StrategyContext<'_>) -> Vec<Signal> {
        (**self).on_event(event, ctx)
    }

    fn after_batch(&mut self, ctx: &StrategyContext<'_>) -> Vec<Signal> {
        (**self).after_batch(ctx)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    #[error("order rejected: {0}")]
    Rejected(String),
    #[error("unknown order {0:?}")]
    UnknownOrder(OrderId),
    #[error("broker error: {0}")]
    Broker(String),
}

/// Where orders go. The simulated broker, a paper account and a live account all implement this.
pub trait ExecutionAdapter {
    fn submit(&mut self, order: OrderRequest, now: Timestamp) -> Result<OrderId, ExecutionError>;
    fn cancel(&mut self, id: OrderId) -> Result<(), ExecutionError>;
    fn open_orders(&self) -> Vec<(OrderId, OrderRequest)>;
    /// Feed the adapter new market data and collect the fills that resulted. A simulated broker
    /// matches pending orders against the event; a live adapter drains fills reported by the broker.
    fn on_market_event(&mut self, event: &Event) -> Vec<Fill>;
}
