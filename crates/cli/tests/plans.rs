//! End to end through the binary: plan files are listed and checked, backtested with an HTML report, and
//! compared over the same dates.

use std::path::Path;
use std::process::{Command, Output};

use chrono::NaiveDate;
use rust_decimal::prelude::ToPrimitive;
use st_data::{SyntheticConfig, generate_daily_bars};
use st_marketdata::{BarWriter, DailyBar};

/// SYN000..SYN009 for 300 sessions from 2024-01-02, adjusted and raw.
fn write_bars(dir: &Path) {
    let bars = generate_daily_bars(&SyntheticConfig {
        symbols: 10,
        days: 300,
        start: NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(),
        seed: 8,
    });
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

fn plans(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_stocktrader"))
        .arg("plans")
        .arg("--plans-dir")
        .arg(dir.join("plans"))
        .args(args)
        .arg("--marketdata-dir")
        .arg(dir.join("marketdata"))
        .arg("--reports-dir")
        .arg(dir.join("reports"))
        .output()
        .unwrap()
}

fn ok(out: &Output) -> String {
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{}\n{stdout}", String::from_utf8_lossy(&out.stderr));
    stdout
}

#[test]
fn plans_are_checked_backtested_and_compared() {
    let dir = tempfile::tempdir().unwrap();
    let market = dir.path().join("marketdata");
    std::fs::create_dir_all(&market).unwrap();
    write_bars(&market);
    let plan_dir = dir.path().join("plans");
    std::fs::create_dir_all(&plan_dir).unwrap();
    std::fs::write(
        plan_dir.join("fast.toml"),
        r#"name = "fast"
notes = "Short trend <test>"
[strategy]
kind = "sma"
fast = 5
slow = 20
weight = 0.1
[universe]
symbols = ["SYN001", "SYN002", "SYN003"]
[backtest]
benchmark = "SYN000"
"#,
    )
    .unwrap();
    std::fs::write(
        plan_dir.join("slow.toml"),
        "name = \"slow\"\n[strategy]\nkind = \"sma\"\nfast = 10\nslow = 50\n[backtest]\nbenchmark = \"SYN000\"\nstart = 2024-03-01\n",
    )
    .unwrap();

    // list only reads plan files, so the extra data flags aren't accepted there.
    let list = Command::new(env!("CARGO_BIN_EXE_stocktrader")).args(["plans", "--plans-dir"]).arg(&plan_dir).arg("list").output().unwrap();
    let listed = ok(&list);
    assert!(listed.contains("fast ") && listed.contains("sma 5/20") && listed.contains("SYN001,SYN002,SYN003"), "{listed}");

    let out = ok(&plans(dir.path(), &["backtest", "fast"]));
    assert!(out.contains("plan              fast (sma 5/20)") && out.contains("benchmark: SYN000"), "{out}");
    let html = std::fs::read_to_string(dir.path().join("reports/fast.html")).unwrap();
    for needle in ["<polyline", "SYN000 buy &amp; hold", "Short trend &lt;test&gt;", "Monthly returns", "<h2>Fills</h2>", "kind = &quot;sma&quot;"] {
        assert!(html.contains(needle), "report lacks {needle}");
    }
    assert!(!html.contains("<test>"), "notes must be escaped");

    // Compare runs both over one window: the later start of the two.
    let out = ok(&plans(dir.path(), &["compare"]));
    assert!(out.contains("fast") && out.contains("slow"), "{out}");
    let html = std::fs::read_to_string(dir.path().join("reports/compare.html")).unwrap();
    assert!(html.contains("Growth of 100") && html.contains("2024-03-01"), "window starts at the later start");

    // A broken plan is reported, and the listing fails.
    std::fs::write(plan_dir.join("broken.toml"), "name = \"broken\"\n[strategy]\nkind = \"sma\"\nfast = 50\nslow = 20\n").unwrap();
    let list = Command::new(env!("CARGO_BIN_EXE_stocktrader")).args(["plans", "--plans-dir"]).arg(&plan_dir).arg("list").output().unwrap();
    assert!(!list.status.success());
    assert!(String::from_utf8_lossy(&list.stdout).contains("broken                   INVALID"));
    // ... and compare refuses rather than silently skipping it.
    assert!(!plans(dir.path(), &["compare"]).status.success());
    let missing = plans(dir.path(), &["backtest", "nope"]);
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no plan nope"));
}
