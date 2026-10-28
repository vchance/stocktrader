//! A fixed basket: hold every symbol in a list at the same weight, optionally only while it is in an uptrend.

use std::collections::{BTreeMap, HashMap, VecDeque};

use chrono::{Datelike, NaiveDate};
use chrono_tz::America::New_York;
use rust_decimal::Decimal;
use st_core::{Event, EventKind, Signal, Strategy, StrategyContext, Symbol, Timestamp};

#[derive(Clone, Debug, Default)]
struct Stock {
    /// Latest adjusted closes, oldest first (the trend window, if any).
    closes: VecDeque<Decimal>,
    last_bar: Option<NaiveDate>,
}

/// Holds each symbol it sees (the plan's list) at `weight`, re-decided on the first session of each month.
/// With a trend filter of `n` sessions, a symbol is held only while its close is above its `n`-session
/// average, and otherwise left in cash; without one, every symbol trading that session is held. A symbol
/// that hasn't started trading yet leaves its slot in cash. A held symbol without a bar on the rebalance day
/// keeps its position if it traded within the last week (a data gap or halt), and is sold if not. Between rebalances the targets are
/// restated each session, so positions are topped up or trimmed once they drift past the rebalance band.
pub struct EqualWeight {
    weight: Decimal,
    trend: Option<usize>,
    stocks: HashMap<Symbol, Stock>,
    rebalanced: Option<(i32, u32)>,
    saw_bars: bool,
    targets: BTreeMap<Symbol, (Decimal, String)>,
}

impl EqualWeight {
    /// `slots` is the number of symbols in the basket: each gets 1 / `slots` of equity.
    pub fn new(slots: usize, trend: Option<usize>) -> Result<Self, String> {
        if slots == 0 {
            return Err("an equal-weight basket needs at least one symbol".into());
        }
        if trend == Some(0) {
            return Err("the trend window must be at least one session".into());
        }
        Ok(Self {
            weight: Decimal::ONE / Decimal::from(slots),
            trend,
            stocks: HashMap::new(),
            rebalanced: None,
            saw_bars: false,
            targets: BTreeMap::new(),
        })
    }

    /// Re-decides every position; `false` until at least one symbol can be judged.
    fn rebalance(&mut self, today: NaiveDate) -> bool {
        // A held name that missed a bar this recently is assumed to be in a gap or halt, not gone.
        const GAP_DAYS: i64 = 7;
        let mut targets = BTreeMap::new();
        let mut judged = 0;
        for (symbol, s) in &self.stocks {
            if s.last_bar != Some(today) {
                continue;
            }
            let held = match self.trend {
                None => Some("in the basket".to_string()),
                Some(n) if s.closes.len() >= n => {
                    let average = s.closes.iter().sum::<Decimal>() / Decimal::from(s.closes.len());
                    let close = *s.closes.back().expect("non-empty");
                    if close > average { Some(format!("above its {n}-session average")) } else { None }
                }
                Some(_) => continue,
            };
            judged += 1;
            if let Some(why) = held {
                targets.insert(symbol.clone(), (self.weight, format!("{why} on {today}")));
            }
        }
        for (symbol, (weight, reason)) in &self.targets {
            if weight.is_zero() || targets.contains_key(symbol) {
                continue;
            }
            let last_bar = self.stocks.get(symbol).and_then(|s| s.last_bar);
            let judged_today = last_bar == Some(today);
            if !judged_today && last_bar.is_some_and(|d| (today - d).num_days() <= GAP_DAYS) {
                targets.insert(symbol.clone(), (*weight, reason.clone()));
                continue;
            }
            let why = if judged_today { "below its trend average" } else { "no longer trading" };
            targets.insert(symbol.clone(), (Decimal::ZERO, format!("{why} on {today}")));
        }
        self.targets = targets;
        judged > 0
    }
}

fn new_york_date(t: Timestamp) -> NaiveDate {
    t.with_timezone(&New_York).date_naive()
}

impl Strategy for EqualWeight {
    fn name(&self) -> &str {
        "equal_weight"
    }

    fn on_event(&mut self, event: &Event, _ctx: &StrategyContext<'_>) -> Vec<Signal> {
        if let EventKind::Bar(bar) = &event.kind {
            self.saw_bars = true;
            let window = self.trend.unwrap_or(1);
            let stock = self.stocks.entry(bar.symbol.clone()).or_default();
            stock.closes.push_back(bar.close);
            if stock.closes.len() > window {
                stock.closes.pop_front();
            }
            stock.last_bar = Some(new_york_date(bar.end));
        }
        Vec::new()
    }

    /// Re-decides on the first session of a month, and after every session restates the current targets.
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
    use chrono::{Days, TimeDelta, TimeZone};
    use rust_decimal_macros::dec;
    use st_core::{Bar, Portfolio};

    use super::*;

    fn day(n: u64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2025, 1, 2).unwrap() + Days::new(n)
    }

    fn bar(symbol: &str, d: NaiveDate, price: Decimal) -> Event {
        let close = New_York.from_local_datetime(&d.and_hms_opt(16, 0, 0).unwrap()).single().unwrap().to_utc();
        Event::bar(Bar {
            symbol: Symbol::new(symbol),
            start: close - TimeDelta::hours(7),
            end: close,
            open: price,
            high: price,
            low: price,
            close: price,
            volume: dec!(1_000_000),
            traded_close: Some(price),
        })
    }

    fn series(symbol: &str, from: u64, n: u64, price: impl Fn(u64) -> Decimal) -> Vec<Event> {
        (from..from + n).map(|i| bar(symbol, day(i), price(i))).collect()
    }

    fn run(strategy: &mut EqualWeight, mut events: Vec<Event>) -> BTreeMap<String, Decimal> {
        events.sort_by_key(|e| e.available_at);
        let portfolio = Portfolio::new(Decimal::ZERO);
        let mut last = BTreeMap::new();
        let mut i = 0;
        while i < events.len() {
            let now = events[i].available_at;
            let ctx = StrategyContext { now, portfolio: &portfolio };
            while i < events.len() && events[i].available_at == now {
                strategy.on_event(&events[i], &ctx);
                i += 1;
            }
            let signals = strategy.after_batch(&ctx);
            if !signals.is_empty() {
                last = signals.into_iter().map(|s| (s.symbol.to_string(), s.target_weight)).collect();
            }
        }
        last
    }

    #[test]
    fn holds_every_symbol_at_one_slot_each_and_late_listings_join_next_month() {
        // Three slots; LATE starts trading on day 35 (February 6), after the February rebalance, so it joins at
        // the March one (day 58). Until then its slot stays in cash.
        let mut events = series("A", 0, 60, |_| dec!(10));
        events.extend(series("B", 0, 60, |_| dec!(20)));
        events.extend(series("LATE", 35, 25, |_| dec!(5)));
        let third = Decimal::ONE / dec!(3);
        let before_march = bar("X", day(58), dec!(1)).available_at;
        let february = run(&mut EqualWeight::new(3, None).unwrap(), events.iter().filter(|e| e.available_at < before_march).cloned().collect());
        assert_eq!(february, BTreeMap::from([("A".into(), third), ("B".into(), third)]));
        let march = run(&mut EqualWeight::new(3, None).unwrap(), events);
        assert_eq!(march.len(), 3);
        assert!(march.values().all(|w| *w == third));
    }

    #[test]
    fn the_trend_filter_holds_only_names_above_their_average() {
        // Trend window 10. On the February rebalance (day 30) UP is above its average and DOWN below.
        let mut events = series("UP", 0, 35, |i| dec!(10) + Decimal::from(i));
        events.extend(series("DOWN", 0, 35, |i| dec!(50) - Decimal::from(i)));
        let t = run(&mut EqualWeight::new(2, Some(10)).unwrap(), events);
        assert_eq!(t, BTreeMap::from([("UP".into(), dec!(0.5))]));

        // DOWN was held in January (rising then) and is sold at the February rebalance once it turns down.
        let mut events = series("DOWN", 0, 35, |i| if i < 25 { dec!(10) + Decimal::from(i) } else { dec!(34) - dec!(3) * Decimal::from(i - 24) });
        events.extend(series("UP", 0, 35, |i| dec!(10) + Decimal::from(i)));
        let t = run(&mut EqualWeight::new(2, Some(10)).unwrap(), events);
        assert_eq!(t.get("DOWN"), Some(&dec!(0)));
        assert_eq!(t.get("UP"), Some(&dec!(0.5)));
    }

    #[test]
    fn a_short_gap_keeps_a_holding_a_long_one_sells_it_and_zeros_clear_next_month() {
        // GAP has no bar on February 1 (day 30, the rebalance) but trades again on day 31: kept at 1/3.
        // GONE stops on day 20: sold at the February rebalance, then dropped from the targets in March.
        let mut events = series("A", 0, 60, |_| dec!(10));
        events.extend(series("GAP", 0, 60, |_| dec!(10)).into_iter().filter(|e| e.available_at != bar("X", day(30), dec!(1)).available_at));
        events.extend(series("GONE", 0, 21, |_| dec!(10)));
        let third = Decimal::ONE / dec!(3);
        let before_march = bar("X", day(58), dec!(1)).available_at;
        let february = run(&mut EqualWeight::new(3, None).unwrap(), events.iter().filter(|e| e.available_at < before_march).cloned().collect());
        assert_eq!(february, BTreeMap::from([("A".into(), third), ("GAP".into(), third), ("GONE".into(), dec!(0))]));
        let march = run(&mut EqualWeight::new(3, None).unwrap(), events);
        assert_eq!(march, BTreeMap::from([("A".into(), third), ("GAP".into(), third)]));
    }

    #[test]
    fn decides_once_a_month() {
        // A trend name that turns down mid-month is held until the next month's rebalance.
        let mut events = series("X", 0, 45, |i| if i < 33 { dec!(10) + Decimal::from(i) } else { dec!(10) });
        events.extend(series("Y", 0, 45, |_| dec!(10)));
        let t = run(&mut EqualWeight::new(2, Some(5)).unwrap(), events);
        assert_eq!(t.get("X"), Some(&dec!(0.5)), "judged above average on February 1, kept all month");
    }

    #[test]
    fn validates_parameters() {
        assert!(EqualWeight::new(0, None).is_err());
        assert!(EqualWeight::new(5, Some(0)).is_err());
    }
}
