use std::time::Duration;

use crate::http::{fetch, secret_header};
use crate::source::{FetchOutcome, Source};

/// Finnhub market news. The free tier only serves the latest ~100 items per category, so this needs
/// continuous polling.
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

    fn max_backoff(&self) -> u32 {
        2
    }

    async fn fetch(&mut self, http: &reqwest::Client) -> FetchOutcome {
        let mut outcome = FetchOutcome::default();
        let token = match secret_header(&self.token) {
            Ok(token) => token,
            Err(e) => {
                outcome.errors.push(e.context("FINNHUB_API_KEY is not a valid header value"));
                return outcome;
            }
        };
        for category in ["general", "merger"] {
            let url = format!("{}/api/v1/news?category={category}", self.base_url);
            let request = http.get(&url).header("X-Finnhub-Token", token.clone());
            match fetch(self.name(), request, url).await {
                Ok(record) => outcome.records.push(record),
                Err(e) => outcome.errors.push(e),
            }
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn sends_token_as_header_and_never_records_it() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/news"))
            .and(header("X-Finnhub-Token", "s3cret"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .expect(2)
            .mount(&server)
            .await;

        let outcome = FinnhubNews::new(server.uri(), "s3cret").fetch(&reqwest::Client::new()).await;
        assert_eq!(outcome.records.len(), 2);
        assert!(outcome.records.iter().all(|r| !r.url.contains("s3cret")));
    }

    #[tokio::test]
    async fn one_failed_category_keeps_the_other() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("category", "general"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param("category", "merger"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .mount(&server)
            .await;

        let outcome = FinnhubNews::new(server.uri(), "s3cret").fetch(&reqwest::Client::new()).await;
        assert_eq!(outcome.records.len(), 1);
        let message = format!("{:#}", outcome.errors[0]);
        assert!(message.contains("429"));
        assert!(!message.contains("s3cret"));
    }
}
