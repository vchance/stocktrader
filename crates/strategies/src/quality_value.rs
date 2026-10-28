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
/// Annual revenue under any of the tags filers use for it. Only its sign matters: it marks an operating
/// business. Commodity trusts and other funds that file 10-Ks report income (gains on their holdings) but
/// no revenue, and would otherwise look extremely cheap.
const REVENUE: [&str; 5] = [
    "us-gaap:Revenues:USD",
    "us-gaap:RevenueFromContractWithCustomerExcludingAssessedTax:USD",
    "us-gaap:RevenueFromContractWithCustomerIncludingAssessedTax:USD",
    "us-gaap:SalesRevenueNet:USD",
    "us-gaap:SalesRevenueGoodsNet:USD",
];
/// Shares outstanding, from the cover page or the balance sheet; the latest period wins.
const SHARES: [&str; 2] = ["dei:EntityCommonStockSharesOutstanding:shares", "us-gaap:CommonStockSharesOutstanding:shares"];
/// A fiscal year, allowing for 52/53-week years.
const ANNUAL_DAYS: std::ops::RangeInclusive<i64> = 350..=380;

#[derive(Clone, Debug, Default)]
struct Stock {
    traded_close: Option<Decimal>,
    dollar_volume: VecDeque<Decimal>,
    /// Session of the latest bar; stocks without a bar today (delisted, halted) can't be bought.
    last_bar: Option<NaiveDate>,
    /// Latest fiscal year's net income, as (period end, value).
    annual_income: Option<(NaiveDate, Decimal)>,
    /// Latest fiscal year's revenue; for one period reported under several tags, the largest (so a later
    /// downward restatement of that period isn't applied; only the sign is used).
    annual_revenue: Option<(NaiveDate, Decimal)>,
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
/// Weights of `1 / top_n` above the risk manager's position cap are clamped by it, leaving cash idle, so
/// callers should keep `top_n` at or above `1 / max_position_weight`.
///
/// Each eligible stock is ranked on two measures from its latest filings, and the ranks are added:
/// - earnings yield: latest annual net income / market cap (shares outstanding x as-traded price);
/// - return on assets: latest annual net income / total assets.
///
/// Eligible means: a bar today (a stock that stopped trading can't be bought), an operating company (positive
/// annual revenue, which excludes commodity trusts and other
/// funds that file 10-Ks), an as-traded price of at least `min_price`, average daily dollar volume of at
/// least `min_dollar_volume` over the last `liquidity_lookback` bars, positive assets and shares, and each
/// figure from a period that ended within `max_report_age`. Only facts already public reach the strategy (the
/// engine releases them the day after filing), so there is no look-ahead.
///
/// Known simplifications: annual figures only (up to ~15 months old); a split after the latest share count
/// misstates market cap until the next filing; for multi-class issuers the share count may cover one class;
/// filers whose income statement uses none of the five revenue tags (banks reporting only interest income,
/// insurers' earned premiums, many REITs' rental revenue, some brokers and utilities) are left out; IFRS filers
/// are out of scope, as only US GAAP income and assets are read.
/// After a rebalance, dropped names are restated at 0 until the next one.
pub struct QualityValue {
    top_n: usize,
    /// Rank only the largest this many eligible companies by market value.
    largest: Option<usize>,
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
    pub const CONCEPTS: &[&str] = &[
        "NetIncomeLoss",
        "Assets",
        "EntityCommonStockSharesOutstanding",
        "CommonStockSharesOutstanding",
        "Revenues",
        "RevenueFromContractWithCustomerExcludingAssessedTax",
        "RevenueFromContractWithCustomerIncludingAssessedTax",
        "SalesRevenueNet",
        "SalesRevenueGoodsNet",
    ];

    /// Equal weight `1 / top_n`, so fully invested once `top_n` names qualify. Filters default to the
    /// risk manager's: $5 and $5M average dollar volume over 20 bars.
    pub fn new(top_n: usize) -> Result<Self, String> {
        if top_n == 0 {
            return Err("top_n must be at least 1".into());
        }
        Ok(Self {
            top_n,
            largest: None,
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

    /// Ranks only the `n` largest eligible companies by market value (shares outstanding x as-traded price),
    /// where data is cleanest and the evidence for the factors strongest.
    pub fn with_largest(mut self, n: usize) -> Self {
        self.largest = Some(n.max(1));
        self
    }

    /// Overrides the eligibility filters, e.g. to match a plan's universe and risk limits.
    pub fn with_filters(mut self, min_price: Decimal, min_dollar_volume: Decimal, liquidity_lookback: usize) -> Self {
        self.min_price = min_price;
        self.min_dollar_volume = min_dollar_volume;
        self.liquidity_lookback = liquidity_lookback.max(1);
        self
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
            m if REVENUE.contains(&m) => {
                let annual = r.period_start.is_some_and(|start| ANNUAL_DAYS.contains(&(r.period_end - start).num_days()));
                match stock.annual_revenue {
                    _ if !annual => {}
                    Some((end, value)) if end == r.period_end => stock.annual_revenue = Some((end, value.max(r.value))),
                    _ => update(&mut stock.annual_revenue, r.period_end, r.value),
                }
            }
            ASSETS => update(&mut stock.assets, r.period_end, r.value),
            m if SHARES.contains(&m) => update(&mut stock.shares, r.period_end, r.value),
            _ => {}
        }
    }

    /// (symbol, earnings yield, return on assets) for every eligible stock on `today`, limited to the
    /// largest by market value when `largest` is set.
    fn candidates(&self, today: NaiveDate) -> Vec<(Symbol, Decimal, Decimal)> {
        let fresh = |slot: Option<(NaiveDate, Decimal)>| slot.filter(|(end, _)| today - *end <= self.max_report_age).map(|(_, v)| v);
        let lookback = Decimal::from(self.liquidity_lookback);
        let mut out: Vec<(Symbol, Decimal, Decimal, Decimal)> = Vec::new();
        for (symbol, s) in &self.stocks {
            let (Some(price), Some(income), Some(assets), Some(shares), Some(revenue)) =
                (s.traded_close, fresh(s.annual_income), fresh(s.assets), fresh(s.shares), fresh(s.annual_revenue))
            else {
                continue;
            };
            if s.last_bar != Some(today) {
                continue;
            }
            let liquid = s.dollar_volume.len() >= self.liquidity_lookback
                && s.dollar_volume.iter().sum::<Decimal>() / lookback >= self.min_dollar_volume;
            if revenue <= Decimal::ZERO || price < self.min_price || !liquid || assets <= Decimal::ZERO || shares <= Decimal::ZERO {
                continue;
            }
            let market_cap = shares * price;
            out.push((symbol.clone(), income / market_cap, income / assets, market_cap));
        }
        if let Some(n) = self.largest {
            out.sort_by(|a, b| b.3.cmp(&a.3).then(a.0.cmp(&b.0)));
            out.truncate(n);
        }
        out.into_iter().map(|(symbol, ey, roa, _)| (symbol, ey, roa)).collect()
    }

    /// Re-ranks; `false` if nothing was eligible.
    fn rebalance(&mut self, today: NaiveDate) -> bool {
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
        !candidates.is_empty()
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
                stock.last_bar = Some(new_york_date(bar.end));
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

    /// Re-ranks on the first session of a month (retrying each session until something is eligible, so the
    /// first holdings don't wait for the next month), and after every session restates the current targets.
    fn after_batch(&mut self, ctx: &StrategyContext<'_>) -> Vec<Signal> {
        if !std::mem::take(&mut self.saw_bars) {
            return Vec::new();
        }
        let today = new_york_date(ctx.now);
        let month = (today.year(), today.month());
        if self.rebalanced != Some(month) && self.rebalance(today) {
            self.rebalanced = Some(month);
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
            fact(symbol, REVENUE[1], Some(start), end, dec!(1e9), filed),
        ]
    }

    #[test]
    fn largest_limits_the_ranking_to_the_biggest_companies() {
        let mut events = Vec::new();
        // TINY is the best by both measures but by far the smallest (10M shares vs 100M at the same price).
        events.extend(financials("TINY", dec!(50e6), dec!(100e6), dec!(10e6), day(0)));
        events.extend(financials("BIG1", dec!(100e6), dec!(1e9), dec!(100e6), day(0)));
        events.extend(financials("BIG2", dec!(50e6), dec!(1e9), dec!(100e6), day(0)));
        for symbol in ["TINY", "BIG1", "BIG2"] {
            events.extend(bars(symbol, 1, dec!(10)));
        }
        let mut all = QualityValue::new(1).unwrap();
        assert_eq!(run(&mut all, events.clone()).keys().collect::<Vec<_>>(), vec!["TINY"]);
        let mut large = QualityValue::new(1).unwrap().with_largest(2);
        assert_eq!(run(&mut large, events).keys().collect::<Vec<_>>(), vec!["BIG1"]);
    }

    #[test]
    fn a_stock_that_stopped_trading_is_not_bought_and_is_sold() {
        let mut events = Vec::new();
        // GONE ranks best but its bars stop in January; at the February rebalance only LIVE trades.
        events.extend(financials("GONE", dec!(500e6), dec!(1e9), dec!(100e6), day(0)));
        events.extend(financials("LIVE", dec!(100e6), dec!(1e9), dec!(100e6), day(0)));
        events.extend(bars("GONE", 1, dec!(10)));
        events.extend(bars("LIVE", 1, dec!(10)));
        events.extend(bars("LIVE", 30, dec!(10)));
        let mut s = QualityValue::new(1).unwrap();
        assert_eq!(run(&mut s, events), BTreeMap::from([("GONE".into(), dec!(0)), ("LIVE".into(), dec!(1))]));
    }

    #[test]
    fn funds_without_revenue_are_not_companies() {
        let mut events = Vec::new();
        // GOLD looks far cheaper and more profitable (gains on its metal) but reports no revenue.
        events.extend(financials("REAL", dec!(100e6), dec!(1e9), dec!(100e6), day(0)));
        let (start, end) = (NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(), NaiveDate::from_ymd_opt(2024, 12, 31).unwrap());
        events.push(fact("GOLD", NET_INCOME, Some(start), end, dec!(900e6), day(0)));
        events.push(fact("GOLD", ASSETS, None, end, dec!(1e9), day(0)));
        events.push(fact("GOLD", SHARES[0], None, end, dec!(100e6), day(0)));
        // ZERO tags revenue, but none: also not an operating business.
        events.push(fact("ZERO", NET_INCOME, Some(start), end, dec!(500e6), day(0)));
        events.push(fact("ZERO", ASSETS, None, end, dec!(1e9), day(0)));
        events.push(fact("ZERO", SHARES[0], None, end, dec!(100e6), day(0)));
        events.push(fact("ZERO", REVENUE[0], Some(start), end, dec!(0), day(0)));
        for symbol in ["REAL", "GOLD", "ZERO"] {
            events.extend(bars(symbol, 1, dec!(10)));
        }
        let mut s = QualityValue::new(3).unwrap();
        let targets = run(&mut s, events);
        assert_eq!(targets.keys().collect::<Vec<_>>(), vec!["REAL"], "{targets:?}");
    }

    #[test]
    fn stale_or_vanished_revenue_makes_a_stock_ineligible() {
        let (s23, e23) = (NaiveDate::from_ymd_opt(2023, 1, 1).unwrap(), NaiveDate::from_ymd_opt(2023, 12, 31).unwrap());
        let (s24, e24) = (NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(), NaiveDate::from_ymd_opt(2024, 12, 31).unwrap());
        let mut events = financials("REAL", dec!(100e6), dec!(1e9), dec!(100e6), day(0));
        // OLD's revenue is from 2022, too old by 2025, while its other figures are fresh.
        let (s22, e22) = (NaiveDate::from_ymd_opt(2022, 1, 1).unwrap(), NaiveDate::from_ymd_opt(2022, 12, 31).unwrap());
        events.push(fact("OLD", NET_INCOME, Some(s24), e24, dec!(500e6), day(0)));
        events.push(fact("OLD", ASSETS, None, e24, dec!(1e9), day(0)));
        events.push(fact("OLD", SHARES[0], None, e24, dec!(100e6), day(0)));
        events.push(fact("OLD", REVENUE[0], Some(s22), e22, dec!(1e9), day(0)));
        // GONE had revenue in 2023 but reports none for 2024.
        events.push(fact("GONE", NET_INCOME, Some(s24), e24, dec!(500e6), day(0)));
        events.push(fact("GONE", ASSETS, None, e24, dec!(1e9), day(0)));
        events.push(fact("GONE", SHARES[0], None, e24, dec!(100e6), day(0)));
        events.push(fact("GONE", REVENUE[0], Some(s23), e23, dec!(1e9), day(0)));
        events.push(fact("GONE", REVENUE[0], Some(s24), e24, dec!(0), day(0)));
        for symbol in ["REAL", "OLD", "GONE"] {
            events.extend(bars(symbol, 1, dec!(10)));
        }
        let mut s = QualityValue::new(3).unwrap();
        assert_eq!(run(&mut s, events).keys().collect::<Vec<_>>(), vec!["REAL"]);
    }

    #[test]
    fn revenue_tags_for_one_year_combine_and_quarters_dont_count() {
        let mut s = QualityValue::new(1).unwrap();
        let portfolio = Portfolio::new(Decimal::ZERO);
        let ctx = StrategyContext { now: Timestamp::default(), portfolio: &portfolio };
        let (start, end) = (NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(), NaiveDate::from_ymd_opt(2024, 12, 31).unwrap());
        let quarter = NaiveDate::from_ymd_opt(2024, 10, 1).unwrap();
        for e in [
            fact("A", REVENUE[0], Some(start), end, dec!(5), day(0)),
            fact("A", REVENUE[1], Some(start), end, dec!(0), day(0)),
            fact("A", REVENUE[0], Some(quarter), end, dec!(99), day(0)),
            // B reports the zero tag first.
            fact("B", REVENUE[1], Some(start), end, dec!(0), day(0)),
            fact("B", REVENUE[0], Some(start), end, dec!(5), day(0)),
        ] {
            s.on_event(&e, &ctx);
        }
        assert_eq!(s.stocks[&Symbol::new("A")].annual_revenue, Some((end, dec!(5))));
        assert_eq!(s.stocks[&Symbol::new("B")].annual_revenue, Some((end, dec!(5))));
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
        // The first holdings come as soon as 20 sessions of liquidity history exist, mid-January.
        let mut s = QualityValue::new(2).unwrap();
        assert_eq!(run(&mut s, events.clone()), BTreeMap::from([("CHEAP".into(), dec!(0.5)), ("MID".into(), dec!(0.5))]));
        assert_eq!(s.rebalanced, Some((2025, 1)));

        // February re-ranks; POOR's income rises enough to overtake MID.
        events.extend(financials("POOR", dec!(150e6), dec!(2e9), dec!(100e6), day(28)));
        events.extend(bars("CHEAP", 30, dec!(10)));
        events.extend(bars("MID", 30, dec!(10)));
        events.extend(bars("POOR", 30, dec!(10)));
        let mut s = QualityValue::new(2).unwrap();
        let targets = run(&mut s, events);
        assert_eq!(targets, BTreeMap::from([("CHEAP".into(), dec!(0.5)), ("MID".into(), dec!(0)), ("POOR".into(), dec!(0.5))]));
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
        // A leads from January (C has no liquidity history until February, and February's rebalance runs
        // before it has enough). C, with far higher income, displaces A in March, and A is restated at 0.
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
