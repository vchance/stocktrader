use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::NaiveDate;
use serde::Deserialize;

/// Concepts extracted from each company, as (taxonomy, concept). Different filers report the same idea
/// under different tags (revenue especially), so several are kept and reconciled downstream.
pub const CONCEPTS: &[(&str, &str)] = &[
    // Income statement
    ("us-gaap", "Revenues"),
    ("us-gaap", "RevenueFromContractWithCustomerExcludingAssessedTax"),
    ("us-gaap", "SalesRevenueNet"),
    ("us-gaap", "CostOfRevenue"),
    ("us-gaap", "GrossProfit"),
    ("us-gaap", "ResearchAndDevelopmentExpense"),
    ("us-gaap", "OperatingIncomeLoss"),
    ("us-gaap", "NetIncomeLoss"),
    ("us-gaap", "EarningsPerShareBasic"),
    ("us-gaap", "EarningsPerShareDiluted"),
    ("us-gaap", "WeightedAverageNumberOfSharesOutstandingBasic"),
    ("us-gaap", "WeightedAverageNumberOfDilutedSharesOutstanding"),
    // Balance sheet
    ("us-gaap", "Assets"),
    ("us-gaap", "AssetsCurrent"),
    ("us-gaap", "Liabilities"),
    ("us-gaap", "LiabilitiesCurrent"),
    ("us-gaap", "StockholdersEquity"),
    ("us-gaap", "CashAndCashEquivalentsAtCarryingValue"),
    ("us-gaap", "LongTermDebt"),
    ("us-gaap", "LongTermDebtNoncurrent"),
    ("us-gaap", "InventoryNet"),
    // Cash flow
    ("us-gaap", "NetCashProvidedByUsedInOperatingActivities"),
    ("us-gaap", "PaymentsToAcquirePropertyPlantAndEquipment"),
    ("us-gaap", "PaymentsOfDividends"),
    ("us-gaap", "PaymentsForRepurchaseOfCommonStock"),
    // Shares
    ("dei", "EntityCommonStockSharesOutstanding"),
    ("us-gaap", "CommonStockSharesOutstanding"),
    // Foreign filers (20-F / 40-F) using IFRS
    ("ifrs-full", "Revenue"),
    ("ifrs-full", "ProfitLoss"),
    ("ifrs-full", "Assets"),
    ("ifrs-full", "Equity"),
];

/// Periodic reports. Current reports (8-K) and registration statements are left out.
pub const FORMS: &[&str] = &["10-K", "10-K/A", "10-Q", "10-Q/A", "20-F", "20-F/A", "40-F", "40-F/A"];

/// One reported value as of one filing.
#[derive(Clone, Debug, PartialEq)]
pub struct Fact {
    pub cik: u32,
    pub taxonomy: String,
    pub concept: String,
    pub unit: String,
    /// `None` for point-in-time (balance sheet) values.
    pub start: Option<NaiveDate>,
    pub end: NaiveDate,
    pub value: f64,
    pub fiscal_year: Option<i32>,
    pub fiscal_period: Option<String>,
    pub form: String,
    /// When the value became public. Use this, never `end`, for point-in-time work.
    pub filed: NaiveDate,
    pub accession: String,
    pub frame: Option<String>,
}

#[derive(Deserialize)]
struct CompanyFacts {
    cik: serde_json::Value,
    #[serde(default)]
    facts: HashMap<String, HashMap<String, ConceptFacts>>,
}

#[derive(Deserialize)]
struct ConceptFacts {
    #[serde(default)]
    units: HashMap<String, Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
struct RawFact {
    start: Option<NaiveDate>,
    end: NaiveDate,
    val: f64,
    accn: String,
    fy: Option<i32>,
    fp: Option<String>,
    form: String,
    filed: NaiveDate,
    frame: Option<String>,
}

/// Parsed facts for one company, plus how many entries of curated concepts were malformed and skipped.
pub struct Parsed {
    pub cik: u32,
    pub facts: Vec<Fact>,
    pub malformed: usize,
}

/// Extracts curated concepts from periodic reports in one `CIK##########.json` document. One malformed
/// entry is skipped rather than losing the whole company.
pub fn parse_company(json: &[u8]) -> Result<Parsed> {
    let doc: CompanyFacts = serde_json::from_slice(json).context("parsing company facts")?;
    let cik = match &doc.cik {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.parse::<u64>().ok(),
        _ => None,
    }
    .and_then(|c| u32::try_from(c).ok())
    .context("missing or invalid cik")?;

    let mut facts = Vec::new();
    let mut malformed = 0;
    for &(taxonomy, concept) in CONCEPTS {
        let Some(found) = doc.facts.get(taxonomy).and_then(|t| t.get(concept)) else {
            continue;
        };
        let mut units: Vec<(&String, &Vec<serde_json::Value>)> = found.units.iter().collect();
        units.sort_by_key(|(unit, _)| unit.as_str());
        for (unit, entries) in units {
            for entry in entries {
                let Ok(raw) = RawFact::deserialize(entry) else {
                    malformed += 1;
                    continue;
                };
                if !FORMS.contains(&raw.form.as_str()) || !raw.val.is_finite() {
                    continue;
                }
                facts.push(Fact {
                    cik,
                    taxonomy: taxonomy.to_string(),
                    concept: concept.to_string(),
                    unit: unit.clone(),
                    start: raw.start,
                    end: raw.end,
                    value: raw.val,
                    fiscal_year: raw.fy,
                    fiscal_period: raw.fp,
                    form: raw.form,
                    filed: raw.filed,
                    accession: raw.accn,
                    frame: raw.frame,
                });
            }
        }
    }
    Ok(Parsed { cik, facts, malformed })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A trimmed company facts document in the shape EDGAR serves.
    pub(crate) fn apple() -> String {
        r#"{
          "cik": 320193,
          "entityName": "Apple Inc.",
          "facts": {
            "dei": {
              "EntityCommonStockSharesOutstanding": {"label": "Shares", "units": {"shares": [
                {"end": "2024-10-18", "val": 15115823000, "accn": "0000320193-24-000123", "fy": 2024, "fp": "FY",
                 "form": "10-K", "filed": "2024-11-01", "frame": "CY2024Q3I"}
              ]}}
            },
            "us-gaap": {
              "Revenues": {"label": "Revenues", "units": {"USD": [
                {"start": "2023-10-01", "end": "2024-09-28", "val": 391035000000, "accn": "0000320193-24-000123",
                 "fy": 2024, "fp": "FY", "form": "10-K", "filed": "2024-11-01", "frame": "CY2024"},
                {"start": "2023-10-01", "end": "2024-09-28", "val": 391035000000, "accn": "0000320193-24-000999",
                 "fy": 2024, "fp": "FY", "form": "8-K", "filed": "2024-10-31"},
                {"start": "2023-10-01", "end": "2024-09-28", "val": "oops", "accn": "x", "form": "10-K", "filed": "2024-11-01"}
              ]}},
              "EarningsPerShareDiluted": {"label": "EPS", "units": {"USD/shares": [
                {"start": "2024-06-30", "end": "2024-09-28", "val": 0.97, "accn": "0000320193-24-000123",
                 "fy": 2024, "fp": "FY", "form": "10-K", "filed": "2024-11-01"},
                {"start": "2024-06-30", "end": "2024-09-28", "val": 0.98, "accn": "0000320193-25-000008",
                 "fy": 2025, "fp": "Q1", "form": "10-Q/A", "filed": "2025-01-31"}
              ]}},
              "SomethingUncurated": {"label": "x", "units": {"USD": [
                {"end": "2024-09-28", "val": 1, "accn": "a", "form": "10-K", "filed": "2024-11-01"}
              ]}}
            }
          }
        }"#
        .to_string()
    }

    #[test]
    fn keeps_curated_periodic_facts_with_filing_dates() {
        let parsed = parse_company(apple().as_bytes()).unwrap();
        assert_eq!(parsed.cik, 320193);
        assert_eq!(parsed.malformed, 1);

        let names: Vec<(&str, &str)> = parsed.facts.iter().map(|f| (f.concept.as_str(), f.form.as_str())).collect();
        assert_eq!(
            names,
            vec![
                ("Revenues", "10-K"),
                ("EarningsPerShareDiluted", "10-K"),
                ("EarningsPerShareDiluted", "10-Q/A"),
                ("EntityCommonStockSharesOutstanding", "10-K"),
            ]
        );
        let restated = &parsed.facts[2];
        assert_eq!(restated.value, 0.98);
        assert_eq!(restated.filed, NaiveDate::from_ymd_opt(2025, 1, 31).unwrap());
        assert_eq!(parsed.facts[3].start, None);
        assert_eq!(parsed.facts[0].unit, "USD");
    }

    #[test]
    fn rejects_documents_without_a_cik() {
        assert!(parse_company(br#"{"facts": {}}"#).is_err());
        assert!(parse_company(br#"{"cik": "abc", "facts": {}}"#).is_err());
        assert_eq!(parse_company(br#"{"cik": "0000000042", "facts": {}}"#).unwrap().cik, 42);
    }
}
