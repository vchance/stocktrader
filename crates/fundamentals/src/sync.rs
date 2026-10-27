use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{Days, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::facts::parse_company;
use crate::sec::{COMPANYFACTS_PATH, Download, SecClient, TICKERS_PATH};
use crate::store::{FactWriter, StagedFacts, Stats, summarize};
use crate::tickers::parse_tickers;

/// A new file must keep at least this share of the previous file's rows and companies, unless forced.
const MIN_RETAINED: f64 = 0.9;
/// Companies file with the SEC every business day; a newest filing older than this means stale data.
const MAX_STALENESS_DAYS: u64 = 10;
/// Unparseable company documents tolerated before the run is considered broken.
const MAX_FAILED_SHARE: f64 = 0.01;

pub struct SyncConfig {
    pub root: PathBuf,
    pub today: NaiveDate,
    /// Re-download even if unchanged, and replace the previous file even if the new one is much smaller.
    pub force: bool,
}

#[derive(Debug)]
pub enum SyncReport {
    /// The SEC's bulk file hasn't changed since the last successful sync.
    Unchanged { tickers: usize },
    Updated { tickers: usize, stats: Stats, documents: usize, failed: usize, malformed: usize },
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    last_modified: Option<String>,
}

pub fn facts_path(root: &Path) -> PathBuf {
    root.join("facts.parquet")
}

/// Snapshots the ticker map, then (if the SEC has published a new bulk file) downloads company facts,
/// extracts the curated concepts and replaces `facts.parquet` if the result passes sanity checks.
pub async fn sync(client: &SecClient, cfg: &SyncConfig) -> Result<SyncReport> {
    fs::create_dir_all(&cfg.root)?;
    let lock = OpenOptions::new().create(true).truncate(false).write(true).open(cfg.root.join(".sync.lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => bail!("another fundamentals sync holds {}", cfg.root.display()),
        Err(TryLockError::Error(e)) => return Err(e).context("locking the fundamentals directory"),
    }

    let ticker_json = client.get_bytes(TICKERS_PATH).await?;
    let tickers = parse_tickers(&ticker_json)?.len();
    write_atomic(&cfg.root.join("tickers").join(format!("{}.json", cfg.today)), &ticker_json)?;
    tracing::info!(tickers, "saved ticker map snapshot");

    let state_path = cfg.root.join("state.json");
    let state: State = match fs::read(&state_path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => State::default(),
    };
    let since = (!cfg.force && facts_path(&cfg.root).exists()).then_some(state.last_modified).flatten();

    let zip = cfg.root.join("companyfacts.zip");
    let downloaded = client.download(COMPANYFACTS_PATH, &zip.with_extension("zip.tmp"), since.as_deref()).await?;
    let last_modified = match downloaded {
        Download::NotModified => return Ok(SyncReport::Unchanged { tickers }),
        Download::Saved { last_modified, bytes } => {
            tracing::info!(bytes, "downloaded company facts");
            fs::rename(zip.with_extension("zip.tmp"), &zip)?;
            last_modified
        }
    };

    let (staged, documents, failed, malformed) = extract(&zip, &facts_path(&cfg.root))?;
    if let Err(e) = check(&staged, cfg, documents, failed) {
        let _ = staged.discard();
        // Over a gigabyte, and the next run downloads afresh anyway (state.json was not advanced).
        let _ = fs::remove_file(&zip);
        return Err(e);
    }
    let stats = staged.stats.clone();
    staged.commit()?;
    write_atomic(&state_path, &serde_json::to_vec(&State { last_modified: last_modified.clone() })?)?;
    let manifest = serde_json::json!({
        "synced_on": cfg.today,
        "source_last_modified": last_modified,
        "rows": stats.rows,
        "companies": stats.companies,
        "last_filed": stats.last_filed,
        "documents": documents,
        "failed_documents": failed,
        "malformed_entries": malformed,
    });
    write_atomic(&cfg.root.join("manifest.json"), &serde_json::to_vec_pretty(&manifest)?)?;
    // Over a gigabyte, and the SEC republishes it nightly; the Parquet file is what we keep. The sync has
    // already succeeded, so failing to delete is only worth a warning.
    if let Err(e) = fs::remove_file(&zip) {
        tracing::warn!("could not delete {}: {e}", zip.display());
    }
    Ok(SyncReport::Updated { tickers, stats, documents, failed, malformed })
}

/// Streams every company document in the zip into a staged Parquet file. Returns the staged file and
/// counts of documents, documents that failed to parse, and malformed entries skipped within documents.
fn extract(zip: &Path, out: &Path) -> Result<(StagedFacts, usize, usize, usize)> {
    let mut archive = zip::ZipArchive::new(File::open(zip)?).context("opening company facts zip")?;
    let mut writer = FactWriter::create(out)?;
    let (mut documents, mut failed, mut malformed) = (0, 0, 0);
    let mut buf = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name()?.into_owned();
        if !name.ends_with(".json") {
            continue;
        }
        documents += 1;
        buf.clear();
        entry.read_to_end(&mut buf).with_context(|| format!("reading {name} from zip"))?;
        match parse_company(&buf) {
            Ok(parsed) => {
                malformed += parsed.malformed;
                writer.write(&parsed.facts)?;
            }
            Err(e) => {
                failed += 1;
                tracing::warn!(document = name, "skipping: {e:#}");
            }
        }
        if documents % 2000 == 0 {
            tracing::info!(documents, "extracting");
        }
    }
    Ok((writer.finish()?, documents, failed, malformed))
}

/// Refuses to replace good data with something empty, stale, much smaller, or extracted from a mostly
/// unreadable download.
fn check(file: &StagedFacts, cfg: &SyncConfig, documents: usize, failed: usize) -> Result<()> {
    let s = &file.stats;
    if s.rows == 0 {
        bail!("no facts extracted; keeping the previous file");
    }
    if failed as f64 > (documents as f64 * MAX_FAILED_SHARE).max(10.0) {
        bail!("{failed} of {documents} company documents failed to parse; keeping the previous file");
    }
    if s.last_filed.is_none_or(|d| d + Days::new(MAX_STALENESS_DAYS) < cfg.today) {
        bail!("newest filing {:?} is too far before {}; keeping the previous file", s.last_filed, cfg.today);
    }
    if s.last_filed.is_some_and(|d| d > cfg.today + Days::new(1)) {
        bail!("newest filing {:?} is after {}; check the clock; keeping the previous file", s.last_filed, cfg.today);
    }
    if cfg.force || !file.path().exists() {
        return Ok(());
    }
    let prev = summarize(file.path()).context("reading the previous facts file")?;
    let shrank = |new: usize, old: usize| (new as f64) < old as f64 * MIN_RETAINED;
    if shrank(s.rows, prev.rows) || shrank(s.companies, prev.companies) {
        bail!(
            "new file has {} rows / {} companies vs {} / {} before; keeping the previous file (use --force if intended)",
            s.rows,
            s.companies,
            prev.rows,
            prev.companies
        );
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};
    use std::time::Duration;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::facts::tests::apple;
    use crate::store::{FactFilter, read_facts};

    const TICKERS: &str = r#"{"fields": ["cik", "name", "ticker", "exchange"], "data": [[320193, "Apple Inc.", "AAPL", "Nasdaq"]]}"#;
    const LAST_MODIFIED: &str = "Tue, 27 Oct 2026 06:00:00 GMT";

    fn zip_of(docs: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in docs {
            writer.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    async fn server(zip: Vec<u8>) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(TICKERS_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string(TICKERS))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(COMPANYFACTS_PATH))
            .and(|req: &wiremock::Request| req.headers.get("if-modified-since").is_some_and(|v| v == LAST_MODIFIED))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(COMPANYFACTS_PATH))
            .respond_with(ResponseTemplate::new(200).insert_header("last-modified", LAST_MODIFIED).set_body_bytes(zip))
            .mount(&server)
            .await;
        server
    }

    fn client(server: &MockServer) -> SecClient {
        let mut c = SecClient::new(server.uri(), "Test Person test@example.com").unwrap();
        c.retry_base = Duration::from_millis(1);
        c
    }

    fn config(root: &Path) -> SyncConfig {
        SyncConfig { root: root.to_path_buf(), today: NaiveDate::from_ymd_opt(2025, 2, 5).unwrap(), force: false }
    }

    #[tokio::test]
    async fn extracts_then_skips_unchanged_download() {
        let apple = apple();
        let zip = zip_of(&[("CIK0000320193.json", &apple), ("CIK0000000002.json", "{not json"), ("readme.txt", "x")]);
        let server = server(zip).await;
        let dir = tempfile::tempdir().unwrap();

        match sync(&client(&server), &config(dir.path())).await.unwrap() {
            SyncReport::Updated { tickers, stats, documents, failed, malformed } => {
                assert_eq!((tickers, documents, failed, malformed), (1, 2, 1, 1));
                assert_eq!((stats.rows, stats.companies), (4, 1));
            }
            other => panic!("expected an update, got {other:?}"),
        }
        assert_eq!(read_facts(&facts_path(dir.path()), &FactFilter::default()).unwrap().len(), 4);
        assert!(dir.path().join("tickers/2025-02-05.json").exists());
        assert!(dir.path().join("manifest.json").exists());
        assert!(!dir.path().join("companyfacts.zip").exists(), "zip is deleted after commit");

        assert!(matches!(sync(&client(&server), &config(dir.path())).await.unwrap(), SyncReport::Unchanged { .. }));
        let forced = SyncConfig { force: true, ..config(dir.path()) };
        assert!(matches!(sync(&client(&server), &forced).await.unwrap(), SyncReport::Updated { .. }));
    }

    #[tokio::test]
    async fn rejects_stale_or_unreadable_downloads() {
        let dir = tempfile::tempdir().unwrap();
        let apple = apple();

        let late = SyncConfig { today: NaiveDate::from_ymd_opt(2025, 6, 1).unwrap(), ..config(dir.path()) };
        let err = sync(&client(&server(zip_of(&[("CIK0000320193.json", &apple)])).await), &late).await.unwrap_err();
        assert!(format!("{err:#}").contains("too far before"));
        assert!(!facts_path(dir.path()).exists());

        let broken: Vec<(String, String)> = (0..20).map(|i| (format!("CIK{i:010}.json"), "{".to_string())).collect();
        let mut docs: Vec<(&str, &str)> = broken.iter().map(|(n, b)| (n.as_str(), b.as_str())).collect();
        docs.push(("CIK0000320193.json", &apple));
        let err = sync(&client(&server(zip_of(&docs)).await), &config(dir.path())).await.unwrap_err();
        assert!(format!("{err:#}").contains("failed to parse"));
        assert!(!facts_path(dir.path()).exists());
    }
}
