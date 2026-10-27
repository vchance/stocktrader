//! Raw data collector: polls news and social sources on a schedule and appends every response,
//! verbatim, to a local archive. Parsing and scoring happen later, from the archive, so better parsers
//! can always be re-run over the full history.
//!
//! Only sources that cannot be backfilled later belong here. SEC filings, for example, are permanently
//! archived by EDGAR with acceptance timestamps, so they are ingested by a backfill job instead.

pub mod archive;
pub mod config;
pub mod http;
pub mod maintenance;
pub mod runner;
pub mod source;
pub mod sources;

pub use archive::{Archive, RawRecord};
pub use config::Config;
pub use source::{AnySource, Source};
