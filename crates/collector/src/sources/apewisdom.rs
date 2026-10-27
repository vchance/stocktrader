use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;

use crate::http::fetch;
use crate::source::{FetchOutcome, Source};

/// Hard cap on pages per poll; the all-stocks feed is currently about 7 pages of 100.
const MAX_PAGES: u32 = 20;

/// ApeWisdom ticker mention counts aggregated from Reddit and 4chan. No key needed. Only the current
/// snapshot is served, so history exists only if we keep polling it. Pages are fetched one after another,
/// so rankings can shift slightly between pages of one snapshot.
pub struct ApeWisdom {
    base_url: String,
    filter: String,
}

impl ApeWisdom {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self { base_url: base_url.into(), filter: "all-stocks".into() }
    }
}

#[derive(Deserialize)]
struct Page {
    pages: u32,
}

impl Source for ApeWisdom {
    fn name(&self) -> &'static str {
        "apewisdom"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(15 * 60)
    }

    async fn fetch(&mut self, http: &reqwest::Client) -> FetchOutcome {
        let mut outcome = FetchOutcome::default();
        let mut pages = 1;
        let mut page = 1;
        while page <= pages.min(MAX_PAGES) {
            let url = format!("{}/api/v1.0/filter/{}/page/{page}", self.base_url, self.filter);
            match fetch(self.name(), http.get(&url), url).await {
                Ok(record) => {
                    if page == 1 {
                        match serde_json::from_str::<Page>(&record.body).context("parsing ApeWisdom page count") {
                            Ok(p) => pages = p.pages,
                            Err(e) => outcome.errors.push(e),
                        }
                    }
                    outcome.records.push(record);
                }
                Err(e) => {
                    outcome.errors.push(e);
                    break;
                }
            }
            page += 1;
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    async fn mount_page(server: &MockServer, page: u32, status: u16) {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1.0/filter/all-stocks/page/{page}")))
            .respond_with(ResponseTemplate::new(status).set_body_string(r#"{"count":250,"pages":3,"results":[]}"#))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn follows_page_count_from_first_page() {
        let server = MockServer::start().await;
        for page in 1..=3 {
            mount_page(&server, page, 200).await;
        }
        let outcome = ApeWisdom::new(server.uri()).fetch(&reqwest::Client::new()).await;
        assert_eq!(outcome.records.len(), 3);
        assert!(outcome.errors.is_empty());
        assert!(outcome.records[2].url.ends_with("/page/3"));
    }

    #[tokio::test]
    async fn keeps_pages_fetched_before_a_failure() {
        let server = MockServer::start().await;
        mount_page(&server, 1, 200).await;
        mount_page(&server, 2, 503).await;
        let outcome = ApeWisdom::new(server.uri()).fetch(&reqwest::Client::new()).await;
        assert_eq!(outcome.records.len(), 1);
        assert_eq!(outcome.errors.len(), 1);
    }
}
