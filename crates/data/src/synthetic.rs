use std::f64::consts::PI;

use chrono::{Datelike, Duration, NaiveDate, Weekday};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use st_core::{Bar, Symbol};

use crate::csv_bars::daily_bar;

/// Deterministic random-walk daily bars for exercising the engine without market data.
#[derive(Clone, Debug)]
pub struct SyntheticConfig {
    pub symbols: usize,
    pub days: usize,
    pub start: NaiveDate,
    pub seed: u64,
}

pub fn generate_daily_bars(cfg: &SyntheticConfig) -> Vec<Bar> {
    let mut rng = Rng::new(cfg.seed);
    let dates = trading_days(cfg.start, cfg.days);
    let mut bars = Vec::with_capacity(cfg.symbols * dates.len());

    for i in 0..cfg.symbols {
        let symbol = Symbol::new(format!("SYN{i:03}"));
        let mut close = 10.0 + rng.uniform() * 190.0;
        let drift = (rng.uniform() - 0.45) * 0.002;
        let vol = 0.01 + rng.uniform() * 0.025;
        let base_volume = 200_000.0 + rng.uniform() * 4_800_000.0;

        for &date in &dates {
            let open = close * (rng.normal() * vol * 0.3).exp();
            close = open * (drift + rng.normal() * vol).exp();
            let high = open.max(close) * (1.0 + rng.normal().abs() * vol * 0.5);
            let low = open.min(close) * (1.0 - rng.normal().abs() * vol * 0.5);
            let volume = base_volume * (0.5 + rng.uniform());
            bars.push(daily_bar(
                symbol.clone(),
                date,
                money(open),
                money(high),
                money(low),
                money(close),
                Decimal::from_f64(volume.round()).unwrap_or_default(),
            ));
        }
    }
    bars
}

fn money(x: f64) -> Decimal {
    Decimal::from_f64(x).unwrap_or_default().round_dp(2)
}

/// Weekdays only; exchange holidays are ignored for synthetic data.
fn trading_days(start: NaiveDate, n: usize) -> Vec<NaiveDate> {
    let mut out = Vec::with_capacity(n);
    let mut d = start;
    while out.len() < n {
        if !matches!(d.weekday(), Weekday::Sat | Weekday::Sun) {
            out.push(d);
        }
        d += Duration::days(1);
    }
    out
}

/// xorshift64*: small, fast and reproducible, which is all synthetic data needs.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self((seed ^ 0x9E37_79B9_7F4A_7C15).max(1))
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn normal(&mut self) -> f64 {
        let u1 = self.uniform().max(f64::MIN_POSITIVE);
        let u2 = self.uniform();
        (-2.0 * u1.ln()).sqrt() * (2.0 * PI * u2).cos()
    }
}
