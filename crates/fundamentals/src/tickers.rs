use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// One row of the SEC's current ticker map. It reflects today's tickers only: a company that changed
/// ticker appears under the new one, and delisted companies may be missing.
#[derive(Clone, Debug, PartialEq)]
pub struct Listing {
    pub cik: u32,
    pub name: String,
    pub ticker: String,
    pub exchange: Option<String>,
}

#[derive(Deserialize)]
struct Table {
    fields: Vec<String>,
    data: Vec<Vec<serde_json::Value>>,
}

/// Parses `company_tickers_exchange.json` (`{"fields": [...], "data": [[...], ...]}`), locating columns by
/// name so a reordering doesn't silently scramble them.
pub fn parse_tickers(json: &[u8]) -> Result<Vec<Listing>> {
    let table: Table = serde_json::from_slice(json).context("parsing ticker map")?;
    let col = |name: &str| table.fields.iter().position(|f| f == name).with_context(|| format!("no {name} column"));
    let (cik, name, ticker, exchange) = (col("cik")?, col("name")?, col("ticker")?, col("exchange").ok());

    let mut out = Vec::with_capacity(table.data.len());
    for row in &table.data {
        let text = |i: usize| row.get(i).and_then(|v| v.as_str()).map(str::to_string);
        let (Some(c), Some(t)) = (row.get(cik).and_then(serde_json::Value::as_u64), text(ticker)) else {
            continue;
        };
        out.push(Listing {
            cik: u32::try_from(c).context("cik out of range")?,
            name: text(name).unwrap_or_default(),
            ticker: t.to_ascii_uppercase(),
            exchange: exchange.and_then(text),
        });
    }
    if out.is_empty() {
        bail!("ticker map is empty");
    }
    Ok(out)
}

/// CIK to its first-listed ticker (the SEC lists a company's primary security first).
pub fn primary_tickers(listings: &[Listing]) -> HashMap<u32, String> {
    let mut map = HashMap::new();
    for l in listings {
        map.entry(l.cik).or_insert_with(|| l.ticker.clone());
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_by_column_name_and_keeps_primary_ticker() {
        let json = br#"{"fields": ["cik", "name", "ticker", "exchange"],
            "data": [[320193, "Apple Inc.", "AAPL", "Nasdaq"],
                     [1652044, "Alphabet Inc.", "GOOGL", "Nasdaq"],
                     [1652044, "Alphabet Inc.", "GOOG", "Nasdaq"],
                     [null, "Broken", "XX", null]]}"#;
        let listings = parse_tickers(json).unwrap();
        assert_eq!(listings.len(), 3);
        assert_eq!(listings[0], Listing { cik: 320193, name: "Apple Inc.".into(), ticker: "AAPL".into(), exchange: Some("Nasdaq".into()) });
        assert_eq!(primary_tickers(&listings)[&1652044], "GOOGL");
    }

    #[test]
    fn rejects_reordered_or_empty_tables_safely() {
        let reordered = br#"{"fields": ["ticker", "cik", "name"], "data": [["MSFT", 789019, "Microsoft"]]}"#;
        let l = parse_tickers(reordered).unwrap();
        assert_eq!((l[0].cik, l[0].ticker.as_str(), l[0].exchange.as_deref()), (789019, "MSFT", None));
        assert!(parse_tickers(br#"{"fields": ["cik", "name", "ticker"], "data": []}"#).is_err());
    }
}
