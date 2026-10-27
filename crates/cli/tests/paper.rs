//! End to end through the binary against a mock Alpaca: `stocktrader paper` plans from Parquet bars and the
//! account's real state, prints without --submit, and submits sells before buys with --submit.

use std::path::Path;
use std::process::{Command, Output};

use chrono::{Datelike, Days, NaiveDate, Weekday};
use serde_json::{Value, json};
use st_marketdata::{BarWriter, DailyBar};
use st_signals::AttentionRow;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn date(d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
}

/// 60 weekday bars ending `last`: UP rises from 100, DOWN falls from 100, both with ample volume.
fn write_bars(dir: &Path, last: NaiveDate) {
    let mut days = Vec::new();
    let mut d = last;
    while days.len() < 60 {
        if !matches!(d.weekday(), Weekday::Sat | Weekday::Sun) {
            days.push(d);
        }
        d = d - Days::new(1);
    }
    days.reverse();
    let mut rows = Vec::new();
    for (symbol, step) in [("DOWN", -1.0), ("UP", 1.0)] {
        for (i, day) in days.iter().enumerate() {
            let close = 100.0 + step * i as f64;
            rows.push(DailyBar {
                symbol: symbol.into(),
                date: *day,
                open: close,
                high: close,
                low: close,
                close,
                volume: 1_000_000.0,
                trade_count: None,
                vwap: None,
            });
        }
    }
    for name in ["bars_daily_all.parquet", "bars_daily_raw.parquet"] {
        let mut w = BarWriter::create(&dir.join(name)).unwrap();
        w.write(&rows).unwrap();
        w.finish().unwrap().commit().unwrap();
    }
}

/// A paper account holding 30 DOWN, with the clock at `now` and sessions on the 23rd, 26th, 27th and 28th.
async fn mock_alpaca(now: &str, next_open: &str) -> MockServer {
    mock_alpaca_at(now, false, next_open).await
}

async fn mock_alpaca_at(now: &str, is_open: bool, next_open: &str) -> MockServer {
    let server = MockServer::start().await;
    let get = |p: &str, body: Value| Mock::given(method("GET")).and(path(p)).respond_with(ResponseTemplate::new(200).set_body_json(body));
    get("/v2/clock", json!({"timestamp": now, "is_open": is_open, "next_open": next_open, "next_close": next_open}))
        .mount(&server)
        .await;
    get("/v2/calendar", json!([{"date": "2026-10-23"}, {"date": "2026-10-26"}, {"date": "2026-10-27"}, {"date": "2026-10-28"}]))
        .mount(&server)
        .await;
    get(
        "/v2/account",
        json!({"status": "ACTIVE", "currency": "USD", "cash": "100000", "equity": "101230", "last_equity": "101000",
               "trading_blocked": false, "account_blocked": false}),
    )
    .mount(&server)
    .await;
    get("/v2/positions", json!([{"symbol": "DOWN", "qty": "30", "avg_entry_price": "60", "current_price": "41"}]))
        .mount(&server)
        .await;
    get("/v2/orders", json!([])).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/v2/orders"))
        .respond_with(|req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            ResponseTemplate::new(200).set_body_json(json!({
                "id": format!("id-{}", body["symbol"].as_str().unwrap()), "client_order_id": body["client_order_id"],
                "symbol": body["symbol"], "side": body["side"], "type": "market", "qty": body["qty"],
                "filled_qty": "0", "status": "accepted"
            }))
        })
        .mount(&server)
        .await;
    server
}

fn paper(dir: &Path, server: &MockServer, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_stocktrader"))
        .current_dir(dir)
        .env("APCA_API_KEY_ID", "kid")
        .env("APCA_API_SECRET_KEY", "ksecret")
        .args(["paper", "--symbols", "UP,DOWN", "--fast", "5", "--slow", "20", "--history-days", "200"])
        .arg("--marketdata-dir")
        .arg(dir)
        .arg("--live-dir")
        .arg(dir.join("live"))
        .args(["--trading-url", &server.uri()])
        .args(extra)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    assert!(out.status.success(), "stderr: {}\nstdout: {}", String::from_utf8_lossy(&out.stderr), String::from_utf8_lossy(&out.stdout));
    String::from_utf8(out.stdout.clone()).unwrap()
}

async fn posted(server: &MockServer) -> Vec<Value> {
    let requests = server.received_requests().await.unwrap();
    requests.iter().filter(|r| r.method.as_str() == "POST").map(|r| serde_json::from_slice(&r.body).unwrap()).collect()
}

const BEFORE_OPEN: (&str, &str) = ("2026-10-27T08:45:00-04:00", "2026-10-27T09:30:00-04:00");

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_prints_and_journals_without_submitting() {
    let dir = tempfile::tempdir().unwrap();
    write_bars(dir.path(), date(26));
    let server = mock_alpaca(BEFORE_OPEN.0, BEFORE_OPEN.1).await;

    let out = stdout(&paper(dir.path(), &server, &[]));
    assert!(out.contains("session           2026-10-27 (bars through 2026-10-26)"), "{out}");
    assert!(out.contains("UP       Buy") && out.contains("DOWN     Sell 30"), "{out}");
    assert!(out.contains("dry run"), "{out}");
    assert!(posted(&server).await.is_empty());

    let journal = std::fs::read_to_string(dir.path().join("live/journal/2026-10-27.jsonl")).unwrap();
    let plan: Value = serde_json::from_str(journal.lines().next().unwrap()).unwrap();
    assert_eq!(plan["kind"], "plan");
    assert_eq!(plan["body"]["mode"], "dry-run");
    assert_eq!(plan["body"]["plan"]["orders"].as_array().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_sends_sells_first_with_stable_ids() {
    let dir = tempfile::tempdir().unwrap();
    write_bars(dir.path(), date(26));
    let server = mock_alpaca(BEFORE_OPEN.0, BEFORE_OPEN.1).await;

    let out = stdout(&paper(dir.path(), &server, &["--submit"]));
    assert!(out.contains("accepted"), "{out}");
    let bodies = posted(&server).await;
    let summary: Vec<(&str, &str, &str)> = bodies
        .iter()
        .map(|b| (b["client_order_id"].as_str().unwrap(), b["type"].as_str().unwrap(), b["time_in_force"].as_str().unwrap()))
        .collect();
    assert_eq!(summary, vec![("st-2026-10-27-DOWN-sell", "market", "day"), ("st-2026-10-27-UP-buy", "market", "day")]);
    assert_eq!(bodies[0]["qty"], "30");
    let journal = std::fs::read_to_string(dir.path().join("live/journal/2026-10-27.jsonl")).unwrap();
    assert_eq!(journal.lines().count(), 3, "one plan line and one line per order");
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_file_allows_exits_only() {
    let dir = tempfile::tempdir().unwrap();
    write_bars(dir.path(), date(26));
    std::fs::create_dir_all(dir.path().join("live")).unwrap();
    std::fs::write(dir.path().join("live/KILL"), "checking a fill\n").unwrap();
    let server = mock_alpaca(BEFORE_OPEN.0, BEFORE_OPEN.1).await;

    let out = stdout(&paper(dir.path(), &server, &["--submit"]));
    assert!(out.contains("kill switch       KILL file: checking a fill"), "{out}");
    let ids: Vec<String> = posted(&server).await.iter().map(|b| b["client_order_id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids, vec!["st-2026-10-27-DOWN-sell"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_data_is_refused_and_closed_days_do_nothing() {
    let dir = tempfile::tempdir().unwrap();
    write_bars(dir.path(), date(23));
    let server = mock_alpaca(BEFORE_OPEN.0, BEFORE_OPEN.1).await;
    let out = paper(dir.path(), &server, &["--submit"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("market data ends 2026-10-23 but the last session was 2026-10-26"));

    let weekend = mock_alpaca("2026-10-24T08:45:00-04:00", "2026-10-26T09:30:00-04:00").await;
    let out = stdout(&paper(dir.path(), &weekend, &["--submit"]));
    assert!(out.contains("nothing to do"), "{out}");
    assert!(posted(&server).await.is_empty() && posted(&weekend).await.is_empty());
}

#[test]
fn refuses_the_live_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_stocktrader"))
        .current_dir(dir.path())
        .env("APCA_API_KEY_ID", "kid")
        .env("APCA_API_SECRET_KEY", "ksecret")
        .args(["paper", "--symbols", "UP", "--trading-url", "https://api.alpaca.markets"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("refusing trading URL"));
}

fn spike(ticker: &str, observed_at: &str) -> AttentionRow {
    let t = chrono::DateTime::parse_from_rfc3339(observed_at).unwrap().to_utc();
    AttentionRow {
        snapshot_at: t,
        observed_at: t,
        ticker: ticker.into(),
        rank: Some(1),
        mentions: 500,
        upvotes: Some(100),
        rank_24h_ago: Some(50),
        mentions_24h_ago: Some(20),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn overnight_attention_is_acted_on_but_not_attention_from_after_now() {
    let dir = tempfile::tempdir().unwrap();
    write_bars(dir.path(), date(26));
    let attention = dir.path().join("apewisdom.parquet");
    // UP spiked overnight, after the 26th's close and before the 08:45 run; DOWN only "spikes" at 11:00,
    // later than the run's clock, which a real run could never have seen.
    st_signals::write_rows(&attention, &[spike("UP", "2026-10-27T06:00:00Z"), spike("DOWN", "2026-10-27T15:00:00Z")]).unwrap();
    let server = mock_alpaca(BEFORE_OPEN.0, BEFORE_OPEN.1).await;

    let out = stdout(&paper(dir.path(), &server, &["--strategy", "attention-spike", "--attention", attention.to_str().unwrap()]));
    assert!(out.contains("UP       Buy"), "{out}");
    assert!(!out.contains("DOWN     Buy"), "{out}");
    assert!(out.contains("left alone        DOWN"), "{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn submitting_during_the_session_needs_allow_open() {
    let dir = tempfile::tempdir().unwrap();
    write_bars(dir.path(), date(26));
    let server = mock_alpaca_at("2026-10-27T11:00:00-04:00", true, "2026-10-28T09:30:00-04:00").await;

    let out = paper(dir.path(), &server, &["--submit"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--allow-open"));
    assert!(posted(&server).await.is_empty());

    stdout(&paper(dir.path(), &server, &[]));
    stdout(&paper(dir.path(), &server, &["--submit", "--allow-open"]));
    assert_eq!(posted(&server).await.len(), 2);
}
