use std::collections::BTreeMap;
use std::io;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use reqwest::header::USER_AGENT;
use serde::Deserialize;

use crate::archive::{Archive, RawRecord};
use crate::http::fetch;
use crate::source::{FetchOutcome, Source};

/// Pages of 100 to walk back per listing per poll. Reddit listings stop at about 1000 items anyway.
const MAX_PAGES: usize = 10;

/// Newest posts and comments from a set of subreddits, via an app-only OAuth token. Reddit listings only
/// reach back about 1000 items, so anything not captured soon after posting is gone for good.
///
/// Each listing pages backwards until it reaches an item no newer than the newest one already archived
/// (the cursor, persisted per `subreddit/listing`). The first poll of a listing fetches one page.
pub struct Reddit {
    auth_url: String,
    api_url: String,
    client_id: String,
    client_secret: String,
    user_agent: String,
    subreddits: Vec<String>,
    token: Option<(String, Instant)>,
    /// Committed `subreddit/listing` -> newest `created_utc` archived.
    cursors: BTreeMap<String, f64>,
    staged: BTreeMap<String, f64>,
}

#[derive(Deserialize)]
struct Listing {
    data: ListingData,
}

#[derive(Deserialize)]
struct ListingData {
    after: Option<String>,
    children: Vec<Child>,
}

#[derive(Deserialize)]
struct Child {
    data: Item,
}

#[derive(Deserialize)]
struct Item {
    created_utc: f64,
}

impl Reddit {
    pub fn new(
        auth_url: impl Into<String>,
        api_url: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        user_agent: impl Into<String>,
        subreddits: Vec<String>,
    ) -> Self {
        Self {
            auth_url: auth_url.into(),
            api_url: api_url.into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            user_agent: user_agent.into(),
            subreddits,
            token: None,
            cursors: BTreeMap::new(),
            staged: BTreeMap::new(),
        }
    }

    async fn token(&mut self, http: &reqwest::Client) -> anyhow::Result<String> {
        if let Some((token, expires)) = &self.token
            && Instant::now() < *expires
        {
            return Ok(token.clone());
        }

        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
            expires_in: u64,
        }

        let response = http
            .post(&self.auth_url)
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .header(USER_AGENT, &self.user_agent)
            .form(&[("grant_type", "client_credentials")])
            .send()
            .await
            .map_err(reqwest::Error::without_url)?;
        let status = response.status();
        let body = response.text().await.map_err(reqwest::Error::without_url)?;
        if !status.is_success() {
            bail!("Reddit token request failed with HTTP {status}");
        }
        let parsed: TokenResponse = serde_json::from_str(&body).context("parsing Reddit token response")?;
        // Refresh a minute early rather than racing expiry.
        let expires = Instant::now() + Duration::from_secs(parsed.expires_in.saturating_sub(60));
        self.token = Some((parsed.access_token.clone(), expires));
        Ok(parsed.access_token)
    }

    /// Pages one listing back to its cursor. Returns the newest `created_utc` seen, to stage as the new
    /// cursor; records fetched before an error are still pushed.
    async fn fetch_listing(
        &self,
        http: &reqwest::Client,
        token: &str,
        key: &str,
        records: &mut Vec<RawRecord>,
    ) -> anyhow::Result<Option<f64>> {
        let cursor = self.cursors.get(key).copied();
        let mut newest = None;
        let mut after: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut url = format!("{}/r/{key}?limit=100&raw_json=1", self.api_url);
            if let Some(a) = &after {
                url.push_str(&format!("&after={a}"));
            }
            let request = http.get(&url).bearer_auth(token).header(USER_AGENT, &self.user_agent);
            let record = fetch(self.name(), request, url).await?;
            let parsed = serde_json::from_str::<Listing>(&record.body);
            records.push(record);
            let page = parsed.context("parsing Reddit listing")?.data;

            newest = newest.or(page.children.first().map(|c| c.data.created_utc));
            let reached_cursor = cursor.is_none_or(|c| page.children.iter().any(|ch| ch.data.created_utc <= c));
            if reached_cursor || page.children.is_empty() || page.after.is_none() {
                return Ok(newest);
            }
            after = page.after;
        }
        tracing::warn!(source = self.name(), listing = key, "more than {MAX_PAGES} pages since last poll; gap likely");
        Ok(newest)
    }
}

impl Source for Reddit {
    fn name(&self) -> &'static str {
        "reddit"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(2 * 60)
    }

    /// Never back off: listings scroll away and missed items are unrecoverable.
    fn max_backoff(&self) -> u32 {
        1
    }

    fn restore(&mut self, archive: &Archive) -> io::Result<()> {
        self.cursors = archive.load_state(self.name())?.unwrap_or_default();
        Ok(())
    }

    async fn fetch(&mut self, http: &reqwest::Client) -> FetchOutcome {
        self.staged.clear();
        let mut outcome = FetchOutcome::default();
        let token = match self.token(http).await {
            Ok(token) => token,
            Err(e) => {
                outcome.errors.push(e);
                return outcome;
            }
        };
        let keys: Vec<String> = self
            .subreddits
            .iter()
            .flat_map(|sub| ["new", "comments"].map(|listing| format!("{sub}/{listing}")))
            .collect();
        for key in keys {
            match self.fetch_listing(http, &token, &key, &mut outcome.records).await {
                Ok(Some(newest)) => {
                    let previous = self.cursors.get(&key).copied().unwrap_or(f64::MIN);
                    self.staged.insert(key, newest.max(previous));
                }
                Ok(None) => {}
                Err(e) => outcome.errors.push(e.context(key)),
            }
        }
        if !outcome.errors.is_empty() {
            // The token may have been revoked; get a fresh one next time.
            self.token = None;
        }
        outcome
    }

    fn commit(&mut self, archive: &Archive) -> io::Result<()> {
        if self.staged.is_empty() {
            return Ok(());
        }
        let mut cursors = self.cursors.clone();
        cursors.append(&mut self.staged);
        archive.save_state(self.name(), &cursors)?;
        self.cursors = cursors;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{basic_auth, bearer_token, body_string_contains, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn reddit(server: &MockServer) -> Reddit {
        Reddit::new(
            format!("{}/api/v1/access_token", server.uri()),
            server.uri(),
            "cid",
            "csecret",
            "test-agent",
            vec!["stocks".into()],
        )
    }

    fn listing(created: &[f64], after: Option<&str>) -> String {
        let children: Vec<String> =
            created.iter().map(|c| format!(r#"{{"data":{{"created_utc":{c}}}}}"#)).collect();
        let after = after.map_or("null".to_string(), |a| format!("\"{a}\""));
        format!(r#"{{"data":{{"after":{after},"children":[{}]}}}}"#, children.join(","))
    }

    async fn mount_token(server: &MockServer, expected_calls: u64) {
        Mock::given(method("POST"))
            .and(path("/api/v1/access_token"))
            .and(basic_auth("cid", "csecret"))
            .and(body_string_contains("grant_type=client_credentials"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"access_token":"tok","expires_in":86400}"#))
            .expect(expected_calls)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn reuses_token_and_pages_back_to_cursor() {
        let server = MockServer::start().await;
        mount_token(&server, 1).await;
        for listing_name in ["new", "comments"] {
            Mock::given(method("GET"))
                .and(path(format!("/r/stocks/{listing_name}")))
                .and(bearer_token("tok"))
                .and(header("user-agent", "test-agent"))
                .and(query_param("after", "older"))
                .respond_with(ResponseTemplate::new(200).set_body_string(listing(&[150.0, 90.0], None)))
                // Only stocks/new has a saved cursor to page back to.
                .expect(u64::from(listing_name == "new"))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path(format!("/r/stocks/{listing_name}")))
                .respond_with(ResponseTemplate::new(200).set_body_string(listing(&[300.0, 200.0], Some("older"))))
                .mount(&server)
                .await;
        }

        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path());
        archive.save_state("reddit", &BTreeMap::from([("stocks/new".to_string(), 100.0)])).unwrap();

        let mut source = reddit(&server);
        source.restore(&archive).unwrap();
        let http = reqwest::Client::new();
        let outcome = source.fetch(&http).await;
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        // stocks/new had a cursor, so it paged back once; stocks/comments had none, so one page.
        assert_eq!(outcome.records.len(), 3);
        source.commit(&archive).unwrap();
        assert_eq!(source.cursors["stocks/new"], 300.0);
        assert_eq!(source.cursors["stocks/comments"], 300.0);

        // Second poll: token reused (expect(1) above), and both cursors now stop after one page.
        let outcome = source.fetch(&http).await;
        assert_eq!(outcome.records.len(), 2);
    }

    #[tokio::test]
    async fn one_failed_listing_keeps_the_others() {
        let server = MockServer::start().await;
        mount_token(&server, 1).await;
        Mock::given(method("GET"))
            .and(path("/r/stocks/new"))
            .respond_with(ResponseTemplate::new(200).set_body_string(listing(&[10.0], None)))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/r/stocks/comments"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path());
        let mut source = reddit(&server);
        let outcome = source.fetch(&reqwest::Client::new()).await;
        assert_eq!(outcome.records.len(), 1);
        assert_eq!(outcome.errors.len(), 1);
        assert!(source.token.is_none());

        source.commit(&archive).unwrap();
        assert_eq!(source.cursors.get("stocks/new"), Some(&10.0));
        assert_eq!(source.cursors.get("stocks/comments"), None);
    }
}
