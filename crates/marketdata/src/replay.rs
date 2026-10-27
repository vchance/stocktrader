//! Streaming bars from Parquet into a backtest without holding the history in memory.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::iter::Peekable;
use std::path::Path;
use std::rc::Rc;

use anyhow::{Result, bail};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use st_core::{Event, EventSource};

use crate::store::{BarFilter, BarReader, DailyBar, to_core_bar};

/// What a [`BarEventSource`] saw, readable while and after the replay runs.
#[derive(Clone, Debug, Default)]
pub struct ReplayStats {
    /// Bars handed to the replay.
    pub bars: usize,
    /// Bars dropped by [`to_core_bar`] (non-finite or non-positive prices and the like).
    pub invalid: usize,
    pub symbols: BTreeSet<String>,
    /// Bars without a matching as-traded close, per symbol (only counted when a raw file is used).
    pub missing_traded: BTreeMap<String, usize>,
    /// A read error ends the replay early; callers must check this afterwards.
    pub error: Option<String>,
}

type Stream = Peekable<BarReader>;

/// Bars from a date-major adjusted file, each carrying its as-traded close from a date-major raw file
/// when one is given. Both files are read in step, merged on (date, symbol), so memory stays at one
/// record batch per file whatever the length of the history.
pub struct BarEventSource {
    adjusted: Stream,
    raw: Option<Stream>,
    last: Option<(NaiveDate, String)>,
    stats: Rc<RefCell<ReplayStats>>,
}

fn open(path: &Path, filter: &BarFilter) -> Result<Stream> {
    let reader = BarReader::open(path, filter)?;
    if !reader.is_date_major() {
        bail!(
            "{} is ordered by symbol (written before date-major files); re-run `marketdata sync` \
             (deploy/collector.sh sync-marketdata) to rewrite it",
            path.display()
        );
    }
    Ok(reader.peekable())
}

fn key(bar: &DailyBar) -> (NaiveDate, &str) {
    (bar.date, bar.symbol.as_str())
}

impl BarEventSource {
    pub fn open(adjusted: &Path, raw: Option<&Path>, filter: &BarFilter) -> Result<(Self, Rc<RefCell<ReplayStats>>)> {
        let stats = Rc::new(RefCell::new(ReplayStats::default()));
        let source = Self {
            adjusted: open(adjusted, filter)?,
            raw: raw.map(|p| open(p, filter)).transpose()?,
            last: None,
            stats: stats.clone(),
        };
        Ok((source, stats))
    }

    fn fail(&mut self, error: String) -> Option<Event> {
        self.stats.borrow_mut().error = Some(error);
        None
    }

    /// The raw close for `bar`, skipping raw rows that sort before it. `Err` on a read error.
    fn traded_close(&mut self, bar: &DailyBar) -> Result<Option<f64>, String> {
        let Some(raw) = self.raw.as_mut() else {
            return Ok(None);
        };
        loop {
            match raw.peek() {
                None => return Ok(None),
                Some(Err(_)) => return Err(format!("{:#}", raw.next().and_then(Result::err).expect("peeked an error"))),
                Some(Ok(r)) if key(r) < key(bar) => {
                    raw.next();
                }
                Some(Ok(r)) if key(r) == key(bar) => return Ok(Some(r.close)),
                Some(Ok(_)) => return Ok(None),
            }
        }
    }
}

impl EventSource for BarEventSource {
    fn next_event(&mut self) -> Option<Event> {
        loop {
            let daily = match self.adjusted.next()? {
                Ok(bar) => bar,
                Err(e) => return self.fail(format!("{e:#}")),
            };
            let this = (daily.date, daily.symbol.clone());
            if self.last.as_ref().is_some_and(|last| *last >= this) {
                return self.fail(format!("bars out of (date, symbol) order at {} {}", daily.symbol, daily.date));
            }
            self.last = Some(this);
            let traded = match self.traded_close(&daily) {
                Ok(t) => t,
                Err(e) => return self.fail(e),
            };
            let mut stats = self.stats.borrow_mut();
            let Some(mut bar) = to_core_bar(&daily) else {
                stats.invalid += 1;
                continue;
            };
            stats.bars += 1;
            if !stats.symbols.contains(&daily.symbol) {
                stats.symbols.insert(daily.symbol.clone());
            }
            if self.raw.is_some() {
                bar.traded_close = traded.and_then(Decimal::from_f64).map(|d| d.round_dp(6));
                if bar.traded_close.is_none() {
                    *stats.missing_traded.entry(daily.symbol).or_default() += 1;
                }
            }
            return Some(Event::bar(bar));
        }
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;
    use st_core::EventKind;

    use super::*;
    use crate::store::BarWriter;

    fn bar(symbol: &str, day: u32, close: f64) -> DailyBar {
        DailyBar {
            symbol: symbol.into(),
            date: NaiveDate::from_ymd_opt(2026, 10, day).unwrap(),
            open: close,
            high: close,
            low: close,
            close,
            volume: 100.0,
            trade_count: None,
            vwap: None,
        }
    }

    fn write(path: &Path, rows: &[DailyBar]) {
        let mut w = BarWriter::create(path).unwrap();
        w.write(rows).unwrap();
        w.finish().unwrap().commit().unwrap();
    }

    fn drain(mut source: BarEventSource) -> Vec<(String, u32, Decimal, Option<Decimal>)> {
        use chrono::Datelike;
        let mut out = Vec::new();
        while let Some(e) = source.next_event() {
            let EventKind::Bar(b) = e.kind else { panic!("bars only") };
            out.push((b.symbol.to_string(), b.end.date_naive().day(), b.close, b.traded_close));
        }
        out
    }

    #[test]
    fn merges_traded_closes_in_date_order() {
        let dir = tempfile::tempdir().unwrap();
        let (adj, raw) = (dir.path().join("all.parquet"), dir.path().join("raw.parquet"));
        write(&adj, &[bar("B", 1, 1.0), bar("B", 2, 2.0), bar("A", 1, 0.5), bar("A", 2, 0.0), bar("C", 2, 3.0)]);
        // Raw lacks C, and has an extra symbol the adjusted file doesn't.
        write(&raw, &[bar("A", 1, 50.0), bar("AA", 1, 9.0), bar("B", 1, 10.0), bar("B", 2, 20.0)]);

        let (source, stats) = BarEventSource::open(&adj, Some(&raw), &BarFilter::default()).unwrap();
        assert_eq!(
            drain(source),
            vec![
                ("A".into(), 1, dec!(0.5), Some(dec!(50))),
                ("B".into(), 1, dec!(1), Some(dec!(10))),
                ("B".into(), 2, dec!(2), Some(dec!(20))),
                ("C".into(), 2, dec!(3), None),
            ]
        );
        let stats = stats.borrow();
        assert_eq!((stats.bars, stats.invalid), (4, 1), "A on the 2nd has a zero close");
        assert_eq!(stats.missing_traded, BTreeMap::from([("C".to_string(), 1)]));
        assert!(stats.error.is_none());
    }

    #[test]
    fn without_raw_prices_traded_close_is_unset() {
        let dir = tempfile::tempdir().unwrap();
        let adj = dir.path().join("all.parquet");
        write(&adj, &[bar("A", 1, 1.0)]);
        let (source, stats) = BarEventSource::open(&adj, None, &BarFilter::default()).unwrap();
        assert_eq!(drain(source), vec![("A".into(), 1, dec!(1), None)]);
        assert!(stats.borrow().missing_traded.is_empty());
    }

    #[test]
    fn refuses_symbol_major_files() {
        use std::fs::File;
        use std::sync::Arc;

        use arrow_array::{ArrayRef, Date32Array, Float64Array, RecordBatch, StringArray, UInt64Array};
        use parquet::arrow::ArrowWriter;

        // A file as written before this layout existed: no layout marker.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.parquet");
        let f = |v: f64| Arc::new(Float64Array::from(vec![v])) as ArrayRef;
        let columns: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from(vec!["A"])),
            Arc::new(Date32Array::from(vec![20_000])),
            f(1.0),
            f(1.0),
            f(1.0),
            f(1.0),
            f(1.0),
            Arc::new(UInt64Array::from(vec![None::<u64>])),
            f(1.0),
        ];
        let batch = RecordBatch::try_new(crate::store::schema(), columns).unwrap();
        let mut w = ArrowWriter::try_new(File::create(&path).unwrap(), batch.schema(), None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();

        let err = BarEventSource::open(&path, None, &BarFilter::default()).err().unwrap();
        assert!(format!("{err:#}").contains("re-run `marketdata sync`"));
    }
}
