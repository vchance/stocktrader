//! Core domain types and the traits that let the same strategy code run in backtest, paper and live modes.

pub mod event;
pub mod order;
pub mod portfolio;
pub mod traits;
pub mod types;

pub use event::{Bar, Event, EventKind, FundamentalRecord, NewsItem, SentimentObservation};
pub use order::{Fill, OrderId, OrderRequest, OrderType, Side, Signal};
pub use portfolio::{Portfolio, Position};
pub use traits::{Clock, EventSource, ExecutionAdapter, ExecutionError, Strategy, StrategyContext};
pub use types::{Price, Qty, Symbol, Timestamp};
