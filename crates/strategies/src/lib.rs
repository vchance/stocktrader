//! Trading strategies. Each one consumes [`Event`]s and emits target-weight [`Signal`]s.
//!
//! Strategies are level-triggered: they restate their full target on every relevant event rather than
//! only on changes, so a target the risk manager rejected or clamped is naturally retried next time.

use std::collections::{HashMap, VecDeque};

use rust_decimal::Decimal;
use st_core::{Event, EventKind, Signal, Strategy, StrategyContext, Symbol};

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
