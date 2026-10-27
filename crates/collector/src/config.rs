use crate::source::AnySource;
use crate::sources::{AlpacaNews, ApeWisdom, FinnhubNews, Reddit};

pub const APEWISDOM_URL: &str = "https://apewisdom.io";
pub const ALPACA_DATA_URL: &str = "https://data.alpaca.markets";
pub const FINNHUB_URL: &str = "https://finnhub.io";
pub const REDDIT_AUTH_URL: &str = "https://www.reddit.com/api/v1/access_token";
pub const REDDIT_API_URL: &str = "https://oauth.reddit.com";

const DEFAULT_REDDIT_USER_AGENT: &str = "macos:stocktrader-collector:v0.1";
const DEFAULT_SUBREDDITS: &str = "wallstreetbets,stocks,investing";

#[derive(Clone, Debug, PartialEq)]
pub struct RedditConfig {
    pub client_id: String,
    pub client_secret: String,
    pub user_agent: String,
    pub subreddits: Vec<String>,
}

/// Credentials from the environment. A source is enabled only when all of its variables are set.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub alpaca: Option<(String, String)>,
    pub finnhub: Option<String>,
    pub reddit: Option<RedditConfig>,
}

impl Config {
    pub fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let var = |key: &str| get(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let alpaca = var("APCA_API_KEY_ID").zip(var("APCA_API_SECRET_KEY"));
        let finnhub = var("FINNHUB_API_KEY");
        let reddit = var("REDDIT_CLIENT_ID").zip(var("REDDIT_CLIENT_SECRET")).map(|(client_id, client_secret)| {
            RedditConfig {
                client_id,
                client_secret,
                user_agent: var("REDDIT_USER_AGENT").unwrap_or_else(|| DEFAULT_REDDIT_USER_AGENT.into()),
                subreddits: var("REDDIT_SUBREDDITS")
                    .unwrap_or_else(|| DEFAULT_SUBREDDITS.into())
                    .split(',')
                    .map(|s| s.trim().trim_start_matches("r/").to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            }
        });
        Self { alpaca, finnhub, reddit }
    }

    /// Each source name with `None` if enabled, or the environment variables it is missing.
    pub fn describe(&self) -> Vec<(&'static str, Option<&'static str>)> {
        vec![
            ("apewisdom", None),
            ("alpaca_news", self.alpaca.is_none().then_some("APCA_API_KEY_ID, APCA_API_SECRET_KEY")),
            ("finnhub_news", self.finnhub.is_none().then_some("FINNHUB_API_KEY")),
            ("reddit", self.reddit.is_none().then_some("REDDIT_CLIENT_ID, REDDIT_CLIENT_SECRET")),
        ]
    }

    pub fn sources(&self) -> Vec<AnySource> {
        let mut sources = vec![AnySource::ApeWisdom(ApeWisdom::new(APEWISDOM_URL))];
        if let Some((key_id, secret)) = &self.alpaca {
            sources.push(AnySource::AlpacaNews(AlpacaNews::new(ALPACA_DATA_URL, key_id, secret)));
        }
        if let Some(token) = &self.finnhub {
            sources.push(AnySource::FinnhubNews(FinnhubNews::new(FINNHUB_URL, token)));
        }
        if let Some(r) = &self.reddit {
            sources.push(AnySource::Reddit(Reddit::new(
                REDDIT_AUTH_URL,
                REDDIT_API_URL,
                &r.client_id,
                &r.client_secret,
                &r.user_agent,
                r.subreddits.clone(),
            )));
        }
        sources
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn config(vars: &[(&str, &str)]) -> Config {
        let map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_lookup(|k| map.get(k).cloned())
    }

    #[test]
    fn only_keyless_source_without_credentials() {
        let c = config(&[("APCA_API_KEY_ID", "only-half"), ("FINNHUB_API_KEY", "  ")]);
        assert_eq!(c, Config { alpaca: None, finnhub: None, reddit: None });
        assert_eq!(c.sources().len(), 1);
    }

    #[test]
    fn all_sources_with_credentials() {
        let c = config(&[
            ("APCA_API_KEY_ID", "a"),
            ("APCA_API_SECRET_KEY", "b"),
            ("FINNHUB_API_KEY", "f"),
            ("REDDIT_CLIENT_ID", "r"),
            ("REDDIT_CLIENT_SECRET", "s"),
            ("REDDIT_SUBREDDITS", "r/stocks, pennystocks,,"),
        ]);
        assert_eq!(c.sources().len(), 4);
        assert!(c.describe().iter().all(|(_, missing)| missing.is_none()));
        assert_eq!(c.reddit.unwrap().subreddits, vec!["stocks", "pennystocks"]);
    }
}
