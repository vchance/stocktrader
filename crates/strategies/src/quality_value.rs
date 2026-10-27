//! A cross-sectional strategy on filed financials: cheap and profitable companies, rebalanced monthly.

use std::collections::{BTreeMap, HashMap, VecDeque};

use chrono::{Datelike, NaiveDate, TimeDelta};
use chrono_tz::America::New_York;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use st_core::{Event, EventKind, FundamentalRecord, Signal, Strategy, StrategyContext, Symbol, Timestamp};

/// Annual net income, the only income figure used: quarterly and year-to-date values would need
/// stitching into a trailing twelve months.
const NET_INCOME: &str = "us-gaap:NetIncomeLoss:USD";
const ASSETS: &str = "us-gaap:Assets:USD";
/// Shares outstanding, from the cover page or the balance sheet; the latest period wins.
const SHARES: [&str; 2] = ["dei:EntityCommonStockSharesOutstanding:shares", "us-gaap:CommonStockSharesOutstanding:shares"];
/// A fiscal year, allowing for 52/53-week years.
const ANNUAL_DAYS: std::ops::RangeInclusive<i64> = 350..=380;

#[derive(Clone, Debug, Default)]
struct Stock {
    traded_close: Option<Decimal>,
    dollar_volume: VecDeque<Decimal>,
    /// Latest fiscal year's net income, as (period end, value).
    annual_income: Option<(NaiveDate, Decimal)>,
    assets: Option<(NaiveDate, Decimal)>,
    shares: Option<(NaiveDate, Decimal)>,
}

/// Keeps the value for the latest period; for the same period, the later-arriving (restated) value.
fn update(slot: &mut Option<(NaiveDate, Decimal)>, period_end: NaiveDate, value: Decimal) {
    if slot.is_none_or(|(end, _)| period_end >= end) {
        *slot = Some((period_end, value));
    }
}

/// Holds the `top_n` best-ranked stocks at equal weight, re-ranked on the first session of each month.
///
/// Each eligible stock is ranked on two measures from its latest filings, and the ranks are added:
/// - earnings yield: latest annual net income / market cap (shares outstanding x as-traded price);
/// - return on assets: latest annual net income / total assets.
///
/// Eligible means: an as-traded price of at least `min_price`, average daily dollar volume of at least
/// `min_dollar_volume` over the last `liquidity_lookback` bars, positive assets and shares, and each figure
/// from a period that ended within `max_report_age`. Only facts already public reach the strategy (the
/// engine releases them the day after filing), so there is no look-ahead.
///
/// Known simplifications: annual figures only (up to ~15 months old); a split after the latest share count
/// misstates market cap until the next filing; for multi-class issuers the share count may cover one class.
/// After a rebalance, dropped names are restated at 0 until the next one.
pub struct QualityValue {
    top_n: usize,
    weight: Decimal,
    min_price: Decimal,
    min_dollar_volume: Decimal,
    liquidity_lookback: usize,
    max_report_age: TimeDelta,
    stocks: HashMap<Symbol, Stock>,
    /// (year, month) of the last rebalance, New York time.
    rebalanced: Option<(i32, u32)>,
    saw_bars: bool,
    targets: BTreeMap<Symbol, (Decimal, String)>,
}

impl QualityValue {
    /// Fundamental concepts this strategy reads, for loaders that want to read nothing else.
    pub const CONCEPTS: &[&str] = &["NetIncomeLoss", "Assets", "EntityCommonStockSharesOutstanding", "CommonStockSharesOutstanding"];

    /// Equal weight `1 / top_n`, so fully invested once `top_n` names qualify. Filters default to the
    /// risk manager's: $5 and $5M average dollar volume over 20 bars.
    pub fn new(top_n: usize) -> Result<Self, String> {
        if top_n == 0 {
            return Err("top_n must be at least 1".into());
        }
        Ok(Self {
            top_n,
            weight: Decimal::ONE / Decimal::from(top_n),
            min_price: dec!(5),
            min_dollar_volume: dec!(5_000_000),
            liquidity_lookback: 20,
            max_report_age: TimeDelta::days(456),
            stocks: HashMap::new(),
            rebalanced: None,
            saw_bars: false,
            targets: BTreeMap::new(),
        })
    }

    fn on_fundamental(&mut self, r: &FundamentalRecord) {
        let stock = self.stocks.entry(r.symbol.clone()).or_default();
        match r.metric.as_str() {
            NET_INCOME => {
                if let Some(start) = r.period_start
                    && ANNUAL_DAYS.contains(&(r.period_end - start).num_days())
                {
                    update(&mut stock.annual_income, r.period_end, r.value);
                }
            }
            ASSETS => update(&mut stock.assets, r.period_end, r.value),
            m if SHARES.contains(&m) => update(&mut stock.shares, r.period_end, r.value),
            _ => {}
        }
    }

    /// (symbol, earnings yield, return on assets) for every eligible stock on `today`.
    fn candidates(&self, today: NaiveDate) -> Vec<(Symbol, Decimal, Decimal)> {
        let fresh = |slot: Option<(NaiveDate, Decimal)>| slot.filter(|(end, _)| today - *end <= self.max_report_age).map(|(_, v)| v);
        let lookback = Decimal::from(self.liquidity_lookback);
        let mut out = Vec::new();
        for (symbol, s) in &self.stocks {
            let (Some(price), Some(income), Some(assets), Some(shares)) =
                (s.traded_close, fresh(s.annual_income), fresh(s.assets), fresh(s.shares))
            else {
                continue;
            };
            let liquid = s.dollar_volume.len() >= self.liquidity_lookback
                && s.dollar_volume.iter().sum::<Decimal>() / lookback >= self.min_dollar_volume;
            if price < self.min_price || !liquid || assets <= Decimal::ZERO || shares <= Decimal::ZERO {
                continue;
            }
            out.push((symbol.clone(), income / (shares * price), income / assets));
        }
        out
    }

    fn rebalance(&mut self, today: NaiveDate) {
        let candidates = self.candidates(today);
        // Rank 0 is the lowest value; ties broken by symbol so results don't depend on hash order.
        let ranks = |key: fn(&(Symbol, Decimal, Decimal)) -> Decimal| -> HashMap<Symbol, usize> {
            let mut sorted: Vec<&(Symbol, Decimal, Decimal)> = candidates.iter().collect();
            sorted.sort_by(|a, b| key(a).cmp(&key(b)).then(a.0.cmp(&b.0)));
            sorted.into_iter().enumerate().map(|(i, c)| (c.0.clone(), i)).collect()
        };
        let (by_yield, by_roa) = (ranks(|c| c.1), ranks(|c| c.2));
        let mut scored: Vec<(usize, &(Symbol, Decimal, Decimal))> =
            candidates.iter().map(|c| (by_yield[&c.0] + by_roa[&c.0], c)).collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.0.cmp(&b.1.0)));

        let mut targets = BTreeMap::new();
        for (i, (_, (symbol, ey, roa))) in scored.iter().take(self.top_n).enumerate() {
            let reason = format!(
                "rank {} of {} on {today}: earnings yield {}%, ROA {}%",
                i + 1,
                scored.len(),
                (ey * Decimal::ONE_HUNDRED).round_dp(1),
                (roa * Decimal::ONE_HUNDRED).round_dp(1)
            );
            targets.insert(symbol.clone(), (self.weight, reason));
        }
        for (symbol, (weight, _)) in &self.targets {
            if !weight.is_zero() && !targets.contains_key(symbol) {
                targets.insert(symbol.clone(), (Decimal::ZERO, format!("dropped from the top {} on {today}", self.top_n)));
            }
        }
        self.targets = targets;
    }
}

fn new_york_date(t: Timestamp) -> NaiveDate {
    t.with_timezone(&New_York).date_naive()
}

impl Strategy for QualityValue {
    fn name(&self) -> &str {
        "quality_value"
    }

    fn on_event(&mut self, event: &Event, _ctx: &StrategyContext<'_>) -> Vec<Signal> {
        match &event.kind {
            EventKind::Bar(bar) => {
                self.saw_bars = true;
                let stock = self.stocks.entry(bar.symbol.clone()).or_default();
                stock.traded_close = Some(bar.traded_close());
                // Same measure as the risk manager's liquidity filter.
                stock.dollar_volume.push_back(bar.close * bar.volume);
                if stock.dollar_volume.len() > self.liquidity_lookback {
                    stock.dollar_volume.pop_front();
                }
            }
            EventKind::Fundamental(r) => self.on_fundamental(r),
            _ => {}
        }
        Vec::new()
    }

    /// Re-ranks on the first session of a month, and after every session restates the current targets.
    fn after_batch(&mut self, ctx: &StrategyContext<'_>) -> Vec<Signal> {
        if !std::mem::take(&mut self.saw_bars) {
            return Vec::new();
        }
        let today = new_york_date(ctx.now);
        let month = (today.year(), today.month());
        if self.rebalanced != Some(month) {
            self.rebalanced = Some(month);
            self.rebalance(today);
        }
        self.targets
            .iter()
            .map(|(symbol, (weight, reason))| Signal { symbol: symbol.clone(), target_weight: *weight, reason: reason.clone() })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Days, TimeZone};
    use st_core::{Bar, Portfolio};

    use super::*;

    fn day(n: u64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2025, 1, 2).unwrap() + Days::new(n)
    }

    fn bar(symbol: &str, d: NaiveDate, price: Decimal, volume: Decimal) -> Event {
        let mut b: Bar = st_data_bar(symbol, d, price, volume);
        b.traded_close = Some(price);
        Event::bar(b)
    }

    fn st_data_bar(symbol: &str, d: NaiveDate, price: Decimal, volume: Decimal) -> Bar {
        let close = New_York.from_local_datetime(&d.and_hms_opt(16, 0, 0).unwrap()).single().unwrap().to_utc();
        Bar {
            symbol: Symbol::new(symbol),
            start: close - TimeDelta::hours(7),
            end: close,
            open: price,
            high: price,
            low: price,
            close: price,
            volume,
            traded_close: None,
        }
    }

    fn fact(symbol: &str, metric: &str, start: Option<NaiveDate>, end: NaiveDate, value: Decimal, filed: NaiveDate) -> Event {
        Event {
            available_at: New_York.from_local_datetime(&filed.and_hms_opt(23, 0, 0).unwrap()).single().unwrap().to_utc(),
            kind: EventKind::Fundamental(FundamentalRecord {
                symbol: Symbol::new(symbol),
                period_start: start,
                period_end: end,
                metric: metric.into(),
                value,
            }),
        }
    }

    /// Annual income, assets and shares for `symbol`, filed on `filed` for the year ending 2024-12-31.
    fn financials(symbol: &str, income: Decimal, assets: Decimal, shares: Decimal, filed: NaiveDate) -> Vec<Event> {
        let (start, end) = (NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(), NaiveDate::from_ymd_opt(2024, 12, 31).unwrap());
        vec![
            fact(symbol, NET_INCOME, Some(start), end, income, filed),
            fact(symbol, ASSETS, None, end, assets, filed),
            fact(symbol, SHARES[0], None, end, shares, filed),
        ]
    }

    /// Runs events grouped by timestamp, as the engine does, returning the last restated targets.
    fn run(strategy: &mut QualityValue, mut events: Vec<Event>) -> BTreeMap<String, Decimal> {
        events.sort_by_key(|e| e.available_at);
        let portfolio = Portfolio::new(Decimal::ZERO);
        let mut last = BTreeMap::new();
        let mut i = 0;
        while i < events.len() {
            let now = events[i].available_at;
            let ctx = StrategyContext { now, portfolio: &portfolio };
            while i < events.len() && events[i].available_at == now {
                assert!(strategy.on_event(&events[i], &ctx).is_empty());
                i += 1;
            }
            let signals = strategy.after_batch(&ctx);
            if !signals.is_empty() {
                last = signals.into_iter().map(|s| (s.symbol.to_string(), s.target_weight)).collect();
            }
        }
        last
    }

    /// 25 sessions of bars at `price` with $10M a day of volume, from day `from`.
    fn bars(symbol: &str, from: u64, price: Decimal) -> Vec<Event> {
        (from..from + 25).map(|i| bar(symbol, day(i), price, dec!(10_000_000) / price)).collect()
    }

    #[test]
    fn holds_the_cheapest_most_profitable_names_and_rebalances_monthly() {
        let mut events = Vec::new();
        // Same price and shares (market cap $1B): CHEAP earns 200M on 1B of assets, MID 100M on 2B, POOR 10M.
        for (symbol, income, assets) in [("CHEAP", dec!(200e6), dec!(1e9)), ("MID", dec!(100e6), dec!(2e9)), ("POOR", dec!(10e6), dec!(2e9))] {
            events.extend(financials(symbol, income, assets, dec!(100e6), day(0)));
            events.extend(bars(symbol, 1, dec!(10)));
        }
        let mut s = QualityValue::new(2).unwrap();
        // Liquidity history isn't full at the January rebalance, so nothing is held until February.
        assert_eq!(run(&mut s, events.clone()), BTreeMap::new());

        events.extend(bars("CHEAP", 30, dec!(10)));
        events.extend(bars("MID", 30, dec!(10)));
        events.extend(bars("POOR", 30, dec!(10)));
        let mut s = QualityValue::new(2).unwrap();
        let targets = run(&mut s, events);
        assert_eq!(targets, BTreeMap::from([("CHEAP".into(), dec!(0.5)), ("MID".into(), dec!(0.5))]));
    }

    #[test]
    fn drops_names_at_the_next_rebalance_and_restates_zero() {
        let mut events = Vec::new();
        for (symbol, income) in [("A", dec!(200e6)), ("B", dec!(100e6))] {
            events.extend(financials(symbol, income, dec!(1e9), dec!(100e6), day(0)));
            for from in [1, 26, 51] {
                events.extend(bars(symbol, from, dec!(10)));
            }
        }
        // A leads in February. C, filed mid-February with far higher income, displaces it in March, and A is
        // restated at 0 so it gets sold.
        events.extend(financials("C", dec!(900e6), dec!(1e9), dec!(100e6), day(40)));
        events.extend(bars("C", 26, dec!(10)));
        events.extend(bars("C", 51, dec!(10)));
        let mut s = QualityValue::new(1).unwrap();
        let targets = run(&mut s, events);
        assert_eq!(targets, BTreeMap::from([("A".into(), dec!(0)), ("C".into(), dec!(1))]));
    }

    #[test]
    fn ignores_quarterly_income_stale_reports_and_illiquid_or_cheap_stocks() {
        let mut events = Vec::new();
        let q_start = NaiveDate::from_ymd_opt(2024, 10, 1).unwrap();
        let end = NaiveDate::from_ymd_opt(2024, 12, 31).unwrap();
        // QTR only has a quarterly figure.
        events.push(fact("QTR", NET_INCOME, Some(q_start), end, dec!(50e6), day(0)));
        events.push(fact("QTR", ASSETS, None, end, dec!(1e9), day(0)));
        events.push(fact("QTR", SHARES[1], None, end, dec!(100e6), day(0)));
        // OLD's figures are for 2022: too old by 2025.
        let (s22, e22) = (NaiveDate::from_ymd_opt(2022, 1, 1).unwrap(), NaiveDate::from_ymd_opt(2022, 12, 31).unwrap());
        events.push(fact("OLD", NET_INCOME, Some(s22), e22, dec!(50e6), day(0)));
        events.push(fact("OLD", ASSETS, None, e22, dec!(1e9), day(0)));
        events.push(fact("OLD", SHARES[0], None, e22, dec!(100e6), day(0)));
        events.extend(financials("PENNY", dec!(50e6), dec!(1e9), dec!(100e6), day(0)));
        events.extend(financials("THIN", dec!(50e6), dec!(1e9), dec!(100e6), day(0)));
        events.extend(financials("OK", dec!(1e6), dec!(1e9), dec!(100e6), day(0)));
        for from in [1, 30] {
            events.extend(bars("QTR", from, dec!(10)));
            events.extend(bars("OLD", from, dec!(10)));
            events.extend(bars("PENNY", from, dec!(4)));
            events.extend((from..from + 25).map(|i| bar("THIN", day(i), dec!(10), dec!(1_000))));
            events.extend(bars("OK", from, dec!(10)));
        }
        let mut s = QualityValue::new(5).unwrap();
        assert_eq!(run(&mut s, events), BTreeMap::from([("OK".into(), dec!(0.2))]));
    }

    #[test]
    fn facts_filed_after_the_rebalance_wait_for_the_next_one() {
        let mut events = Vec::new();
        for symbol in ["A", "B"] {
            events.extend(bars(symbol, 1, dec!(10)));
            events.extend(bars(symbol, 30, dec!(10)));
        }
        events.extend(financials("A", dec!(100e6), dec!(1e9), dec!(100e6), day(0)));
        // B's (better) report is filed after the close of February's first session, when the rebalance runs.
        let first_feb = (30..).map(day).find(|d| d.month() == 2).unwrap();
        events.extend(financials("B", dec!(500e6), dec!(1e9), dec!(100e6), first_feb));
        let mut s = QualityValue::new(1).unwrap();
        assert_eq!(run(&mut s, events), BTreeMap::from([("A".into(), dec!(1))]));
    }

    #[test]
    fn restatements_replace_the_same_period_and_later_periods_win() {
        let mut s = QualityValue::new(1).unwrap();
        let end = NaiveDate::from_ymd_opt(2024, 12, 31).unwrap();
        let ctx_portfolio = Portfolio::new(Decimal::ZERO);
        let ctx = StrategyContext { now: Timestamp::default(), portfolio: &ctx_portfolio };
        for (value, period_end) in [(dec!(1), end), (dec!(2), end), (dec!(3), end - Days::new(365))] {
            s.on_event(&fact("A", ASSETS, None, period_end, value, day(0)), &ctx);
        }
        assert_eq!(s.stocks[&Symbol::new("A")].assets, Some((end, dec!(2))));
    }
}
