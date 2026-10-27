//! Signals derived from the raw collector archive. Everything here is rebuildable from the archive, so
//! derived files can always be regenerated with better code.

pub mod apewisdom;
pub mod store;

pub use apewisdom::{AttentionRow, Build, build_rows, to_events};
pub use store::{read_rows, write_rows};
