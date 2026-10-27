//! Point-in-time company fundamentals from SEC EDGAR's XBRL "company facts".
//!
//! Every reported value is kept with the date it was filed, including the repeats and restatements that
//! later filings carry for earlier periods, so a backtest can ask "what was known on day D" and never see a
//! number before it was public. Only a curated set of core concepts is extracted; see [`facts::CONCEPTS`].

pub mod events;
pub mod facts;
pub mod sec;
pub mod store;
pub mod sync;
pub mod tickers;

pub use events::{available_at, load_events, market_symbol, to_events};
pub use facts::{CONCEPTS, FORMS, Fact, parse_company};
pub use sec::SecClient;
pub use store::{FactFilter, FactWriter, read_facts};
pub use sync::{SyncConfig, SyncReport, sync};
pub use tickers::{Listing, parse_tickers};
