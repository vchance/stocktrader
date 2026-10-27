use std::fs::{self, File};
use std::path::Path;
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

/// Replaces `path` with `rows`, written to a temporary file and renamed into place. Refuses to write an
/// empty table: an empty result means the archive wasn't found, not that nothing was ever collected.
pub fn write_rows(path: &Path, rows: &[AttentionRow]) -> Result<()> {
    if rows.is_empty() {
        bail!("no rows to write; is the archive path right?");
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("parquet.tmp");
    let props = WriterProperties::builder().set_compression(Compression::SNAPPY).build();
    let mut writer = ArrowWriter::try_new(File::create(&tmp)?, schema(), Some(props))?;
    for chunk in rows.chunks(100_000) {
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
        writer.write(&RecordBatch::try_new(schema(), columns)?)?;
    }
    writer.into_inner()?.sync_all()?;
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
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

/// Reads rows, optionally only for `ticker`.
pub fn read_rows(path: &Path, ticker: Option<&str>) -> Result<Vec<AttentionRow>> {
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
            if ticker.is_some_and(|t| t != tickers.value(i)) {
                continue;
            }
            out.push(AttentionRow {
                snapshot_at: time(snapshot_at.value(i))?,
                observed_at: time(observed_at.value(i))?,
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
        write_rows(&path, &build.rows).unwrap();

        assert_eq!(read_rows(&path, None).unwrap(), build.rows);
        assert_eq!(read_rows(&path, Some("MU")).unwrap(), vec![build.rows[1].clone()]);
        assert!(write_rows(&path, &[]).is_err(), "never replaces data with nothing");
        assert_eq!(read_rows(&path, None).unwrap().len(), 2);
    }
}
