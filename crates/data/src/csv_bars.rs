use std::path::Path;

use chrono::{NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::America::New_York;
use rust_decimal::Decimal;
use serde::Deserialize;
use st_core::{Bar, Price, Qty, Symbol, Timestamp};

use crate::DataError;

#[derive(Deserialize)]
struct Row {
    symbol: String,
    date: NaiveDate,
    open: Decimal,
    high: Decimal,
    low: Decimal,
    close: Decimal,
    volume: Decimal,
}

/// Loads daily bars from a CSV with header `symbol,date,open,high,low,close,volume`.
pub fn load_daily_bars(path: &Path) -> Result<Vec<Bar>, DataError> {
    let mut reader = csv::Reader::from_path(path)?;
    reader
        .deserialize::<Row>()
        .map(|row| {
            let r = row?;
            Ok(daily_bar(Symbol::new(r.symbol), r.date, r.open, r.high, r.low, r.close, r.volume))
        })
        .collect()
}

/// A regular-session daily bar: 09:30 to 16:00 New York time, so the close is only knowable at 16:00 ET.
pub fn daily_bar(
    symbol: Symbol,
    date: NaiveDate,
    open: Price,
    high: Price,
    low: Price,
    close: Price,
    volume: Qty,
) -> Bar {
    Bar {
        symbol,
        start: new_york(date, 9, 30),
        end: new_york(date, 16, 0),
        open,
        high,
        low,
        close,
        volume,
        traded_close: None,
    }
}

fn new_york(date: NaiveDate, hour: u32, minute: u32) -> Timestamp {
    let local = date.and_time(NaiveTime::from_hms_opt(hour, minute, 0).expect("valid time"));
    New_York
        .from_local_datetime(&local)
        .single()
        .expect("market hours never fall in a DST transition")
        .with_timezone(&Utc)
}
