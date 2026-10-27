use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{Days, NaiveDate};
use reqwest::StatusCode;

use crate::alpaca::{Adjustment, AlpacaClient, HttpError};
use crate::store::{BarWriter, StagedFile, Stats, summarize};
use crate::universe::{EXCHANGES, save_snapshot, universe};

const FEEDS: &[&str] = &["sip", "iex"];
/// A new file must keep at least this share of the previous file's rows and symbols, unless forced.
const MIN_RETAINED: f64 = 0.9;
/// The newest bar must be at most this many days before `end` (a weekend plus a holiday, with slack).
const MAX_STALENESS_DAYS: u64 = 5;

pub struct SyncConfig {
    pub root: PathBuf,
    pub start: NaiveDate,
    /// Last session to include.
    pub end: NaiveDate,
    /// Snapshot file date for the asset lists.
    pub today: NaiveDate,
    /// `sip` (all US venues) or `iex` (one venue, far lower volume).
    pub feed: String,
    pub batch_size: usize,
    /// Abort once more symbols than this are rejected in one pass: beyond a handful, the request itself
    /// is wrong, and isolating symbols one by one would only burn hours of rate limit.
    pub max_rejected: usize,
    /// Replace the previous files even if the new ones are much smaller.
    pub force: bool,
}

#[derive(Debug)]
pub struct SyncReport {
    pub symbols: usize,
    pub files: Vec<(Adjustment, Stats)>,
    /// Symbols the API rejected individually, per pass; everything else was downloaded.
    pub skipped: Vec<(Adjustment, String)>,
}

pub fn bars_path(root: &Path, adjustment: Adjustment) -> PathBuf {
    root.join(format!("bars_daily_{adjustment}.parquet"))
}

/// A request rejected because of what it asked for (e.g. a symbol the API won't accept), as opposed to
/// auth, subscription or rate-limit problems that would fail for every symbol.
fn rejected_request(err: &anyhow::Error) -> bool {
    err.downcast_ref::<HttpError>().is_some_and(|e| {
        matches!(e.status, StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::UNPROCESSABLE_ENTITY)
    })
}

/// Snapshots the asset lists, downloads the full adjusted and raw daily history for the universe, and
/// replaces both Parquet files together only if both downloads completed and pass sanity checks.
pub async fn sync(client: &mut AlpacaClient, cfg: &SyncConfig) -> Result<SyncReport> {
    if !FEEDS.contains(&cfg.feed.as_str()) {
        bail!("unknown feed {:?}; expected one of {FEEDS:?}", cfg.feed);
    }
    if cfg.start > cfg.end {
        bail!("start {} is after end {}", cfg.start, cfg.end);
    }
    fs::create_dir_all(&cfg.root)?;
    let lock = OpenOptions::new().create(true).truncate(false).write(true).open(cfg.root.join(".sync.lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => bail!("another marketdata sync holds {}", cfg.root.display()),
        Err(TryLockError::Error(e)) => return Err(e).context("locking the market data directory"),
    }

    let mut assets = Vec::new();
    for status in ["active", "inactive"] {
        let (body, mut listed) = client.assets(status).await?;
        let path = save_snapshot(&cfg.root.join("assets"), cfg.today, status, &body)?;
        tracing::info!(status, count = listed.len(), path = %path.display(), "saved asset snapshot");
        assets.append(&mut listed);
    }
    let mut excluded: BTreeMap<&str, usize> = BTreeMap::new();
    for a in assets.iter().filter(|a| !EXCHANGES.contains(&a.exchange.as_str())) {
        *excluded.entry(a.exchange.as_str()).or_default() += 1;
    }
    tracing::info!(?excluded, "assets outside the traded exchanges");
    let not_tickers =
        assets.iter().filter(|a| EXCHANGES.contains(&a.exchange.as_str()) && !crate::universe::is_ticker(&a.symbol)).count();
    tracing::info!(not_tickers, "listed identifiers that aren't tickers (CUSIP-style, renamed delistings) skipped");
    let symbols = universe(&assets);
    if symbols.is_empty() {
        bail!("asset lists contained no listed US equities");
    }
    tracing::info!(symbols = symbols.len(), "universe");

    let mut report = SyncReport { symbols: symbols.len(), files: Vec::new(), skipped: Vec::new() };
    let mut staged: Vec<StagedFile> = Vec::new();
    let mut outcome = Ok(());
    for adjustment in [Adjustment::All, Adjustment::Raw] {
        match download(client, cfg, &symbols, adjustment, &mut report.skipped).await {
            Ok(file) => {
                report.files.push((adjustment, file.stats.clone()));
                staged.push(file);
            }
            Err(e) => {
                outcome = Err(e);
                break;
            }
        }
    }
    if outcome.is_ok() {
        outcome = staged.iter().try_for_each(|file| check(file, cfg));
    }
    if let Err(e) = outcome {
        for file in staged {
            let _ = file.discard();
        }
        // A pass that failed mid-download dropped its writer, leaving a partial temporary file.
        for adjustment in [Adjustment::All, Adjustment::Raw] {
            let _ = fs::remove_file(bars_path(&cfg.root, adjustment).with_extension("parquet.tmp"));
        }
        return Err(e);
    }

    for file in staged {
        file.commit()?;
    }
    write_manifest(cfg, &report)?;
    Ok(report)
}

async fn download(
    client: &mut AlpacaClient,
    cfg: &SyncConfig,
    symbols: &[String],
    adjustment: Adjustment,
    skipped: &mut Vec<(Adjustment, String)>,
) -> Result<StagedFile> {
    let mut writer = BarWriter::create(&bars_path(&cfg.root, adjustment))?;
    let mut queue: VecDeque<Vec<String>> = symbols.chunks(cfg.batch_size.max(1)).map(<[_]>::to_vec).collect();
    let (mut done, mut batches, mut rejected) = (0, 0, 0);
    while let Some(batch) = queue.pop_front() {
        let mut pages = 0;
        let result = client
            .daily_bars(&batch, cfg.start, cfg.end, adjustment, &cfg.feed, |bars| {
                pages += 1;
                writer.write(&bars)
            })
            .await;
        match result {
            Ok(()) => done += batch.len(),
            // Bisect to isolate the symbol the API rejects. Only safe before any page was written,
            // otherwise retrying would duplicate rows.
            Err(e) if pages == 0 && rejected_request(&e) => {
                if let [symbol] = batch.as_slice() {
                    rejected += 1;
                    if rejected > cfg.max_rejected {
                        return Err(e.context(format!(
                            "more than {} symbols rejected; the request itself is probably wrong",
                            cfg.max_rejected
                        )));
                    }
                    tracing::warn!(symbol, %adjustment, "skipping symbol: {e:#}");
                    skipped.push((adjustment, symbol.clone()));
                    done += 1;
                } else {
                    let (left, right) = batch.split_at(batch.len() / 2);
                    queue.push_front(right.to_vec());
                    queue.push_front(left.to_vec());
                }
            }
            Err(e) => return Err(e.context(format!("downloading {adjustment} bars"))),
        }
        batches += 1;
        if batches % 20 == 0 {
            tracing::info!(%adjustment, done, total = symbols.len(), "progress");
        }
    }
    let file = writer.finish()?;
    tracing::info!(%adjustment, rows = file.stats.rows, symbols = file.stats.symbols, "bars staged");
    Ok(file)
}

/// Refuses to replace good data with something empty, stale or much smaller.
fn check(file: &StagedFile, cfg: &SyncConfig) -> Result<()> {
    let name = file.path().display();
    let s = &file.stats;
    if s.rows == 0 {
        bail!("{name}: no bars downloaded; keeping the previous file");
    }
    if s.last.is_none_or(|last| last + Days::new(MAX_STALENESS_DAYS) < cfg.end) {
        bail!("{name}: newest bar {:?} is too far before {}; keeping the previous file", s.last, cfg.end);
    }
    if cfg.force || !file.path().exists() {
        return Ok(());
    }
    let prev = summarize(file.path()).with_context(|| format!("reading previous {name}"))?;
    let shrank = |new: usize, old: usize| (new as f64) < old as f64 * MIN_RETAINED;
    if shrank(s.rows, prev.rows) || shrank(s.symbols, prev.symbols) {
        bail!(
            "{name}: new file has {} rows / {} symbols vs {} / {} before; keeping the previous file \
             (pass --force if the shrink is intended)",
            s.rows,
            s.symbols,
            prev.rows,
            prev.symbols
        );
    }
    Ok(())
}

/// Records what the committed pair of files contains, so consumers can tell they belong together.
fn write_manifest(cfg: &SyncConfig, report: &SyncReport) -> Result<()> {
    let files: serde_json::Map<String, serde_json::Value> = report
        .files
        .iter()
        .map(|(adjustment, s)| {
            let name = format!("bars_daily_{adjustment}.parquet");
            (name, serde_json::json!({ "rows": s.rows, "symbols": s.symbols, "last": s.last }))
        })
        .collect();
    let manifest = serde_json::json!({
        "synced_on": cfg.today,
        "start": cfg.start,
        "end": cfg.end,
        "feed": cfg.feed,
        "files": files,
    });
    let path = cfg.root.join("manifest.json");
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&manifest)?)?;
    fs::rename(tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::store::{BarFilter, read_bars};

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
    }

    /// One bar per requested symbol, or 400 if the request includes a symbol starting with BAD.
    fn bars_responder(req: &Request) -> ResponseTemplate {
        let symbols = req.url.query_pairs().find(|(k, _)| k == "symbols").map(|(_, v)| v.into_owned()).unwrap();
        if symbols.split(',').any(|s| s.starts_with("BAD")) {
            return ResponseTemplate::new(400).set_body_string("invalid symbol");
        }
        let bars: Vec<String> = symbols
            .split(',')
            .map(|s| format!(r#""{s}":[{{"t":"2026-10-01T04:00:00Z","o":1,"h":2,"l":0.5,"c":1.5,"v":10}}]"#))
            .collect();
        ResponseTemplate::new(200).set_body_string(format!(r#"{{"bars":{{{}}},"next_page_token":null}}"#, bars.join(",")))
    }

    fn listed(status: &str, symbols: &[&str]) -> String {
        let items: Vec<String> = symbols
            .iter()
            .map(|s| format!(r#"{{"symbol":"{s}","exchange":"NYSE","status":"{status}","class":"us_equity"}}"#))
            .collect();
        format!("[{}]", items.join(","))
    }

    async fn server(active: &[&str], inactive: &[&str]) -> MockServer {
        let server = MockServer::start().await;
        for (status, symbols) in [("active", active), ("inactive", inactive)] {
            Mock::given(method("GET"))
                .and(path("/v2/assets"))
                .and(query_param("status", status))
                .respond_with(ResponseTemplate::new(200).set_body_string(listed(status, symbols)))
                .mount(&server)
                .await;
        }
        Mock::given(method("GET")).and(path("/v2/stocks/bars")).respond_with(bars_responder).mount(&server).await;
        server
    }

    fn client(server: &MockServer) -> AlpacaClient {
        let mut c = AlpacaClient::new(reqwest::Client::new(), server.uri(), server.uri(), "k", "s").unwrap();
        c.min_interval = Duration::ZERO;
        c
    }

    fn config(root: &Path) -> SyncConfig {
        SyncConfig {
            root: root.to_path_buf(),
            start: day(1),
            end: day(2),
            today: day(27),
            feed: "sip".into(),
            batch_size: 4,
            max_rejected: 50,
            force: false,
        }
    }

    #[tokio::test]
    async fn snapshots_assets_and_isolates_rejected_symbols() {
        let server = server(&["AAA", "BADA", "CCC"], &["DDD"]).await;
        let dir = tempfile::tempdir().unwrap();

        let report = sync(&mut client(&server), &config(dir.path())).await.unwrap();
        assert_eq!(report.symbols, 4);
        assert_eq!(report.skipped, vec![(Adjustment::All, "BADA".into()), (Adjustment::Raw, "BADA".into())]);
        assert!(report.files.iter().all(|(_, s)| s.rows == 3 && s.symbols == 3));
        assert!(dir.path().join("assets/2026-10-27-inactive.json").exists());
        assert!(dir.path().join("manifest.json").exists());

        let bars = read_bars(&bars_path(dir.path(), Adjustment::Raw), &BarFilter::default()).unwrap();
        let symbols: Vec<&str> = bars.iter().map(|b| b.symbol.as_str()).collect();
        assert_eq!(symbols, vec!["AAA", "CCC", "DDD"]);
    }

    #[tokio::test]
    async fn too_many_rejections_abort_and_keep_previous_files() {
        let dir = tempfile::tempdir().unwrap();
        let good = server(&["AAA", "BBB"], &[]).await;
        sync(&mut client(&good), &config(dir.path())).await.unwrap();

        let bad = server(&["AAA", "BADA", "BADB", "BADC"], &[]).await;
        let cfg = SyncConfig { max_rejected: 2, ..config(dir.path()) };
        let err = sync(&mut client(&bad), &cfg).await.unwrap_err();
        assert!(format!("{err:#}").contains("more than 2 symbols rejected"));
        assert!(!bars_path(dir.path(), Adjustment::All).with_extension("parquet.tmp").exists());

        let bars = read_bars(&bars_path(dir.path(), Adjustment::All), &BarFilter::default()).unwrap();
        assert_eq!(bars.len(), 2, "previous file must survive");
    }

    #[tokio::test]
    async fn refuses_to_shrink_unless_forced() {
        let dir = tempfile::tempdir().unwrap();
        let symbols: Vec<String> = (0..20u8).map(|i| format!("S{}", char::from(b'A' + i))).collect();
        let refs: Vec<&str> = symbols.iter().map(String::as_str).collect();
        sync(&mut client(&server(&refs, &[]).await), &config(dir.path())).await.unwrap();

        let smaller = server(&refs[..10], &[]).await;
        let err = sync(&mut client(&smaller), &config(dir.path())).await.unwrap_err();
        assert!(format!("{err:#}").contains("keeping the previous file"));
        let raw = read_bars(&bars_path(dir.path(), Adjustment::Raw), &BarFilter::default()).unwrap();
        assert_eq!(raw.len(), 20, "neither file may be replaced");

        let forced = SyncConfig { force: true, ..config(dir.path()) };
        sync(&mut client(&smaller), &forced).await.unwrap();
        assert_eq!(read_bars(&bars_path(dir.path(), Adjustment::All), &BarFilter::default()).unwrap().len(), 10);
    }

    #[tokio::test]
    async fn stale_data_is_not_committed() {
        let dir = tempfile::tempdir().unwrap();
        let server = server(&["AAA"], &[]).await;
        // Bars are dated Oct 1; claiming the sync runs through Oct 20 makes them stale.
        let cfg = SyncConfig { end: day(20), ..config(dir.path()) };
        let err = sync(&mut client(&server), &cfg).await.unwrap_err();
        assert!(format!("{err:#}").contains("too far before"));
        assert!(!bars_path(dir.path(), Adjustment::All).exists());
    }

    #[tokio::test]
    async fn validates_inputs_and_holds_a_lock() {
        let dir = tempfile::tempdir().unwrap();
        let server = server(&["AAA"], &[]).await;

        let bad_feed = SyncConfig { feed: "sipp".into(), ..config(dir.path()) };
        assert!(sync(&mut client(&server), &bad_feed).await.is_err());
        let backwards = SyncConfig { start: day(3), ..config(dir.path()) };
        assert!(sync(&mut client(&server), &backwards).await.is_err());
        assert!(server.received_requests().await.unwrap().is_empty(), "validation must precede any request");

        let held = OpenOptions::new().create(true).truncate(false).write(true).open(dir.path().join(".sync.lock")).unwrap();
        held.lock().unwrap();
        let err = sync(&mut client(&server), &config(dir.path())).await.unwrap_err();
        assert!(format!("{err:#}").contains("another marketdata sync"));
    }

    #[tokio::test]
    async fn subscription_errors_fail_instead_of_bisecting() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/assets"))
            .respond_with(ResponseTemplate::new(200).set_body_string(listed("active", &["AAA", "BBB"])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/stocks/bars"))
            .respond_with(ResponseTemplate::new(403).set_body_string("subscription does not permit querying recent SIP data"))
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let err = sync(&mut client(&server), &SyncConfig { batch_size: 100, ..config(dir.path()) }).await.unwrap_err();
        assert!(format!("{err:#}").contains("403"));
        assert!(!bars_path(dir.path(), Adjustment::All).exists());
    }
}
