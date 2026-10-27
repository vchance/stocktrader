use crate::archive::Archive;
use crate::source::Source;

#[derive(Debug)]
pub struct PollResult {
    pub records: usize,
    /// Requests that failed while others in the same poll succeeded.
    pub errors: Vec<anyhow::Error>,
}

/// Fetches once, archives whatever succeeded, then lets the source commit its cursors.
/// An `Err` means the archive write or cursor commit failed, so nothing was committed.
pub async fn poll<S: Source>(source: &mut S, http: &reqwest::Client, archive: &Archive) -> anyhow::Result<PollResult> {
    let outcome = source.fetch(http).await;
    archive.append(&outcome.records)?;
    source.commit(archive)?;
    Ok(PollResult { records: outcome.records.len(), errors: outcome.errors })
}

/// Polls forever. After a poll with any error the interval doubles, up to the source's `max_backoff`.
pub async fn run_forever<S: Source>(mut source: S, http: reqwest::Client, archive: Archive) {
    let name = source.name();
    if let Err(e) = source.restore(&archive) {
        tracing::error!(source = name, "could not load saved cursor, starting fresh: {e}");
    }
    let mut failures = 0u32;
    loop {
        match poll(&mut source, &http, &archive).await {
            Ok(result) => {
                tracing::info!(source = name, records = result.records, "archived");
                for e in &result.errors {
                    tracing::warn!(source = name, "request failed: {e:#}");
                }
                failures = if result.errors.is_empty() { 0 } else { failures + 1 };
            }
            Err(e) => {
                failures += 1;
                tracing::error!(source = name, "archive write failed: {e:#}");
            }
        }
        let multiplier = 2u32.saturating_pow(failures).min(source.max_backoff().max(1));
        tokio::time::sleep(source.interval() * multiplier).await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::Utc;

    use super::*;
    use crate::archive::RawRecord;
    use crate::source::FetchOutcome;

    struct Counting {
        commits: usize,
    }

    impl Source for Counting {
        fn name(&self) -> &'static str {
            "counting"
        }

        fn interval(&self) -> Duration {
            Duration::from_secs(1)
        }

        async fn fetch(&mut self, _http: &reqwest::Client) -> FetchOutcome {
            let record = RawRecord {
                fetched_at: Utc::now(),
                source: "counting".into(),
                url: "u".into(),
                status: 200,
                body: "b".into(),
            };
            FetchOutcome { records: vec![record], errors: vec![anyhow::anyhow!("one request failed")] }
        }

        fn commit(&mut self, _archive: &Archive) -> std::io::Result<()> {
            self.commits += 1;
            Ok(())
        }
    }

    #[tokio::test]
    async fn partial_failures_still_archive_and_commit() {
        let dir = tempfile::tempdir().unwrap();
        let mut source = Counting { commits: 0 };
        let result = poll(&mut source, &reqwest::Client::new(), &Archive::new(dir.path())).await.unwrap();
        assert_eq!(result.records, 1);
        assert_eq!(result.errors.len(), 1);
        assert_eq!(source.commits, 1);
    }

    #[tokio::test]
    async fn failed_archive_write_does_not_commit() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"").unwrap();
        let mut source = Counting { commits: 0 };
        assert!(poll(&mut source, &reqwest::Client::new(), &Archive::new(&blocker)).await.is_err());
        assert_eq!(source.commits, 0);
    }
}
