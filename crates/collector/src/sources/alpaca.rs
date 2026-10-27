use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use reqwest::Url;
use serde::Deserialize;

use crate::archive::RawRecord;
use crate::http::fetch;
use crate::source::Source;

const MAX_PAGES: usize = 20;
/// Each poll re-requests this much before the previous poll's start, so late-indexed items aren't missed.
const OVERLAP: TimeDelta = TimeDelta::minutes(10);
/// How far back the first poll after startup reaches.
const FIRST_LOOKBACK: TimeDelta = TimeDelta::hours(1);

/// Alpaca news (Benzinga). Keys go in headers, so URLs are safe to record.
pub struct AlpacaNews {
    base_url: String,
    key_id: String,
    secret_key: String,
    last_poll: Option<DateTime<Utc>>,
}

impl AlpacaNews {
    pub fn new(base_url: impl Into<String>, key_id: impl Into<String>, secret_key: impl Into<String>) -> Self {
        Self { base_url: base_url.into(), key_id: key_id.into(), secret_key: secret_key.into(), last_poll: None }
    }
}

#[derive(Deserialize)]
struct Page {
    next_page_token: Option<String>,
}

impl Source for AlpacaNews {
    fn name(&self) -> &'static str {
        "alpaca_news"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(5 * 60)
    }

    async fn fetch(&mut self, http: &reqwest::Client) -> anyhow::Result<Vec<RawRecord>> {
        let poll_started = Utc::now();
        let start = self.last_poll.map_or(poll_started - FIRST_LOOKBACK, |t| t - OVERLAP);
        let start = start.to_rfc3339_opts(SecondsFormat::Secs, true);

        let mut records = Vec::new();
        let mut page_token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut params = vec![("start", start.as_str()), ("limit", "50"), ("sort", "asc")];
            if let Some(token) = &page_token {
                params.push(("page_token", token));
            }
            let url = Url::parse_with_params(&format!("{}/v1beta1/news", self.base_url), &params)?;
            let request = http
                .get(url.clone())
                .header("APCA-API-KEY-ID", &self.key_id)
                .header("APCA-API-SECRET-KEY", &self.secret_key);
            let record = fetch(self.name(), request, url.to_string()).await?;
            page_token = serde_json::from_str::<Page>(&record.body)
                .context("parsing Alpaca news page")?
                .next_page_token
                .filter(|t| !t.is_empty());
            records.push(record);
            if page_token.is_none() {
                break;
            }
        }
        if page_token.is_some() {
            tracing::warn!(source = self.name(), "stopped after {MAX_PAGES} pages; some items may be missing");
        }
        // Only advance after a fully successful poll, so a failed poll is retried from the same point.
        self.last_poll = Some(poll_started);
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn pages_until_no_token_and_sends_keys_as_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1beta1/news"))
            .and(header("APCA-API-KEY-ID", "kid"))
            .and(header("APCA-API-SECRET-KEY", "ksecret"))
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

        let mut source = AlpacaNews::new(server.uri(), "kid", "ksecret");
        let records = source.fetch(&reqwest::Client::new()).await.unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|r| !r.url.contains("ksecret")));
        assert!(source.last_poll.is_some());
    }

    #[tokio::test]
    async fn failed_poll_does_not_advance_window() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(500)).mount(&server).await;

        let mut source = AlpacaNews::new(server.uri(), "kid", "ksecret");
        assert!(source.fetch(&reqwest::Client::new()).await.is_err());
        assert!(source.last_poll.is_none());
    }
}
