//! Historical daily bars and the tradable universe from Alpaca, stored as Parquet.
//!
//! Every sync re-downloads the full history rather than appending: adjusted prices change retroactively
//! whenever a split or dividend happens, so an incrementally updated adjusted series would silently mix
//! adjustment bases. Both split/dividend-adjusted and raw prices are kept: returns need adjusted prices,
//! while filters such as "price above $5" need what the stock actually traded at.

pub mod alpaca;
pub mod replay;
pub mod store;
pub mod sync;
pub mod universe;

pub use alpaca::{Adjustment, AlpacaClient, Asset};
pub use replay::{BarEventSource, ReplayStats};
pub use store::{
    BarFilter, BarReader, BarWriter, DailyBar, StagedFile, Stats, Summary, date_range, read_bars, summarize, to_core_bar,
};
pub use sync::{SyncConfig, SyncReport, bars_path, sync};
