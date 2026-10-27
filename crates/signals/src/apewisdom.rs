use std::collections::HashSet;

use chrono::{DateTime, TimeDelta, Utc};
use serde::Deserialize;
use st_collector::RawRecord;
use st_core::{AttentionObservation, Event, EventKind, Symbol};

pub const SOURCE: &str = "apewisdom";
/// ApeWisdom counts mentions over the trailing 24 hours.
const WINDOW_HOURS: u32 = 24;
/// Pages of one poll arrive within seconds; a later page further than this from page 1 starts a new
/// (partial) snapshot instead of joining a stale one.
const MAX_SNAPSHOT_SPAN: TimeDelta = TimeDelta::minutes(10);

/// One ticker in one ApeWisdom snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct AttentionRow {
    /// When the poll that produced this row started (its page 1 was fetched).
    pub snapshot_at: DateTime<Utc>,
    /// When this row's page was fetched: the moment the numbers were known to us.
    pub observed_at: DateTime<Utc>,
    pub ticker: String,
    pub rank: Option<u32>,
    pub mentions: u32,
    /// Net votes; can be negative.
    pub upvotes: Option<i32>,
    pub rank_24h_ago: Option<u32>,
    pub mentions_24h_ago: Option<u32>,
}

#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    results: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct Row {
    ticker: String,
    mentions: u32,
    rank: Option<u32>,
    upvotes: Option<i32>,
    rank_24h_ago: Option<u32>,
    mentions_24h_ago: Option<u32>,
}

#[derive(Debug, Default)]
pub struct Build {
    pub rows: Vec<AttentionRow>,
    pub snapshots: usize,
    /// Pages or rows that could not be parsed.
    pub malformed: usize,
}

fn page_number(url: &str) -> Option<u32> {
    url.rsplit_once("/page/")?.1.split(['?', '/']).next()?.parse().ok()
}

/// Turns archived ApeWisdom page responses into one row per ticker per snapshot, one record at a time, so
/// nothing but the current page is held. Feed records in archive order ([`st_collector::read_source`]
/// order: chronological within each day file). Exact repeats of a record within a day (a day present as
/// both `.jsonl` and `.jsonl.gz` after an interrupted compaction) are dropped. A ticker that appears on two
/// pages of one snapshot (rankings shift while pages are fetched) is kept once, from the earlier page.
#[derive(Default)]
pub struct SnapshotBuilder {
    snapshot: Option<DateTime<Utc>>,
    seen_tickers: HashSet<String>,
    day: Option<chrono::NaiveDate>,
    seen_records: HashSet<(DateTime<Utc>, String)>,
    pub snapshots: usize,
    /// Pages or rows that could not be parsed.
    pub malformed: usize,
}

impl SnapshotBuilder {
    pub fn push(&mut self, record: &RawRecord, mut emit: impl FnMut(AttentionRow)) {
        if record.status != 200 {
            return;
        }
        let day = record.fetched_at.date_naive();
        if self.day != Some(day) {
            self.day = Some(day);
            self.seen_records.clear();
        }
        if !self.seen_records.insert((record.fetched_at, record.url.clone())) {
            return;
        }
        let starts_new = page_number(&record.url) == Some(1)
            || self.snapshot.is_none_or(|start| {
                let gap = record.fetched_at - start;
                gap > MAX_SNAPSHOT_SPAN || gap < TimeDelta::zero()
            });
        if starts_new {
            self.snapshot = Some(record.fetched_at);
            self.seen_tickers.clear();
            self.snapshots += 1;
        }
        let snapshot_at = self.snapshot.expect("set above");
        let Ok(page) = serde_json::from_str::<Page>(&record.body) else {
            self.malformed += 1;
            return;
        };
        for value in page.results {
            let Ok(row) = Row::deserialize(&value) else {
                self.malformed += 1;
                continue;
            };
            let ticker = row.ticker.trim().to_ascii_uppercase();
            if ticker.is_empty() || !self.seen_tickers.insert(ticker.clone()) {
                continue;
            }
            emit(AttentionRow {
                snapshot_at,
                observed_at: record.fetched_at,
                ticker,
                rank: row.rank,
                mentions: row.mentions,
                upvotes: row.upvotes,
                rank_24h_ago: row.rank_24h_ago,
                mentions_24h_ago: row.mentions_24h_ago,
            });
        }
    }
}

/// In-memory convenience over [`SnapshotBuilder`] for records in any order: sorts them first.
pub fn build_rows(records: impl IntoIterator<Item = RawRecord>) -> Build {
    let mut records: Vec<RawRecord> = records.into_iter().collect();
    records.sort_by(|a, b| (a.fetched_at, &a.url).cmp(&(b.fetched_at, &b.url)));
    let mut builder = SnapshotBuilder::default();
    let mut rows = Vec::new();
    for record in &records {
        builder.push(record, |row| rows.push(row));
    }
    Build { rows, snapshots: builder.snapshots, malformed: builder.malformed }
}

/// Engine events, available when each row was observed and sorted by that time.
pub fn to_events(rows: &[AttentionRow]) -> Vec<Event> {
    let mut events: Vec<Event> = rows
        .iter()
        .map(|r| Event {
            available_at: r.observed_at,
            kind: EventKind::Attention(AttentionObservation {
                symbol: Symbol::new(&r.ticker),
                source: SOURCE.to_string(),
                window_hours: WINDOW_HOURS,
                mentions: r.mentions,
                mentions_prev: r.mentions_24h_ago,
                rank: r.rank,
                rank_prev: r.rank_24h_ago,
                upvotes: r.upvotes,
            }),
        })
        .collect();
    events.sort_by_key(|e| e.available_at);
    events
}

#[cfg(test)]
pub(crate) mod tests {
    use chrono::TimeZone;

    use super::*;

    pub(crate) fn page(at: DateTime<Utc>, n: u32, results: &str) -> RawRecord {
        RawRecord {
            fetched_at: at,
            source: SOURCE.into(),
            url: format!("https://apewisdom.io/api/v1.0/filter/all-stocks/page/{n}"),
            status: 200,
            body: format!(r#"{{"count":3,"pages":2,"current_page":{n},"results":[{results}]}}"#),
        }
    }

    pub(crate) fn at(hour: u32, sec: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 27, hour, 0, sec).unwrap()
    }

    #[test]
    fn groups_pages_into_snapshots_and_keeps_point_in_time_stamps() {
        let p1 = page(at(6, 0), 1, r#"{"rank":1,"ticker":"SPY","name":"S&amp;P","mentions":97,"upvotes":300,"rank_24h_ago":1,"mentions_24h_ago":69},
                                      {"rank":2,"ticker":"mu","mentions":79,"upvotes":174,"rank_24h_ago":3,"mentions_24h_ago":null}"#);
        // Rankings shifted while fetching: MU shows up again on page 2.
        let p2 = page(at(6, 1), 2, r#"{"rank":3,"ticker":"MU","mentions":78,"upvotes":1,"rank_24h_ago":4,"mentions_24h_ago":5},
                                      {"rank":4,"ticker":"TSLA","mentions":"lots"},
                                      {"rank":5,"ticker":"AMD","mentions":40,"upvotes":-5}"#);
        let next = page(at(7, 0), 1, r#"{"rank":1,"ticker":"SPY","mentions":100}"#);
        let broken = RawRecord { body: "{".into(), ..page(at(7, 1), 2, "") };

        // Out of order, with a duplicate, as read_source may deliver them.
        let build = build_rows(vec![next.clone(), p2.clone(), p1.clone(), p1, broken]);
        assert_eq!(build.snapshots, 2);
        assert_eq!(build.malformed, 2, "the non-numeric row and the unreadable page");

        let summary: Vec<(String, DateTime<Utc>, DateTime<Utc>, u32)> =
            build.rows.iter().map(|r| (r.ticker.clone(), r.snapshot_at, r.observed_at, r.mentions)).collect();
        assert_eq!(
            summary,
            vec![
                ("SPY".into(), at(6, 0), at(6, 0), 97),
                ("MU".into(), at(6, 0), at(6, 0), 79),
                ("AMD".into(), at(6, 0), at(6, 1), 40),
                ("SPY".into(), at(7, 0), at(7, 0), 100),
            ]
        );
        assert_eq!(build.rows[1].mentions_24h_ago, None);
        assert_eq!(build.rows[0].mentions_24h_ago, Some(69));
        assert_eq!(build.rows[2].upvotes, Some(-5), "net votes can be negative");
    }

    #[test]
    fn streaming_in_archive_order_drops_a_replayed_day() {
        let p1 = page(at(6, 0), 1, r#"{"rank":1,"ticker":"SPY","mentions":97}"#);
        let p2 = page(at(6, 1), 2, r#"{"rank":2,"ticker":"MU","mentions":79}"#);
        // The same day read again from its .jsonl.gz copy.
        let mut builder = SnapshotBuilder::default();
        let mut rows = Vec::new();
        for record in [&p1, &p2, &p1, &p2] {
            builder.push(record, |r| rows.push(r));
        }
        assert_eq!(builder.snapshots, 1);
        assert_eq!(rows.iter().map(|r| r.ticker.as_str()).collect::<Vec<_>>(), vec!["SPY", "MU"]);
    }

    #[test]
    fn late_page_without_its_first_page_starts_a_partial_snapshot() {
        let p1 = page(at(6, 0), 1, r#"{"rank":1,"ticker":"SPY","mentions":1}"#);
        let orphan = page(at(8, 0), 3, r#"{"rank":201,"ticker":"XYZ","mentions":2}"#);
        let build = build_rows(vec![p1, orphan]);
        assert_eq!(build.snapshots, 2);
        assert_eq!(build.rows[1].snapshot_at, at(8, 0));
    }

    #[test]
    fn events_are_available_when_observed() {
        let build = build_rows(vec![page(at(6, 0), 1, r#"{"rank":2,"ticker":"MU","mentions":79,"mentions_24h_ago":30}"#)]);
        let events = to_events(&build.rows);
        assert_eq!(events[0].available_at, at(6, 0));
        let EventKind::Attention(a) = &events[0].kind else { panic!("expected attention") };
        assert_eq!((a.symbol.as_str(), a.mentions, a.mentions_prev, a.rank), ("MU", 79, Some(30), Some(2)));
    }

    #[test]
    fn parses_page_numbers() {
        assert_eq!(page_number("https://apewisdom.io/api/v1.0/filter/all-stocks/page/7"), Some(7));
        assert_eq!(page_number("https://x/page/2?x=1"), Some(2));
        assert_eq!(page_number("https://x/other"), None);
    }
}
