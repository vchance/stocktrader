//! The Alpaca trading endpoints the paper runner needs. Only the paper endpoint is accepted: this client
//! cannot be pointed at a live-money account.

use std::fmt;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, FixedOffset, NaiveDate};
use chrono_tz::America::New_York;
use reqwest::header::HeaderValue;
use reqwest::{Method, StatusCode, Url};
use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const PAPER_URL: &str = "https://paper-api.alpaca.markets";

#[derive(Clone, Debug, Deserialize)]
pub struct Account {
    pub status: String,
    pub currency: String,
    pub cash: Decimal,
    pub equity: Decimal,
    /// Equity at the previous session's close.
    pub last_equity: Decimal,
    #[serde(default)]
    pub trading_blocked: bool,
    #[serde(default)]
    pub account_blocked: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Position {
    pub symbol: String,
    /// Signed: negative for shorts.
    pub qty: Decimal,
    pub avg_entry_price: Decimal,
    #[serde(default)]
    pub current_price: Option<Decimal>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Order {
    pub id: String,
    pub client_order_id: String,
    pub symbol: String,
    pub side: String,
    #[serde(rename = "type")]
    pub order_type: String,
    /// `None` for orders placed by dollar amount (`notional`).
    #[serde(default)]
    pub qty: Option<Decimal>,
    #[serde(default)]
    pub filled_qty: Decimal,
    #[serde(default)]
    pub notional: Option<Decimal>,
    #[serde(default)]
    pub limit_price: Option<Decimal>,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct MarketClock {
    pub timestamp: DateTime<FixedOffset>,
    pub is_open: bool,
    pub next_open: DateTime<FixedOffset>,
}

/// One trading session; days missing from the calendar are weekends and market holidays.
#[derive(Clone, Debug, Deserialize)]
pub struct Session {
    pub date: NaiveDate,
}

/// The session orders placed now would trade in, and the session before it, whose closing bars the plan
/// must be built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sessions {
    pub today: NaiveDate,
    pub previous: NaiveDate,
}

/// `None` unless today (New York) is a session that hasn't closed yet: before the open, or during it.
/// After the close, orders would queue for the next session while being sized on data a day stale.
pub fn sessions(clock: &MarketClock, calendar: &[Session]) -> Result<Option<Sessions>> {
    let ny = |t: DateTime<FixedOffset>| t.with_timezone(&New_York).date_naive();
    let today = ny(clock.timestamp);
    if !clock.is_open && ny(clock.next_open) != today {
        return Ok(None);
    }
    let Some(previous) = calendar.iter().map(|s| s.date).filter(|d| *d < today).max() else {
        bail!("the calendar has no session before {today}");
    };
    Ok(Some(Sessions { today, previous }))
}

/// A whole-share market order good for the day.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NewOrder {
    pub symbol: String,
    pub qty: Decimal,
    pub side: &'static str,
    #[serde(rename = "type")]
    pub order_type: &'static str,
    pub time_in_force: &'static str,
    /// Alpaca rejects a second order with the same id, which makes resubmitting safe.
    pub client_order_id: String,
}

#[derive(Debug)]
pub enum Submitted {
    Accepted(Box<Order>),
    /// An order with this `client_order_id` already exists (an earlier run, or a retry after a lost response).
    Duplicate,
}

#[derive(Debug)]
pub struct HttpError {
    pub status: StatusCode,
    pub detail: String,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP {}: {}", self.status, self.detail)
    }
}

impl std::error::Error for HttpError {}

pub struct TradingClient {
    http: reqwest::Client,
    base: Url,
    key_id: HeaderValue,
    secret_key: HeaderValue,
    pub max_attempts: u32,
    pub retry_base: Duration,
}

fn secret(value: &str) -> Result<HeaderValue> {
    let mut header = HeaderValue::from_str(value).context("API key is not a valid header value")?;
    header.set_sensitive(true);
    Ok(header)
}

/// The paper endpoint, or a loopback address (for tests and local mocks). Anything else, including the
/// live endpoint, is refused.
fn check_base_url(base: &str) -> Result<Url> {
    let url = Url::parse(base).with_context(|| format!("invalid trading URL {base}"))?;
    let paper = url.scheme() == "https" && url.host_str() == Some("paper-api.alpaca.markets");
    let loopback = url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if !(paper || loopback) || !url.path().trim_end_matches('/').is_empty() {
        bail!("refusing trading URL {base}: only {PAPER_URL} is allowed");
    }
    Ok(url)
}

impl TradingClient {
    pub fn new(http: reqwest::Client, base_url: &str, key_id: &str, secret_key: &str) -> Result<Self> {
        Ok(Self {
            http,
            base: check_base_url(base_url)?,
            key_id: secret(key_id)?,
            secret_key: secret(secret_key)?,
            max_attempts: 4,
            retry_base: Duration::from_secs(2),
        })
    }

    /// Sends a request, retrying 429, 5xx and transport errors. Retrying an order POST is safe because a
    /// duplicate `client_order_id` is rejected rather than filled twice. Other errors return [`HttpError`].
    async fn send<B: Serialize>(&self, method: Method, path: &str, query: &[(&str, &str)], body: Option<&B>) -> Result<String> {
        let mut url = self.base.join(path)?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut request = self
                .http
                .request(method.clone(), url.clone())
                .header("APCA-API-KEY-ID", self.key_id.clone())
                .header("APCA-API-SECRET-KEY", self.secret_key.clone());
            if let Some(body) = body {
                request = request.json(body);
            }
            let problem = match request.send().await.map_err(reqwest::Error::without_url) {
                Ok(response) => {
                    let status = response.status();
                    let text = response.text().await.unwrap_or_default();
                    if status.is_success() {
                        return Ok(text);
                    }
                    let detail: String = text.chars().take(300).collect();
                    if status != StatusCode::TOO_MANY_REQUESTS && !status.is_server_error() {
                        return Err(HttpError { status, detail: format!("{method} {path}: {detail}") }.into());
                    }
                    format!("HTTP {status}: {detail}")
                }
                Err(e) => e.to_string(),
            };
            if attempt >= self.max_attempts {
                bail!("giving up on {method} {path} after {attempt} attempts: {problem}");
            }
            let wait = self.retry_base * 2u32.saturating_pow(attempt - 1);
            tracing::warn!(attempt, "{problem} from {method} {path}; retrying in {wait:?}");
            tokio::time::sleep(wait).await;
        }
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        let body = self.send::<()>(Method::GET, path, query, None).await?;
        serde_json::from_str(&body).with_context(|| format!("parsing {path}"))
    }

    pub async fn account(&self) -> Result<Account> {
        self.get("/v2/account", &[]).await
    }

    pub async fn positions(&self) -> Result<Vec<Position>> {
        self.get("/v2/positions", &[]).await
    }

    /// Orders not yet filled, cancelled or expired.
    pub async fn open_orders(&self) -> Result<Vec<Order>> {
        self.get("/v2/orders", &[("status", "open"), ("limit", "500")]).await
    }

    pub async fn clock(&self) -> Result<MarketClock> {
        self.get("/v2/clock", &[]).await
    }

    /// Sessions from `start` through `end`, inclusive.
    pub async fn calendar(&self, start: NaiveDate, end: NaiveDate) -> Result<Vec<Session>> {
        let (start, end) = (start.to_string(), end.to_string());
        self.get("/v2/calendar", &[("start", &start), ("end", &end)]).await
    }

    pub async fn submit(&self, order: &NewOrder) -> Result<Submitted> {
        match self.send(Method::POST, "/v2/orders", &[], Some(order)).await {
            Ok(body) => Ok(Submitted::Accepted(Box::new(serde_json::from_str(&body).context("parsing order")?))),
            Err(e) => match e.downcast_ref::<HttpError>() {
                Some(h) if h.status == StatusCode::UNPROCESSABLE_ENTITY && h.detail.contains("client_order_id must be unique") => {
                    Ok(Submitted::Duplicate)
                }
                _ => Err(e),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn client(server: &MockServer) -> TradingClient {
        let mut c = TradingClient::new(reqwest::Client::new(), &server.uri(), "kid", "ksecret").unwrap();
        c.retry_base = Duration::from_millis(1);
        c
    }

    fn order() -> NewOrder {
        NewOrder {
            symbol: "AAPL".into(),
            qty: dec!(10),
            side: "buy",
            order_type: "market",
            time_in_force: "day",
            client_order_id: "st-2026-10-27-AAPL-buy".into(),
        }
    }

    #[test]
    fn only_the_paper_endpoint_is_allowed() {
        let ok = |u: &str| TradingClient::new(reqwest::Client::new(), u, "k", "s").is_ok();
        assert!(ok(PAPER_URL));
        assert!(ok("https://paper-api.alpaca.markets/"));
        assert!(ok("http://127.0.0.1:8080"));
        assert!(!ok("https://api.alpaca.markets"));
        assert!(!ok("http://paper-api.alpaca.markets"));
        assert!(!ok("https://paper-api.alpaca.markets.evil.example"));
        assert!(!ok("https://paper-api.alpaca.markets/v2"));
    }

    #[tokio::test]
    async fn reads_account_positions_and_orders() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/account"))
            .and(header("APCA-API-KEY-ID", "kid"))
            .and(header("APCA-API-SECRET-KEY", "ksecret"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"id":"x","status":"ACTIVE","currency":"USD","cash":"5000.5","equity":"10000","last_equity":"9900",
                    "trading_blocked":false,"account_blocked":false,"buying_power":"20000"}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/positions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[{"symbol":"MSFT","qty":"-3","avg_entry_price":"400.1","current_price":"410","side":"short"}]"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/orders"))
            .and(query_param("status", "open"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[{"id":"o1","client_order_id":"c1","symbol":"AAPL","side":"buy","type":"limit","qty":"10",
                     "filled_qty":"4","notional":null,"limit_price":"150","status":"partially_filled"}]"#,
            ))
            .mount(&server)
            .await;

        let c = client(&server);
        let account = c.account().await.unwrap();
        assert_eq!((account.cash, account.last_equity), (dec!(5000.5), dec!(9900)));
        let positions = c.positions().await.unwrap();
        assert_eq!(positions[0].qty, dec!(-3));
        let orders = c.open_orders().await.unwrap();
        assert_eq!((orders[0].qty, orders[0].filled_qty, orders[0].limit_price), (Some(dec!(10)), dec!(4), Some(dec!(150))));
    }

    #[tokio::test]
    async fn reads_clock_and_calendar() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/clock"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"timestamp":"2026-10-27T08:45:01.123-04:00","is_open":false,
                    "next_open":"2026-10-27T09:30:00-04:00","next_close":"2026-10-27T16:00:00-04:00"}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/calendar"))
            .and(query_param("start", "2026-10-20"))
            .and(query_param("end", "2026-10-27"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[{"date":"2026-10-23","open":"09:30","close":"16:00"},{"date":"2026-10-26","open":"09:30","close":"16:00"}]"#,
            ))
            .mount(&server)
            .await;

        let c = client(&server);
        let clock = c.clock().await.unwrap();
        assert!(!clock.is_open);
        let day = |d| NaiveDate::from_ymd_opt(2026, 10, d).unwrap();
        let sessions = c.calendar(day(20), day(27)).await.unwrap();
        assert_eq!(sessions.iter().map(|s| s.date).collect::<Vec<_>>(), vec![day(23), day(26)]);
    }

    #[test]
    fn trades_only_before_or_during_a_session() {
        let t = |s: &str| DateTime::parse_from_rfc3339(s).unwrap();
        let day = |d| NaiveDate::from_ymd_opt(2026, 10, d).unwrap();
        // Fri 23rd and Mon 26th are sessions; Tue 27th is today.
        let calendar = [Session { date: day(23) }, Session { date: day(26) }, Session { date: day(27) }];
        let clock = |now: &str, is_open, next_open: &str| MarketClock { timestamp: t(now), is_open, next_open: t(next_open) };

        let before_open = clock("2026-10-27T08:45:00-04:00", false, "2026-10-27T09:30:00-04:00");
        assert_eq!(sessions(&before_open, &calendar).unwrap(), Some(Sessions { today: day(27), previous: day(26) }));
        let during = clock("2026-10-27T11:00:00-04:00", true, "2026-10-28T09:30:00-04:00");
        assert_eq!(sessions(&during, &calendar).unwrap().map(|s| s.previous), Some(day(26)));
        let after_close = clock("2026-10-27T17:00:00-04:00", false, "2026-10-28T09:30:00-04:00");
        assert_eq!(sessions(&after_close, &calendar).unwrap(), None);
        // Saturday morning: the next open is Monday.
        let weekend = clock("2026-10-24T08:45:00-04:00", false, "2026-10-26T09:30:00-04:00");
        assert_eq!(sessions(&weekend, &calendar).unwrap(), None);
        // A UTC timestamp late in the New York evening still belongs to that New York day.
        let late_utc = clock("2026-10-28T01:00:00Z", false, "2026-10-28T13:30:00Z");
        assert_eq!(sessions(&late_utc, &calendar).unwrap(), None);
        assert!(sessions(&before_open, &calendar[2..]).is_err());
    }

    #[tokio::test]
    async fn submits_market_day_orders_and_recognises_duplicates() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/orders"))
            .and(body_json(serde_json::json!({
                "symbol": "AAPL", "qty": "10", "side": "buy", "type": "market", "time_in_force": "day",
                "client_order_id": "st-2026-10-27-AAPL-buy"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"id":"o9","client_order_id":"st-2026-10-27-AAPL-buy","symbol":"AAPL","side":"buy","type":"market",
                    "qty":"10","filled_qty":"0","status":"accepted"}"#,
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/orders"))
            .respond_with(
                ResponseTemplate::new(422)
                    .set_body_string(r#"{"code":40010001,"message":"client_order_id must be unique"}"#),
            )
            .mount(&server)
            .await;

        let c = client(&server);
        assert!(matches!(c.submit(&order()).await.unwrap(), Submitted::Accepted(o) if o.id == "o9"));
        assert!(matches!(c.submit(&order()).await.unwrap(), Submitted::Duplicate));
    }

    #[tokio::test]
    async fn other_client_order_id_errors_are_failures() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(422)
                    .set_body_string(r#"{"message":"client_order_id must be no more than 128 characters"}"#),
            )
            .mount(&server)
            .await;
        assert!(client(&server).submit(&order()).await.is_err());
    }

    #[tokio::test]
    async fn retries_server_errors_but_not_rejections() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(503)).up_to_n_times(1).mount(&server).await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_string(r#"{"message":"insufficient buying power"}"#))
            .mount(&server)
            .await;

        let err = client(&server).submit(&order()).await.unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("403") && message.contains("buying power"), "{message}");
        assert!(!message.contains("ksecret"));
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }
}
