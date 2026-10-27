use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
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

/// Append-only JSON Lines files at `<root>/<source>/<YYYY-MM-DD>.jsonl`, dated by UTC fetch time.
/// Polls overlap on purpose, so the same item can appear more than once; readers dedupe.
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
    /// leave half a batch behind. Each file's lines go out in a single `write_all`.
    pub fn append(&self, records: &[RawRecord]) -> io::Result<()> {
        let mut batches: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
        for record in records {
            let buf = batches.entry(self.path_for(&record.source, record.fetched_at)).or_default();
            serde_json::to_writer(&mut *buf, record).map_err(io::Error::other)?;
            buf.push(b'\n');
        }
        for (path, buf) in batches {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            OpenOptions::new().create(true).append(true).open(&path)?.write_all(&buf)?;
        }
        Ok(())
    }
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

    fn read_lines(path: &Path) -> Vec<RawRecord> {
        fs::read_to_string(path).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
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

        assert_eq!(read_lines(&dir.path().join("reddit/2026-10-27.jsonl")), vec![a1, a2]);
        assert_eq!(read_lines(&dir.path().join("reddit/2026-10-28.jsonl")), vec![b]);
        assert_eq!(read_lines(&dir.path().join("finnhub/2026-10-27.jsonl")), vec![c]);
    }
}
