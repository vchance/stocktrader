//! Sending a plan to the broker and recording what happened.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};
use serde::Serialize;
use serde_json::json;
use st_core::Side;

use crate::alpaca::{NewOrder, Submitted, TradingClient};
use crate::plan::{Plan, PlannedOrder};

/// Append-only JSON lines, one file per session date, under `<live dir>/journal/`.
pub struct Journal {
    path: PathBuf,
}

impl Journal {
    pub fn open(live_dir: &Path, date: NaiveDate) -> Result<Self> {
        let dir = live_dir.join("journal");
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self { path: dir.join(format!("{date}.jsonl")) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes one line, stamped with the current time. Each call opens and flushes the file, so a crash
    /// mid-run loses nothing already written.
    pub fn record(&self, kind: &str, body: impl Serialize) -> Result<()> {
        let line = json!({ "at": Utc::now(), "kind": kind, "body": body });
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        writeln!(file, "{line}").with_context(|| format!("writing {}", self.path.display()))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Accepted { broker_id: String, status: String },
    /// Already submitted under this id, by an earlier run today or a retried request.
    Duplicate,
    Failed { error: String },
}

pub fn new_order(planned: &PlannedOrder) -> NewOrder {
    NewOrder {
        symbol: planned.order.symbol.to_string(),
        qty: planned.order.qty,
        side: match planned.order.side {
            Side::Buy => "buy",
            Side::Sell => "sell",
        },
        order_type: "market",
        time_in_force: "day",
        client_order_id: planned.client_order_id.clone(),
    }
}

/// Submits sells before buys, so a rejected buy can't hold up an exit. One failure doesn't stop the
/// rest; every outcome is journaled.
pub async fn submit_plan(client: &TradingClient, plan: &Plan, journal: &Journal) -> Result<Vec<(PlannedOrder, Outcome)>> {
    let mut orders: Vec<&PlannedOrder> = plan.orders.iter().collect();
    orders.sort_by_key(|o| o.order.side == Side::Buy);
    let mut results = Vec::new();
    for planned in orders {
        let outcome = match client.submit(&new_order(planned)).await {
            Ok(Submitted::Accepted(order)) => Outcome::Accepted { broker_id: order.id, status: order.status },
            Ok(Submitted::Duplicate) => Outcome::Duplicate,
            Err(e) => Outcome::Failed { error: format!("{e:#}") },
        };
        journal.record("submit", json!({ "order": planned, "outcome": outcome }))?;
        results.push((planned.clone(), outcome));
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;
    use st_core::{OrderRequest, OrderType, Symbol};
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn planned(symbol: &str, side: Side) -> PlannedOrder {
        let side_name = if side == Side::Buy { "buy" } else { "sell" };
        PlannedOrder {
            client_order_id: format!("st-2026-10-27-{symbol}-{side_name}"),
            order: OrderRequest {
                symbol: Symbol::new(symbol),
                side,
                qty: dec!(3),
                order_type: OrderType::Market,
                reason: "test".into(),
            },
            target_weight: dec!(0.05),
        }
    }

    #[tokio::test]
    async fn submits_sells_first_and_journals_every_outcome() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/orders"))
            .and(body_partial_json(json!({"symbol": "OUT", "side": "sell"})))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"id":"s1","client_order_id":"st-2026-10-27-OUT-sell","symbol":"OUT","side":"sell","type":"market",
                    "qty":"3","filled_qty":"0","status":"accepted"}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"symbol": "DUP"})))
            .respond_with(ResponseTemplate::new(422).set_body_string(r#"{"message":"client_order_id must be unique"}"#))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"symbol": "BAD"})))
            .respond_with(ResponseTemplate::new(403).set_body_string(r#"{"message":"insufficient buying power"}"#))
            .mount(&server)
            .await;

        let client = TradingClient::new(reqwest::Client::new(), &server.uri(), "k", "s").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 10, 27).unwrap();
        let journal = Journal::open(dir.path(), date).unwrap();
        let plan = Plan {
            orders: vec![planned("BAD", Side::Buy), planned("DUP", Side::Buy), planned("OUT", Side::Sell)],
            ..Plan::default()
        };

        let results = submit_plan(&client, &plan, &journal).await.unwrap();
        let summary: Vec<(&str, &Outcome)> = results.iter().map(|(p, o)| (p.order.symbol.as_str(), o)).collect();
        assert_eq!(summary[0], ("OUT", &Outcome::Accepted { broker_id: "s1".into(), status: "accepted".into() }));
        assert_eq!(summary[2], ("DUP", &Outcome::Duplicate));
        assert!(matches!(summary[1], ("BAD", Outcome::Failed { error }) if error.contains("buying power")));

        let lines = fs::read_to_string(dir.path().join("journal/2026-10-27.jsonl")).unwrap();
        assert_eq!(lines.lines().count(), 3);
        let first: serde_json::Value = serde_json::from_str(lines.lines().next().unwrap()).unwrap();
        assert_eq!(first["body"]["order"]["client_order_id"], "st-2026-10-27-OUT-sell");
        assert_eq!(first["body"]["outcome"]["accepted"]["broker_id"], "s1");
    }
}
