use std::collections::HashSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use arrow_array::{Array, ArrayRef, Int32Array, RecordBatch, StringArray, TimestampMicrosecondArray, UInt32Array};
use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};
use chrono::{DateTime, Utc};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;

use crate::apewisdom::AttentionRow;

fn schema() -> SchemaRef {
    let time = |name| Field::new(name, DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())), false);
    let count = |name, nullable| Field::new(name, DataType::UInt32, nullable);
    Arc::new(Schema::new(vec![
        time("snapshot_at"),
        time("observed_at"),
        Field::new("ticker", DataType::Utf8, false),
        count("rank", true),
        count("mentions", false),
        Field::new("upvotes", DataType::Int32, true),
        count("rank_24h_ago", true),
        count("mentions_24h_ago", true),
    ]))
}

fn micros(rows: &[AttentionRow], f: fn(&AttentionRow) -> DateTime<Utc>) -> ArrayRef {
    Arc::new(TimestampMicrosecondArray::from_iter_values(rows.iter().map(|r| f(r).timestamp_micros())).with_timezone("UTC"))
}

fn counts(rows: &[AttentionRow], f: fn(&AttentionRow) -> Option<u32>) -> ArrayRef {
    Arc::new(UInt32Array::from_iter(rows.iter().map(f)))
}

const BATCH_ROWS: usize = 100_000;

/// Streams rows into `<path>.tmp` in batches, so memory stays flat however large the archive grows, and
/// renames it into place on [`AttentionWriter::finish`].
pub struct AttentionWriter {
    writer: ArrowWriter<File>,
    tmp: PathBuf,
    path: PathBuf,
    buffer: Vec<AttentionRow>,
    rows: usize,
}

impl AttentionWriter {
    pub fn create(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("parquet.tmp");
        let props = WriterProperties::builder().set_compression(Compression::SNAPPY).build();
        let writer = ArrowWriter::try_new(File::create(&tmp)?, schema(), Some(props))?;
        Ok(Self { writer, tmp, path: path.to_path_buf(), buffer: Vec::with_capacity(BATCH_ROWS), rows: 0 })
    }

    pub fn push(&mut self, row: AttentionRow) -> Result<()> {
        self.buffer.push(row);
        if self.buffer.len() >= BATCH_ROWS {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let chunk = &self.buffer;
        let columns: Vec<ArrayRef> = vec![
            micros(chunk, |r| r.snapshot_at),
            micros(chunk, |r| r.observed_at),
            Arc::new(StringArray::from_iter_values(chunk.iter().map(|r| r.ticker.as_str()))),
            counts(chunk, |r| r.rank),
            Arc::new(UInt32Array::from_iter_values(chunk.iter().map(|r| r.mentions))),
            Arc::new(Int32Array::from_iter(chunk.iter().map(|r| r.upvotes))),
            counts(chunk, |r| r.rank_24h_ago),
            counts(chunk, |r| r.mentions_24h_ago),
        ];
        self.writer.write(&RecordBatch::try_new(schema(), columns)?)?;
        self.rows += self.buffer.len();
        self.buffer.clear();
        Ok(())
    }

    /// Replaces the target and returns the row count. Refuses to replace it with an empty table: if
    /// records were read but no rows came out, the input is broken, not empty.
    pub fn finish(mut self) -> Result<usize> {
        self.flush()?;
        let file = self.writer.into_inner()?;
        if self.rows == 0 {
            drop(file);
            let _ = fs::remove_file(&self.tmp);
            bail!("no rows to write; keeping the previous table");
        }
        file.sync_all()?;
        fs::rename(&self.tmp, &self.path).with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(self.rows)
    }
}

/// Replaces `path` with `rows` (see [`AttentionWriter`]).
pub fn write_rows(path: &Path, rows: &[AttentionRow]) -> Result<usize> {
    let mut writer = AttentionWriter::create(path)?;
    for row in rows {
        writer.push(row.clone())?;
    }
    writer.finish()
}

fn column<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> Result<&'a T> {
    batch
        .column_by_name(name)
        .with_context(|| format!("missing column {name}"))?
        .as_any()
        .downcast_ref::<T>()
        .with_context(|| format!("column {name} has an unexpected type"))
}

fn time(us: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_micros(us).context("timestamp out of range")
}

/// Which rows [`read_rows`] keeps. Backtests should narrow to the symbols and period they replay: a full
/// table holds tens of thousands of rows per day.
#[derive(Clone, Debug, Default)]
pub struct RowFilter {
    pub tickers: Option<HashSet<String>>,
    /// Inclusive bounds on `observed_at`.
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

impl RowFilter {
    pub fn ticker(ticker: &str) -> Self {
        Self { tickers: Some(HashSet::from([ticker.to_string()])), ..Self::default() }
    }
}

/// Reads rows matching `filter`, in file order.
pub fn read_rows(path: &Path, filter: &RowFilter) -> Result<Vec<AttentionRow>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
    let mut out = Vec::new();
    for batch in reader {
        let batch = batch?;
        let snapshot_at = column::<TimestampMicrosecondArray>(&batch, "snapshot_at")?;
        let observed_at = column::<TimestampMicrosecondArray>(&batch, "observed_at")?;
        let tickers = column::<StringArray>(&batch, "ticker")?;
        let rank = column::<UInt32Array>(&batch, "rank")?;
        let mentions = column::<UInt32Array>(&batch, "mentions")?;
        let upvotes = column::<Int32Array>(&batch, "upvotes")?;
        let rank_prev = column::<UInt32Array>(&batch, "rank_24h_ago")?;
        let mentions_prev = column::<UInt32Array>(&batch, "mentions_24h_ago")?;
        let opt = |a: &UInt32Array, i: usize| a.is_valid(i).then(|| a.value(i));
        for i in 0..batch.num_rows() {
            if filter.tickers.as_ref().is_some_and(|t| !t.contains(tickers.value(i))) {
                continue;
            }
            let observed = time(observed_at.value(i))?;
            if filter.from.is_some_and(|f| observed < f) || filter.to.is_some_and(|t| observed > t) {
                continue;
            }
            out.push(AttentionRow {
                snapshot_at: time(snapshot_at.value(i))?,
                observed_at: observed,
                ticker: tickers.value(i).to_string(),
                rank: opt(rank, i),
                mentions: mentions.value(i),
                upvotes: upvotes.is_valid(i).then(|| upvotes.value(i)),
                rank_24h_ago: opt(rank_prev, i),
                mentions_24h_ago: opt(mentions_prev, i),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apewisdom::build_rows;
    use crate::apewisdom::tests::{at, page};

    #[test]
    fn round_trips_and_filters_by_ticker() {
        let build = build_rows(vec![page(
            at(6, 0),
            1,
            r#"{"rank":1,"ticker":"SPY","mentions":97,"upvotes":300,"rank_24h_ago":1,"mentions_24h_ago":69},
               {"rank":2,"ticker":"MU","mentions":79}"#,
        )]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("signals/apewisdom.parquet");
        assert_eq!(write_rows(&path, &build.rows).unwrap(), 2);

        assert_eq!(read_rows(&path, &RowFilter::default()).unwrap(), build.rows);
        assert_eq!(read_rows(&path, &RowFilter::ticker("MU")).unwrap(), vec![build.rows[1].clone()]);
        let window = |from, to| RowFilter { from: Some(from), to: Some(to), ..RowFilter::default() };
        assert_eq!(read_rows(&path, &window(at(6, 0), at(6, 0))).unwrap().len(), 2, "bounds are inclusive");
        assert!(read_rows(&path, &window(at(7, 0), at(8, 0))).unwrap().is_empty());

        assert!(write_rows(&path, &[]).is_err(), "never replaces data with nothing");
        assert_eq!(read_rows(&path, &RowFilter::default()).unwrap().len(), 2);
        assert!(!path.with_extension("parquet.tmp").exists());
    }

    #[test]
    fn streams_more_rows_than_one_batch() {
        let row = build_rows(vec![page(at(6, 0), 1, r#"{"rank":1,"ticker":"SPY","mentions":1}"#)]).rows.remove(0);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.parquet");
        let mut writer = AttentionWriter::create(&path).unwrap();
        for _ in 0..(BATCH_ROWS * 2 + 7) {
            writer.push(row.clone()).unwrap();
        }
        assert_eq!(writer.finish().unwrap(), BATCH_ROWS * 2 + 7);
        assert_eq!(read_rows(&path, &RowFilter::default()).unwrap().len(), BATCH_ROWS * 2 + 7);
    }
}
