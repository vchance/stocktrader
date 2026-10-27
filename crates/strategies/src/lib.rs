//! Trading strategies. Each one consumes [`Event`]s and emits target-weight [`Signal`]s.
//!
//! Strategies are level-triggered: they restate their full target on every relevant event rather than
//! only on changes, so a target the risk manager rejected or clamped is naturally retried next time.

use std::collections::{HashMap, VecDeque};

use chrono::TimeDelta;
use rust_decimal::Decimal;
use st_core::{Event, EventKind, Signal, Strategy, StrategyContext, Symbol, Timestamp};

/// Buys a symbol when its social-media mentions jump versus a day earlier, and holds it for a fixed number
/// of calendar days after the latest spike (checked at each bar close, so the exit fills at the next open).
/// A worked example of trading on attention events; not a validated strategy.
///
/// Notes: it trades whatever the attention source ranks, including ETFs such as SPY, wherever there are
/// bars for it; a symbol with no previous-window count (new to the source, or previously zero) is never
/// bought, because its jump can't be measured, so brand-new attention is deliberately ignored.
pub struct AttentionSpike {
    min_mentions: u32,
    min_ratio: f64,
    hold: TimeDelta,
    weight: Decimal,
    /// Time of each held symbol's most recent qualifying spike.
    last_spike: HashMap<Symbol, Timestamp>,
}

impl AttentionSpike {
    /// Enter when mentions are at least `min_mentions` and at least `min_ratio` times the previous
    /// window's; exit `hold_days` after the most recent such spike.
    pub fn new(min_mentions: u32, min_ratio: f64, hold_days: u32, weight: Decimal) -> Result<Self, String> {
        if !(min_ratio.is_finite() && min_ratio > 1.0) {
            return Err(format!("ratio must be above 1, got {min_ratio}"));
        }
        if hold_days == 0 {
            return Err("hold must be at least one day".into());
        }
        if weight <= Decimal::ZERO || weight > Decimal::ONE {
            return Err(format!("weight must be in (0, 1], got {weight}"));
        }
        Ok(Self {
            min_mentions,
            min_ratio,
            hold: TimeDelta::days(i64::from(hold_days)),
            weight,
            last_spike: HashMap::new(),
        })
    }
}

impl Strategy for AttentionSpike {
    fn name(&self) -> &str {
        "attention_spike"
    }

    fn on_event(&mut self, event: &Event, ctx: &StrategyContext<'_>) -> Vec<Signal> {
        match &event.kind {
            EventKind::Attention(a) => {
                let Some(prev) = a.mentions_prev.filter(|p| *p > 0) else {
                    return Vec::new();
                };
                let ratio = f64::from(a.mentions) / f64::from(prev);
                if a.mentions < self.min_mentions || ratio < self.min_ratio {
                    return Vec::new();
                }
                self.last_spike.insert(a.symbol.clone(), ctx.now);
                vec![Signal {
                    symbol: a.symbol.clone(),
                    target_weight: self.weight,
                    reason: format!("{} mentions vs {prev} a day earlier ({ratio:.1}x) on {}", a.mentions, a.source),
                }]
            }
            // Restate the target on every bar while holding, so a rejected entry is retried and the exit
            // happens on schedule.
            EventKind::Bar(bar) => match self.last_spike.get(&bar.symbol) {
                Some(&spike) if ctx.now - spike <= self.hold => vec![Signal {
                    symbol: bar.symbol.clone(),
                    target_weight: self.weight,
                    reason: "holding after attention spike".into(),
                }],
                Some(_) => {
                    self.last_spike.remove(&bar.symbol);
                    vec![Signal {
                        symbol: bar.symbol.clone(),
                        target_weight: Decimal::ZERO,
                        reason: format!("{} days since the last attention spike", self.hold.num_days()),
                    }]
                }
                None => Vec::new(),
            },
            _ => Vec::new(),
        }
    }
}

/// Moving-average crossover: long `weight` while the fast SMA is above the slow SMA, flat otherwise.
///
/// This is a plumbing test for the engine, not a strategy with an edge.
pub struct SmaCrossover {
    fast: usize,
    slow: usize,
    weight: Decimal,
    closes: HashMap<Symbol, VecDeque<Decimal>>,
}

impl SmaCrossover {
    pub fn new(fast: usize, slow: usize, weight: Decimal) -> Result<Self, String> {
        if fast == 0 || fast >= slow {
            return Err(format!("need 0 < fast < slow, got fast={fast} slow={slow}"));
        }
        if weight <= Decimal::ZERO || weight > Decimal::ONE {
            return Err(format!("weight must be in (0, 1], got {weight}"));
        }
        Ok(Self { fast, slow, weight, closes: HashMap::new() })
    }
}

impl Strategy for SmaCrossover {
    fn name(&self) -> &str {
        "sma_crossover"
    }

    fn on_event(&mut self, event: &Event, _ctx: &StrategyContext<'_>) -> Vec<Signal> {
        let EventKind::Bar(bar) = &event.kind else {
            return Vec::new();
        };
        let closes = self.closes.entry(bar.symbol.clone()).or_default();
        closes.push_back(bar.close);
        if closes.len() > self.slow {
            closes.pop_front();
        }
        if closes.len() < self.slow {
            return Vec::new();
        }

        let fast_sma = closes.iter().rev().take(self.fast).copied().sum::<Decimal>() / Decimal::from(self.fast);
        let slow_sma = closes.iter().copied().sum::<Decimal>() / Decimal::from(self.slow);
        let (target_weight, relation) =
            if fast_sma > slow_sma { (self.weight, "above") } else { (Decimal::ZERO, "at or below") };
        let reason = format!(
            "SMA{} {} {relation} SMA{} {}",
            self.fast,
            fast_sma.round_dp(2),
            self.slow,
            slow_sma.round_dp(2)
        );
        vec![Signal { symbol: bar.symbol.clone(), target_weight, reason }]
    }
}
