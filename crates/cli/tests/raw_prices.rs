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
    write_pair_without(dir, with_raw, None);
}

/// As [`write_pair`], leaving `missing` out of the raw file only.
fn write_pair_without(dir: &Path, with_raw: bool, missing: Option<&str>) {
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
        let raw = daily(1.0).into_iter().filter(|b| Some(b.symbol.as_str()) != missing).collect();
        write("bars_daily_raw.parquet", raw);
    }
}

fn command(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_stocktrader"));
    cmd.args(["backtest", "--benchmark", "none", "--fast", "5", "--slow", "20", "--parquet"])
        .arg(dir.join("bars_daily_all.parquet"));
    cmd
}

fn run(dir: &Path) -> (String, String) {
    let out = command(dir).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    (String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap())
}

#[test]
fn symbols_missing_from_the_raw_file_fail_closed_and_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    write_pair_without(dir.path(), true, Some("SYN000"));
    let (stdout, stderr) = run(dir.path());
    assert!(stderr.contains("have no as-traded price") && stderr.contains("SYN000"), "{stderr}");
    assert!(fills(&stdout) > 0, "symbols with traded prices still trade");

    let out = command(dir.path()).arg("--raw").arg(dir.path().join("typo.parquet")).output().unwrap();
    assert!(!out.status.success(), "a mistyped --raw must not be silently ignored");
    assert!(String::from_utf8_lossy(&out.stderr).contains("does not exist"));
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

#[test]
fn attention_is_limited_to_symbols_and_sessions_with_bars() {
    let dir = tempfile::tempdir().unwrap();
    write_pair(dir.path(), true);
    let row = |ticker: &str, at: &str| {
        let t = chrono::DateTime::parse_from_rfc3339(at).unwrap().to_utc();
        st_signals::AttentionRow {
            snapshot_at: t,
            observed_at: t,
            ticker: ticker.into(),
            rank: Some(1),
            mentions: 100,
            upvotes: None,
            rank_24h_ago: None,
            mentions_24h_ago: Some(10),
        }
    };
    let attention = dir.path().join("apewisdom.parquet");
    // Bars run from 2024-01-02 for SYN000..SYN009; only the first row is for a symbol and time with bars.
    st_signals::write_rows(
        &attention,
        &[row("SYN001", "2024-02-01T12:00:00Z"), row("NOBARS", "2024-02-01T12:00:00Z"), row("SYN001", "2023-06-01T12:00:00Z")],
    )
    .unwrap();
    let out = command(dir.path()).arg("--attention").arg(&attention).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("attention events  1\n"), "{stdout}");
}
