use std::collections::HashMap;

use chrono::{Days, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::America::New_York;
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use st_core::{Event, EventKind, FundamentalRecord, Symbol, Timestamp};

use crate::facts::Fact;

/// When a fact filed on `filed` may be used: midnight New York time at the start of the next day. The
/// bulk data carries only the filing date, and EDGAR accepts filings until 22:00 ET, so anything earlier
/// could let a backtest trade on a filing that hadn't been made yet. This makes it usable from the next
/// session's open.
pub fn available_at(filed: NaiveDate) -> Timestamp {
    let midnight = (filed + Days::new(1)).and_time(NaiveTime::MIN);
    New_York
        .from_local_datetime(&midnight)
        .earliest()
        .expect("New York DST transitions are at 2am, so midnight always exists")
        .with_timezone(&Utc)
}

/// Converts facts into engine events for companies with a ticker in `tickers` (CIK to symbol). The metric
/// name is `taxonomy:concept:unit`, e.g. `us-gaap:Revenues:USD`.
pub fn to_events(facts: &[Fact], tickers: &HashMap<u32, String>) -> Vec<Event> {
    facts
        .iter()
        .filter_map(|f| {
            let ticker = tickers.get(&f.cik)?;
            Some(Event {
                available_at: available_at(f.filed),
                kind: EventKind::Fundamental(FundamentalRecord {
                    symbol: Symbol::new(ticker),
                    period_start: f.start,
                    period_end: f.end,
                    metric: format!("{}:{}:{}", f.taxonomy, f.concept, f.unit),
                    value: Decimal::from_f64(f.value)?,
                }),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::parse_company;
    use crate::facts::tests::apple;

    #[test]
    fn facts_become_available_the_morning_after_filing() {
        // 2024-11-01 is EDT (UTC-4): next midnight New York is 04:00 UTC on Nov 2.
        assert_eq!(available_at(NaiveDate::from_ymd_opt(2024, 11, 1).unwrap()).to_rfc3339(), "2024-11-02T04:00:00+00:00");
        // After the DST change (EST, UTC-5).
        assert_eq!(available_at(NaiveDate::from_ymd_opt(2024, 11, 4).unwrap()).to_rfc3339(), "2024-11-05T05:00:00+00:00");
    }

    #[test]
    fn converts_facts_with_a_ticker() {
        let facts = parse_company(apple().as_bytes()).unwrap().facts;
        let tickers = HashMap::from([(320193, "AAPL".to_string())]);
        let events = to_events(&facts, &tickers);
        assert_eq!(events.len(), facts.len());
        let EventKind::Fundamental(first) = &events[0].kind else { panic!("expected a fundamental event") };
        assert_eq!(first.symbol.as_str(), "AAPL");
        assert_eq!(first.metric, "us-gaap:Revenues:USD");
        assert_eq!(first.value, Decimal::new(391_035_000_000, 0));
        assert!(first.period_start.is_some());

        assert!(to_events(&facts, &HashMap::new()).is_empty(), "no ticker, no event");
    }
}
