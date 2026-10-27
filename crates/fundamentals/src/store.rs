use std::collections::HashSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow_array::{Array, ArrayRef, Date32Array, Float64Array, Int32Array, RecordBatch, StringArray, UInt32Array};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use chrono::{NaiveDate, TimeDelta};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;

use crate::facts::Fact;

fn schema() -> SchemaRef {
    let text = |name, nullable| Field::new(name, DataType::Utf8, nullable);
    Arc::new(Schema::new(vec![
        Field::new("cik", DataType::UInt32, false),
        text("taxonomy", false),
        text("concept", false),
        text("unit", false),
        Field::new("start", DataType::Date32, true),
        Field::new("end", DataType::Date32, false),
        Field::new("value", DataType::Float64, false),
        Field::new("fiscal_year", DataType::Int32, true),
        text("fiscal_period", true),
        text("form", false),
        Field::new("filed", DataType::Date32, false),
        text("accession", false),
        text("frame", true),
    ]))
}

fn epoch() -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid date")
}

fn days(d: NaiveDate) -> i32 {
    (d - epoch()).num_days() as i32
}

/// Signed: a filer's mistyped pre-1970 date must not make the whole file unreadable.
fn date(days: i32) -> Result<NaiveDate> {
    epoch().checked_add_signed(TimeDelta::days(i64::from(days))).context("date out of range")
}

fn to_batch(facts: &[Fact]) -> Result<RecordBatch> {
    let strings = |f: fn(&Fact) -> &str| Arc::new(StringArray::from_iter_values(facts.iter().map(f))) as ArrayRef;
    let columns: Vec<ArrayRef> = vec![
        Arc::new(UInt32Array::from_iter_values(facts.iter().map(|f| f.cik))),
        strings(|f| &f.taxonomy),
        strings(|f| &f.concept),
        strings(|f| &f.unit),
        Arc::new(Date32Array::from_iter(facts.iter().map(|f| f.start.map(days)))),
        Arc::new(Date32Array::from_iter_values(facts.iter().map(|f| days(f.end)))),
        Arc::new(Float64Array::from_iter_values(facts.iter().map(|f| f.value))),
        Arc::new(Int32Array::from_iter(facts.iter().map(|f| f.fiscal_year))),
        Arc::new(StringArray::from_iter(facts.iter().map(|f| f.fiscal_period.as_deref()))),
        strings(|f| &f.form),
        Arc::new(Date32Array::from_iter_values(facts.iter().map(|f| days(f.filed)))),
        strings(|f| &f.accession),
        Arc::new(StringArray::from_iter(facts.iter().map(|f| f.frame.as_deref()))),
    ];
    Ok(RecordBatch::try_new(schema(), columns)?)
}

/// Row count, distinct companies and latest filing date of a facts file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub rows: usize,
    pub companies: usize,
    pub last_filed: Option<NaiveDate>,
}

/// Streams facts into `<path>.tmp`; nothing replaces `path` until [`StagedFacts::commit`].
pub struct FactWriter {
    writer: ArrowWriter<File>,
    tmp: PathBuf,
    path: PathBuf,
    stats: Stats,
    companies: HashSet<u32>,
}

impl FactWriter {
    pub fn create(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("parquet.tmp");
        let file = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let props = WriterProperties::builder().set_compression(Compression::SNAPPY).build();
        let writer = ArrowWriter::try_new(file, schema(), Some(props))?;
        Ok(Self { writer, tmp, path: path.to_path_buf(), stats: Stats::default(), companies: HashSet::new() })
    }

    pub fn write(&mut self, facts: &[Fact]) -> Result<()> {
        if facts.is_empty() {
            return Ok(());
        }
        self.writer.write(&to_batch(facts)?)?;
        self.stats.rows += facts.len();
        for f in facts {
            self.companies.insert(f.cik);
            self.stats.last_filed = self.stats.last_filed.max(Some(f.filed));
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<StagedFacts> {
        let file = self.writer.into_inner()?;
        file.sync_all()?;
        self.stats.companies = self.companies.len();
        Ok(StagedFacts { tmp: self.tmp, path: self.path, stats: self.stats })
    }
}

#[derive(Debug)]
pub struct StagedFacts {
    tmp: PathBuf,
    path: PathBuf,
    pub stats: Stats,
}

impl StagedFacts {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn commit(self) -> Result<()> {
        fs::rename(&self.tmp, &self.path).with_context(|| format!("replacing {}", self.path.display()))
    }

    pub fn discard(self) -> Result<()> {
        Ok(fs::remove_file(&self.tmp)?)
    }
}

#[derive(Clone, Debug, Default)]
pub struct FactFilter {
    pub ciks: Option<HashSet<u32>>,
    pub concepts: Option<HashSet<String>>,
    /// Only facts usable at the start of this date, i.e. filed before it. This is the same rule as
    /// [`crate::available_at`], which releases a fact at midnight after its filing date.
    pub as_of: Option<NaiveDate>,
}

fn column<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> Result<&'a T> {
    batch
        .column_by_name(name)
        .with_context(|| format!("missing column {name}"))?
        .as_any()
        .downcast_ref::<T>()
        .with_context(|| format!("column {name} has an unexpected type"))
}

/// Reads facts matching `filter`, in file order.
pub fn read_facts(path: &Path, filter: &FactFilter) -> Result<Vec<Fact>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
    let mut out = Vec::new();
    for batch in reader {
        let batch = batch?;
        let cik = column::<UInt32Array>(&batch, "cik")?;
        let taxonomy = column::<StringArray>(&batch, "taxonomy")?;
        let concept = column::<StringArray>(&batch, "concept")?;
        let unit = column::<StringArray>(&batch, "unit")?;
        let start = column::<Date32Array>(&batch, "start")?;
        let end = column::<Date32Array>(&batch, "end")?;
        let value = column::<Float64Array>(&batch, "value")?;
        let fiscal_year = column::<Int32Array>(&batch, "fiscal_year")?;
        let fiscal_period = column::<StringArray>(&batch, "fiscal_period")?;
        let form = column::<StringArray>(&batch, "form")?;
        let filed = column::<Date32Array>(&batch, "filed")?;
        let accession = column::<StringArray>(&batch, "accession")?;
        let frame = column::<StringArray>(&batch, "frame")?;
        for i in 0..batch.num_rows() {
            let filed_on = date(filed.value(i))?;
            if filter.ciks.as_ref().is_some_and(|c| !c.contains(&cik.value(i)))
                || filter.concepts.as_ref().is_some_and(|c| !c.contains(concept.value(i)))
                || filter.as_of.is_some_and(|d| filed_on >= d)
            {
                continue;
            }
            let opt_text = |a: &StringArray| a.is_valid(i).then(|| a.value(i).to_string());
            out.push(Fact {
                cik: cik.value(i),
                taxonomy: taxonomy.value(i).to_string(),
                concept: concept.value(i).to_string(),
                unit: unit.value(i).to_string(),
                start: if start.is_valid(i) { Some(date(start.value(i))?) } else { None },
                end: date(end.value(i))?,
                value: value.value(i),
                fiscal_year: fiscal_year.is_valid(i).then(|| fiscal_year.value(i)),
                fiscal_period: opt_text(fiscal_period),
                form: form.value(i).to_string(),
                filed: filed_on,
                accession: accession.value(i).to_string(),
                frame: opt_text(frame),
            });
        }
    }
    Ok(out)
}

/// Streams a facts file for its [`Stats`].
pub fn summarize(path: &Path) -> Result<Stats> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
    let mut stats = Stats::default();
    let mut companies = HashSet::new();
    for batch in reader {
        let batch = batch?;
        stats.rows += batch.num_rows();
        let cik = column::<UInt32Array>(&batch, "cik")?;
        let filed = column::<Date32Array>(&batch, "filed")?;
        for i in 0..batch.num_rows() {
            companies.insert(cik.value(i));
            stats.last_filed = stats.last_filed.max(Some(date(filed.value(i))?));
        }
    }
    stats.companies = companies.len();
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::parse_company;
    use crate::facts::tests::apple;

    #[test]
    fn round_trips_filters_and_summarizes() {
        let facts = parse_company(apple().as_bytes()).unwrap().facts;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("facts.parquet");
        let mut writer = FactWriter::create(&path).unwrap();
        writer.write(&facts[..2]).unwrap();
        writer.write(&facts[2..]).unwrap();
        let staged = writer.finish().unwrap();
        assert!(!path.exists());
        assert_eq!(staged.stats, Stats { rows: 4, companies: 1, last_filed: NaiveDate::from_ymd_opt(2025, 1, 31) });
        staged.commit().unwrap();

        assert_eq!(read_facts(&path, &FactFilter::default()).unwrap(), facts);
        assert_eq!(summarize(&path).unwrap().rows, 4);

        // As of 2024-12-31 the 10-Q/A restatement wasn't public yet.
        let eps = |as_of: NaiveDate| {
            let filter = FactFilter {
                concepts: Some(HashSet::from(["EarningsPerShareDiluted".to_string()])),
                as_of: Some(as_of),
                ..FactFilter::default()
            };
            read_facts(&path, &filter).unwrap().iter().map(|f| f.value).collect::<Vec<_>>()
        };
        assert_eq!(eps(NaiveDate::from_ymd_opt(2024, 12, 31).unwrap()), vec![0.97]);

        // The restatement was filed 2025-01-31: not usable that day, usable from the next.
        let filed = NaiveDate::from_ymd_opt(2025, 1, 31).unwrap();
        assert_eq!(eps(filed), vec![0.97]);
        assert_eq!(eps(filed + chrono::Days::new(1)), vec![0.97, 0.98]);
        // Same rule as the events: available at midnight New York at the start of the next day.
        let next_midnight = crate::available_at(filed).with_timezone(&chrono_tz::America::New_York);
        assert_eq!(next_midnight.date_naive(), filed + chrono::Days::new(1));
    }

    #[test]
    fn pre_1970_dates_do_not_break_the_file() {
        let mut fact = parse_company(apple().as_bytes()).unwrap().facts.remove(0);
        fact.end = NaiveDate::from_ymd_opt(201, 9, 28).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("facts.parquet");
        let mut writer = FactWriter::create(&path).unwrap();
        writer.write(std::slice::from_ref(&fact)).unwrap();
        writer.finish().unwrap().commit().unwrap();
        assert_eq!(read_facts(&path, &FactFilter::default()).unwrap(), vec![fact]);
    }
}
