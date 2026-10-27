use std::collections::{BTreeMap, HashMap};

use rust_decimal::Decimal;
use rust_decimal::prelude::Signed;

use crate::order::Fill;
use crate::types::{Price, Qty, Symbol};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Position {
    /// Signed: positive is long, negative is short.
    pub qty: Qty,
    pub avg_cost: Price,
}

/// Cash, positions and last-known prices. Positions are valued at the last marked price.
#[derive(Clone, Debug)]
pub struct Portfolio {
    cash: Decimal,
    positions: BTreeMap<Symbol, Position>,
    last_prices: HashMap<Symbol, Price>,
    realized_pnl: Decimal,
    commissions: Decimal,
}

impl Portfolio {
    pub fn new(cash: Decimal) -> Self {
        Self {
            cash,
            positions: BTreeMap::new(),
            last_prices: HashMap::new(),
            realized_pnl: Decimal::ZERO,
            commissions: Decimal::ZERO,
        }
    }

    pub fn cash(&self) -> Decimal {
        self.cash
    }

    pub fn realized_pnl(&self) -> Decimal {
        self.realized_pnl
    }

    pub fn commissions(&self) -> Decimal {
        self.commissions
    }

    pub fn positions(&self) -> impl Iterator<Item = (&Symbol, &Position)> {
        self.positions.iter()
    }

    pub fn qty(&self, symbol: &Symbol) -> Qty {
        self.positions.get(symbol).map_or(Decimal::ZERO, |p| p.qty)
    }

    pub fn last_price(&self, symbol: &Symbol) -> Option<Price> {
        self.last_prices.get(symbol).copied()
    }

    pub fn mark(&mut self, symbol: &Symbol, price: Price) {
        self.last_prices.insert(symbol.clone(), price);
    }

    /// Signed market value of the position (negative for shorts).
    pub fn market_value(&self, symbol: &Symbol) -> Decimal {
        self.positions.get(symbol).map_or(Decimal::ZERO, |p| {
            p.qty * self.last_price(symbol).unwrap_or(p.avg_cost)
        })
    }

    pub fn gross_exposure(&self) -> Decimal {
        self.positions.keys().map(|s| self.market_value(s).abs()).sum()
    }

    pub fn equity(&self) -> Decimal {
        self.cash + self.positions.keys().map(|s| self.market_value(s)).sum::<Decimal>()
    }

    /// Signed fraction of equity held in `symbol`.
    pub fn weight(&self, symbol: &Symbol) -> Decimal {
        let equity = self.equity();
        if equity <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        self.market_value(symbol) / equity
    }

    pub fn apply_fill(&mut self, fill: &Fill) {
        let signed = fill.side.sign() * fill.qty;
        self.cash -= signed * fill.price + fill.commission;
        self.commissions += fill.commission;

        let pos = self.positions.entry(fill.symbol.clone()).or_default();
        let old = pos.qty;
        let new = old + signed;
        if old.is_zero() || old.signum() == signed.signum() {
            pos.avg_cost = (pos.avg_cost * old.abs() + fill.price * signed.abs()) / new.abs();
        } else {
            let closed = signed.abs().min(old.abs());
            self.realized_pnl += closed * (fill.price - pos.avg_cost) * old.signum();
            if !new.is_zero() && new.signum() != old.signum() {
                // Flipped through zero: the remainder is a fresh position at the fill price.
                pos.avg_cost = fill.price;
            }
        }
        pos.qty = new;
        if new.is_zero() {
            self.positions.remove(&fill.symbol);
        }
        self.mark(&fill.symbol, fill.price);
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use rust_decimal_macros::dec;

    use super::*;
    use crate::order::{OrderId, Side};

    fn fill(side: Side, qty: Decimal, price: Decimal) -> Fill {
        Fill {
            order_id: OrderId(1),
            symbol: Symbol::new("AAPL"),
            side,
            qty,
            price,
            commission: dec!(1),
            at: Utc::now(),
        }
    }

    #[test]
    fn buy_then_partial_sell_tracks_cost_and_realized_pnl() {
        let mut p = Portfolio::new(dec!(10_000));
        let aapl = Symbol::new("aapl");

        p.apply_fill(&fill(Side::Buy, dec!(10), dec!(100)));
        p.apply_fill(&fill(Side::Buy, dec!(10), dec!(110)));
        assert_eq!(p.qty(&aapl), dec!(20));
        assert_eq!(p.positions.get(&aapl).unwrap().avg_cost, dec!(105));
        assert_eq!(p.cash(), dec!(10_000) - dec!(2100) - dec!(2));

        p.apply_fill(&fill(Side::Sell, dec!(5), dec!(120)));
        assert_eq!(p.qty(&aapl), dec!(15));
        assert_eq!(p.realized_pnl(), dec!(75));
        assert_eq!(p.equity(), p.cash() + dec!(15) * dec!(120));
    }

    #[test]
    fn closing_removes_position() {
        let mut p = Portfolio::new(dec!(1_000));
        p.apply_fill(&fill(Side::Buy, dec!(5), dec!(10)));
        p.apply_fill(&fill(Side::Sell, dec!(5), dec!(12)));
        assert_eq!(p.positions().count(), 0);
        assert_eq!(p.realized_pnl(), dec!(10));
        assert_eq!(p.equity(), dec!(1_000) + dec!(10) - dec!(2));
    }
}
