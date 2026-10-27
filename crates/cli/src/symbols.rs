//! `stocktrader symbols`: how well SEC company tickers line up with the Alpaca universe, i.e. how many
//! tradable symbols fundamentals strategies can actually see.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use st_fundamentals::sync::facts_path;
use st_fundamentals::tickers::latest_snapshot;
use st_fundamentals::{FactFilter, Listing, market_symbol, parse_tickers, read_facts};
use st_marketdata::Asset;
use st_marketdata::universe::EXCHANGES;

#[derive(Args)]
pub struct SymbolsArgs {
    /// Directory from `marketdata sync`; its newest assets/<date>-active.json is the universe.
    #[arg(long, env = "STOCKTRADER_MARKETDATA_DIR", default_value = "data/marketdata")]
    marketdata_dir: PathBuf,
    /// Directory from `fundamentals sync`: the newest ticker snapshot and facts.parquet.
    #[arg(long, env = "STOCKTRADER_FUNDAMENTALS_DIR", default_value = "data/fundamentals")]
    fundamentals: PathBuf,
    /// How many unmatched symbols to list.
    #[arg(long, default_value_t = 30)]
    show: usize,
}

#[derive(Debug, Default, PartialEq)]
struct Report {
    /// Active, tradable US equities on the traded exchanges.
    universe: usize,
    /// Of those, symbols the SEC ticker map knows.
    matched: usize,
    /// Of those, symbols whose company has filed an annual net income, if facts were read.
    with_income: Option<usize>,
    /// Alpaca symbol and the SEC ticker it probably is, where they differ only in punctuation.
    format_mismatches: Vec<(String, String)>,
    /// Universe symbols with no SEC ticker, sorted. Mostly ETFs, funds and foreign listings, which file no
    /// 10-K and so are expected here.
    unmatched: Vec<String>,
}

fn letters(s: &str) -> String {
    s.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_uppercase()
}

fn compare(assets: &[Asset], listings: &[Listing], income_ciks: Option<&HashSet<u32>>) -> Report {
    let universe: Vec<&str> = {
        let mut u: Vec<&str> = assets
            .iter()
            .filter(|a| a.asset_class == "us_equity" && a.tradable && a.status == "active" && EXCHANGES.contains(&a.exchange.as_str()))
            .map(|a| a.symbol.as_str())
            .collect();
        u.sort_unstable();
        u.dedup();
        u
    };
    let mut by_symbol: HashMap<String, u32> = HashMap::new();
    let mut by_letters: HashMap<String, &str> = HashMap::new();
    for l in listings {
        by_symbol.entry(market_symbol(&l.ticker).as_str().to_string()).or_insert(l.cik);
        by_letters.entry(letters(&l.ticker)).or_insert(&l.ticker);
    }
    let mut report = Report { universe: universe.len(), ..Report::default() };
    let mut with_income = 0;
    for symbol in universe {
        match by_symbol.get(symbol) {
            Some(cik) => {
                report.matched += 1;
                if income_ciks.is_some_and(|c| c.contains(cik)) {
                    with_income += 1;
                }
            }
            None => match by_letters.get(&letters(symbol)) {
                Some(sec) => report.format_mismatches.push((symbol.to_string(), sec.to_string())),
                None => report.unmatched.push(symbol.to_string()),
            },
        }
    }
    report.with_income = income_ciks.map(|_| with_income);
    report
}

/// The newest `<date>-active.json` in `<marketdata>/assets`.
fn latest_assets(dir: &Path) -> Result<PathBuf> {
    let dir = dir.join("assets");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .with_context(|| format!("no asset snapshots in {}; run `marketdata sync` first", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with("-active.json")))
        .collect();
    files.sort();
    files.pop().with_context(|| format!("no *-active.json in {}", dir.display()))
}

/// Companies with at least one fiscal-year (350-380 day) net income in USD, which is what quality-value
/// needs. `None` if there is no facts file yet.
fn income_ciks(root: &Path) -> Result<Option<HashSet<u32>>> {
    let path = facts_path(root);
    if !path.exists() {
        return Ok(None);
    }
    let filter = FactFilter { concepts: Some(HashSet::from(["NetIncomeLoss".to_string()])), ..FactFilter::default() };
    let ciks = read_facts(&path, &filter)?
        .into_iter()
        .filter(|f| f.unit == "USD" && f.start.is_some_and(|s| (350..=380).contains(&(f.end - s).num_days())))
        .map(|f| f.cik)
        .collect();
    Ok(Some(ciks))
}

fn percent(part: usize, whole: usize) -> String {
    format!("{:.1}%", 100.0 * part as f64 / whole.max(1) as f64)
}

pub fn run(args: &SymbolsArgs) -> Result<()> {
    let assets_path = latest_assets(&args.marketdata_dir)?;
    let assets: Vec<Asset> =
        serde_json::from_slice(&fs::read(&assets_path)?).with_context(|| format!("parsing {}", assets_path.display()))?;
    let tickers_path = latest_snapshot(&args.fundamentals)?;
    let listings = parse_tickers(&fs::read(&tickers_path)?)?;
    let income = income_ciks(&args.fundamentals)?;
    let r = compare(&assets, &listings, income.as_ref());

    println!("assets            {}", assets_path.display());
    println!("sec tickers       {}", tickers_path.display());
    println!("universe          {} active tradable US equities on {}", r.universe, EXCHANGES.join("/"));
    println!("in SEC ticker map {} ({})", r.matched, percent(r.matched, r.universe));
    match r.with_income {
        Some(n) => println!("with annual NI    {} ({}): what quality-value can rank", n, percent(n, r.universe)),
        None => println!("with annual NI    - (no facts.parquet yet; run `fundamentals sync`)"),
    }
    println!("format mismatches {}", r.format_mismatches.len());
    for (alpaca, sec) in &r.format_mismatches {
        println!("  {alpaca:<10} SEC has {sec}");
    }
    println!("unmatched         {} (mostly ETFs, funds and foreign listings, which file no 10-K)", r.unmatched.len());
    let sample: Vec<&str> = r.unmatched.iter().take(args.show).map(String::as_str).collect();
    if !sample.is_empty() {
        println!("  first {}: {}", sample.len(), sample.join(" "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(symbol: &str, exchange: &str, tradable: bool) -> Asset {
        Asset {
            symbol: symbol.into(),
            exchange: exchange.into(),
            status: "active".into(),
            asset_class: "us_equity".into(),
            tradable,
            name: None,
        }
    }

    fn listing(cik: u32, ticker: &str) -> Listing {
        Listing { cik, name: String::new(), ticker: ticker.into(), exchange: None }
    }

    #[test]
    fn counts_matches_income_and_punctuation_mismatches() {
        let assets = vec![
            asset("AAPL", "NASDAQ", true),
            asset("BRK.B", "NYSE", true),
            asset("SPY", "ARCA", true),
            asset("ABCW", "NASDAQ", true),
            asset("OTCX", "OTC", true),
            asset("HALT", "NYSE", false),
            asset("AAPL", "NASDAQ", true),
        ];
        // BRK-B maps to BRK.B as market data writes it; ABC.W is a format the mapping doesn't fix.
        let listings = vec![listing(1, "AAPL"), listing(2, "BRK-B"), listing(3, "ABC.W"), listing(4, "OTCX"), listing(5, "HALT")];
        let income = HashSet::from([1]);
        let r = compare(&assets, &listings, Some(&income));
        assert_eq!(
            r,
            Report {
                universe: 4,
                matched: 2,
                with_income: Some(1),
                format_mismatches: vec![("ABCW".into(), "ABC.W".into())],
                unmatched: vec!["SPY".into()],
            }
        );
        assert_eq!(compare(&assets, &listings, None).with_income, None);
    }
}
