use std::collections::HashSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow_array::cast::AsArray;
use arrow_array::types::{Date32Type, Float64Type, UInt64Type};
use arrow_array::{Array, ArrayRef, Date32Array, Float64Array, RecordBatch, StringArray, UInt64Array};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use chrono::{Days, NaiveDate};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use st_core::{Bar, Symbol};

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

fn schema() -> SchemaRef {
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

/// Streams bars into a Parquet file. Rows go to `<path>.tmp` and are renamed into place by
/// [`BarWriter::finish`], so readers never see a partial file and a failed download leaves the previous
/// file untouched.
pub struct BarWriter {
    writer: ArrowWriter<File>,
    tmp: PathBuf,
    path: PathBuf,
    rows: usize,
}

impl BarWriter {
    pub fn create(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("parquet.tmp");
        let file = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let props = WriterProperties::builder().set_compression(Compression::SNAPPY).build();
        let writer = ArrowWriter::try_new(file, schema(), Some(props))?;
        Ok(Self { writer, tmp, path: path.to_path_buf(), rows: 0 })
    }

    pub fn write(&mut self, bars: &[DailyBar]) -> Result<()> {
        if bars.is_empty() {
            return Ok(());
        }
        self.writer.write(&to_batch(bars)?)?;
        self.rows += bars.len();
        Ok(())
    }

    /// Returns the number of rows written.
    pub fn finish(self) -> Result<usize> {
        let file = self.writer.into_inner()?;
        file.sync_all()?;
        fs::rename(&self.tmp, &self.path)?;
        Ok(self.rows)
    }
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

/// Reads every bar matching `filter`, in file order (symbol, then date).
pub fn read_bars(path: &Path, filter: &BarFilter) -> Result<Vec<DailyBar>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
    let mut out = Vec::new();
    for batch in reader {
        let batch = batch?;
        let col = |name: &str| batch.column_by_name(name).with_context(|| format!("missing column {name}"));
        let symbol = col("symbol")?.as_string::<i32>();
        let date = col("date")?.as_primitive::<Date32Type>();
        let open = col("open")?.as_primitive::<Float64Type>();
        let high = col("high")?.as_primitive::<Float64Type>();
        let low = col("low")?.as_primitive::<Float64Type>();
        let close = col("close")?.as_primitive::<Float64Type>();
        let volume = col("volume")?.as_primitive::<Float64Type>();
        let trade_count = col("trade_count")?.as_primitive::<UInt64Type>();
        let vwap = col("vwap")?.as_primitive::<Float64Type>();
        for i in 0..batch.num_rows() {
            let day = epoch() + Days::new(u64::try_from(date.value(i)).context("date before 1970")?);
            if !filter.keeps(symbol.value(i), day) {
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
    }
    Ok(out)
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
        let symbol = batch.column_by_name("symbol").context("missing column symbol")?.as_string::<i32>();
        let date = batch.column_by_name("date").context("missing column date")?.as_primitive::<Date32Type>();
        for i in 0..batch.num_rows() {
            if !symbols.contains(symbol.value(i)) {
                symbols.insert(symbol.value(i).to_string());
            }
            let d = date.value(i);
            first = Some(first.map_or(d, |f| f.min(d)));
            last = Some(last.map_or(d, |l| l.max(d)));
        }
    }
    let to_date = |d: i32| u64::try_from(d).ok().map(|d| epoch() + Days::new(d));
    Ok(Summary { rows, symbols: symbols.len(), first: first.and_then(to_date), last: last.and_then(to_date) })
}

/// Converts to the engine's bar type, with prices rounded to 4 decimal places. `None` if any value is not
/// a finite number.
pub fn to_core_bar(bar: &DailyBar) -> Option<Bar> {
    let price = |x: f64| Decimal::from_f64(x).map(|d| d.round_dp(4));
    Some(st_data::daily_bar(
        Symbol::new(&bar.symbol),
        bar.date,
        price(bar.open)?,
        price(bar.high)?,
        price(bar.low)?,
        price(bar.close)?,
        Decimal::from_f64(bar.volume)?.round(),
    ))
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
        let all = vec![bar("AAPL", 1, 100.0), bar("AAPL", 2, 101.25), bar("MSFT", 1, 300.0), bar("MSFT", 2, 301.0)];

        let mut writer = BarWriter::create(&path).unwrap();
        writer.write(&all[..2]).unwrap();
        writer.write(&all[2..]).unwrap();
        assert!(!path.exists(), "file must not appear before finish");
        assert_eq!(writer.finish().unwrap(), 4);

        assert_eq!(read_bars(&path, &BarFilter::default()).unwrap(), all);

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
    fn unfinished_writer_leaves_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bars.parquet");
        let mut first = BarWriter::create(&path).unwrap();
        first.write(&[bar("AAPL", 1, 100.0)]).unwrap();
        first.finish().unwrap();

        let mut second = BarWriter::create(&path).unwrap();
        second.write(&[bar("MSFT", 1, 300.0)]).unwrap();
        drop(second);

        assert_eq!(read_bars(&path, &BarFilter::default()).unwrap(), vec![bar("AAPL", 1, 100.0)]);
    }

    #[test]
    fn converts_to_core_bar() {
        let core = to_core_bar(&bar("aapl", 2, 101.123456)).unwrap();
        assert_eq!(core.symbol.as_str(), "AAPL");
        assert_eq!(core.close, Decimal::new(1011235, 4));
        assert_eq!(core.volume, Decimal::new(1234, 0));
        assert!(to_core_bar(&DailyBar { close: f64::NAN, ..bar("X", 1, 1.0) }).is_none());
    }
}
