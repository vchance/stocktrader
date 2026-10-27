//! End to end through the binary: `stocktrader plans paper` trades each paper-mode plan in its own (mock)
//! Alpaca account, with per-plan keys, journals and kill files.

use std::path::Path;
use std::process::{Command, Output};

use chrono::{Datelike, Days, NaiveDate, Weekday};
use serde_json::{Value, json};
use st_marketdata::{BarWriter, DailyBar};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// 60 weekday bars ending 2026-10-26: UP and UP2 rise, DOWN falls.
fn write_bars(dir: &Path) {
    let mut days = Vec::new();
    let mut d = NaiveDate::from_ymd_opt(2026, 10, 26).unwrap();
    while days.len() < 60 {
        if !matches!(d.weekday(), Weekday::Sat | Weekday::Sun) {
            days.push(d);
        }
        d = d - Days::new(1);
    }
    days.reverse();
    let mut rows = Vec::new();
    for (symbol, step) in [("DOWN", -1.0), ("UP", 1.0), ("UP2", 0.5)] {
        for (i, day) in days.iter().enumerate() {
            let close = 100.0 + step * i as f64;
            rows.push(DailyBar { symbol: symbol.into(), date: *day, open: close, high: close, low: close, close, volume: 1e6, trade_count: None, vwap: None });
        }
    }
    for name in ["bars_daily_all.parquet", "bars_daily_raw.parquet"] {
        let mut w = BarWriter::create(&dir.join(name)).unwrap();
        w.write(&rows).unwrap();
        w.finish().unwrap().commit().unwrap();
    }
}

/// Two accounts behind one mock, told apart by key: `ka` holds 30 DOWN, `kb` holds nothing.
async fn mock_alpaca() -> MockServer {
    let server = MockServer::start().await;
    let ok = |body: Value| ResponseTemplate::new(200).set_body_json(body);
    Mock::given(method("GET")).and(path("/v2/clock"))
        .respond_with(ok(json!({"timestamp": "2026-10-27T08:45:00-04:00", "is_open": false, "next_open": "2026-10-27T09:30:00-04:00", "next_close": "2026-10-27T16:00:00-04:00"})))
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/v2/calendar"))
        .respond_with(ok(json!([{"date": "2026-10-23"}, {"date": "2026-10-26"}, {"date": "2026-10-27"}])))
        .mount(&server).await;
    for (key, positions) in [("ka", json!([{"symbol": "DOWN", "qty": "30", "avg_entry_price": "60", "current_price": "41"}])), ("kb", json!([]))] {
        Mock::given(method("GET")).and(path("/v2/account")).and(header("APCA-API-KEY-ID", key))
            .respond_with(ok(json!({"status": "ACTIVE", "currency": "USD", "cash": "100000", "equity": "100000", "last_equity": "100000", "trading_blocked": false, "account_blocked": false})))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/v2/positions")).and(header("APCA-API-KEY-ID", key)).respond_with(ok(positions)).mount(&server).await;
    }
    Mock::given(method("GET")).and(path("/v2/orders")).respond_with(ok(json!([]))).mount(&server).await;
    Mock::given(method("POST")).and(path("/v2/orders"))
        .respond_with(|req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            ResponseTemplate::new(200).set_body_json(json!({"id": "x", "client_order_id": body["client_order_id"], "symbol": body["symbol"],
                "side": body["side"], "type": "market", "qty": body["qty"], "filled_qty": "0", "status": "accepted"}))
        })
        .mount(&server).await;
    server
}

fn write_plans(dir: &Path) {
    let plans = dir.join("plans");
    std::fs::create_dir_all(&plans).unwrap();
    let plan = |name: &str, mode: &str, symbols: &str| {
        format!("name = \"{name}\"\nmode = \"{mode}\"\n[strategy]\nkind = \"sma\"\nfast = 5\nslow = 20\nweight = 0.1\n[universe]\nsymbols = [{symbols}]\n")
    };
    std::fs::write(plans.join("alpha.toml"), plan("alpha", "paper", "\"UP\", \"DOWN\"")).unwrap();
    std::fs::write(plans.join("beta.toml"), plan("beta", "paper", "\"UP2\"")).unwrap();
    std::fs::write(plans.join("gamma.toml"), plan("gamma", "backtest", "\"UP\"")).unwrap();
}

fn run(dir: &Path, server: &MockServer, keys: &[(&str, &str)], args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_stocktrader"));
    cmd.current_dir(dir).env_remove("APCA_API_KEY_ID").env_remove("APCA_API_SECRET_KEY");
    for (k, v) in keys {
        cmd.env(k, v);
    }
    cmd.args(["plans", "--plans-dir"]).arg(dir.join("plans")).arg("paper").args(args)
        .arg("--marketdata-dir").arg(dir.join("marketdata"))
        .arg("--live-dir").arg(dir.join("live"))
        .args(["--trading-url", &server.uri()])
        .output()
        .unwrap()
}

const KEYS: [(&str, &str); 4] =
    [("APCA_API_KEY_ID_ALPHA", "ka"), ("APCA_API_SECRET_KEY_ALPHA", "sa"), ("APCA_API_KEY_ID_BETA", "kb"), ("APCA_API_SECRET_KEY_BETA", "sb")];

/// (key, client_order_id) of every order posted.
async fn posted(server: &MockServer) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            (r.headers.get("APCA-API-KEY-ID").unwrap().to_str().unwrap().to_string(), body["client_order_id"].as_str().unwrap().to_string())
        })
        .collect();
    out.sort();
    out
}

fn setup() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let market = dir.path().join("marketdata");
    std::fs::create_dir_all(&market).unwrap();
    write_bars(&market);
    write_plans(dir.path());
    dir
}

#[tokio::test(flavor = "multi_thread")]
async fn each_paper_plan_trades_its_own_account() {
    let dir = setup();
    let server = mock_alpaca().await;

    let out = run(dir.path(), &server, &KEYS, &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}\n{stdout}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("plan              alpha") && stdout.contains("plan              beta"), "{stdout}");
    assert!(!stdout.contains("gamma"), "backtest-mode plans are skipped: {stdout}");
    assert!(posted(&server).await.is_empty(), "dry run");

    let out = run(dir.path(), &server, &KEYS, &["--submit"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        posted(&server).await,
        vec![
            ("ka".into(), "st-2026-10-27-DOWN-sell".into()),
            ("ka".into(), "st-2026-10-27-UP-buy".into()),
            ("kb".into(), "st-2026-10-27-UP2-buy".into()),
        ]
    );
    for plan in ["alpha", "beta"] {
        assert!(dir.path().join(format!("live/{plan}/journal/2026-10-27.jsonl")).exists(), "{plan} journal");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plans_kill_file_only_stops_that_plan() {
    let dir = setup();
    let server = mock_alpaca().await;
    std::fs::create_dir_all(dir.path().join("live/alpha")).unwrap();
    std::fs::write(dir.path().join("live/alpha/KILL"), "pausing alpha\n").unwrap();
    let out = run(dir.path(), &server, &KEYS, &["--submit"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // alpha may still exit DOWN, but not buy UP; beta is unaffected.
    assert_eq!(
        posted(&server).await,
        vec![("ka".into(), "st-2026-10-27-DOWN-sell".into()), ("kb".into(), "st-2026-10-27-UP2-buy".into())]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn accounts_must_be_separate_and_present_and_plans_in_paper_mode() {
    let dir = setup();
    let server = mock_alpaca().await;
    let shared = [("APCA_API_KEY_ID_ALPHA", "ka"), ("APCA_API_SECRET_KEY_ALPHA", "sa"), ("APCA_API_KEY_ID_BETA", "ka"), ("APCA_API_SECRET_KEY_BETA", "sa")];
    let out = run(dir.path(), &server, &shared, &["--submit"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("use the same Alpaca account"));

    let out = run(dir.path(), &server, &KEYS[..2], &["--submit"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("APCA_API_KEY_ID_BETA is not set"));

    let out = run(dir.path(), &server, &KEYS, &["gamma"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("is in backtest mode"));
    assert!(posted(&server).await.is_empty(), "nothing is sent when the setup is wrong");
}
