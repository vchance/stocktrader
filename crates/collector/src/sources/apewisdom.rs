use std::time::Duration;

use serde::Deserialize;

use crate::archive::RawRecord;
use crate::http::fetch;
use crate::source::Source;

/// Hard cap on pages per poll; the all-stocks feed is currently about 7 pages of 100.
const MAX_PAGES: u32 = 20;

/// ApeWisdom ticker mention counts aggregated from Reddit and 4chan. No key needed. Only the current
/// snapshot is served, so history exists only if we keep polling it.
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

    async fn fetch(&mut self, http: &reqwest::Client) -> anyhow::Result<Vec<RawRecord>> {
        let mut records = Vec::new();
        let mut pages = 1;
        let mut page = 1;
        while page <= pages.min(MAX_PAGES) {
            let url = format!("{}/api/v1.0/filter/{}/page/{page}", self.base_url, self.filter);
            let record = fetch(self.name(), http.get(&url), url).await?;
            if page == 1 {
                pages = serde_json::from_str::<Page>(&record.body)?.pages;
            }
            records.push(record);
            page += 1;
        }
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn follows_page_count_from_first_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1.0/filter/all-stocks/page/1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"count":150,"pages":2,"results":[]}"#))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1.0/filter/all-stocks/page/2"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"count":150,"pages":2,"results":[]}"#))
            .expect(1)
            .mount(&server)
            .await;

        let records = ApeWisdom::new(server.uri()).fetch(&reqwest::Client::new()).await.unwrap();
        assert_eq!(records.len(), 2);
        assert!(records[1].url.ends_with("/page/2"));
    }
}
