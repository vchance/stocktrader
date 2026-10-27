use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use st_core::{Fill, Timestamp};

/// Summary statistics. Ratios are `f64`: they are for reporting, not accounting.
#[derive(Clone, Debug, Default)]
pub struct Metrics {
    pub start_equity: Decimal,
    pub end_equity: Decimal,
    pub total_return: f64,
    pub cagr: f64,
    pub max_drawdown: f64,
    /// Annualized assuming one equity point per trading day (sqrt(252)).
    pub sharpe: f64,
    pub num_fills: usize,
    pub commissions: Decimal,
}

pub fn compute(curve: &[(Timestamp, Decimal)], fills: &[Fill]) -> Metrics {
    let (Some(&(t0, first)), Some(&(t1, last))) = (curve.first(), curve.last()) else {
        return Metrics::default();
    };
    let values: Vec<f64> = curve.iter().map(|(_, e)| e.to_f64().unwrap_or(0.0)).collect();
    let start = values[0];
    let end = values[values.len() - 1];

    let total_return = if start > 0.0 { end / start - 1.0 } else { 0.0 };
    let years = (t1 - t0).num_seconds() as f64 / (365.25 * 86_400.0);
    let cagr = if years > 0.0 && start > 0.0 && end > 0.0 { (end / start).powf(1.0 / years) - 1.0 } else { total_return };

    let mut peak = f64::MIN;
    let mut max_drawdown = 0.0_f64;
    for &v in &values {
        peak = peak.max(v);
        if peak > 0.0 {
            max_drawdown = max_drawdown.max(1.0 - v / peak);
        }
    }

    let returns: Vec<f64> = values.windows(2).filter(|w| w[0] > 0.0).map(|w| w[1] / w[0] - 1.0).collect();
    let sharpe = if returns.len() > 1 {
        let n = returns.len() as f64;
        let mean = returns.iter().sum::<f64>() / n;
        let var = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
        if var > 0.0 { mean / var.sqrt() * 252f64.sqrt() } else { 0.0 }
    } else {
        0.0
    };

    Metrics {
        start_equity: first,
        end_equity: last,
        total_return,
        cagr,
        max_drawdown,
        sharpe,
        num_fills: fills.len(),
        commissions: fills.iter().map(|f| f.commission).sum(),
    }
}
