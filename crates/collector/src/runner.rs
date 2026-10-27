use crate::archive::Archive;
use crate::source::Source;

/// Fetches once and archives the result, returning the number of records written.
pub async fn run_once<S: Source>(source: &mut S, http: &reqwest::Client, archive: &Archive) -> anyhow::Result<usize> {
    let records = source.fetch(http).await?;
    archive.append(&records)?;
    Ok(records.len())
}

/// Polls forever. Consecutive failures back off exponentially, up to 16x the normal interval.
pub async fn run_forever<S: Source>(mut source: S, http: reqwest::Client, archive: Archive) {
    let mut failures = 0u32;
    loop {
        match run_once(&mut source, &http, &archive).await {
            Ok(records) => {
                failures = 0;
                tracing::info!(source = source.name(), records, "archived");
            }
            Err(e) => {
                failures += 1;
                tracing::warn!(source = source.name(), failures, "fetch failed: {e:#}");
            }
        }
        tokio::time::sleep(source.interval() * 2u32.pow(failures.min(4))).await;
    }
}
