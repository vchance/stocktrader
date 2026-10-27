//! Trading strategies. Each one consumes [`Event`]s and emits target-weight [`Signal`]s.

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
    long: HashMap<Symbol, bool>,
}

impl SmaCrossover {
    pub fn new(fast: usize, slow: usize, weight: Decimal) -> Self {
        assert!(fast > 0 && fast < slow, "need 0 < fast < slow");
        Self { fast, slow, weight, closes: HashMap::new(), long: HashMap::new() }
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
        let want_long = fast_sma > slow_sma;
        let was_long = self.long.insert(bar.symbol.clone(), want_long).unwrap_or(false);
        if want_long == was_long {
            return Vec::new();
        }

        let (target_weight, reason) = if want_long {
            (self.weight, format!("SMA{} {} crossed above SMA{} {}", self.fast, fast_sma.round_dp(2), self.slow, slow_sma.round_dp(2)))
        } else {
            (Decimal::ZERO, format!("SMA{} {} crossed below SMA{} {}", self.fast, fast_sma.round_dp(2), self.slow, slow_sma.round_dp(2)))
        };
        vec![Signal { symbol: bar.symbol.clone(), target_weight, reason }]
    }
}
