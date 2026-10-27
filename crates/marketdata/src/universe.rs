use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::NaiveDate;

use crate::alpaca::Asset;

/// Listing venues we trade. Excludes OTC, where data is thin and manipulation common.
pub const EXCHANGES: &[&str] = &["NYSE", "NASDAQ", "ARCA", "AMEX", "BATS"];

/// Sorted, de-duplicated symbols of US equities on [`EXCHANGES`], whatever their status, so that
/// delisted names stay in the history.
pub fn universe<'a>(assets: impl IntoIterator<Item = &'a Asset>) -> Vec<String> {
    assets
        .into_iter()
        .filter(|a| a.asset_class == "us_equity" && EXCHANGES.contains(&a.exchange.as_str()))
        .map(|a| a.symbol.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Archives the raw asset-list response as `<dir>/<date>-<status>.json`. Kept forever: the sequence of
/// daily snapshots is our record of listings and delistings.
pub fn save_snapshot(dir: &Path, date: NaiveDate, status: &str, body: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{date}-{status}.json"));
    let tmp = path.with_extension("json.tmp");
    let mut file = fs::File::create(&tmp)?;
    file.write_all(body.as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(symbol: &str, exchange: &str, status: &str) -> Asset {
        Asset {
            symbol: symbol.into(),
            exchange: exchange.into(),
            status: status.into(),
            asset_class: "us_equity".into(),
            tradable: status == "active",
            name: None,
        }
    }

    #[test]
    fn keeps_listed_names_including_delisted_and_drops_otc() {
        let assets = [
            asset("MSFT", "NASDAQ", "active"),
            asset("AAPL", "NASDAQ", "active"),
            asset("GONE", "NYSE", "inactive"),
            asset("PINK", "OTC", "active"),
            asset("AAPL", "NASDAQ", "inactive"),
        ];
        assert_eq!(universe(&assets), vec!["AAPL", "GONE", "MSFT"]);
    }

    #[test]
    fn snapshot_is_written_by_date_and_status() {
        let dir = tempfile::tempdir().unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 10, 27).unwrap();
        let path = save_snapshot(dir.path(), date, "active", "[]").unwrap();
        assert_eq!(path.file_name().unwrap(), "2026-10-27-active.json");
        assert_eq!(fs::read_to_string(path).unwrap(), "[]");
    }
}
