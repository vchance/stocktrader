//! Runners that wire a strategy, the risk manager and an execution adapter together.

pub mod backtest;
pub mod broker;
pub mod clock;
pub mod metrics;
pub mod sizing;
pub mod walkforward;

pub use backtest::{Backtest, BacktestError, BacktestReport, Rejection};
pub use broker::{FillModel, SimulatedBroker};
pub use clock::SimClock;
pub use metrics::Metrics;
pub use sizing::size_order;
pub use walkforward::{Fold, WalkForwardConfig, WalkForwardReport, walk_forward};

use chrono::NaiveDate;
use st_core::Timestamp;

/// The US equity session date a timestamp belongs to.
pub fn exchange_date(t: Timestamp) -> NaiveDate {
    t.with_timezone(&chrono_tz::America::New_York).date_naive()
}
