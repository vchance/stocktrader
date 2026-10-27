//! Pre-trade risk checks. Every signal passes through [`RiskManager::check`] before it becomes an order.
//! Reducing exposure is always allowed; anything that adds exposure must clear every limit.

use std::collections::{HashMap, VecDeque};

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use st_core::{Event, EventKind, OrderId, OrderRequest, Portfolio, Signal, Symbol};

#[derive(Clone, Debug)]
pub struct RiskLimits {
    /// Largest fraction of equity any single position may reach.
    pub max_position_weight: Decimal,
    /// Largest sum of absolute position values as a fraction of equity (1.0 = no leverage).
    pub max_gross_exposure: Decimal,
    /// Halt new exposure for the rest of the session once equity falls this fraction below the session start.
    pub max_daily_loss: Decimal,
    /// Skip penny stocks, which are the main target of social-media pump-and-dumps.
    pub min_price: Decimal,
    /// Minimum average daily dollar volume over `liquidity_lookback` bars.
    pub min_avg_dollar_volume: Decimal,
    /// Bars in the liquidity window; values below 1 are treated as 1.
    pub liquidity_lookback: usize,
    /// Reject entries for a symbol whose latest bar has no as-traded close, instead of falling back to the
    /// (possibly adjusted) close. Set when traded prices are supplied, so a missing one fails closed.
    pub require_traded_price: bool,
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
            require_traded_price: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RiskDecision {
    /// Possibly with `target_weight` clamped to fit the limits.
    Approve(Signal),
    Reject { signal: Signal, reason: String },
}

pub struct RiskManager {
    limits: RiskLimits,
    kill_reason: Option<String>,
    session_start_equity: Option<Decimal>,
    dollar_volume: HashMap<Symbol, VecDeque<Decimal>>,
    /// Latest bar's as-traded close per symbol (`None` if that bar didn't carry one), for price filters.
    traded_close: HashMap<Symbol, Option<Decimal>>,
}

impl RiskManager {
    pub fn new(mut limits: RiskLimits) -> Self {
        limits.liquidity_lookback = limits.liquidity_lookback.max(1);
        Self {
            limits,
            kill_reason: None,
            session_start_equity: None,
            dollar_volume: HashMap::new(),
            traded_close: HashMap::new(),
        }
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

    /// Called by the runner when a new exchange session begins, before any of that session's data is
    /// applied, with equity as of the previous close. The daily loss limit is measured from here.
    pub fn start_session(&mut self, equity: Decimal) {
        self.session_start_equity = Some(equity);
    }

    pub fn on_market_event(&mut self, event: &Event) {
        if let EventKind::Bar(bar) = &event.kind {
            self.traded_close.insert(bar.symbol.clone(), bar.traded_close);
            // Splits scale adjusted price and volume inversely, so their product is the traded dollar
            // volume; dividend adjustment lowers older prices only, so older dollar volume reads slightly
            // low (conservative: it can only over-reject).
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

    /// `open_orders` are counted as if already filled, so signals in the same bar cannot each
    /// claim the same headroom.
    pub fn check(
        &mut self,
        mut signal: Signal,
        portfolio: &Portfolio,
        open_orders: &[(OrderId, OrderRequest)],
    ) -> RiskDecision {
        if signal.target_weight < Decimal::ZERO {
            // Long-only for now: a short target means "get out", not "flip".
            signal.target_weight = Decimal::ZERO;
            signal.reason.push_str(" (shorting disabled; clamped to flat)");
        }

        let equity = portfolio.equity();
        let value = portfolio.projected_value(&signal.symbol, open_orders);
        let current = if equity > Decimal::ZERO { value / equity } else { Decimal::ZERO };
        let target = signal.target_weight;
        let reducing = target.is_zero() || (current > Decimal::ZERO && target <= current);
        if reducing {
            return RiskDecision::Approve(signal);
        }

        let reject = |signal: Signal, reason: String| RiskDecision::Reject { signal, reason };

        if equity <= Decimal::ZERO {
            return reject(signal, "equity is not positive".into());
        }
        if let Some(reason) = &self.kill_reason {
            return reject(signal, format!("kill switch engaged: {reason}"));
        }
        if let Some(start) = self.session_start_equity
            && equity < start * (Decimal::ONE - self.limits.max_daily_loss)
        {
            return reject(signal, "daily loss limit reached".into());
        }
        if portfolio.last_price(&signal.symbol).is_none() {
            return reject(signal, "no price available".into());
        }
        // The level filter uses the price the stock actually traded at, not split-adjusted history.
        let traded = match self.traded_close.get(&signal.symbol).copied().flatten() {
            Some(p) => Some(p),
            None if self.limits.require_traded_price => {
                return reject(signal, "no traded price for this symbol; refusing to judge on adjusted prices".into());
            }
            None => portfolio.last_price(&signal.symbol),
        };
        if let Some(p) = traded
            && p < self.limits.min_price
        {
            return reject(signal, format!("traded price {p} below minimum {}", self.limits.min_price));
        }
        match self.avg_dollar_volume(&signal.symbol) {
            None => return reject(signal, "insufficient liquidity history".into()),
            Some(adv) if adv < self.limits.min_avg_dollar_volume => {
                return reject(signal, format!("avg dollar volume {} below minimum", adv.round()));
            }
            Some(_) => {}
        }

        let capped = target.min(self.limits.max_position_weight);
        if capped <= current {
            return reject(signal, "position already at max weight".into());
        }
        let other_gross = (portfolio.projected_gross_exposure(open_orders) - value.abs()) / equity;
        let allowed = capped.min(self.limits.max_gross_exposure - other_gross);
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
    use st_core::{Bar, Fill, OrderType, Side, Timestamp};

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
            traded_close: None,
        })
    }

    fn signal(symbol: &str, w: Decimal) -> Signal {
        Signal { symbol: Symbol::new(symbol), target_weight: w, reason: "test".into() }
    }

    fn buy(portfolio: &mut Portfolio, symbol: &str, qty: Decimal, price: Decimal) {
        portfolio.apply_fill(&Fill {
            order_id: OrderId(1),
            symbol: Symbol::new(symbol),
            side: Side::Buy,
            qty,
            price,
            commission: Decimal::ZERO,
            at: t0(),
        });
    }

    fn warmed_up(symbols: &[&str], close: Decimal, volume: Decimal) -> (RiskManager, Portfolio) {
        let mut risk = RiskManager::new(RiskLimits::default());
        let mut portfolio = Portfolio::new(dec!(100_000));
        for s in symbols {
            for i in 0..20 {
                risk.on_market_event(&bar(s, close, volume, i));
            }
            portfolio.mark(&Symbol::new(*s), close);
        }
        risk.start_session(portfolio.equity());
        (risk, portfolio)
    }

    fn approved_weight(decision: RiskDecision) -> Decimal {
        match decision {
            RiskDecision::Approve(s) => s.target_weight,
            other => panic!("expected approval, got {other:?}"),
        }
    }

    #[test]
    fn clamps_to_max_position_weight() {
        let (mut risk, portfolio) = warmed_up(&["ABC"], dec!(50), dec!(1_000_000));
        assert_eq!(approved_weight(risk.check(signal("ABC", dec!(0.5)), &portfolio, &[])), dec!(0.10));
    }

    #[test]
    fn price_filter_uses_traded_not_adjusted_price() {
        let with_traded = |adjusted: Decimal, traded: Decimal| {
            let mut risk = RiskManager::new(RiskLimits::default());
            let mut portfolio = Portfolio::new(dec!(100_000));
            for i in 0..20 {
                let mut event = bar("ABC", adjusted, dec!(10_000_000), i);
                if let EventKind::Bar(b) = &mut event.kind {
                    b.traded_close = Some(traded);
                }
                risk.on_market_event(&event);
            }
            portfolio.mark(&Symbol::new("ABC"), adjusted);
            risk.check(signal("ABC", dec!(0.05)), &portfolio, &[])
        };
        // Traded at $30 before a later 10:1 split: adjusted history shows $3, but it was no penny stock.
        assert!(matches!(with_traded(dec!(3), dec!(30)), RiskDecision::Approve(_)));
        // Traded at $3 before a later 1:10 reverse split: adjusted history shows $30.
        assert!(matches!(with_traded(dec!(30), dec!(3)), RiskDecision::Reject { .. }));
    }

    #[test]
    fn missing_traded_price_fails_closed_when_required() {
        let (mut lenient, portfolio) = warmed_up(&["ABC"], dec!(30), dec!(10_000_000));
        assert!(matches!(lenient.check(signal("ABC", dec!(0.05)), &portfolio, &[]), RiskDecision::Approve(_)));

        let mut strict = RiskManager::new(RiskLimits { require_traded_price: true, ..RiskLimits::default() });
        for i in 0..20 {
            strict.on_market_event(&bar("ABC", dec!(30), dec!(10_000_000), i));
        }
        let decision = strict.check(signal("ABC", dec!(0.05)), &portfolio, &[]);
        assert!(matches!(decision, RiskDecision::Reject { ref reason, .. } if reason.contains("no traded price")));
    }

    #[test]
    fn rejects_penny_stocks_and_illiquid_names() {
        let (mut risk, portfolio) = warmed_up(&["ABC"], dec!(2), dec!(100_000_000));
        assert!(matches!(risk.check(signal("ABC", dec!(0.05)), &portfolio, &[]), RiskDecision::Reject { .. }));

        let (mut risk, portfolio) = warmed_up(&["ABC"], dec!(50), dec!(1_000));
        assert!(matches!(risk.check(signal("ABC", dec!(0.05)), &portfolio, &[]), RiskDecision::Reject { .. }));
    }

    #[test]
    fn kill_switch_blocks_new_exposure_but_allows_exits() {
        let (mut risk, mut portfolio) = warmed_up(&["ABC"], dec!(50), dec!(1_000_000));
        buy(&mut portfolio, "ABC", dec!(100), dec!(50));
        risk.kill("manual");
        assert!(matches!(risk.check(signal("ABC", dec!(0.10)), &portfolio, &[]), RiskDecision::Reject { .. }));
        assert!(matches!(risk.check(signal("ABC", Decimal::ZERO), &portfolio, &[]), RiskDecision::Approve(_)));
    }

    #[test]
    fn pending_orders_count_toward_gross_exposure() {
        let names: Vec<String> = (0..12).map(|i| format!("S{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let (mut risk, portfolio) = warmed_up(&refs, dec!(50), dec!(1_000_000));

        // Ten pending buys of 10% each already use the whole 1.0 gross budget.
        let open: Vec<(OrderId, OrderRequest)> = refs[..10]
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let req = OrderRequest {
                    symbol: Symbol::new(*s),
                    side: Side::Buy,
                    qty: dec!(200),
                    order_type: OrderType::Market,
                    reason: "test".into(),
                };
                (OrderId(i as u64), req)
            })
            .collect();
        let decision = risk.check(signal("S10", dec!(0.10)), &portfolio, &open);
        assert!(matches!(decision, RiskDecision::Reject { ref reason, .. } if reason.contains("gross")));
    }

    #[test]
    fn daily_loss_limit_measured_from_session_start() {
        let (mut risk, mut portfolio) = warmed_up(&["ABC", "XYZ"], dec!(50), dec!(1_000_000));
        buy(&mut portfolio, "ABC", dec!(1_000), dec!(50));
        risk.start_session(portfolio.equity());
        portfolio.mark(&Symbol::new("ABC"), dec!(40)); // -10,000 on 100,000 = -10%
        let decision = risk.check(signal("XYZ", dec!(0.05)), &portfolio, &[]);
        assert!(matches!(decision, RiskDecision::Reject { ref reason, .. } if reason.contains("daily loss")));
    }

    #[test]
    fn short_target_closes_long_instead_of_being_rejected() {
        let (mut risk, mut portfolio) = warmed_up(&["ABC"], dec!(50), dec!(1_000_000));
        buy(&mut portfolio, "ABC", dec!(100), dec!(50));
        assert_eq!(approved_weight(risk.check(signal("ABC", dec!(-0.05)), &portfolio, &[])), Decimal::ZERO);
    }

    #[test]
    fn rejects_new_exposure_when_equity_not_positive() {
        let (mut risk, _) = warmed_up(&["ABC"], dec!(50), dec!(1_000_000));
        let broke = Portfolio::new(Decimal::ZERO);
        let mut broke = broke;
        broke.mark(&Symbol::new("ABC"), dec!(50));
        assert!(matches!(risk.check(signal("ABC", dec!(0.05)), &broke, &[]), RiskDecision::Reject { .. }));
    }
}
