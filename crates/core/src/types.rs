use std::fmt;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Prices and money are always `Decimal`, never `f64`.
pub type Price = Decimal;
/// Share quantities are `Decimal` so fractional shares need no type change later.
pub type Qty = Decimal;
pub type Timestamp = chrono::DateTime<chrono::Utc>;

/// An upper-cased ticker symbol.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Symbol(String);

impl Symbol {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into().to_ascii_uppercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
