use std::collections::BTreeMap;

use chrono::NaiveDate;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use st_core::{Bar, Fill, Timestamp};

use crate::exchange_date;

/// Summary statistics. Ratios are `f64`: they are for reporting, not accounting.
#[derive(Clone, Debug, Default)]
pub struct Metrics {
    pub start_equity: Decimal,
    pub end_equity: Decimal,
    pub total_return: f64,
    pub cagr: f64,
    pub max_drawdown: f64,
    /// From daily returns, annualized with sqrt(252).
    pub sharpe: f64,
    pub num_fills: usize,
    pub commissions: Decimal,
}

/// One point (the last) per exchange date.
fn resample_daily(curve: &[(Timestamp, Decimal)]) -> Vec<(Timestamp, Decimal)> {
    let mut daily: Vec<(Timestamp, Decimal)> = Vec::new();
    for &(t, equity) in curve {
        match daily.last_mut() {
            Some(last) if exchange_date(last.0) == exchange_date(t) => *last = (t, equity),
            _ => daily.push((t, equity)),
        }
    }
    daily
}

/// Daily returns keyed by exchange date.
pub fn daily_returns(curve: &[(Timestamp, Decimal)]) -> BTreeMap<NaiveDate, f64> {
    let daily = resample_daily(curve);
    daily
        .windows(2)
        .filter_map(|w| {
            let (prev, cur) = (w[0].1.to_f64()?, w[1].1.to_f64()?);
            (prev > 0.0).then(|| (exchange_date(w[1].0), cur / prev - 1.0))
        })
        .collect()
}

/// Buy-and-hold equity for one symbol's bars (any order): `start_equity` invested at the first close,
/// valued at each later close. Uses the bars' (adjusted) closes, so dividends count as return.
pub fn buy_and_hold(bars: &[&Bar], start_equity: Decimal) -> Vec<(Timestamp, Decimal)> {
    let mut bars: Vec<&&Bar> = bars.iter().collect();
    bars.sort_by_key(|b| b.end);
    let Some(first) = bars.first().map(|b| b.close).filter(|c| *c > Decimal::ZERO) else {
        return Vec::new();
    };
    bars.iter().map(|b| (b.end, start_equity * b.close / first)).collect()
}

/// Pearson correlation of daily returns, computed only between consecutive dates both curves have (so a
/// day missing from one never pairs a two-day move with a one-day move). `None` with fewer than 3 such
/// returns or a flat series.
pub fn correlation(a: &[(Timestamp, Decimal)], b: &[(Timestamp, Decimal)]) -> Option<f64> {
    let by_date = |curve| -> BTreeMap<NaiveDate, f64> {
        resample_daily(curve).into_iter().filter_map(|(t, v)| Some((exchange_date(t), v.to_f64()?))).collect()
    };
    let (va, vb) = (by_date(a), by_date(b));
    let shared: Vec<(f64, f64)> = va.iter().filter_map(|(d, x)| vb.get(d).map(|y| (*x, *y))).collect();
    let pairs: Vec<(f64, f64)> = shared
        .windows(2)
        .filter(|w| w[0].0 > 0.0 && w[0].1 > 0.0)
        .map(|w| (w[1].0 / w[0].0 - 1.0, w[1].1 / w[0].1 - 1.0))
        .collect();
    if pairs.len() < 3 {
        return None;
    }
    let n = pairs.len() as f64;
    let (mx, my) = (pairs.iter().map(|p| p.0).sum::<f64>() / n, pairs.iter().map(|p| p.1).sum::<f64>() / n);
    let cov: f64 = pairs.iter().map(|(x, y)| (x - mx) * (y - my)).sum();
    let (vx, vy): (f64, f64) =
        (pairs.iter().map(|(x, _)| (x - mx).powi(2)).sum(), pairs.iter().map(|(_, y)| (y - my).powi(2)).sum());
    (vx > 0.0 && vy > 0.0).then(|| cov / (vx * vy).sqrt())
}

/// The curve is first resampled to one point (the last) per exchange date, so intraday bars and
/// non-bar events don't distort the daily statistics.
pub fn compute(curve: &[(Timestamp, Decimal)], fills: &[Fill]) -> Metrics {
    let daily = resample_daily(curve);
    let (Some(&(t0, first)), Some(&(t1, last))) = (daily.first(), daily.last()) else {
        return Metrics::default();
    };
    let values: Vec<f64> = daily.iter().map(|(_, e)| e.to_f64().unwrap_or(0.0)).collect();
    let start = values[0];
    let end = values[values.len() - 1];

    let total_return = if start > 0.0 { end / start - 1.0 } else { f64::NAN };
    let years = (t1 - t0).num_seconds() as f64 / (365.25 * 86_400.0);
    let cagr = if start <= 0.0 {
        f64::NAN
    } else if end <= 0.0 {
        -1.0
    } else if years > 0.0 {
        (end / start).powf(1.0 / years) - 1.0
    } else {
        total_return
    };

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
