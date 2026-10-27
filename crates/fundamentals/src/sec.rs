use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use reqwest::header::{CONTENT_LENGTH, IF_MODIFIED_SINCE, LAST_MODIFIED, RETRY_AFTER};

pub const SEC_URL: &str = "https://www.sec.gov";
/// Nightly bulk file: every filer's XBRL company facts, one `CIK##########.json` per company.
pub const COMPANYFACTS_PATH: &str = "/Archives/edgar/daily-index/xbrl/companyfacts.zip";
pub const TICKERS_PATH: &str = "/files/company_tickers_exchange.json";

#[derive(Debug, PartialEq)]
pub enum Download {
    /// The server's copy is unchanged since `If-Modified-Since`; nothing was written.
    NotModified,
    Saved { last_modified: Option<String>, bytes: u64 },
}

/// Minimal client for sec.gov. The SEC requires every automated request to identify a contact
/// ("Name email@example.com") and blocks clients that don't.
pub struct SecClient {
    http: reqwest::Client,
    base: String,
    pub max_attempts: u32,
    pub retry_base: Duration,
}

impl SecClient {
    pub fn new(base: impl Into<String>, user_agent: &str) -> Result<Self> {
        let user_agent = user_agent.trim();
        if !user_agent.contains('@') || !user_agent.contains(' ') {
            bail!("SEC_USER_AGENT must name you and give a contact email, e.g. \"Jane Doe jane@example.com\"");
        }
        let http = reqwest::Client::builder()
            .user_agent(user_agent)
            .connect_timeout(Duration::from_secs(30))
            // Per-read rather than total, so a slow multi-gigabyte download isn't cut off.
            .read_timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self { http, base: base.into(), max_attempts: 5, retry_base: Duration::from_secs(5) })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Small documents, fetched whole with retries.
    pub async fn get_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let result = async {
                let response = self.http.get(self.url(path)).send().await.map_err(reqwest::Error::without_url)?;
                let response = check_status(response, path)?;
                Ok::<_, anyhow::Error>(response.bytes().await.map_err(reqwest::Error::without_url)?.to_vec())
            }
            .await;
            match result {
                Ok(bytes) => return Ok(bytes),
                Err(e) => self.retry_or_fail(e, attempt, path).await?,
            }
        }
    }

    /// Streams `path` into `dest`, retrying the whole download on failure. With `if_modified_since`, an
    /// unchanged file comes back as [`Download::NotModified`] without a body.
    pub async fn download(&self, path: &str, dest: &Path, if_modified_since: Option<&str>) -> Result<Download> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.download_once(path, dest, if_modified_since).await {
                Ok(outcome) => return Ok(outcome),
                Err(e) => self.retry_or_fail(e, attempt, path).await?,
            }
        }
    }

    async fn download_once(&self, path: &str, dest: &Path, if_modified_since: Option<&str>) -> Result<Download> {
        let mut request = self.http.get(self.url(path));
        if let Some(since) = if_modified_since {
            request = request.header(IF_MODIFIED_SINCE, since);
        }
        let response = request.send().await.map_err(reqwest::Error::without_url)?;
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(Download::NotModified);
        }
        let mut response = check_status(response, path)?;
        let header = |name| response.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
        let last_modified = header(LAST_MODIFIED);
        let expected: Option<u64> = header(CONTENT_LENGTH).and_then(|v| v.parse().ok());

        let mut file = File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
        let mut bytes = 0u64;
        while let Some(chunk) = response.chunk().await.map_err(reqwest::Error::without_url)? {
            file.write_all(&chunk)?;
            bytes += chunk.len() as u64;
        }
        file.sync_all()?;
        if let Some(expected) = expected
            && bytes != expected
        {
            bail!("{path}: got {bytes} bytes, expected {expected}");
        }
        Ok(Download::Saved { last_modified, bytes })
    }

    /// Fails fast on [`Fatal`] responses, honours `Retry-After`, and otherwise (5xx, dropped connections,
    /// short bodies) backs off exponentially, up to `max_attempts`.
    async fn retry_or_fail(&self, err: anyhow::Error, attempt: u32, path: &str) -> Result<()> {
        if err.downcast_ref::<Fatal>().is_some() {
            return Err(err);
        }
        let wait = match err.downcast_ref::<Retryable>() {
            Some(Retryable::After(wait)) => *wait,
            _ => self.retry_base * 2u32.saturating_pow(attempt - 1),
        };
        if attempt >= self.max_attempts {
            return Err(err.context(format!("giving up on {path} after {attempt} attempts")));
        }
        tracing::warn!(attempt, "{err:#}; retrying {path} in {wait:?}");
        tokio::time::sleep(wait).await;
        Ok(())
    }
}

#[derive(Debug)]
enum Retryable {
    /// 429 with the server's requested wait.
    After(Duration),
    /// 5xx.
    Backoff,
}

impl std::fmt::Display for Retryable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("retryable HTTP status")
    }
}

impl std::error::Error for Retryable {}

/// A response that retrying won't fix (e.g. 403 for a missing or rejected user agent).
#[derive(Debug)]
struct Fatal(String);

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Fatal {}

fn check_status(response: reqwest::Response, path: &str) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        let wait = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(Duration::from_secs(60), Duration::from_secs);
        return Err(anyhow::Error::new(Retryable::After(wait)).context(format!("HTTP {status} from {path}")));
    }
    if status.is_server_error() {
        return Err(anyhow::Error::new(Retryable::Backoff).context(format!("HTTP {status} from {path}")));
    }
    let hint = if status == StatusCode::FORBIDDEN { " (the SEC blocks requests without a valid contact user agent)" } else { "" };
    Err(Fatal(format!("HTTP {status} from {path}{hint}")).into())
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const UA: &str = "Test Person test@example.com";

    fn client(server: &MockServer) -> SecClient {
        let mut c = SecClient::new(server.uri(), UA).unwrap();
        c.retry_base = Duration::from_millis(1);
        c
    }

    #[test]
    fn requires_a_contact_user_agent() {
        assert!(SecClient::new("http://x", "stocktrader").is_err());
        assert!(SecClient::new("http://x", "a@b.c").is_err());
        assert!(SecClient::new("http://x", UA).is_ok());
    }

    #[tokio::test]
    async fn downloads_conditionally_with_user_agent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/big.zip"))
            .and(header("user-agent", UA))
            // Exact match: wiremock's header() splits values on commas, which HTTP dates contain.
            .and(|req: &wiremock::Request| {
                req.headers.get("if-modified-since").is_some_and(|v| v == "Mon, 26 Oct 2026 06:00:00 GMT")
            })
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/big.zip"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("last-modified", "Tue, 27 Oct 2026 06:00:00 GMT")
                    .set_body_bytes(vec![7u8; 100_000]),
            )
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("big.zip");
        let c = client(&server);
        assert_eq!(c.download("/big.zip", &dest, Some("Mon, 26 Oct 2026 06:00:00 GMT")).await.unwrap(), Download::NotModified);
        assert!(!dest.exists());

        let saved = c.download("/big.zip", &dest, None).await.unwrap();
        assert_eq!(saved, Download::Saved { last_modified: Some("Tue, 27 Oct 2026 06:00:00 GMT".into()), bytes: 100_000 });
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), 100_000);
    }

    #[tokio::test]
    async fn retries_server_errors_but_not_forbidden() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/t.json")).respond_with(ResponseTemplate::new(503)).up_to_n_times(2).mount(&server).await;
        Mock::given(method("GET")).and(path("/t.json")).respond_with(ResponseTemplate::new(200).set_body_string("{}")).mount(&server).await;
        Mock::given(method("GET")).and(path("/blocked")).respond_with(ResponseTemplate::new(403)).expect(1).mount(&server).await;

        let c = client(&server);
        assert_eq!(c.get_bytes("/t.json").await.unwrap(), b"{}");
        let err = c.get_bytes("/blocked").await.unwrap_err();
        assert!(format!("{err:#}").contains("user agent"));
    }
}
