//! Cross-sectional momentum: hold the stocks that rose most over the past year, skipping the latest month.

use std::collections::{BTreeMap, HashMap, VecDeque};

use chrono::{Datelike, NaiveDate};
use chrono_tz::America::New_York;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use st_core::{Event, EventKind, Signal, Strategy, StrategyContext, Symbol, Timestamp};

#[derive(Clone, Debug, Default)]
struct Stock {
    /// Adjusted closes, oldest first: the last `lookback + 1` sessions.
    closes: VecDeque<Decimal>,
    traded_close: Option<Decimal>,
    dollar_volume: VecDeque<Decimal>,
    /// Session of the latest bar; stocks without a bar today (delisted, halted) can't be bought.
    last_bar: Option<NaiveDate>,
}

/// Holds the `top_n` stocks with the highest return from `lookback` sessions ago to `skip` sessions ago,
/// at equal weight, re-ranked on the first session of each month (retrying each session until enough history
/// exists). Skipping the latest month avoids its short-term reversal. Uses adjusted closes, so splits and
/// dividends don't distort returns.
///
/// Eligible: a bar today, an as-traded price of at least `min_price`, average daily dollar volume of at least
/// `min_dollar_volume` over `liquidity_lookback` sessions, and a full `lookback` of history. With `largest`,
/// only the most heavily traded eligible stocks (by that average dollar volume, a proxy for size that needs
/// no fundamentals) are ranked. Dropped names are restated at 0 until the next rebalance. Windows count the
/// stock's own bars, so a stock with missing sessions looks back slightly further in calendar time.
pub struct Momentum {
    top_n: usize,
    weight: Decimal,
    lookback: usize,
    skip: usize,
    largest: Option<usize>,
    min_price: Decimal,
    min_dollar_volume: Decimal,
    liquidity_lookback: usize,
    stocks: HashMap<Symbol, Stock>,
    rebalanced: Option<(i32, u32)>,
    saw_bars: bool,
    targets: BTreeMap<Symbol, (Decimal, String)>,
}

impl Momentum {
    /// Equal weight `1 / top_n`; returns measured from `lookback` to `skip` sessions ago (252 and 21: a year
    /// excluding the last month). Filters default to the risk manager's: $5 and $5M over 20 sessions.
    pub fn new(top_n: usize, lookback: usize, skip: usize) -> Result<Self, String> {
        if top_n == 0 {
            return Err("top_n must be at least 1".into());
        }
        if skip >= lookback {
            return Err(format!("skip ({skip}) must be shorter than lookback ({lookback})"));
        }
        Ok(Self {
            top_n,
            weight: Decimal::ONE / Decimal::from(top_n),
            lookback,
            skip,
            largest: None,
            min_price: dec!(5),
            min_dollar_volume: dec!(5_000_000),
            liquidity_lookback: 20,
            stocks: HashMap::new(),
            rebalanced: None,
            saw_bars: false,
            targets: BTreeMap::new(),
        })
    }

    /// Ranks only the `n` most heavily traded eligible stocks.
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

    /// (symbol, momentum return) for every eligible stock on `today`.
    fn candidates(&self, today: NaiveDate) -> Vec<(Symbol, Decimal)> {
        let window = Decimal::from(self.liquidity_lookback);
        let mut out: Vec<(Symbol, Decimal, Decimal)> = Vec::new();
        for (symbol, s) in &self.stocks {
            let Some(price) = s.traded_close else { continue };
            if s.last_bar != Some(today) || price < self.min_price || s.closes.len() <= self.lookback {
                continue;
            }
            if s.dollar_volume.len() < self.liquidity_lookback {
                continue;
            }
            let avg_volume = s.dollar_volume.iter().sum::<Decimal>() / window;
            if avg_volume < self.min_dollar_volume {
                continue;
            }
            let start = s.closes[0];
            let end = s.closes[s.closes.len() - 1 - self.skip];
            if start <= Decimal::ZERO {
                continue;
            }
            out.push((symbol.clone(), end / start - Decimal::ONE, avg_volume));
        }
        if let Some(n) = self.largest {
            out.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
            out.truncate(n);
        }
        out.into_iter().map(|(s, r, _)| (s, r)).collect()
    }

    /// Re-ranks; `false` if nothing was eligible.
    fn rebalance(&mut self, today: NaiveDate) -> bool {
        let mut candidates = self.candidates(today);
        candidates.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut targets = BTreeMap::new();
        for (i, (symbol, ret)) in candidates.iter().take(self.top_n).enumerate() {
            let reason = format!(
                "momentum rank {} of {} on {today}: {:+}% over the year to a month ago",
                i + 1,
                candidates.len(),
                (ret * Decimal::ONE_HUNDRED).round_dp(1)
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

impl Strategy for Momentum {
    fn name(&self) -> &str {
        "momentum"
    }

    fn on_event(&mut self, event: &Event, _ctx: &StrategyContext<'_>) -> Vec<Signal> {
        if let EventKind::Bar(bar) = &event.kind {
            self.saw_bars = true;
            let (lookback, liquidity) = (self.lookback, self.liquidity_lookback);
            let stock = self.stocks.entry(bar.symbol.clone()).or_default();
            stock.closes.push_back(bar.close);
            if stock.closes.len() > lookback + 1 {
                stock.closes.pop_front();
            }
            stock.traded_close = Some(bar.traded_close());
            stock.dollar_volume.push_back(bar.close * bar.volume);
            if stock.dollar_volume.len() > liquidity {
                stock.dollar_volume.pop_front();
            }
            stock.last_bar = Some(new_york_date(bar.end));
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
    use st_core::{Bar, Portfolio};

    use super::*;

    fn day(n: u64) -> NaiveDate {
        NaiveDate::from_ymd_opt(2025, 1, 2).unwrap() + Days::new(n)
    }

    fn bar(symbol: &str, d: NaiveDate, price: Decimal, volume: Decimal) -> Event {
        let close = New_York.from_local_datetime(&d.and_hms_opt(16, 0, 0).unwrap()).single().unwrap().to_utc();
        Event::bar(Bar {
            symbol: Symbol::new(symbol),
            start: close - TimeDelta::hours(7),
            end: close,
            open: price,
            high: price,
            low: price,
            close: price,
            volume,
            traded_close: Some(price),
        })
    }

    /// Daily bars from day `from` for `n` days, the price following `price(i)`, with $10M of daily volume.
    fn series(symbol: &str, from: u64, n: u64, price: impl Fn(u64) -> Decimal) -> Vec<Event> {
        (from..from + n).map(|i| bar(symbol, day(i), price(i), dec!(10_000_000) / price(i))).collect()
    }

    /// Runs events grouped by timestamp, as the engine does, returning the last restated targets.
    fn run(strategy: &mut Momentum, mut events: Vec<Event>) -> BTreeMap<String, Decimal> {
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

    #[test]
    fn holds_the_strongest_past_returns_skipping_the_last_month() {
        // Lookback 40 sessions, skip 10, daily bars to day 58 (March 1, the last rebalance). UP rose steadily;
        // LATE was flat until a spike from day 52, inside the skipped window at that rebalance; DOWN fell.
        let up = series("UP", 0, 59, |i| dec!(10) + Decimal::from(i) / dec!(10));
        let late = series("LATE", 0, 59, |i| if i >= 52 { dec!(30) } else { dec!(10) });
        let down = series("DOWN", 0, 59, |i| dec!(20) - Decimal::from(i) / dec!(10));
        let events: Vec<Event> = [up, late, down].concat();
        let mut skipping = Momentum::new(1, 40, 10).unwrap();
        assert_eq!(run(&mut skipping, events.clone()), BTreeMap::from([("UP".into(), dec!(1))]));
        // Without the skip, the recent spike would win.
        let mut not_skipping = Momentum::new(1, 40, 0).unwrap();
        assert_eq!(run(&mut not_skipping, events).get("LATE"), Some(&dec!(1)));
    }

    #[test]
    fn return_runs_from_lookback_to_skip_sessions_ago() {
        // Closes 10, 11, ..., 14 on days 0-4; lookback 3 and skip 1 on day 4: from day 1 (11) to day 3 (13).
        let mut s = Momentum::new(1, 3, 1).unwrap().with_filters(Decimal::ZERO, Decimal::ZERO, 1);
        let portfolio = Portfolio::new(Decimal::ZERO);
        for i in 0..5u64 {
            let e = bar("X", day(i), dec!(10) + Decimal::from(i), dec!(1));
            s.on_event(&e, &StrategyContext { now: e.available_at, portfolio: &portfolio });
        }
        assert_eq!(s.candidates(day(4)), vec![(Symbol::new("X"), dec!(13) / dec!(11) - Decimal::ONE)]);
        // A day later (no new bar), X has no bar "today" and isn't eligible.
        assert!(s.candidates(day(5)).is_empty());
    }

    #[test]
    fn needs_history_liquidity_price_and_a_bar_today_and_largest_limits_by_volume() {
        // GONE stops trading before the March rebalance (it may be held in February, then is set to 0); CHEAP
        // stays under $5; THIN trades $1k a day; YOUNG has too little history. Only OK is held in March.
        let mut events = series("OK", 0, 70, |i| dec!(10) + Decimal::from(i) / dec!(10));
        events.extend(series("GONE", 0, 50, |i| dec!(10) + Decimal::from(i)));
        events.extend(series("CHEAP", 0, 70, |i| dec!(1) + Decimal::from(i) / dec!(100)));
        events.extend((0..70).map(|i| bar("THIN", day(i), dec!(10) + Decimal::from(i), dec!(100))));
        events.extend(series("YOUNG", 50, 20, |i| dec!(10) + Decimal::from(i)));
        let mut s = Momentum::new(5, 40, 10).unwrap();
        let targets = run(&mut s, events.clone());
        let held: Vec<&String> = targets.iter().filter(|(_, w)| !w.is_zero()).map(|(s, _)| s).collect();
        assert_eq!(held, vec!["OK"], "{targets:?}");
        assert_eq!(targets.get("GONE"), Some(&dec!(0)), "a stock that stopped trading is sold");

        // BIG trades 10x OK's volume but rose less; limited to the single largest, only BIG is ranked.
        events.extend((0..70).map(|i| bar("BIG", day(i), dec!(10), dec!(10_000_000))));
        let mut s = Momentum::new(1, 40, 10).unwrap().with_largest(1);
        assert_eq!(run(&mut s, events).keys().collect::<Vec<_>>(), vec!["BIG"]);
    }

    #[test]
    fn validates_parameters() {
        assert!(Momentum::new(0, 252, 21).is_err());
        assert!(Momentum::new(10, 21, 21).is_err());
        assert!(Momentum::new(10, 252, 21).is_ok());
    }
}
