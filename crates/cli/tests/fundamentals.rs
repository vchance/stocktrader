//! End to end through the binary: filed financials from a `fundamentals sync` directory drive the
//! quality-value strategy, limited to symbols with bars and to facts public during the replay.

use std::path::Path;
use std::process::Command;

use chrono::NaiveDate;
use rust_decimal::prelude::ToPrimitive;
use st_data::{SyntheticConfig, generate_daily_bars};
use st_fundamentals::{Fact, FactWriter};
use st_marketdata::{BarWriter, DailyBar};

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

/// SYN000..SYN009 for 300 sessions from 2024-01-02, as both adjusted and raw files.
fn write_bars(dir: &Path) {
    let bars = generate_daily_bars(&SyntheticConfig { symbols: 10, days: 300, start: date(2024, 1, 2), seed: 5 });
    let rows: Vec<DailyBar> = bars
        .iter()
        .map(|b| DailyBar {
            symbol: b.symbol.to_string(),
            date: b.end.with_timezone(&chrono_tz::America::New_York).date_naive(),
            open: b.open.to_f64().unwrap(),
            high: b.high.to_f64().unwrap(),
            low: b.low.to_f64().unwrap(),
            close: b.close.to_f64().unwrap(),
            volume: b.volume.to_f64().unwrap(),
            trade_count: None,
            vwap: None,
        })
        .collect();
    for name in ["bars_daily_all.parquet", "bars_daily_raw.parquet"] {
        let mut w = BarWriter::create(&dir.join(name)).unwrap();
        w.write(&rows).unwrap();
        w.finish().unwrap().commit().unwrap();
    }
}

fn fact(cik: u32, taxonomy: &str, concept: &str, unit: &str, start: Option<NaiveDate>, value: f64, filed: NaiveDate) -> Fact {
    Fact {
        cik,
        taxonomy: taxonomy.into(),
        concept: concept.into(),
        unit: unit.into(),
        start,
        end: date(2023, 12, 31),
        value,
        fiscal_year: Some(2023),
        fiscal_period: Some("FY".into()),
        form: "10-K".into(),
        filed,
        accession: format!("{cik:010}-24-000001"),
        frame: None,
    }
}

/// FY2023 income, assets and shares for CIKs 1..=6, filed in February 2024. CIKs 1..=5 are SYN000..SYN004;
/// CIK 6 is a ticker with no bars. CIK 1 also has a figure filed after the replay ends, and an unused concept.
fn write_fundamentals(dir: &Path) {
    let mut facts = Vec::new();
    for cik in 1..=6u32 {
        let filed = date(2024, 2, 15);
        let income = 50e6 * f64::from(cik);
        facts.push(fact(cik, "us-gaap", "NetIncomeLoss", "USD", Some(date(2023, 1, 1)), income, filed));
        facts.push(fact(cik, "us-gaap", "Assets", "USD", None, 1e9, filed));
        facts.push(fact(cik, "dei", "EntityCommonStockSharesOutstanding", "shares", None, 50e6, filed));
    }
    facts.push(fact(1, "us-gaap", "Assets", "USD", None, 2e9, date(2030, 1, 1)));
    facts.push(fact(1, "us-gaap", "InventoryNet", "USD", None, 1e6, date(2024, 2, 15)));
    let mut w = FactWriter::create(&dir.join("facts.parquet")).unwrap();
    w.write(&facts).unwrap();
    w.finish().unwrap().commit().unwrap();

    let rows: Vec<String> = (1..=6)
        .map(|cik| {
            let ticker = if cik == 6 { "NOBARS".to_string() } else { format!("SYN{:03}", cik - 1) };
            format!(r#"[{cik}, "Company {cik}", "{ticker}", "Nasdaq"]"#)
        })
        .collect();
    std::fs::create_dir_all(dir.join("tickers")).unwrap();
    let json = format!(r#"{{"fields": ["cik", "name", "ticker", "exchange"], "data": [{}]}}"#, rows.join(","));
    std::fs::write(dir.join("tickers/2026-01-01.json"), json).unwrap();
}

#[test]
fn quality_value_trades_on_filed_financials() {
    let dir = tempfile::tempdir().unwrap();
    write_bars(dir.path());
    let fundamentals = dir.path().join("fundamentals");
    std::fs::create_dir_all(&fundamentals).unwrap();
    write_fundamentals(&fundamentals);

    let out = Command::new(env!("CARGO_BIN_EXE_stocktrader"))
        .args(["backtest", "--benchmark", "none", "--strategy", "quality-value", "--parquet"])
        .arg(dir.path().join("bars_daily_all.parquet"))
        .arg("--fundamentals")
        .arg(&fundamentals)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}\n{stdout}", String::from_utf8_lossy(&out.stderr));
    // Three figures for each of the five companies with bars; not NOBARS, the 2030 filing or InventoryNet.
    assert!(stdout.contains("fundamental events 15\n"), "{stdout}");
    let fills: usize = stdout.lines().find_map(|l| l.strip_prefix("fills")).unwrap().trim().parse().unwrap();
    assert!(fills > 0, "{stdout}");

    // Walk-forward over the same data with short windows picks among 10, 20 and 40 names.
    let walk = Command::new(env!("CARGO_BIN_EXE_stocktrader"))
        .args(["walk-forward", "--benchmark", "none", "--strategy", "quality-value", "--warmup-days", "70"])
        .args(["--train-days", "120", "--test-days", "60", "--parquet"])
        .arg(dir.path().join("bars_daily_all.parquet"))
        .arg("--fundamentals")
        .arg(&fundamentals)
        .output()
        .unwrap();
    let walk_out = String::from_utf8_lossy(&walk.stdout);
    assert!(walk.status.success(), "{}\n{walk_out}", String::from_utf8_lossy(&walk.stderr));
    assert!(walk_out.contains("quality-value top"), "{walk_out}");
    // The filings predate every fold's warm-up; folds still trade because each run sees earlier filings.
    let fold_fills: Vec<usize> = walk_out
        .lines()
        .filter(|l| l.starts_with("2024-") || l.starts_with("2025-"))
        .map(|l| l.split_whitespace().last().unwrap().parse().unwrap())
        .collect();
    assert!(!fold_fills.is_empty() && fold_fills.iter().all(|n| *n > 0), "{walk_out}");

    let too_few = Command::new(env!("CARGO_BIN_EXE_stocktrader"))
        .args(["backtest", "--strategy", "quality-value", "--top-n", "2", "--parquet"])
        .arg(dir.path().join("bars_daily_all.parquet"))
        .arg("--fundamentals")
        .arg(&fundamentals)
        .output()
        .unwrap();
    assert!(!too_few.status.success());
    assert!(String::from_utf8_lossy(&too_few.stderr).contains("position cap"));

    let missing = Command::new(env!("CARGO_BIN_EXE_stocktrader"))
        .args(["backtest", "--strategy", "quality-value", "--parquet"])
        .arg(dir.path().join("bars_daily_all.parquet"))
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("needs --fundamentals"));
}
