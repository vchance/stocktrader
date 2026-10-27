use std::future::Future;
use std::io;
use std::time::Duration;

use crate::archive::{Archive, RawRecord};
use crate::sources::{AlpacaNews, ApeWisdom, FinnhubNews, Reddit};

/// Everything one poll produced. Good responses are kept even when other requests in the poll failed.
#[derive(Debug, Default)]
pub struct FetchOutcome {
    pub records: Vec<RawRecord>,
    pub errors: Vec<anyhow::Error>,
}

/// A polled data source.
///
/// Cursor protocol: `fetch` may only *stage* cursor changes. The runner calls `commit` after the records
/// from that fetch are durably archived, and only then may the source apply and persist them, so a failed
/// write never skips data. `fetch` must discard anything staged by an earlier, uncommitted fetch.
pub trait Source: Send + 'static {
    /// Also the archive subdirectory and state-file name.
    fn name(&self) -> &'static str;
    fn interval(&self) -> Duration;
    /// Largest multiple of `interval` to back off to after repeated failures. Keep it at 1 for sources
    /// whose data disappears if not fetched promptly.
    fn max_backoff(&self) -> u32 {
        4
    }
    /// Loads persisted cursors. Called once before the first fetch.
    fn restore(&mut self, _archive: &Archive) -> io::Result<()> {
        Ok(())
    }
    fn fetch(&mut self, http: &reqwest::Client) -> impl Future<Output = FetchOutcome> + Send;
    /// Applies and persists cursors staged by the last `fetch`.
    fn commit(&mut self, _archive: &Archive) -> io::Result<()> {
        Ok(())
    }
}

/// Every configured source, so the runner can hold them in one collection.
pub enum AnySource {
    ApeWisdom(ApeWisdom),
    AlpacaNews(AlpacaNews),
    FinnhubNews(FinnhubNews),
    Reddit(Reddit),
}

macro_rules! delegate {
    ($self:ident, $s:ident => $call:expr) => {
        match $self {
            AnySource::ApeWisdom($s) => $call,
            AnySource::AlpacaNews($s) => $call,
            AnySource::FinnhubNews($s) => $call,
            AnySource::Reddit($s) => $call,
        }
    };
}

impl Source for AnySource {
    fn name(&self) -> &'static str {
        delegate!(self, s => s.name())
    }

    fn interval(&self) -> Duration {
        delegate!(self, s => s.interval())
    }

    fn max_backoff(&self) -> u32 {
        delegate!(self, s => s.max_backoff())
    }

    fn restore(&mut self, archive: &Archive) -> io::Result<()> {
        delegate!(self, s => s.restore(archive))
    }

    async fn fetch(&mut self, http: &reqwest::Client) -> FetchOutcome {
        delegate!(self, s => s.fetch(http).await)
    }

    fn commit(&mut self, archive: &Archive) -> io::Result<()> {
        delegate!(self, s => s.commit(archive))
    }
}
