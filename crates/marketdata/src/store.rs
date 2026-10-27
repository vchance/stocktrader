use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow_array::{Array, ArrayRef, Date32Array, Float64Array, RecordBatch, StringArray, UInt64Array};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use chrono::{Datelike, Days, NaiveDate};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use parquet::basic::Compression;
use parquet::file::metadata::{KeyValue, ParquetMetaData};
use parquet::file::properties::WriterProperties;
use parquet::file::statistics::Statistics;
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use st_core::{Bar, Symbol};

/// Parquet key-value metadata recording how rows are ordered.
const LAYOUT_KEY: &str = "stocktrader.layout";
/// Rows sorted by (date, symbol), so a replay can stream the file from start to end.
const DATE_MAJOR: &str = "date-major";
/// Row group size of a finished file: about two weeks of the whole US market, so date filters skip most of
/// a file and a streaming reader holds little at a time.
const ROW_GROUP_ROWS: usize = 128 * 1024;
/// Rows a [`BarWriter`] buffers before spilling them to disk (tens of MB).
const SPILL_ROWS: usize = 256 * 1024;

/// One regular-session daily bar as delivered by the data vendor. Prices stay `f64` here because that is
/// what the vendor sends; conversion to `Decimal` happens once, in [`to_core_bar`].
#[derive(Clone, Debug, PartialEq)]
pub struct DailyBar {
    pub symbol: String,
    /// Exchange (New York) session date.
    pub date: NaiveDate,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    /// `f64` because split-adjusted volume can be fractional.
    pub volume: f64,
    pub trade_count: Option<u64>,
    pub vwap: Option<f64>,
}

pub(crate) fn schema() -> SchemaRef {
    let price = |name| Field::new(name, DataType::Float64, false);
    Arc::new(Schema::new(vec![
        Field::new("symbol", DataType::Utf8, false),
        Field::new("date", DataType::Date32, false),
        price("open"),
        price("high"),
        price("low"),
        price("close"),
        price("volume"),
        Field::new("trade_count", DataType::UInt64, true),
        Field::new("vwap", DataType::Float64, true),
    ]))
}

fn epoch() -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid date")
}

fn from_days(d: i32) -> Result<NaiveDate> {
    Ok(epoch() + Days::new(u64::try_from(d).context("date before 1970")?))
}

fn to_batch(bars: &[DailyBar]) -> Result<RecordBatch> {
    let f64s = |f: fn(&DailyBar) -> f64| Arc::new(Float64Array::from_iter_values(bars.iter().map(f))) as ArrayRef;
    let columns: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from_iter_values(bars.iter().map(|b| b.symbol.as_str()))),
        Arc::new(Date32Array::from_iter_values(bars.iter().map(|b| (b.date - epoch()).num_days() as i32))),
        f64s(|b| b.open),
        f64s(|b| b.high),
        f64s(|b| b.low),
        f64s(|b| b.close),
        f64s(|b| b.volume),
        Arc::new(UInt64Array::from_iter(bars.iter().map(|b| b.trade_count))),
        Arc::new(Float64Array::from_iter(bars.iter().map(|b| b.vwap))),
    ];
    Ok(RecordBatch::try_new(schema(), columns)?)
}

fn writer(path: &Path, row_group_rows: usize, metadata: Option<Vec<KeyValue>>) -> Result<ArrowWriter<File>> {
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .set_max_row_group_row_count(Some(row_group_rows))
        .set_dictionary_enabled(metadata.is_some())
        .set_key_value_metadata(metadata)
        .build();
    Ok(ArrowWriter::try_new(file, schema(), Some(props))?)
}

/// Streams bars into `<path>.tmp`, whatever order they arrive in, and orders them by (date, symbol).
/// [`BarWriter::finish`] closes it and returns a [`StagedFile`]; nothing replaces `path` until
/// [`StagedFile::commit`], so callers can sanity-check (and commit several files together) first. A dropped
/// writer leaves the previous file untouched.
///
/// Incoming rows are bucketed by calendar quarter and, once [`SPILL_ROWS`] are buffered, written out as one
/// small file per quarter. `finish` sorts each quarter in memory and appends it to the result, so memory is
/// bounded by the buffer plus one quarter of data rather than by the whole history.
pub struct BarWriter {
    spill_dir: PathBuf,
    buffered: BTreeMap<i32, Vec<DailyBar>>,
    buffered_rows: usize,
    /// Spill files written so far, per quarter.
    spilled: BTreeMap<i32, usize>,
    tmp: PathBuf,
    path: PathBuf,
    rows: usize,
    symbols: HashSet<String>,
    last: Option<NaiveDate>,
}

/// Row count, distinct symbols and latest date of a bars file.
#[derive(Clone, Debug, PartialEq)]
pub struct Stats {
    pub rows: usize,
    pub symbols: usize,
    pub last: Option<NaiveDate>,
}

fn quarter(date: NaiveDate) -> i32 {
    date.year() * 4 + (date.month0() / 3) as i32
}

impl BarWriter {
    pub fn create(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let spill_dir = path.with_extension("parquet.spill");
        if spill_dir.exists() {
            fs::remove_dir_all(&spill_dir).with_context(|| format!("clearing {}", spill_dir.display()))?;
        }
        fs::create_dir_all(&spill_dir).with_context(|| format!("creating {}", spill_dir.display()))?;
        Ok(Self {
            spill_dir,
            buffered: BTreeMap::new(),
            buffered_rows: 0,
            spilled: BTreeMap::new(),
            tmp: path.with_extension("parquet.tmp"),
            path: path.to_path_buf(),
            rows: 0,
            symbols: HashSet::new(),
            last: None,
        })
    }

    pub fn write(&mut self, bars: &[DailyBar]) -> Result<()> {
        for bar in bars {
            self.buffered.entry(quarter(bar.date)).or_default().push(bar.clone());
            if !self.symbols.contains(&bar.symbol) {
                self.symbols.insert(bar.symbol.clone());
            }
            self.last = self.last.max(Some(bar.date));
        }
        self.rows += bars.len();
        self.buffered_rows += bars.len();
        if self.buffered_rows >= SPILL_ROWS {
            self.spill()?;
        }
        Ok(())
    }

    fn spill_path(&self, q: i32, n: usize) -> PathBuf {
        self.spill_dir.join(format!("{q}-{n}.parquet"))
    }

    fn spill(&mut self) -> Result<()> {
        for (q, rows) in std::mem::take(&mut self.buffered) {
            let n = *self.spilled.get(&q).unwrap_or(&0);
            let mut w = writer(&self.spill_path(q, n), SPILL_ROWS, None)?;
            w.write(&to_batch(&rows)?)?;
            w.close()?;
            self.spilled.insert(q, n + 1);
        }
        self.buffered_rows = 0;
        Ok(())
    }

    /// Sorts each quarter into the temporary file and syncs it, without replacing anything yet.
    pub fn finish(mut self) -> Result<StagedFile> {
        let layout = Some(vec![KeyValue::new(LAYOUT_KEY.to_string(), DATE_MAJOR.to_string())]);
        let mut out = writer(&self.tmp, ROW_GROUP_ROWS, layout)?;
        let quarters: BTreeSet<i32> = self.buffered.keys().chain(self.spilled.keys()).copied().collect();
        for q in quarters {
            let mut rows = self.buffered.remove(&q).unwrap_or_default();
            for n in 0..*self.spilled.get(&q).unwrap_or(&0) {
                let part = self.spill_path(q, n);
                rows.extend(read_bars(&part, &BarFilter::default())?);
                fs::remove_file(&part)?;
            }
            rows.sort_unstable_by(|a, b| (a.date, &a.symbol).cmp(&(b.date, &b.symbol)));
            for chunk in rows.chunks(ROW_GROUP_ROWS) {
                out.write(&to_batch(chunk)?)?;
            }
        }
        let file = out.into_inner()?;
        file.sync_all()?;
        let stats = Stats { rows: self.rows, symbols: self.symbols.len(), last: self.last };
        Ok(StagedFile { tmp: self.tmp.clone(), path: self.path.clone(), stats })
    }
}

impl Drop for BarWriter {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.spill_dir);
    }
}

/// A complete file waiting to replace its target.
#[derive(Debug)]
pub struct StagedFile {
    tmp: PathBuf,
    path: PathBuf,
    pub stats: Stats,
}

impl StagedFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn commit(self) -> Result<()> {
        fs::rename(&self.tmp, &self.path).with_context(|| format!("replacing {}", self.path.display()))
    }

    /// Deletes the temporary file.
    pub fn discard(self) -> Result<()> {
        Ok(fs::remove_file(&self.tmp)?)
    }
}

/// Looks up a column and checks its type, so a foreign Parquet file is an error rather than a panic.
fn column<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> Result<&'a T> {
    batch
        .column_by_name(name)
        .with_context(|| format!("missing column {name}"))?
        .as_any()
        .downcast_ref::<T>()
        .with_context(|| format!("column {name} has an unexpected type"))
}

#[derive(Clone, Debug, Default)]
pub struct BarFilter {
    pub symbols: Option<HashSet<String>>,
    pub start: Option<NaiveDate>,
    pub end: Option<NaiveDate>,
}

impl BarFilter {
    fn keeps(&self, symbol: &str, date: NaiveDate) -> bool {
        self.symbols.as_ref().is_none_or(|s| s.contains(symbol))
            && self.start.is_none_or(|s| date >= s)
            && self.end.is_none_or(|e| date <= e)
    }
}

/// Smallest and largest `date` in each row group, from the file's statistics (`None` where absent).
fn row_group_dates(meta: &ParquetMetaData) -> Result<Vec<Option<(NaiveDate, NaiveDate)>>> {
    let index = meta
        .file_metadata()
        .schema_descr()
        .columns()
        .iter()
        .position(|c| c.name() == "date")
        .context("missing column date")?;
    meta.row_groups()
        .iter()
        .map(|rg| match rg.column(index).statistics() {
            Some(Statistics::Int32(s)) => match (s.min_opt(), s.max_opt()) {
                (Some(lo), Some(hi)) => Ok(Some((from_days(*lo)?, from_days(*hi)?))),
                _ => Ok(None),
            },
            _ => Ok(None),
        })
        .collect()
}

/// Bars matching a filter, read lazily one record batch at a time in file order. Row groups whose dates
/// fall outside the filter are never read.
pub struct BarReader {
    batches: ParquetRecordBatchReader,
    filter: BarFilter,
    pending: std::vec::IntoIter<DailyBar>,
    date_major: bool,
}

impl BarReader {
    pub fn open(path: &Path, filter: &BarFilter) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).with_context(|| format!("reading {}", path.display()))?;
        let meta = builder.metadata().clone();
        let date_major = meta
            .file_metadata()
            .key_value_metadata()
            .is_some_and(|kv| kv.iter().any(|e| e.key == LAYOUT_KEY && e.value.as_deref() == Some(DATE_MAJOR)));
        let groups: Vec<usize> = row_group_dates(&meta)?
            .into_iter()
            .enumerate()
            .filter(|(_, range)| {
                range.is_none_or(|(lo, hi)| filter.start.is_none_or(|s| hi >= s) && filter.end.is_none_or(|e| lo <= e))
            })
            .map(|(i, _)| i)
            .collect();
        let batches = builder.with_row_groups(groups).build()?;
        Ok(Self { batches, filter: filter.clone(), pending: Vec::new().into_iter(), date_major })
    }

    /// Whether the file is ordered by (date, symbol), as every file written by [`BarWriter`] is. Files from
    /// older versions are ordered by symbol.
    pub fn is_date_major(&self) -> bool {
        self.date_major
    }

    fn decode(&self, batch: &RecordBatch) -> Result<Vec<DailyBar>> {
        let symbol = column::<StringArray>(batch, "symbol")?;
        let date = column::<Date32Array>(batch, "date")?;
        let open = column::<Float64Array>(batch, "open")?;
        let high = column::<Float64Array>(batch, "high")?;
        let low = column::<Float64Array>(batch, "low")?;
        let close = column::<Float64Array>(batch, "close")?;
        let volume = column::<Float64Array>(batch, "volume")?;
        let trade_count = column::<UInt64Array>(batch, "trade_count")?;
        let vwap = column::<Float64Array>(batch, "vwap")?;
        let mut out = Vec::new();
        for i in 0..batch.num_rows() {
            let day = from_days(date.value(i))?;
            if !self.filter.keeps(symbol.value(i), day) {
                continue;
            }
            out.push(DailyBar {
                symbol: symbol.value(i).to_string(),
                date: day,
                open: open.value(i),
                high: high.value(i),
                low: low.value(i),
                close: close.value(i),
                volume: volume.value(i),
                trade_count: trade_count.is_valid(i).then(|| trade_count.value(i)),
                vwap: vwap.is_valid(i).then(|| vwap.value(i)),
            });
        }
        Ok(out)
    }
}

impl Iterator for BarReader {
    type Item = Result<DailyBar>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(bar) = self.pending.next() {
                return Some(Ok(bar));
            }
            let batch = match self.batches.next()? {
                Ok(batch) => batch,
                Err(e) => return Some(Err(e.into())),
            };
            match self.decode(&batch) {
                Ok(bars) => self.pending = bars.into_iter(),
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

/// Reads every bar matching `filter`, in file order: (date, symbol) for files written by [`BarWriter`].
pub fn read_bars(path: &Path, filter: &BarFilter) -> Result<Vec<DailyBar>> {
    BarReader::open(path, filter)?.collect()
}

/// First and last date in a bars file, from its statistics, without reading any rows.
pub fn date_range(path: &Path) -> Result<Option<(NaiveDate, NaiveDate)>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let ranges = row_group_dates(builder.metadata())?;
    if ranges.iter().any(Option::is_none) {
        let s = summarize(path)?;
        return Ok(s.first.zip(s.last));
    }
    Ok(ranges.into_iter().flatten().reduce(|(a, b), (c, d)| (a.min(c), b.max(d))))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub rows: usize,
    pub symbols: usize,
    pub first: Option<NaiveDate>,
    pub last: Option<NaiveDate>,
}

/// Row count, distinct symbols and date range, streamed so large files never sit in memory.
pub fn summarize(path: &Path) -> Result<Summary> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
    let mut symbols = HashSet::new();
    let (mut rows, mut first, mut last) = (0, None::<i32>, None::<i32>);
    for batch in reader {
        let batch = batch?;
        rows += batch.num_rows();
        let symbol = column::<StringArray>(&batch, "symbol")?;
        let date = column::<Date32Array>(&batch, "date")?;
        for i in 0..batch.num_rows() {
            if !symbols.contains(symbol.value(i)) {
                symbols.insert(symbol.value(i).to_string());
            }
            let d = date.value(i);
            first = Some(first.map_or(d, |f| f.min(d)));
            last = Some(last.map_or(d, |l| l.max(d)));
        }
    }
    let to_date = |d: i32| from_days(d).ok();
    Ok(Summary { rows, symbols: symbols.len(), first: first.and_then(to_date), last: last.and_then(to_date) })
}

/// Converts to the engine's bar type, with prices rounded to 6 decimal places (split-adjusted history can
/// be tiny). `None` for bars the engine must not see: non-finite values, non-positive prices, `low > high`
/// or negative volume.
pub fn to_core_bar(bar: &DailyBar) -> Option<Bar> {
    let price = |x: f64| Decimal::from_f64(x).map(|d| d.round_dp(6)).filter(|d| *d > Decimal::ZERO);
    let (open, high, low, close) = (price(bar.open)?, price(bar.high)?, price(bar.low)?, price(bar.close)?);
    let volume = Decimal::from_f64(bar.volume)?.round();
    if low > high || volume < Decimal::ZERO {
        return None;
    }
    Some(st_data::daily_bar(Symbol::new(&bar.symbol), bar.date, open, high, low, close, volume))
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use super::*;

    fn bar(symbol: &str, day: u32, close: f64) -> DailyBar {
        DailyBar {
            symbol: symbol.into(),
            date: NaiveDate::from_ymd_opt(2026, 10, day).unwrap(),
            open: close - 1.0,
            high: close + 1.0,
            low: close - 2.0,
            close,
            volume: 1234.5,
            trade_count: day.is_multiple_of(2).then_some(42),
            vwap: None,
        }
    }

    #[test]
    fn round_trips_and_filters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bars.parquet");
        let all = [bar("AAPL", 1, 100.0), bar("AAPL", 2, 101.25), bar("MSFT", 1, 300.0), bar("MSFT", 2, 301.0)];

        let mut writer = BarWriter::create(&path).unwrap();
        writer.write(&all[..2]).unwrap();
        writer.write(&all[2..]).unwrap();
        let staged = writer.finish().unwrap();
        assert!(!path.exists(), "file must not appear before commit");
        assert_eq!(staged.stats, Stats { rows: 4, symbols: 2, last: Some(NaiveDate::from_ymd_opt(2026, 10, 2).unwrap()) });
        staged.commit().unwrap();

        // Written symbol by symbol, read back by date then symbol.
        let by_date = vec![all[0].clone(), all[2].clone(), all[1].clone(), all[3].clone()];
        assert_eq!(read_bars(&path, &BarFilter::default()).unwrap(), by_date);
        assert!(BarReader::open(&path, &BarFilter::default()).unwrap().is_date_major());
        assert!(!path.with_extension("parquet.spill").exists(), "spill files are cleaned up");

        let filter = BarFilter {
            symbols: Some(HashSet::from(["MSFT".to_string()])),
            start: Some(NaiveDate::from_ymd_opt(2026, 10, 2).unwrap()),
            end: None,
        };
        assert_eq!(read_bars(&path, &filter).unwrap(), vec![all[3].clone()]);

        let summary = summarize(&path).unwrap();
        assert_eq!((summary.rows, summary.symbols), (4, 2));
        assert_eq!(summary.first, Some(NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()));
        assert_eq!(summary.last, Some(NaiveDate::from_ymd_opt(2026, 10, 2).unwrap()));
    }

    #[test]
    fn orders_years_of_history_by_date_and_skips_row_groups_outside_the_filter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bars.parquet");
        let start = NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
        let days: Vec<NaiveDate> = (0..1500).map(|i| start + Days::new(i)).collect();
        let mut writer = BarWriter::create(&path).unwrap();
        // Symbol-major input, as the vendor delivers it: each symbol's full history in turn.
        for symbol in ["ZZZ", "AAA", "MMM"] {
            let rows: Vec<DailyBar> = days.iter().map(|d| DailyBar { date: *d, ..bar(symbol, 1, 10.0) }).collect();
            writer.write(&rows).unwrap();
        }
        writer.finish().unwrap().commit().unwrap();

        let all = read_bars(&path, &BarFilter::default()).unwrap();
        assert_eq!(all.len(), 4500);
        assert!(all.windows(2).all(|w| (w[0].date, &w[0].symbol) < (w[1].date, &w[1].symbol)));
        assert_eq!(date_range(&path).unwrap(), Some((days[0], days[1499])));

        let filter = BarFilter { start: Some(days[1400]), end: Some(days[1401]), ..BarFilter::default() };
        let got: Vec<(NaiveDate, String)> = read_bars(&path, &filter).unwrap().into_iter().map(|b| (b.date, b.symbol)).collect();
        assert_eq!(got.len(), 6);
        assert_eq!(got[0], (days[1400], "AAA".to_string()));
    }

    #[test]
    fn files_without_the_layout_marker_are_not_date_major() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.parquet");
        let mut w = writer(&path, ROW_GROUP_ROWS, None).unwrap();
        w.write(&to_batch(&[bar("AAPL", 1, 1.0)]).unwrap()).unwrap();
        w.close().unwrap();
        let reader = BarReader::open(&path, &BarFilter::default()).unwrap();
        assert!(!reader.is_date_major());
        assert_eq!(reader.count(), 1);
    }

    #[test]
    fn unfinished_writer_leaves_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bars.parquet");
        let mut first = BarWriter::create(&path).unwrap();
        first.write(&[bar("AAPL", 1, 100.0)]).unwrap();
        first.finish().unwrap().commit().unwrap();

        let mut dropped = BarWriter::create(&path).unwrap();
        dropped.write(&[bar("MSFT", 1, 300.0)]).unwrap();
        drop(dropped);
        assert!(!path.with_extension("parquet.spill").exists());
        let mut discarded = BarWriter::create(&path).unwrap();
        discarded.write(&[bar("MSFT", 1, 300.0)]).unwrap();
        discarded.finish().unwrap().discard().unwrap();

        assert_eq!(read_bars(&path, &BarFilter::default()).unwrap(), vec![bar("AAPL", 1, 100.0)]);
    }

    #[test]
    fn foreign_parquet_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("other.parquet");
        let schema = Arc::new(Schema::new(vec![Field::new("symbol", DataType::Float64, false)]));
        let batch =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(Float64Array::from(vec![1.0])) as ArrayRef]).unwrap();
        let mut writer = ArrowWriter::try_new(File::create(&path).unwrap(), schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();

        assert!(read_bars(&path, &BarFilter::default()).is_err());
        assert!(summarize(&path).is_err());
    }

    #[test]
    fn converts_to_core_bar_and_rejects_bad_prices() {
        let core = to_core_bar(&bar("aapl", 2, 101.1234567)).unwrap();
        assert_eq!(core.symbol.as_str(), "AAPL");
        assert_eq!(core.close, Decimal::new(101_123_457, 6));
        assert_eq!(core.volume, Decimal::new(1234, 0));

        let tiny = DailyBar { open: 0.00002, high: 0.00003, low: 0.00001, close: 0.00002, ..bar("X", 1, 1.0) };
        assert_eq!(to_core_bar(&tiny).unwrap().close, Decimal::new(20, 6));

        assert!(to_core_bar(&DailyBar { close: f64::NAN, ..bar("X", 1, 1.0) }).is_none());
        assert!(to_core_bar(&DailyBar { close: 0.0, ..bar("X", 1, 1.0) }).is_none());
        assert!(to_core_bar(&DailyBar { low: 5.0, high: 4.0, ..bar("X", 1, 1.0) }).is_none());
        assert!(to_core_bar(&DailyBar { open: 0.0000001, ..bar("X", 1, 1.0) }).is_none());
    }
}
