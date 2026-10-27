use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// One HTTP response body, stored verbatim.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RawRecord {
    pub fetched_at: DateTime<Utc>,
    pub source: String,
    /// Request URL with any credentials removed.
    pub url: String,
    pub status: u16,
    pub body: String,
}

/// Append-only JSON Lines files at `<root>/<source>/<YYYY-MM-DD>.jsonl`, dated by UTC fetch time (not the
/// exchange date). Polls overlap on purpose, so the same item can appear more than once; readers dedupe.
/// Readers should also skip lines that fail to parse: a crash mid-write can leave one torn line, though
/// never more than one, because the next append starts on a fresh line.
///
/// Source cursors live in `<root>/.state/<source>.json`.
#[derive(Clone, Debug)]
pub struct Archive {
    root: PathBuf,
}

impl Archive {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path_for(&self, source: &str, fetched_at: DateTime<Utc>) -> PathBuf {
        self.root.join(source).join(format!("{}.jsonl", fetched_at.format("%Y-%m-%d")))
    }

    /// Synchronous on purpose: with no await point mid-write, cancelling a collector task can never
    /// leave half a batch behind. Each file's lines go out in a single `write_all` and are synced to disk
    /// before returning, so callers may advance cursors afterwards.
    pub fn append(&self, records: &[RawRecord]) -> io::Result<()> {
        let mut batches: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
        for record in records {
            let buf = batches.entry(self.path_for(&record.source, record.fetched_at)).or_default();
            serde_json::to_writer(&mut *buf, record).map_err(io::Error::other)?;
            buf.push(b'\n');
        }
        for (path, mut buf) in batches {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            let mut file = OpenOptions::new().create(true).read(true).append(true).open(&path)?;
            if ends_mid_line(&mut file)? {
                buf.insert(0, b'\n');
            }
            file.write_all(&buf)?;
            file.sync_data()?;
        }
        Ok(())
    }

    fn state_path(&self, source: &str) -> PathBuf {
        self.root.join(".state").join(format!("{source}.json"))
    }

    pub fn load_state<T: DeserializeOwned>(&self, source: &str) -> io::Result<Option<T>> {
        match fs::read(self.state_path(source)) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Written to a temporary file and renamed into place, so a crash leaves either the old or new state.
    pub fn save_state<T: Serialize>(&self, source: &str, state: &T) -> io::Result<()> {
        let path = self.state_path(source);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let mut file = File::create(&tmp)?;
        serde_json::to_writer(&mut file, state).map_err(io::Error::other)?;
        file.sync_data()?;
        fs::rename(tmp, path)
    }
}

/// True if the file is non-empty and its last byte is not a newline, i.e. a previous write was torn.
fn ends_mid_line(file: &mut File) -> io::Result<bool> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(false);
    }
    let mut last = [0u8];
    file.seek(SeekFrom::Start(len - 1))?;
    file.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn record(source: &str, day: u32, body: &str) -> RawRecord {
        RawRecord {
            fetched_at: Utc.with_ymd_and_hms(2026, 10, day, 12, 0, 0).unwrap(),
            source: source.into(),
            url: "https://example.com/x".into(),
            status: 200,
            body: body.into(),
        }
    }

    /// Parses every line that parses, the way archive readers are expected to.
    fn read_records(path: &Path) -> Vec<RawRecord> {
        fs::read_to_string(path).unwrap().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
    }

    #[test]
    fn appends_by_source_and_day() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path());
        let a1 = record("reddit", 27, "{\"a\":1}\nwith newline");
        let a2 = record("reddit", 27, "second");
        let b = record("reddit", 28, "next day");
        let c = record("finnhub", 27, "other source");

        archive.append(&[a1.clone(), b.clone(), c.clone()]).unwrap();
        archive.append(std::slice::from_ref(&a2)).unwrap();

        assert_eq!(read_records(&dir.path().join("reddit/2026-10-27.jsonl")), vec![a1, a2]);
        assert_eq!(read_records(&dir.path().join("reddit/2026-10-28.jsonl")), vec![b]);
        assert_eq!(read_records(&dir.path().join("finnhub/2026-10-27.jsonl")), vec![c]);
    }

    #[test]
    fn torn_line_does_not_corrupt_next_record() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path());
        let first = record("reddit", 27, "first");
        let next = record("reddit", 27, "after crash");
        archive.append(std::slice::from_ref(&first)).unwrap();

        let path = archive.path_for("reddit", first.fetched_at);
        OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"fetched_at\":\"20").unwrap();
        archive.append(std::slice::from_ref(&next)).unwrap();

        assert_eq!(read_records(&path), vec![first, next]);
    }

    #[test]
    fn state_round_trips_and_is_absent_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path());
        assert_eq!(archive.load_state::<Vec<u32>>("alpaca_news").unwrap(), None);
        archive.save_state("alpaca_news", &vec![1u32, 2]).unwrap();
        archive.save_state("alpaca_news", &vec![3u32]).unwrap();
        assert_eq!(archive.load_state::<Vec<u32>>("alpaca_news").unwrap(), Some(vec![3]));
    }
}
