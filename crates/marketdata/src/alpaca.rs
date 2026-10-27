use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::America::New_York;
use reqwest::header::{HeaderValue, RETRY_AFTER};
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use crate::store::DailyBar;

pub const DATA_URL: &str = "https://data.alpaca.markets";
/// Paper-trading keys only work against the paper endpoint.
pub const PAPER_TRADING_URL: &str = "https://paper-api.alpaca.markets";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Adjustment {
    /// Prices as traded.
    Raw,
    /// Adjusted for splits and dividends.
    All,
}

impl Adjustment {
    pub fn as_str(self) -> &'static str {
        match self {
            Adjustment::Raw => "raw",
            Adjustment::All => "all",
        }
    }
}

impl fmt::Display for Adjustment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A non-retryable HTTP error response (4xx other than 429).
#[derive(Debug)]
pub struct HttpError {
    pub status: StatusCode,
    pub detail: String,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP {} from {}", self.status, self.detail)
    }
}

impl std::error::Error for HttpError {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Asset {
    pub symbol: String,
    pub exchange: String,
    pub status: String,
    #[serde(rename = "class")]
    pub asset_class: String,
    #[serde(default)]
    pub tradable: bool,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Deserialize)]
struct BarsPage {
    #[serde(default)]
    bars: Option<HashMap<String, Vec<RawBar>>>,
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct RawBar {
    t: DateTime<Utc>,
    o: f64,
    h: f64,
    l: f64,
    c: f64,
    v: f64,
    n: Option<u64>,
    vw: Option<f64>,
}

/// Alpaca REST client for the endpoints this crate needs. Requests are paced to stay under the account's
/// rate limit and retried on 429 (honouring `Retry-After`) and 5xx.
pub struct AlpacaClient {
    http: reqwest::Client,
    data_url: String,
    trading_url: String,
    key_id: HeaderValue,
    secret_key: HeaderValue,
    /// Minimum spacing between requests. 300 ms keeps under the free plan's 200 requests per minute.
    pub min_interval: Duration,
    pub max_attempts: u32,
    /// Base delay for exponential backoff on 5xx and transport errors.
    pub retry_base: Duration,
    /// Wait after a 429 that carries no `Retry-After`.
    pub rate_limit_wait: Duration,
    last_request: Option<Instant>,
}

fn secret(value: &str) -> Result<HeaderValue> {
    let mut header = HeaderValue::from_str(value).context("API key is not a valid header value")?;
    header.set_sensitive(true);
    Ok(header)
}

impl AlpacaClient {
    pub fn new(
        http: reqwest::Client,
        data_url: impl Into<String>,
        trading_url: impl Into<String>,
        key_id: &str,
        secret_key: &str,
    ) -> Result<Self> {
        Ok(Self {
            http,
            data_url: data_url.into(),
            trading_url: trading_url.into(),
            key_id: secret(key_id)?,
            secret_key: secret(secret_key)?,
            min_interval: Duration::from_millis(300),
            max_attempts: 6,
            retry_base: Duration::from_secs(2),
            rate_limit_wait: Duration::from_secs(30),
            last_request: None,
        })
    }

    async fn pace(&mut self) {
        if let Some(last) = self.last_request {
            tokio::time::sleep_until(last + self.min_interval).await;
        }
        self.last_request = Some(Instant::now());
    }

    async fn get(&mut self, url: Url) -> Result<String> {
        let shown = format!("{}{}", url.path(), url.query().map(|q| format!("?{q}")).unwrap_or_default());
        let mut attempt = 0;
        loop {
            attempt += 1;
            self.pace().await;
            let response = self
                .http
                .get(url.clone())
                .header("APCA-API-KEY-ID", self.key_id.clone())
                .header("APCA-API-SECRET-KEY", self.secret_key.clone())
                .send()
                .await
                .map_err(reqwest::Error::without_url);

            let (wait, problem) = match response {
                Ok(response) if response.status().is_success() => {
                    return response.text().await.map_err(reqwest::Error::without_url).context("reading body");
                }
                Ok(response) => {
                    let status = response.status();
                    let retry_after = response
                        .headers()
                        .get(RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(Duration::from_secs);
                    let body: String = response.text().await.unwrap_or_default().chars().take(200).collect();
                    if status == StatusCode::TOO_MANY_REQUESTS {
                        (retry_after.unwrap_or(self.rate_limit_wait), format!("HTTP {status}"))
                    } else if status.is_server_error() {
                        (self.retry_base * 2u32.saturating_pow(attempt - 1), format!("HTTP {status}: {body}"))
                    } else {
                        return Err(HttpError { status, detail: format!("{shown}: {body}") }.into());
                    }
                }
                Err(e) => (self.retry_base * 2u32.saturating_pow(attempt - 1), e.to_string()),
            };
            if attempt >= self.max_attempts {
                bail!("giving up on {shown} after {attempt} attempts: {problem}");
            }
            tracing::warn!(attempt, "{problem} from {shown}; retrying in {wait:?}");
            tokio::time::sleep(wait).await;
        }
    }

    /// US equity assets with the given status (`active` or `inactive`). Returns the raw response body too,
    /// so it can be archived as-is.
    pub async fn assets(&mut self, status: &str) -> Result<(String, Vec<Asset>)> {
        let url = Url::parse_with_params(
            &format!("{}/v2/assets", self.trading_url),
            &[("status", status), ("asset_class", "us_equity")],
        )?;
        let body = self.get(url).await?;
        let assets = serde_json::from_str(&body).context("parsing assets")?;
        Ok((body, assets))
    }

    /// Daily bars for `symbols` from `start` through `end` (inclusive). Each page is handed to `on_page`
    /// as soon as it arrives, sorted by symbol then date, so callers can stream to disk.
    pub async fn daily_bars(
        &mut self,
        symbols: &[String],
        start: NaiveDate,
        end: NaiveDate,
        adjustment: Adjustment,
        feed: &str,
        mut on_page: impl FnMut(Vec<DailyBar>) -> Result<()>,
    ) -> Result<()> {
        let symbols = symbols.join(",");
        let start = start.to_string();
        let end = end.to_string();
        let mut page_token: Option<String> = None;
        loop {
            let mut params = vec![
                ("symbols", symbols.as_str()),
                ("timeframe", "1Day"),
                ("start", start.as_str()),
                ("end", end.as_str()),
                ("adjustment", adjustment.as_str()),
                ("feed", feed),
                ("limit", "10000"),
            ];
            if let Some(token) = &page_token {
                params.push(("page_token", token));
            }
            let url = Url::parse_with_params(&format!("{}/v2/stocks/bars", self.data_url), &params)?;
            let page: BarsPage = serde_json::from_str(&self.get(url).await?).context("parsing bars page")?;

            let mut bars: Vec<DailyBar> = page
                .bars
                .unwrap_or_default()
                .into_iter()
                .flat_map(|(symbol, raw)| {
                    raw.into_iter().map(move |b| DailyBar {
                        symbol: symbol.clone(),
                        date: b.t.with_timezone(&New_York).date_naive(),
                        open: b.o,
                        high: b.h,
                        low: b.l,
                        close: b.c,
                        volume: b.v,
                        trade_count: b.n,
                        vwap: b.vw,
                    })
                })
                .collect();
            bars.sort_by(|a, b| (&a.symbol, a.date).cmp(&(&b.symbol, b.date)));
            on_page(bars)?;

            page_token = page.next_page_token.filter(|t| !t.is_empty());
            if page_token.is_none() {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn client(server: &MockServer) -> AlpacaClient {
        let mut c = AlpacaClient::new(reqwest::Client::new(), server.uri(), server.uri(), "kid", "ksecret").unwrap();
        c.min_interval = Duration::ZERO;
        c.retry_base = Duration::from_millis(1);
        c.rate_limit_wait = Duration::from_millis(1);
        c
    }

    fn date(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
    }

    #[tokio::test]
    async fn pages_bars_and_uses_new_york_session_dates() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/stocks/bars"))
            .and(header("APCA-API-KEY-ID", "kid"))
            .and(query_param("symbols", "AAPL,MSFT"))
            .and(query_param("adjustment", "all"))
            .and(query_param_is_missing("page_token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"bars":{"MSFT":[{"t":"2026-10-01T04:00:00Z","o":1,"h":2,"l":0.5,"c":1.5,"v":10,"n":3,"vw":1.2}],
                            "AAPL":[{"t":"2026-10-01T04:00:00Z","o":1,"h":2,"l":0.5,"c":1.5,"v":10}]},
                   "next_page_token":"p2"}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/stocks/bars"))
            .and(query_param("page_token", "p2"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"bars":{"AAPL":[{"t":"2026-10-02T04:00:00Z","o":1,"h":2,"l":0.5,"c":1.5,"v":10}]},"next_page_token":null}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let mut pages = Vec::new();
        client(&server)
            .daily_bars(&["AAPL".into(), "MSFT".into()], date(1), date(2), Adjustment::All, "sip", |bars| {
                pages.push(bars);
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(pages.len(), 2);
        let first: Vec<(&str, NaiveDate)> = pages[0].iter().map(|b| (b.symbol.as_str(), b.date)).collect();
        assert_eq!(first, vec![("AAPL", date(1)), ("MSFT", date(1))]);
        assert_eq!(pages[0][1].trade_count, Some(3));
        assert_eq!(pages[1][0].date, date(2));
    }

    #[tokio::test]
    async fn retries_rate_limits_and_server_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(503)).up_to_n_times(1).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/v2/assets"))
            .and(query_param("status", "inactive"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[{"symbol":"OLD","exchange":"NYSE","status":"inactive","class":"us_equity","tradable":false}]"#,
            ))
            .mount(&server)
            .await;

        let (_, assets) = client(&server).assets("inactive").await.unwrap();
        assert_eq!(assets[0].symbol, "OLD");
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn client_errors_fail_fast_without_leaking_keys() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).set_body_string("subscription does not permit"))
            .expect(1)
            .mount(&server)
            .await;

        let err = client(&server).assets("active").await.unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("403") && message.contains("subscription"));
        assert!(!message.contains("ksecret"));
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
        let mut c = client(&server);
        c.max_attempts = 3;
        assert!(c.assets("active").await.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }
}
