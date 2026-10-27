use std::collections::{BTreeMap, HashMap};

use chrono::{Days, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::America::New_York;
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use st_core::{Event, EventKind, FundamentalRecord, Symbol, Timestamp};

use crate::facts::Fact;

/// When a fact filed on `filed` may be used: midnight New York time at the start of the next day. The
/// bulk data carries only the filing date, and EDGAR accepts filings until 22:00 ET, so anything earlier
/// could let a backtest trade on a filing that hadn't been made yet. This makes it usable from the next
/// session's open. [`crate::FactFilter::as_of`] applies the same rule.
pub fn available_at(filed: NaiveDate) -> Timestamp {
    let midnight = (filed + Days::new(1)).and_time(NaiveTime::MIN);
    New_York
        .from_local_datetime(&midnight)
        .earliest()
        .expect("New York DST transitions are at 2am, so midnight always exists")
        .with_timezone(&Utc)
}

/// The SEC writes share-class tickers with a dash (`BRK-B`); market data uses a dot (`BRK.B`).
pub fn market_symbol(sec_ticker: &str) -> Symbol {
    Symbol::new(sec_ticker.replace('-', "."))
}

/// Converts facts into engine events, sorted by `available_at`, for companies with a ticker in `tickers`
/// (CIK to SEC ticker). The metric name is `taxonomy:concept:unit`, e.g. `us-gaap:Revenues:USD`.
///
/// Facts that would produce the same event key (symbol, metric, period, availability) with different
/// values, such as a 10-K and its amendment filed the same day, prefer an amendment form (`/A`), then the
/// later accession number (accession numbers are only ordered within one filer agent, hence the form
/// first); within one accession (e.g. per-class share counts of a multi-class issuer) the largest value is
/// kept. Facts whose value doesn't fit a `Decimal` are dropped.
///
/// `tickers` is a single snapshot of the SEC's *current* mapping. Applied to history it attributes a
/// company's whole past to today's ticker: a ticker that once belonged to another company gets this
/// company's fundamentals, renamed companies lose their old symbol, and delisted companies are absent.
/// Use dated snapshots (`tickers/<date>.json`) to resolve symbols as of each fact once enough exist.
pub fn to_events(facts: &[Fact], tickers: &HashMap<u32, String>) -> Vec<Event> {
    type Key = (Timestamp, Symbol, String, Option<NaiveDate>, NaiveDate);
    let mut chosen: BTreeMap<Key, (bool, &str, Decimal)> = BTreeMap::new();
    for f in facts {
        let (Some(ticker), Some(value)) = (tickers.get(&f.cik), Decimal::from_f64(f.value)) else {
            continue;
        };
        let metric = format!("{}:{}:{}", f.taxonomy, f.concept, f.unit);
        let key = (available_at(f.filed), market_symbol(ticker), metric, f.start, f.end);
        let candidate = (f.form.ends_with("/A"), f.accession.as_str(), value);
        chosen.entry(key).and_modify(|best| *best = (*best).max(candidate)).or_insert(candidate);
    }
    chosen
        .into_iter()
        .map(|((available_at, symbol, metric, period_start, period_end), (_, _, value))| Event {
            available_at,
            kind: EventKind::Fundamental(FundamentalRecord { symbol, period_start, period_end, metric, value }),
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
    fn converts_sorted_events_with_a_ticker() {
        let facts = parse_company(apple().as_bytes()).unwrap().facts;
        let tickers = HashMap::from([(320193, "AAPL".to_string())]);
        let events = to_events(&facts, &tickers);
        assert_eq!(events.len(), facts.len());
        assert!(events.windows(2).all(|w| w[0].available_at <= w[1].available_at), "sorted by availability");
        let revenue = events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::Fundamental(r) if r.metric == "us-gaap:Revenues:USD" => Some(r),
                _ => None,
            })
            .unwrap();
        assert_eq!(revenue.symbol.as_str(), "AAPL");
        assert_eq!(revenue.value, Decimal::new(391_035_000_000, 0));
        assert!(revenue.period_start.is_some());

        assert!(to_events(&facts, &HashMap::new()).is_empty(), "no ticker, no event");
    }

    #[test]
    fn same_day_duplicates_resolve_deterministically() {
        let mut original = parse_company(apple().as_bytes()).unwrap().facts.remove(0);
        original.accession = "0000320193-24-000100".into();
        let amended = Fact { accession: "0000320193-24-000200".into(), value: 1.0, ..original.clone() };
        let tickers = HashMap::from([(320193, "BRK-B".to_string())]);

        for facts in [vec![original.clone(), amended.clone()], vec![amended, original]] {
            let events = to_events(&facts, &tickers);
            assert_eq!(events.len(), 1);
            let EventKind::Fundamental(r) = &events[0].kind else { panic!("expected a fundamental event") };
            assert_eq!(r.value, Decimal::ONE, "later accession wins regardless of input order");
            assert_eq!(r.symbol.as_str(), "BRK.B");
        }
    }

    #[test]
    fn amendment_wins_even_from_an_earlier_sorting_filer_agent() {
        let mut original = parse_company(apple().as_bytes()).unwrap().facts.remove(0);
        original.accession = "0000320193-24-000100".into();
        // Filed through an agent whose ID sorts before the company's own.
        let amended = Fact {
            accession: "0000000001-24-000001".into(),
            form: "10-K/A".into(),
            value: 2.0,
            ..original.clone()
        };
        let tickers = HashMap::from([(320193, "AAPL".to_string())]);
        for facts in [vec![original.clone(), amended.clone()], vec![amended, original]] {
            let events = to_events(&facts, &tickers);
            let EventKind::Fundamental(r) = &events[0].kind else { panic!("expected a fundamental event") };
            assert_eq!(r.value, Decimal::TWO);
        }
    }
}
