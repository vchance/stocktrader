//! `stocktrader options-lab`: how monthly option strategies on a stock or ETF would have behaved, with
//! option prices *estimated* by the Black-Scholes formula from the stock's own recent volatility. A learning
//! tool: real option prices (skew, events, spreads) differ, so results are approximations, not evidence a
//! strategy is tradeable. Prices are split- and dividend-adjusted closes, treated as a stock that pays no
//! dividend: no dividend yield enters the option prices (real calls on dividend payers are cheaper and can
//! be exercised early near ex-dividend dates), and the prices and strikes shown are adjusted levels.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use clap::{Args, ValueEnum};
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use st_core::Timestamp;
use st_engine::metrics;
use st_marketdata::{BarFilter, read_bars, to_core_bar};

use crate::report;

#[derive(Clone, Copy, Debug, PartialEq, ValueEnum)]
pub enum OptionStrategy {
    /// Own the shares; each cycle sell a call above the price: income, but gains above the strike are given up.
    CoveredCall,
    /// Own the shares; each cycle buy a put below the price: a cost, but losses below the strike are capped.
    ProtectivePut,
    /// Both: the call's income pays for the put.
    Collar,
}

#[derive(Args)]
pub struct OptionsArgs {
    /// Stock or ETF to hold, e.g. SPY.
    #[arg(long, default_value = "SPY")]
    symbol: String,
    #[arg(long, value_enum, default_value_t = OptionStrategy::CoveredCall)]
    strategy: OptionStrategy,
    /// Call strike as a fraction above the price when sold (0.03 = 3% above).
    #[arg(long, default_value_t = 0.03)]
    call_otm: f64,
    /// Put strike as a fraction below the price when bought (0.05 = 5% below).
    #[arg(long, default_value_t = 0.05)]
    put_otm: f64,
    /// Sessions per option cycle (21 = about a month).
    #[arg(long, default_value_t = 21)]
    cycle: usize,
    /// Implied volatility as a multiple of recent realized volatility. Options usually price in more movement
    /// than happens, which is what option sellers earn.
    #[arg(long, default_value_t = 1.15)]
    vol_markup: f64,
    /// Sessions of daily returns used to estimate volatility.
    #[arg(long, default_value_t = 20)]
    vol_window: usize,
    /// Annual risk-free rate, used in option prices and earned on cash.
    #[arg(long, default_value_t = 0.03)]
    rate: f64,
    /// Cost of crossing the bid-ask spread, as a fraction of each option's price.
    #[arg(long, default_value_t = 0.05)]
    spread: f64,
    #[arg(long, default_value = "100000")]
    capital: Decimal,
    #[arg(long)]
    start: Option<NaiveDate>,
    #[arg(long)]
    end: Option<NaiveDate>,
    /// Directory with bars_daily_all.parquet from `marketdata sync`.
    #[arg(long, env = "STOCKTRADER_MARKETDATA_DIR", default_value = "data/marketdata")]
    marketdata_dir: PathBuf,
    #[arg(long, env = "STOCKTRADER_REPORTS_DIR", default_value = "reports")]
    reports_dir: PathBuf,
}

/// Standard normal cumulative distribution, via erf (Abramowitz and Stegun 7.1.26, error below 1.5e-7).
fn norm_cdf(x: f64) -> f64 {
    let z = x.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.327_591_1 * z);
    let poly = t * (0.254_829_592 + t * (-0.284_496_736 + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let erf = 1.0 - poly * (-z * z).exp();
    if x >= 0.0 { 0.5 * (1.0 + erf) } else { 0.5 * (1.0 - erf) }
}

/// Black-Scholes price of a European call (`call`) or put on a non-dividend stock: spot `s`, strike `k`,
/// years to expiry `t`, rate `r`, volatility `vol`. At or past expiry, the payoff.
pub fn black_scholes(call: bool, s: f64, k: f64, t: f64, r: f64, vol: f64) -> f64 {
    if t <= 0.0 || vol <= 0.0 {
        return if call { (s - k).max(0.0) } else { (k - s).max(0.0) };
    }
    let d1 = ((s / k).ln() + (r + vol * vol / 2.0) * t) / (vol * t.sqrt());
    let d2 = d1 - vol * t.sqrt();
    let discount = (-r * t).exp();
    if call { s * norm_cdf(d1) - k * discount * norm_cdf(d2) } else { k * discount * norm_cdf(-d2) - s * norm_cdf(-d1) }
}

/// Annualized volatility of daily log returns over the last `window` returns ending at index `i`.
fn realized_vol(closes: &[f64], i: usize, window: usize) -> Option<f64> {
    if i < window {
        return None;
    }
    let returns: Vec<f64> = (i - window + 1..=i).map(|j| (closes[j] / closes[j - 1]).ln()).collect();
    let mean = returns.iter().sum::<f64>() / returns.len() as f64;
    let var = returns.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (returns.len() - 1).max(1) as f64;
    Some((var * 252.0).sqrt())
}

/// One option cycle, for the report.
#[derive(Clone, Debug, PartialEq)]
pub struct Cycle {
    pub opened: NaiveDate,
    pub price: f64,
    pub vol: f64,
    pub call_strike: Option<f64>,
    pub put_strike: Option<f64>,
    /// Premium received (positive) or paid (negative) per share, after the spread.
    pub net_premium: f64,
    pub closing_price: f64,
    pub call_assigned: bool,
    pub put_paid: bool,
}

pub struct LabResult {
    pub curve: Vec<(Timestamp, Decimal)>,
    pub hold: Vec<(Timestamp, Decimal)>,
    pub cycles: Vec<Cycle>,
}

pub struct LabParams {
    pub strategy: OptionStrategy,
    pub call_otm: f64,
    pub put_otm: f64,
    pub cycle: usize,
    pub vol_markup: f64,
    pub vol_window: usize,
    pub rate: f64,
    pub spread: f64,
    pub capital: f64,
}

/// Runs the strategy over daily `(time, date, close)` rows: buys the shares at the first close after the
/// volatility window is filled, then opens options at that close and every `cycle` sessions, settling each at
/// the next cycle's close. Options are marked at their model price every day. Cash earns `rate`.
pub fn simulate(rows: &[(Timestamp, NaiveDate, f64)], p: &LabParams) -> Result<LabResult> {
    let closes: Vec<f64> = rows.iter().map(|r| r.2).collect();
    let first = p.vol_window;
    if rows.len() <= first + 1 {
        bail!("need more than {} sessions of prices, got {}", first + 1, rows.len());
    }
    let (sells_call, buys_put) = match p.strategy {
        OptionStrategy::CoveredCall => (true, false),
        OptionStrategy::ProtectivePut => (false, true),
        OptionStrategy::Collar => (true, true),
    };
    let shares = p.capital / closes[first];
    let daily_rate = p.rate / 252.0;
    let mut cash = 0.0;
    let mut cycles: Vec<Cycle> = Vec::new();
    // Open option: (call strike, put strike, volatility it was priced at).
    let mut open: Option<(Option<f64>, Option<f64>, f64)> = None;
    let mut opened_at = first;
    let to_dec = |x: f64| Decimal::from_f64(x).unwrap_or_default().round_dp(2);
    // Both curves start from the capital at the session before the first option, so the strategy's first
    // bid-ask cost counts against it like every later one.
    let mut curve = vec![(rows[first - 1].0, to_dec(p.capital))];
    let mut hold = vec![(rows[first - 1].0, to_dec(p.capital))];

    for i in first..rows.len() {
        let s = closes[i];
        if i > first {
            cash *= 1.0 + daily_rate;
        }
        // Settle the expiring option at this close.
        if let Some((call_k, put_k, _)) = open
            && i - opened_at >= p.cycle
        {
            let call_payoff = call_k.map_or(0.0, |k| (s - k).max(0.0));
            let put_payoff = put_k.map_or(0.0, |k| (k - s).max(0.0));
            cash += shares * (put_payoff - call_payoff);
            if let Some(c) = cycles.last_mut() {
                c.closing_price = s;
                c.call_assigned = call_payoff > 0.0;
                c.put_paid = put_payoff > 0.0;
            }
            open = None;
        }
        // Open the next cycle, unless too few sessions remain for a full one.
        if open.is_none() && i + p.cycle < rows.len() {
            let vol = realized_vol(&closes, i, p.vol_window).unwrap_or(0.2) * p.vol_markup;
            let t = p.cycle as f64 / 252.0;
            let call_k = sells_call.then_some(s * (1.0 + p.call_otm));
            let put_k = buys_put.then_some(s * (1.0 - p.put_otm));
            // Selling receives the model price less the spread; buying pays it plus the spread.
            let call_income = call_k.map_or(0.0, |k| black_scholes(true, s, k, t, p.rate, vol) * (1.0 - p.spread));
            let put_cost = put_k.map_or(0.0, |k| black_scholes(false, s, k, t, p.rate, vol) * (1.0 + p.spread));
            cash += shares * (call_income - put_cost);
            cycles.push(Cycle {
                opened: rows[i].1,
                price: s,
                vol,
                call_strike: call_k,
                put_strike: put_k,
                net_premium: call_income - put_cost,
                closing_price: s,
                call_assigned: false,
                put_paid: false,
            });
            open = Some((call_k, put_k, vol));
            opened_at = i;
        }
        // Mark the position: shares, cash, minus the short call's value, plus the long put's.
        let mut value = shares * s + cash;
        if let Some((call_k, put_k, vol)) = open {
            let t = (p.cycle - (i - opened_at)) as f64 / 252.0;
            value -= call_k.map_or(0.0, |k| shares * black_scholes(true, s, k, t, p.rate, vol));
            value += put_k.map_or(0.0, |k| shares * black_scholes(false, s, k, t, p.rate, vol));
        }
        curve.push((rows[i].0, to_dec(value)));
        hold.push((rows[i].0, to_dec(shares * s)));
    }
    if cycles.is_empty() {
        bail!("no option cycle fits: {} sessions after the volatility window, but a cycle needs {}", rows.len() - first, p.cycle + 1);
    }
    Ok(LabResult { curve, hold, cycles })
}

pub fn run(args: &OptionsArgs) -> Result<()> {
    let symbol = args.symbol.trim().to_ascii_uppercase();
    let path = args.marketdata_dir.join("bars_daily_all.parquet");
    let filter = BarFilter { symbols: Some(HashSet::from([symbol.clone()])), start: args.start, end: args.end };
    let mut bars: Vec<_> = read_bars(&path, &filter).with_context(|| format!("reading {}", path.display()))?.iter().filter_map(to_core_bar).collect();
    bars.sort_by_key(|b| b.end);
    if bars.is_empty() {
        bail!("no prices for {symbol} in {}", path.display());
    }
    let rows: Vec<(Timestamp, NaiveDate, f64)> = bars
        .iter()
        .filter_map(|b| Some((b.end, st_engine::exchange_date(b.end), b.close.to_f64()?)))
        .collect();
    let checks = [
        (args.call_otm >= 0.0 && args.call_otm < 1.0, "--call-otm must be at least 0 and below 1"),
        (args.put_otm >= 0.0 && args.put_otm < 1.0, "--put-otm must be at least 0 and below 1"),
        (args.vol_markup > 0.0, "--vol-markup must be positive"),
        ((0.0..1.0).contains(&args.spread), "--spread must be at least 0 and below 1"),
        (args.rate > -1.0, "--rate must be above -1"),
        (rows.iter().all(|r| r.2 > 0.0), "prices must be positive"),
    ];
    if let Some((_, msg)) = checks.iter().find(|(ok, _)| !ok) {
        bail!("{msg}");
    }
    let params = LabParams {
        strategy: args.strategy,
        call_otm: args.call_otm,
        put_otm: args.put_otm,
        cycle: args.cycle.max(1),
        vol_markup: args.vol_markup,
        vol_window: args.vol_window.max(2),
        rate: args.rate,
        spread: args.spread,
        capital: args.capital.to_f64().context("capital")?,
    };
    let result = simulate(&rows, &params)?;
    let m = metrics::compute(&result.curve, &[]);
    let h = metrics::compute(&result.hold, &[]);
    let label = strategy_label(&params, &symbol);
    let assigned = result.cycles.iter().filter(|c| c.call_assigned).count();
    let paid = result.cycles.iter().filter(|c| c.put_paid).count();
    let premium: f64 = result.cycles.iter().map(|c| c.net_premium).sum();

    println!("strategy          {label}");
    println!("ESTIMATED option prices (Black-Scholes, {:.2}x recent volatility): a learning tool, not tradeable evidence", params.vol_markup);
    println!("{:<22} {:>9} {:>9} {:>8} {:>8}", "", "return", "CAGR", "max DD", "Sharpe");
    for (name, x) in [("with options", &m), ("hold the shares", &h)] {
        println!("{name:<22} {:>8.2}% {:>8.2}% {:>7.2}% {:>8.2}", x.total_return * 100.0, x.cagr * 100.0, x.max_drawdown * 100.0, x.sharpe);
    }
    println!("cycles            {} (calls exercised {assigned}, puts paid off {paid})", result.cycles.len());
    println!("net premium       {premium:+.2} per share in total ({})", if premium >= 0.0 { "received" } else { "paid" });
    let html = report::options_report(&label, &symbol, &params, &result, &m, &h);
    std::fs::create_dir_all(&args.reports_dir).with_context(|| format!("creating {}", args.reports_dir.display()))?;
    let file = args.reports_dir.join(format!("options-{}-{}.html", symbol.to_ascii_lowercase(), strategy_slug(params.strategy)));
    std::fs::write(&file, html).with_context(|| format!("writing {}", file.display()))?;
    println!("report            {}", file.display());
    Ok(())
}

fn strategy_slug(s: OptionStrategy) -> &'static str {
    match s {
        OptionStrategy::CoveredCall => "covered-call",
        OptionStrategy::ProtectivePut => "protective-put",
        OptionStrategy::Collar => "collar",
    }
}

pub fn strategy_label(p: &LabParams, symbol: &str) -> String {
    let call = format!("call {:.0}% above", p.call_otm * 100.0);
    let put = format!("put {:.0}% below", p.put_otm * 100.0);
    let legs = match p.strategy {
        OptionStrategy::CoveredCall => call,
        OptionStrategy::ProtectivePut => put,
        OptionStrategy::Collar => format!("{call}, {put}"),
    };
    format!("{symbol} {} ({legs}, every {} sessions)", strategy_slug(p.strategy), p.cycle)
}

#[cfg(test)]
mod tests {
    use chrono::{Days, TimeZone, Utc};

    use super::*;

    #[test]
    fn black_scholes_matches_textbook_values() {
        // Hull's example: S=42, K=40, r=10%, vol=20%, T=0.5: call 4.76, put 0.81.
        assert!((black_scholes(true, 42.0, 40.0, 0.5, 0.1, 0.2) - 4.76).abs() < 0.01);
        assert!((black_scholes(false, 42.0, 40.0, 0.5, 0.1, 0.2) - 0.81).abs() < 0.01);
        // Put-call parity: C - P = S - K e^{-rT}.
        let (c, p) = (black_scholes(true, 100.0, 105.0, 0.25, 0.03, 0.3), black_scholes(false, 100.0, 105.0, 0.25, 0.03, 0.3));
        assert!((c - p - (100.0 - 105.0 * (-0.03f64 * 0.25).exp())).abs() < 1e-6);
        assert_eq!(black_scholes(true, 110.0, 100.0, 0.0, 0.03, 0.3), 10.0);
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-9 && (norm_cdf(1.96) - 0.975).abs() < 1e-3);
    }

    fn rows(prices: &[f64]) -> Vec<(Timestamp, NaiveDate, f64)> {
        let d0 = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
        prices
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let d = d0 + Days::new(i as u64);
                (Utc.from_utc_datetime(&d.and_hms_opt(21, 0, 0).unwrap()), d, *p)
            })
            .collect()
    }

    fn params(strategy: OptionStrategy) -> LabParams {
        LabParams { strategy, call_otm: 0.03, put_otm: 0.05, cycle: 5, vol_markup: 1.0, vol_window: 3, rate: 0.0, spread: 0.0, capital: 1000.0 }
    }

    /// Prices that wiggle (so volatility is positive) then rise steadily and sharply.
    fn rising() -> Vec<f64> {
        let mut p = vec![100.0, 101.0, 99.0, 100.0];
        p.extend((1..=20).map(|i| 100.0 + 3.0 * f64::from(i)));
        p
    }

    #[test]
    fn covered_call_gives_up_a_strong_rally_but_collects_premium() {
        let r = simulate(&rows(&rising()), &params(OptionStrategy::CoveredCall)).unwrap();
        let (end, hold) = (r.curve.last().unwrap().1, r.hold.last().unwrap().1);
        assert!(end < hold, "capped upside: {end} vs {hold}");
        // A steady climb has almost no volatility, so later premiums are near zero; the first sees the wiggles.
        assert!(r.cycles[0].net_premium > 0.0 && r.cycles.iter().all(|c| c.net_premium >= 0.0));
        assert!(r.cycles.iter().all(|c| c.call_assigned), "every cycle rallied past the strike");
    }

    #[test]
    fn protective_put_limits_a_crash_and_costs_premium_in_calm_markets() {
        let mut falling = vec![100.0, 101.0, 99.0, 100.0];
        falling.extend((1..=20).map(|i| 100.0 - 3.0 * f64::from(i)));
        let r = simulate(&rows(&falling), &params(OptionStrategy::ProtectivePut)).unwrap();
        let (end, hold) = (r.curve.last().unwrap().1, r.hold.last().unwrap().1);
        assert!(end > hold, "puts paid off: {end} vs {hold}");
        assert!(r.cycles.iter().any(|c| c.put_paid));
        assert!(r.cycles[0].net_premium < 0.0 && r.cycles.iter().all(|c| c.net_premium <= 0.0));
    }

    #[test]
    fn one_cycle_accounts_premium_spread_and_settlement_exactly() {
        // Window 3, cycle 5: shares bought at 100 (index 3), one call opened there and settled at index 8.
        let prices = [100.0, 102.0, 98.0, 100.0, 101.0, 103.0, 104.0, 105.0, 106.0, 106.0];
        let p = LabParams { spread: 0.1, ..params(OptionStrategy::CoveredCall) };
        let r = simulate(&rows(&prices), &p).unwrap();
        let shares = 1000.0 / 100.0;
        let vol = realized_vol(&prices, 3, 3).unwrap();
        let model = black_scholes(true, 100.0, 103.0, 5.0 / 252.0, 0.0, vol);
        assert_eq!(r.curve[0].1, Decimal::from(1000), "starts at the capital");
        // On the opening day the call is marked at its model price, so only the spread is lost.
        let open_value = 1000.0 - shares * model * p.spread;
        assert!((r.curve[1].1.to_f64().unwrap() - open_value).abs() < 0.01, "{} vs {open_value}", r.curve[1].1);
        // Settled at 106 against strike 103: shares worth 1060, premium kept, 3 a share paid on the call.
        let settled = shares * 106.0 + shares * model * (1.0 - p.spread) - shares * 3.0;
        assert!((r.curve[6].1.to_f64().unwrap() - settled).abs() < 0.01, "{} vs {settled}", r.curve[6].1);
        assert_eq!(r.cycles.len(), 1);
        assert!(r.cycles[0].call_assigned && (r.cycles[0].net_premium - model * (1.0 - p.spread)).abs() < 1e-9);
    }

    #[test]
    fn later_prices_never_change_an_earlier_cycle() {
        let mut a = rising();
        let first = simulate(&rows(&a), &params(OptionStrategy::Collar)).unwrap();
        // Change everything after the second cycle opened (index 3 + 5 = 8).
        for x in a.iter_mut().skip(9) {
            *x *= 0.5;
        }
        let second = simulate(&rows(&a), &params(OptionStrategy::Collar)).unwrap();
        for i in 0..2 {
            let (x, y) = (&first.cycles[i], &second.cycles[i]);
            assert_eq!((x.opened, x.vol, x.call_strike, x.put_strike, x.net_premium), (y.opened, y.vol, y.call_strike, y.put_strike, y.net_premium));
        }
    }

    #[test]
    fn collar_nets_the_two_legs_and_too_little_data_is_an_error() {
        let r = simulate(&rows(&rising()), &params(OptionStrategy::Collar)).unwrap();
        let c = &r.cycles[0];
        assert!(c.call_strike.is_some() && c.put_strike.is_some());
        assert!(simulate(&rows(&[100.0, 101.0, 102.0]), &params(OptionStrategy::Collar)).is_err());
        // Enough prices for the window but not for a whole cycle: no option would ever open.
        assert!(simulate(&rows(&[100.0, 101.0, 99.0, 100.0, 101.0]), &params(OptionStrategy::Collar)).is_err());
    }
}
