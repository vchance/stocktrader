//! Data loading and replay. Live ingestion clients (Alpaca, EDGAR, Reddit, ...) will live here too.

pub mod csv_bars;
pub mod source;
pub mod synthetic;

pub use csv_bars::{daily_bar, load_daily_bars, session_close, session_open};
pub use source::{MergeSource, VecEventSource};
pub use synthetic::{SyntheticConfig, generate_daily_bars};

#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("csv error: {0}")]
    Csv(#[from] csv::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
