//! Archive upkeep: compressing finished day files and checking that every source is still producing.

use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::Deserialize;

/// Day files are named `<YYYY-MM-DD>.jsonl`, or `.jsonl.gz` once compacted.
fn file_date(path: &Path) -> Option<NaiveDate> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".jsonl.gz").or_else(|| name.strip_suffix(".jsonl"))?;
    NaiveDate::parse_from_str(stem, "%Y-%m-%d").ok()
}

/// Source directories under the archive root; dot-directories (`.state`) are not sources.
fn source_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for entry in fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && !entry.file_name().to_string_lossy().starts_with('.') {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// Gzips every `.jsonl` day file dated before `before`, returning the compressed files.
///
/// The collector only appends to the file for the current UTC date, so callers pass a date at least a day
/// back to stay clear of midnight. Each file is compressed to a temporary file, verified by decompressing
/// and comparing byte for byte, renamed into place, and only then is the original deleted. An interrupted
/// run can leave both versions, which readers dedupe like any other overlap; the next run redoes it.
pub fn compact(root: &Path, before: NaiveDate) -> Result<Vec<PathBuf>> {
    let mut done = Vec::new();
    for dir in source_dirs(root)? {
        let mut files: Vec<PathBuf> = fs::read_dir(&dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .filter(|p| file_date(p).is_some_and(|d| d < before))
            .collect();
        files.sort();
        for path in files {
            compress_file(&path).with_context(|| format!("compressing {}", path.display()))?;
            done.push(path.with_extension("jsonl.gz"));
        }
    }
    Ok(done)
}

fn compress_file(path: &Path) -> Result<()> {
    let gz = path.with_extension("jsonl.gz");
    let tmp = path.with_extension("jsonl.gz.tmp");

    let mut encoder = GzEncoder::new(File::create(&tmp)?, Compression::default());
    io::copy(&mut File::open(path)?, &mut encoder)?;
    encoder.finish()?.sync_all()?;

    if !same_contents(&mut File::open(path)?, &mut GzDecoder::new(File::open(&tmp)?))? {
        let _ = fs::remove_file(&tmp);
        bail!("compressed copy does not match the original");
    }
    fs::rename(&tmp, &gz)?;
    fs::remove_file(path)?;
    Ok(())
}

fn same_contents(a: &mut impl Read, b: &mut impl Read) -> io::Result<bool> {
    let (mut buf_a, mut buf_b) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let n = read_full(a, &mut buf_a)?;
        let m = read_full(b, &mut buf_b)?;
        if n != m || buf_a[..n] != buf_b[..m] {
            return Ok(false);
        }
        if n == 0 {
            return Ok(true);
        }
    }
}

/// Fills `buf` unless the reader ends first; returns the bytes read.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Stale,
    Missing,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub name: String,
    pub status: Status,
    /// Newest data seen; `None` if there is none.
    pub last: Option<DateTime<Utc>>,
    pub detail: String,
}

/// How long a source may go without new records before it counts as stale: four polling intervals, but
/// never less than half an hour.
pub fn stale_after(interval: Duration) -> Duration {
    (interval * 4).max(Duration::from_secs(30 * 60))
}

/// Checks the newest archived record of one source.
pub fn source_check(root: &Path, name: &str, stale_after: Duration, now: DateTime<Utc>) -> Result<Check> {
    let last = newest_record(&root.join(name))?;
    Ok(classify(name, last, stale_after, now))
}

fn classify(name: &str, last: Option<DateTime<Utc>>, limit: Duration, now: DateTime<Utc>) -> Check {
    let limit_text = humanize(limit);
    match last {
        None => Check { name: name.into(), status: Status::Missing, last, detail: "no records yet".into() },
        Some(t) => {
            let age = (now - t).to_std().unwrap_or_default();
            let status = if age > limit { Status::Stale } else { Status::Ok };
            Check { name: name.into(), status, last, detail: format!("last record {} ago (limit {limit_text})", humanize(age)) }
        }
    }
}

fn humanize(d: Duration) -> String {
    let mins = d.as_secs() / 60;
    match mins {
        0..60 => format!("{mins}m"),
        60..2880 => format!("{}h{:02}m", mins / 60, mins % 60),
        _ => format!("{}d", mins / 1440),
    }
}

#[derive(Deserialize)]
struct Fetched {
    fetched_at: DateTime<Utc>,
}

/// Time of the newest record in a source directory. Only the newest day file is consulted. If that file
/// is already compressed, the source has written nothing since, so the end of that day is a safe bound.
fn newest_record(dir: &Path) -> Result<Option<DateTime<Utc>>> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(None);
    };
    let newest = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter_map(|p| file_date(&p).map(|d| (d, p)))
        .max_by(|(da, pa), (db, pb)| (da, pa.extension() == Some("jsonl".as_ref())).cmp(&(db, pb.extension() == Some("jsonl".as_ref()))));
    match newest {
        None => Ok(None),
        Some((_, path)) if path.extension().is_some_and(|e| e == "jsonl") => last_fetched_at(&path),
        Some((date, _)) => Ok(Some(date.and_time(NaiveTime::from_hms_opt(23, 59, 59).expect("valid")).and_utc())),
    }
}

/// `fetched_at` of the last parseable line, reading backwards so large day files aren't read whole.
fn last_fetched_at(path: &Path) -> Result<Option<DateTime<Utc>>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut window: u64 = 64 * 1024;
    loop {
        let start = len.saturating_sub(window);
        file.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::with_capacity((len - start) as usize);
        BufReader::new(&mut file).read_to_end(&mut buf)?;
        let mut lines: Vec<&[u8]> = buf.split(|b| *b == b'\n').collect();
        if start > 0 {
            // The first piece may be the tail of a line that starts before the window.
            lines.remove(0);
        }
        for line in lines.iter().rev().filter(|l| !l.is_empty()) {
            if let Ok(f) = serde_json::from_slice::<Fetched>(line) {
                return Ok(Some(f.fetched_at));
            }
        }
        if start == 0 {
            return Ok(None);
        }
        window = window.saturating_mul(4);
    }
}

#[derive(Deserialize)]
struct Manifest {
    synced_on: NaiveDate,
}

/// Checks the market data manifest. `Ok(None)` if no sync has ever completed.
pub fn marketdata_check(dir: &Path, max_days: u64, today: NaiveDate) -> Result<Option<Check>> {
    let path = dir.join("manifest.json");
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let manifest: Manifest = serde_json::from_slice(&bytes).context("parsing marketdata manifest")?;
    let age = (today - manifest.synced_on).num_days();
    let status = if age > max_days as i64 { Status::Stale } else { Status::Ok };
    Ok(Some(Check {
        name: "marketdata".into(),
        status,
        last: Some(manifest.synced_on.and_time(NaiveTime::MIN).and_utc()),
        detail: format!("last sync {} ({age}d ago, limit {max_days}d)", manifest.synced_on),
    }))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use chrono::TimeZone;

    use super::*;
    use crate::archive::{Archive, RawRecord};

    fn record(source: &str, at: DateTime<Utc>, body: &str) -> RawRecord {
        RawRecord { fetched_at: at, source: source.into(), url: "u".into(), status: 200, body: body.into() }
    }

    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, day, hour, 0, 0).unwrap()
    }

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
    }

    #[test]
    fn compacts_only_older_days_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path());
        archive.append(&[record("reddit", at(25, 1), "old"), record("reddit", at(26, 1), "yesterday")]).unwrap();
        archive.append(&[record("reddit", at(27, 1), "today")]).unwrap();
        fs::create_dir_all(dir.path().join(".state")).unwrap();
        fs::write(dir.path().join(".state/2026-10-01.jsonl"), "not a source").unwrap();
        let original = fs::read(dir.path().join("reddit/2026-10-25.jsonl")).unwrap();

        let done = compact(dir.path(), day(26)).unwrap();
        assert_eq!(done, vec![dir.path().join("reddit/2026-10-25.jsonl.gz")]);
        assert!(!dir.path().join("reddit/2026-10-25.jsonl").exists());
        assert!(dir.path().join("reddit/2026-10-26.jsonl").exists());
        assert!(dir.path().join(".state/2026-10-01.jsonl").exists());

        let mut restored = Vec::new();
        GzDecoder::new(File::open(&done[0]).unwrap()).read_to_end(&mut restored).unwrap();
        assert_eq!(restored, original);

        assert!(compact(dir.path(), day(26)).unwrap().is_empty(), "second run is a no-op");
    }

    #[test]
    fn finds_newest_record_even_with_large_lines_and_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path());
        let big = "x".repeat(300 * 1024);
        archive.append(&[record("alpaca_news", at(27, 1), &big), record("alpaca_news", at(27, 2), &big)]).unwrap();
        let path = archive.path_for("alpaca_news", at(27, 2));
        fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"fetched_at\":\"20").unwrap();

        assert_eq!(newest_record(&dir.path().join("alpaca_news")).unwrap(), Some(at(27, 2)));
    }

    #[test]
    fn classifies_fresh_stale_and_missing_sources() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path());
        archive.append(&[record("apewisdom", at(27, 10), "{}")]).unwrap();
        archive.append(&[record("reddit", at(25, 10), "{}")]).unwrap();
        compact(dir.path(), day(26)).unwrap();

        let limit = Duration::from_secs(3600);
        let now = at(27, 10) + chrono::TimeDelta::minutes(20);
        assert_eq!(source_check(dir.path(), "apewisdom", limit, now).unwrap().status, Status::Ok);
        let reddit = source_check(dir.path(), "reddit", limit, now).unwrap();
        assert_eq!(reddit.status, Status::Stale, "only a compressed old file exists");
        assert_eq!(source_check(dir.path(), "finnhub_news", limit, now).unwrap().status, Status::Missing);
        assert_eq!(stale_after(Duration::from_secs(120)), Duration::from_secs(1800));
    }

    #[test]
    fn checks_marketdata_manifest() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(marketdata_check(dir.path(), 4, day(27)).unwrap(), None);
        fs::write(dir.path().join("manifest.json"), r#"{"synced_on":"2026-10-22","files":{}}"#).unwrap();
        assert_eq!(marketdata_check(dir.path(), 4, day(27)).unwrap().unwrap().status, Status::Stale);
        assert_eq!(marketdata_check(dir.path(), 5, day(27)).unwrap().unwrap().status, Status::Ok);
    }
}
