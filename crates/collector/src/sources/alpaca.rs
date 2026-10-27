use std::io;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::archive::{Archive, RawRecord};
use crate::http::{fetch, secret_header};
use crate::source::{FetchOutcome, Source};

const MAX_PAGES: usize = 20;
/// Each poll re-requests this much before the committed cursor, so late-indexed items aren't missed.
const OVERLAP: TimeDelta = TimeDelta::minutes(10);
/// How far back the very first poll reaches when no cursor has been saved.
const FIRST_LOOKBACK: TimeDelta = TimeDelta::hours(24);
/// Longest catch-up after downtime; anything older is left to a historical backfill.
const MAX_LOOKBACK: TimeDelta = TimeDelta::days(7);

/// Alpaca news (Benzinga), with full article content. Keys go in headers, so URLs are safe to record.
pub struct AlpacaNews {
    base_url: String,
    key_id: String,
    secret_key: String,
    /// Committed: everything created before this has been archived.
    since: Option<DateTime<Utc>>,
    staged: Option<DateTime<Utc>>,
}

impl AlpacaNews {
    pub fn new(base_url: impl Into<String>, key_id: impl Into<String>, secret_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            key_id: key_id.into(),
            secret_key: secret_key.into(),
            since: None,
            staged: None,
        }
    }

    async fn fetch_pages(
        &self,
        http: &reqwest::Client,
        start: &str,
        records: &mut Vec<RawRecord>,
    ) -> anyhow::Result<Option<DateTime<Utc>>> {
        let key_id = secret_header(&self.key_id)?;
        let secret_key = secret_header(&self.secret_key)?;
        let mut page_token: Option<String> = None;
        let mut last_created = None;
        for _ in 0..MAX_PAGES {
            let mut params = vec![("start", start), ("limit", "50"), ("sort", "asc"), ("include_content", "true")];
            if let Some(token) = &page_token {
                params.push(("page_token", token));
            }
            let url = Url::parse_with_params(&format!("{}/v1beta1/news", self.base_url), &params)?;
            let request = http
                .get(url.clone())
                .header("APCA-API-KEY-ID", key_id.clone())
                .header("APCA-API-SECRET-KEY", secret_key.clone());
            let record = fetch(self.name(), request, url.to_string()).await?;
            let page: Page = serde_json::from_str(&record.body).context("parsing Alpaca news page")?;
            records.push(record);
            last_created = page.news.last().map(|n| n.created_at).or(last_created);
            page_token = page.next_page_token.filter(|t| !t.is_empty());
            if page_token.is_none() {
                return Ok(None);
            }
        }
        // Truncated: the caller may only advance as far as what was actually received.
        Ok(Some(last_created.unwrap_or_default()))
    }
}

#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    news: Vec<NewsItem>,
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct NewsItem {
    created_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
struct Cursor {
    since: DateTime<Utc>,
}

impl Source for AlpacaNews {
    fn name(&self) -> &'static str {
        "alpaca_news"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(5 * 60)
    }

    fn max_backoff(&self) -> u32 {
        16
    }

    fn restore(&mut self, archive: &Archive) -> io::Result<()> {
        self.since = archive.load_state::<Cursor>(self.name())?.map(|c| c.since);
        Ok(())
    }

    async fn fetch(&mut self, http: &reqwest::Client) -> FetchOutcome {
        self.staged = None;
        let poll_started = Utc::now();
        let start = self
            .since
            .map_or(poll_started - FIRST_LOOKBACK, |t| t - OVERLAP)
            .max(poll_started - MAX_LOOKBACK);
        if self.since.is_some_and(|t| t < poll_started - MAX_LOOKBACK) {
            tracing::warn!(source = self.name(), "cursor older than {MAX_LOOKBACK}; the gap needs a backfill");
        }
        let start = start.to_rfc3339_opts(SecondsFormat::Secs, true);

        let mut outcome = FetchOutcome::default();
        match self.fetch_pages(http, &start, &mut outcome.records).await {
            Ok(None) => self.staged = Some(poll_started),
            Ok(Some(last_created)) => {
                tracing::warn!(source = self.name(), "stopped after {MAX_PAGES} pages; resuming next poll");
                // `since - OVERLAP` re-covers the boundary; never move backwards.
                self.staged = self.since.max(Some(last_created));
            }
            // Leave the cursor where it was: the next poll retries the same window.
            Err(e) => outcome.errors.push(e),
        }
        outcome
    }

    fn commit(&mut self, archive: &Archive) -> io::Result<()> {
        if let Some(since) = self.staged.take() {
            archive.save_state(self.name(), &Cursor { since })?;
            self.since = Some(since);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn pages_until_no_token_and_commits_only_when_asked() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1beta1/news"))
            .and(header("APCA-API-KEY-ID", "kid"))
            .and(header("APCA-API-SECRET-KEY", "ksecret"))
            .and(query_param("include_content", "true"))
            .and(query_param_is_missing("page_token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"news":[],"next_page_token":"p2"}"#))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1beta1/news"))
            .and(query_param("page_token", "p2"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"news":[],"next_page_token":null}"#))
            .expect(1)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path());
        let mut source = AlpacaNews::new(server.uri(), "kid", "ksecret");
        let outcome = source.fetch(&reqwest::Client::new()).await;
        assert_eq!(outcome.records.len(), 2);
        assert!(outcome.records.iter().all(|r| !r.url.contains("ksecret")));
        assert!(source.since.is_none(), "fetch must only stage the cursor");

        source.commit(&archive).unwrap();
        let mut restored = AlpacaNews::new(server.uri(), "kid", "ksecret");
        restored.restore(&archive).unwrap();
        assert!(restored.since.is_some());
        assert_eq!(restored.since, source.since);
    }

    #[tokio::test]
    async fn failed_poll_does_not_advance_window() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(500)).mount(&server).await;

        let dir = tempfile::tempdir().unwrap();
        let mut source = AlpacaNews::new(server.uri(), "kid", "ksecret");
        let outcome = source.fetch(&reqwest::Client::new()).await;
        assert_eq!(outcome.errors.len(), 1);
        source.commit(&Archive::new(dir.path())).unwrap();
        assert!(source.since.is_none());
    }

    #[tokio::test]
    async fn truncated_poll_advances_only_to_last_item_received() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"news":[{"created_at":"2026-10-20T13:00:00Z"}],"next_page_token":"more"}"#,
            ))
            .expect(MAX_PAGES as u64)
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let mut source = AlpacaNews::new(server.uri(), "kid", "ksecret");
        let outcome = source.fetch(&reqwest::Client::new()).await;
        assert_eq!(outcome.records.len(), MAX_PAGES);
        source.commit(&Archive::new(dir.path())).unwrap();
        assert_eq!(source.since, Some(Utc.with_ymd_and_hms(2026, 10, 20, 13, 0, 0).unwrap()));
    }
}
