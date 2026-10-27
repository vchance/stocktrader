//! Pre-trade risk checks. Every signal passes through [`RiskManager::check`] before it becomes an order.
//! Reducing exposure is always allowed; anything that adds exposure must clear every limit.

use std::collections::{HashMap, VecDeque};

use chrono::NaiveDate;
use rust_decimal::Decimal;
use rust_decimal::prelude::Signed;
use rust_decimal_macros::dec;
use st_core::{Event, EventKind, Portfolio, Signal, Symbol, Timestamp};

#[derive(Clone, Debug)]
pub struct RiskLimits {
    /// Largest fraction of equity any single position may reach.
    pub max_position_weight: Decimal,
    /// Largest sum of absolute position values as a fraction of equity (1.0 = no leverage).
    pub max_gross_exposure: Decimal,
    /// Halt new exposure for the rest of the day once equity falls this fraction below the day's start.
    pub max_daily_loss: Decimal,
    /// Skip penny stocks, which are the main target of social-media pump-and-dumps.
    pub min_price: Decimal,
    /// Minimum average daily dollar volume over `liquidity_lookback` bars.
    pub min_avg_dollar_volume: Decimal,
    pub liquidity_lookback: usize,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_position_weight: dec!(0.10),
            max_gross_exposure: dec!(1.0),
            max_daily_loss: dec!(0.03),
            min_price: dec!(5),
            min_avg_dollar_volume: dec!(5_000_000),
            liquidity_lookback: 20,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RiskDecision {
    /// Possibly with `target_weight` clamped down to fit the limits.
    Approve(Signal),
    Reject { signal: Signal, reason: String },
}

pub struct RiskManager {
    limits: RiskLimits,
    kill_reason: Option<String>,
    day_start: Option<(NaiveDate, Decimal)>,
    dollar_volume: HashMap<Symbol, VecDeque<Decimal>>,
}

impl RiskManager {
    pub fn new(limits: RiskLimits) -> Self {
        Self { limits, kill_reason: None, day_start: None, dollar_volume: HashMap::new() }
    }

    pub fn limits(&self) -> &RiskLimits {
        &self.limits
    }

    /// Kill switch: block all new exposure until [`RiskManager::reset_kill`] is called.
    pub fn kill(&mut self, reason: impl Into<String>) {
        self.kill_reason = Some(reason.into());
    }

    pub fn reset_kill(&mut self) {
        self.kill_reason = None;
    }

    pub fn is_killed(&self) -> bool {
        self.kill_reason.is_some()
    }

    pub fn on_market_event(&mut self, event: &Event) {
        if let EventKind::Bar(bar) = &event.kind {
            let window = self.dollar_volume.entry(bar.symbol.clone()).or_default();
            window.push_back(bar.close * bar.volume);
            if window.len() > self.limits.liquidity_lookback {
                window.pop_front();
            }
        }
    }

    /// `None` until a full lookback window has been seen.
    pub fn avg_dollar_volume(&self, symbol: &Symbol) -> Option<Decimal> {
        let window = self.dollar_volume.get(symbol)?;
        if window.len() < self.limits.liquidity_lookback {
            return None;
        }
        Some(window.iter().sum::<Decimal>() / Decimal::from(window.len()))
    }

    pub fn check(&mut self, mut signal: Signal, portfolio: &Portfolio, now: Timestamp) -> RiskDecision {
        let equity = portfolio.equity();
        let today = now.date_naive();
        if self.day_start.is_none_or(|(day, _)| day != today) {
            self.day_start = Some((today, equity));
        }

        let current = portfolio.weight(&signal.symbol);
        let target = signal.target_weight;
        let reducing =
            target.abs() <= current.abs() && (target.is_zero() || target.signum() == current.signum());
        if reducing {
            return RiskDecision::Approve(signal);
        }

        let reject = |signal: Signal, reason: String| RiskDecision::Reject { signal, reason };

        if let Some(reason) = &self.kill_reason {
            return reject(signal, format!("kill switch engaged: {reason}"));
        }
        if let Some((_, start)) = self.day_start
            && equity < start * (Decimal::ONE - self.limits.max_daily_loss)
        {
            return reject(signal, "daily loss limit reached".into());
        }
        if target < Decimal::ZERO {
            return reject(signal, "short selling is disabled".into());
        }
        match portfolio.last_price(&signal.symbol) {
            None => return reject(signal, "no price available".into()),
            Some(p) if p < self.limits.min_price => {
                return reject(signal, format!("price {p} below minimum {}", self.limits.min_price));
            }
            Some(_) => {}
        }
        match self.avg_dollar_volume(&signal.symbol) {
            None => return reject(signal, "insufficient liquidity history".into()),
            Some(adv) if adv < self.limits.min_avg_dollar_volume => {
                return reject(signal, format!("avg dollar volume {} below minimum", adv.round()));
            }
            Some(_) => {}
        }

        let mut allowed = target.min(self.limits.max_position_weight);
        if equity > Decimal::ZERO {
            let other_gross =
                (portfolio.gross_exposure() - portfolio.market_value(&signal.symbol).abs()) / equity;
            allowed = allowed.min(self.limits.max_gross_exposure - other_gross);
        }
        if allowed <= current {
            return reject(signal, "gross exposure limit reached".into());
        }
        signal.target_weight = allowed;
        RiskDecision::Approve(signal)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};
    use st_core::{Bar, Fill, OrderId, Side};

    use super::*;

    fn t0() -> Timestamp {
        Utc.with_ymd_and_hms(2026, 1, 5, 21, 0, 0).unwrap()
    }

    fn bar(symbol: &str, close: Decimal, volume: Decimal, i: i64) -> Event {
        let end = t0() + Duration::days(i);
        Event::bar(Bar {
            symbol: Symbol::new(symbol),
            start: end - Duration::hours(6),
            end,
            open: close,
            high: close,
            low: close,
            close,
            volume,
        })
    }

    fn signal(symbol: &str, w: Decimal) -> Signal {
        Signal { symbol: Symbol::new(symbol), target_weight: w, reason: "test".into() }
    }

    fn warmed_up(close: Decimal, volume: Decimal) -> (RiskManager, Portfolio) {
        let mut risk = RiskManager::new(RiskLimits::default());
        let mut portfolio = Portfolio::new(dec!(100_000));
        for i in 0..20 {
            let ev = bar("ABC", close, volume, i);
            risk.on_market_event(&ev);
        }
        portfolio.mark(&Symbol::new("ABC"), close);
        (risk, portfolio)
    }

    #[test]
    fn clamps_to_max_position_weight() {
        let (mut risk, portfolio) = warmed_up(dec!(50), dec!(1_000_000));
        match risk.check(signal("ABC", dec!(0.5)), &portfolio, t0()) {
            RiskDecision::Approve(s) => assert_eq!(s.target_weight, dec!(0.10)),
            other => panic!("expected approval, got {other:?}"),
        }
    }

    #[test]
    fn rejects_penny_stocks_and_illiquid_names() {
        let (mut risk, portfolio) = warmed_up(dec!(2), dec!(100_000_000));
        assert!(matches!(risk.check(signal("ABC", dec!(0.05)), &portfolio, t0()), RiskDecision::Reject { .. }));

        let (mut risk, portfolio) = warmed_up(dec!(50), dec!(1_000));
        assert!(matches!(risk.check(signal("ABC", dec!(0.05)), &portfolio, t0()), RiskDecision::Reject { .. }));
    }

    #[test]
    fn kill_switch_blocks_new_exposure_but_allows_exits() {
        let (mut risk, mut portfolio) = warmed_up(dec!(50), dec!(1_000_000));
        portfolio.apply_fill(&Fill {
            order_id: OrderId(1),
            symbol: Symbol::new("ABC"),
            side: Side::Buy,
            qty: dec!(100),
            price: dec!(50),
            commission: Decimal::ZERO,
            at: t0(),
        });
        risk.kill("manual");
        assert!(matches!(risk.check(signal("ABC", dec!(0.10)), &portfolio, t0()), RiskDecision::Reject { .. }));
        assert!(matches!(risk.check(signal("ABC", Decimal::ZERO), &portfolio, t0()), RiskDecision::Approve(_)));
    }
}
