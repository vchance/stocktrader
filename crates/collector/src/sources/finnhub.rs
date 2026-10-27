use std::time::Duration;

use crate::archive::RawRecord;
use crate::http::fetch;
use crate::source::Source;

/// Finnhub market news. The free tier only serves recent items, so this needs continuous polling.
pub struct FinnhubNews {
    base_url: String,
    token: String,
}

impl FinnhubNews {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self { base_url: base_url.into(), token: token.into() }
    }
}

impl Source for FinnhubNews {
    fn name(&self) -> &'static str {
        "finnhub_news"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(10 * 60)
    }

    async fn fetch(&mut self, http: &reqwest::Client) -> anyhow::Result<Vec<RawRecord>> {
        let mut records = Vec::new();
        for category in ["general", "merger"] {
            let url = format!("{}/api/v1/news?category={category}", self.base_url);
            // Finnhub takes the key as a query parameter; it is added to the request only, never to `url`.
            let request = http.get(&url).query(&[("token", &self.token)]);
            records.push(fetch(self.name(), request, url).await?);
        }
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn sends_token_but_never_records_it() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/news"))
            .and(query_param("token", "s3cret"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .expect(2)
            .mount(&server)
            .await;

        let records = FinnhubNews::new(server.uri(), "s3cret").fetch(&reqwest::Client::new()).await.unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|r| !r.url.contains("s3cret")));
    }

    #[tokio::test]
    async fn errors_do_not_leak_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .mount(&server)
            .await;

        let err = FinnhubNews::new(server.uri(), "s3cret").fetch(&reqwest::Client::new()).await.unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("429"));
        assert!(!message.contains("s3cret"));
    }
}
