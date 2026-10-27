use anyhow::bail;
use chrono::Utc;

use crate::archive::RawRecord;

/// Sends `request` and returns the body as a record. Non-2xx responses are errors and are not archived.
/// `record_url` is what gets stored and logged, so it must not contain credentials; for the same reason
/// transport errors are stripped of their URL.
pub async fn fetch(source: &str, request: reqwest::RequestBuilder, record_url: String) -> anyhow::Result<RawRecord> {
    let response = request.send().await.map_err(reqwest::Error::without_url)?;
    let status = response.status();
    let body = response.text().await.map_err(reqwest::Error::without_url)?;
    if !status.is_success() {
        bail!("HTTP {status} from {record_url}: {}", body.chars().take(200).collect::<String>());
    }
    Ok(RawRecord { fetched_at: Utc::now(), source: source.to_string(), url: record_url, status: status.as_u16(), body })
}
