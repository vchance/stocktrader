//! Runners that wire a strategy, the risk manager and an execution adapter together.

pub mod backtest;
pub mod broker;
pub mod clock;
pub mod metrics;

pub use backtest::{Backtest, BacktestError, BacktestReport, Rejection};
pub use broker::{FillModel, SimulatedBroker};
pub use clock::SimClock;
pub use metrics::Metrics;
