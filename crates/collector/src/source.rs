use std::future::Future;
use std::time::Duration;

use crate::archive::RawRecord;
use crate::sources::{AlpacaNews, ApeWisdom, FinnhubNews, Reddit};

pub trait Source: Send + 'static {
    /// Also the archive subdirectory.
    fn name(&self) -> &'static str;
    fn interval(&self) -> Duration;
    fn fetch(&mut self, http: &reqwest::Client) -> impl Future<Output = anyhow::Result<Vec<RawRecord>>> + Send;
}

/// Every configured source, so the runner can hold them in one collection.
pub enum AnySource {
    ApeWisdom(ApeWisdom),
    AlpacaNews(AlpacaNews),
    FinnhubNews(FinnhubNews),
    Reddit(Reddit),
}

impl Source for AnySource {
    fn name(&self) -> &'static str {
        match self {
            AnySource::ApeWisdom(s) => s.name(),
            AnySource::AlpacaNews(s) => s.name(),
            AnySource::FinnhubNews(s) => s.name(),
            AnySource::Reddit(s) => s.name(),
        }
    }

    fn interval(&self) -> Duration {
        match self {
            AnySource::ApeWisdom(s) => s.interval(),
            AnySource::AlpacaNews(s) => s.interval(),
            AnySource::FinnhubNews(s) => s.interval(),
            AnySource::Reddit(s) => s.interval(),
        }
    }

    async fn fetch(&mut self, http: &reqwest::Client) -> anyhow::Result<Vec<RawRecord>> {
        match self {
            AnySource::ApeWisdom(s) => s.fetch(http).await,
            AnySource::AlpacaNews(s) => s.fetch(http).await,
            AnySource::FinnhubNews(s) => s.fetch(http).await,
            AnySource::Reddit(s) => s.fetch(http).await,
        }
    }
}
