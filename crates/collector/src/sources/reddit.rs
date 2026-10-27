use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use reqwest::header::USER_AGENT;
use serde::Deserialize;

use crate::archive::RawRecord;
use crate::http::fetch;
use crate::source::Source;

/// Newest posts and comments from a set of subreddits, via an app-only OAuth token. Reddit listings only
/// reach back about 1000 items, so anything not captured soon after posting is gone for good.
pub struct Reddit {
    auth_url: String,
    api_url: String,
    client_id: String,
    client_secret: String,
    user_agent: String,
    subreddits: Vec<String>,
    token: Option<(String, Instant)>,
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

    async fn fetch_listings(&self, http: &reqwest::Client, token: &str) -> anyhow::Result<Vec<RawRecord>> {
        let mut records = Vec::new();
        for subreddit in &self.subreddits {
            for listing in ["new", "comments"] {
                let url = format!("{}/r/{subreddit}/{listing}?limit=100&raw_json=1", self.api_url);
                let request = http.get(&url).bearer_auth(token).header(USER_AGENT, &self.user_agent);
                records.push(fetch(self.name(), request, url).await?);
            }
        }
        Ok(records)
    }
}

impl Source for Reddit {
    fn name(&self) -> &'static str {
        "reddit"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(2 * 60)
    }

    async fn fetch(&mut self, http: &reqwest::Client) -> anyhow::Result<Vec<RawRecord>> {
        let token = self.token(http).await?;
        let result = self.fetch_listings(http, &token).await;
        if result.is_err() {
            // The token may have been revoked; get a fresh one next time.
            self.token = None;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{basic_auth, bearer_token, body_string_contains, header, method, path};
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

    #[tokio::test]
    async fn fetches_token_once_and_reuses_it() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/access_token"))
            .and(basic_auth("cid", "csecret"))
            .and(body_string_contains("grant_type=client_credentials"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"access_token":"tok","expires_in":86400}"#))
            .expect(1)
            .mount(&server)
            .await;
        for listing in ["new", "comments"] {
            Mock::given(method("GET"))
                .and(path(format!("/r/stocks/{listing}")))
                .and(bearer_token("tok"))
                .and(header("user-agent", "test-agent"))
                .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
                .expect(2)
                .mount(&server)
                .await;
        }

        let mut source = reddit(&server);
        let http = reqwest::Client::new();
        assert_eq!(source.fetch(&http).await.unwrap().len(), 2);
        assert_eq!(source.fetch(&http).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn listing_failure_drops_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"access_token":"tok","expires_in":86400}"#))
            .mount(&server)
            .await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(401)).mount(&server).await;

        let mut source = reddit(&server);
        assert!(source.fetch(&reqwest::Client::new()).await.is_err());
        assert!(source.token.is_none());
    }
}
