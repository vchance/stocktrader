//! End to end through the binary: the price filter must use as-traded prices when the raw file is there.

use std::path::Path;
use std::process::Command;

use chrono::NaiveDate;
use rust_decimal::prelude::ToPrimitive;
use st_data::{SyntheticConfig, generate_daily_bars};
use st_marketdata::{BarWriter, DailyBar};

/// Writes `bars_daily_all.parquet` with every price divided by 100 (as if each stock later split 100:1,
/// putting all adjusted history under $5) and, optionally, `bars_daily_raw.parquet` with the prices as
/// traded (roughly $10-300).
fn write_pair(dir: &Path, with_raw: bool) {
    let bars = generate_daily_bars(&SyntheticConfig {
        symbols: 10,
        days: 200,
        start: NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(),
        seed: 3,
    });
    let daily = |scale: f64| -> Vec<DailyBar> {
        bars.iter()
            .map(|b| DailyBar {
                symbol: b.symbol.to_string(),
                date: b.end.with_timezone(&chrono_tz::America::New_York).date_naive(),
                open: b.open.to_f64().unwrap() / scale,
                high: b.high.to_f64().unwrap() / scale,
                low: b.low.to_f64().unwrap() / scale,
                close: b.close.to_f64().unwrap() / scale,
                volume: b.volume.to_f64().unwrap() * scale,
                trade_count: None,
                vwap: None,
            })
            .collect()
    };
    let write = |name: &str, rows: Vec<DailyBar>| {
        let mut w = BarWriter::create(&dir.join(name)).unwrap();
        w.write(&rows).unwrap();
        w.finish().unwrap().commit().unwrap();
    };
    write("bars_daily_all.parquet", daily(100.0));
    if with_raw {
        write("bars_daily_raw.parquet", daily(1.0));
    }
}

fn run(dir: &Path) -> (String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_stocktrader"))
        .args(["backtest", "--benchmark", "none", "--fast", "5", "--slow", "20", "--parquet"])
        .arg(dir.join("bars_daily_all.parquet"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    (String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap())
}

fn fills(stdout: &str) -> usize {
    stdout.lines().find_map(|l| l.strip_prefix("fills")).unwrap().trim().parse().unwrap()
}

#[test]
fn price_filter_uses_raw_prices_beside_the_adjusted_file() {
    let with_raw = tempfile::tempdir().unwrap();
    write_pair(with_raw.path(), true);
    let (stdout, stderr) = run(with_raw.path());
    assert!(!stderr.contains("no raw bars"), "{stderr}");
    let traded = fills(&stdout);

    let without_raw = tempfile::tempdir().unwrap();
    write_pair(without_raw.path(), false);
    let (stdout, stderr) = run(without_raw.path());
    assert!(stderr.contains("no raw bars"), "warns when it can only see adjusted prices");
    let adjusted_only = fills(&stdout);

    // Every adjusted price is under $5, so the biased filter blocks every entry the raw one allows.
    assert!(traded > 0, "raw prices are $10-200, so entries pass");
    assert_eq!(adjusted_only, 0, "adjusted-only filtering wrongly treats them all as penny stocks");
}
