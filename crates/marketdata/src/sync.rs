use std::collections::VecDeque;
use std::path::PathBuf;

use anyhow::{Result, bail};
use chrono::NaiveDate;
use reqwest::StatusCode;

use crate::alpaca::{Adjustment, AlpacaClient, HttpError};
use crate::store::BarWriter;
use crate::universe::{save_snapshot, universe};

pub struct SyncConfig {
    pub root: PathBuf,
    pub start: NaiveDate,
    pub end: NaiveDate,
    /// Snapshot file date for the asset lists.
    pub today: NaiveDate,
    /// `sip` (all US venues) or `iex` (one venue, far lower volume).
    pub feed: String,
    pub batch_size: usize,
}

#[derive(Debug)]
pub struct SyncReport {
    pub symbols: usize,
    pub rows: Vec<(Adjustment, usize)>,
    /// Symbols the API rejected individually; everything else was downloaded.
    pub skipped: Vec<String>,
}

pub fn bars_path(root: &std::path::Path, adjustment: Adjustment) -> PathBuf {
    root.join(format!("bars_daily_{adjustment}.parquet"))
}

/// A request rejected because of what it asked for (e.g. a symbol the API won't accept), as opposed to
/// auth, subscription or rate-limit problems that would fail for every symbol.
fn rejected_request(err: &anyhow::Error) -> bool {
    err.downcast_ref::<HttpError>().is_some_and(|e| {
        matches!(e.status, StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::UNPROCESSABLE_ENTITY)
    })
}

/// Snapshots the asset lists, then rewrites the full adjusted and raw daily history for the universe.
pub async fn sync(client: &mut AlpacaClient, cfg: &SyncConfig) -> Result<SyncReport> {
    let mut assets = Vec::new();
    for status in ["active", "inactive"] {
        let (body, mut listed) = client.assets(status).await?;
        let path = save_snapshot(&cfg.root.join("assets"), cfg.today, status, &body)?;
        tracing::info!(status, count = listed.len(), path = %path.display(), "saved asset snapshot");
        assets.append(&mut listed);
    }
    let symbols = universe(&assets);
    if symbols.is_empty() {
        bail!("asset lists contained no listed US equities");
    }
    tracing::info!(symbols = symbols.len(), "universe");

    let mut report = SyncReport { symbols: symbols.len(), rows: Vec::new(), skipped: Vec::new() };
    for adjustment in [Adjustment::All, Adjustment::Raw] {
        let mut writer = BarWriter::create(&bars_path(&cfg.root, adjustment))?;
        let mut queue: VecDeque<Vec<String>> = symbols.chunks(cfg.batch_size.max(1)).map(<[_]>::to_vec).collect();
        let mut done = 0;
        let mut batches = 0;
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
                        tracing::warn!(symbol, "skipping symbol: {e:#}");
                        if adjustment == Adjustment::All {
                            report.skipped.push(symbol.clone());
                        }
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
        if report.skipped.len() == symbols.len() {
            bail!("every symbol was rejected; check the feed and date range");
        }
        let rows = writer.finish()?;
        tracing::info!(%adjustment, rows, "bars written");
        report.rows.push((adjustment, rows));
    }
    Ok(report)
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

    /// Returns one bar per requested symbol, or 400 if the request includes BAD.
    fn bars_responder(req: &Request) -> ResponseTemplate {
        let symbols = req.url.query_pairs().find(|(k, _)| k == "symbols").map(|(_, v)| v.into_owned()).unwrap();
        if symbols.split(',').any(|s| s == "BAD") {
            return ResponseTemplate::new(400).set_body_string("invalid symbol: BAD");
        }
        let bars: Vec<String> = symbols
            .split(',')
            .map(|s| format!(r#""{s}":[{{"t":"2026-10-01T04:00:00Z","o":1,"h":2,"l":0.5,"c":1.5,"v":10}}]"#))
            .collect();
        ResponseTemplate::new(200).set_body_string(format!(r#"{{"bars":{{{}}},"next_page_token":null}}"#, bars.join(",")))
    }

    #[tokio::test]
    async fn snapshots_assets_and_isolates_rejected_symbols() {
        let server = MockServer::start().await;
        let listed = |status: &str, symbols: &[&str]| {
            let items: Vec<String> = symbols
                .iter()
                .map(|s| format!(r#"{{"symbol":"{s}","exchange":"NYSE","status":"{status}","class":"us_equity"}}"#))
                .collect();
            format!("[{}]", items.join(","))
        };
        Mock::given(method("GET"))
            .and(path("/v2/assets"))
            .and(query_param("status", "active"))
            .respond_with(ResponseTemplate::new(200).set_body_string(listed("active", &["AAA", "BAD", "CCC"])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/assets"))
            .and(query_param("status", "inactive"))
            .respond_with(ResponseTemplate::new(200).set_body_string(listed("inactive", &["DDD"])))
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/v2/stocks/bars")).respond_with(bars_responder).mount(&server).await;

        let dir = tempfile::tempdir().unwrap();
        let mut client = AlpacaClient::new(reqwest::Client::new(), server.uri(), server.uri(), "k", "s").unwrap();
        client.min_interval = Duration::ZERO;
        let cfg = SyncConfig {
            root: dir.path().to_path_buf(),
            start: day(1),
            end: day(2),
            today: day(27),
            feed: "sip".into(),
            batch_size: 4,
        };

        let report = sync(&mut client, &cfg).await.unwrap();
        assert_eq!(report.symbols, 4);
        assert_eq!(report.skipped, vec!["BAD"]);
        assert_eq!(report.rows, vec![(Adjustment::All, 3), (Adjustment::Raw, 3)]);
        assert!(dir.path().join("assets/2026-10-27-inactive.json").exists());

        let bars = read_bars(&bars_path(dir.path(), Adjustment::Raw), &BarFilter::default()).unwrap();
        let symbols: Vec<&str> = bars.iter().map(|b| b.symbol.as_str()).collect();
        assert_eq!(symbols, vec!["AAA", "CCC", "DDD"]);
    }

    #[tokio::test]
    async fn subscription_errors_fail_instead_of_bisecting() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/assets"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[{"symbol":"AAA","exchange":"NYSE","status":"active","class":"us_equity"},
                    {"symbol":"BBB","exchange":"NYSE","status":"active","class":"us_equity"}]"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/stocks/bars"))
            .respond_with(ResponseTemplate::new(403).set_body_string("subscription does not permit querying recent SIP data"))
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let mut client = AlpacaClient::new(reqwest::Client::new(), server.uri(), server.uri(), "k", "s").unwrap();
        client.min_interval = Duration::ZERO;
        let cfg = SyncConfig {
            root: dir.path().to_path_buf(),
            start: day(1),
            end: day(2),
            today: day(27),
            feed: "sip".into(),
            batch_size: 100,
        };
        let err = sync(&mut client, &cfg).await.unwrap_err();
        assert!(format!("{err:#}").contains("403"));
        assert!(!bars_path(dir.path(), Adjustment::All).exists());
    }
}
