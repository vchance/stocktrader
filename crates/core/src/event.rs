use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::types::{Price, Qty, Symbol, Timestamp};

/// An OHLCV bar. `end` is when the bar closed, i.e. the earliest moment its close is knowable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    pub symbol: Symbol,
    pub start: Timestamp,
    pub end: Timestamp,
    pub open: Price,
    pub high: Price,
    pub low: Price,
    pub close: Price,
    pub volume: Qty,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NewsItem {
    pub symbols: Vec<Symbol>,
    pub source: String,
    pub headline: String,
    pub url: Option<String>,
}

/// One reported fundamental value (e.g. `Revenues` for a fiscal period). The owning [`Event`]'s
/// `available_at` must be the filing date, not `period_end`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FundamentalRecord {
    pub symbol: Symbol,
    pub period_end: NaiveDate,
    pub metric: String,
    pub value: Decimal,
}

/// Aggregated social or news sentiment for one symbol over a window ending at the event's `available_at`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SentimentObservation {
    pub symbol: Symbol,
    pub source: String,
    pub window_start: Timestamp,
    /// -1.0 (bearish) to 1.0 (bullish).
    pub score: f64,
    pub mentions: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EventKind {
    Bar(Bar),
    News(NewsItem),
    Fundamental(FundamentalRecord),
    Sentiment(SentimentObservation),
}

/// Every piece of data entering the system is stamped with the moment it became public. Backtests only
/// release an event once the simulated clock reaches `available_at`, which is what prevents look-ahead bias.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub available_at: Timestamp,
    pub kind: EventKind,
}

impl Event {
    pub fn bar(bar: Bar) -> Self {
        Self { available_at: bar.end, kind: EventKind::Bar(bar) }
    }
}
