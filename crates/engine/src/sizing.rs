//! Turning an approved target weight into an order. Shared by the backtester and the paper runner, so
//! what is measured in a backtest is what gets traded.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use st_core::{OrderId, OrderRequest, OrderType, Portfolio, Side, Signal};

/// Skip resizing an existing position by less than this fraction of equity, so equity drift doesn't
/// churn small trades. Entries and exits to flat always go through.
pub const REBALANCE_BAND: Decimal = dec!(0.01);
/// Buys smaller than this fraction of equity are skipped, so target weights below it never trade.
pub const MIN_BUY: Decimal = dec!(0.0025);
/// Headroom kept when sizing buys from cash, since a market order fills at the next open, not the last close.
pub const CASH_BUFFER: Decimal = dec!(0.02);

/// Converts a target weight into a whole-share market order for the difference between the target and the
/// projected position (current position plus `open_orders`). Buys are capped by cash not already committed
/// to other pending buys. `None` when there is nothing worth trading or no price to size with.
pub fn size_order(signal: Signal, portfolio: &Portfolio, open_orders: &[(OrderId, OrderRequest)]) -> Option<OrderRequest> {
    let price = portfolio.last_price(&signal.symbol).filter(|p| *p > Decimal::ZERO)?;
    let equity = portfolio.equity();
    let target_qty = (equity * signal.target_weight / price).trunc().max(Decimal::ZERO);
    let projected = portfolio.projected_qty(&signal.symbol, open_orders);
    let mut delta = target_qty - projected;

    let resize = !target_qty.is_zero() && !projected.is_zero();
    if resize && (delta * price).abs() < equity * REBALANCE_BAND {
        return None;
    }
    if delta > Decimal::ZERO {
        let reserved: Decimal = open_orders
            .iter()
            .filter(|(_, o)| o.side == Side::Buy)
            .map(|(_, o)| o.qty * portfolio.last_price(&o.symbol).unwrap_or(price))
            .sum();
        let available = portfolio.cash() - reserved * (Decimal::ONE + CASH_BUFFER);
        let affordable = (available / (price * (Decimal::ONE + CASH_BUFFER))).trunc().max(Decimal::ZERO);
        delta = delta.min(affordable);
        // Cash or gross-exposure headroom can shrink an order to a sliver; skip those.
        if delta * price < equity * MIN_BUY {
            return None;
        }
    }
    if delta.is_zero() {
        return None;
    }

    let side = if delta > Decimal::ZERO { Side::Buy } else { Side::Sell };
    Some(OrderRequest { symbol: signal.symbol, side, qty: delta.abs(), order_type: OrderType::Market, reason: signal.reason })
}

#[cfg(test)]
mod tests {
    use st_core::{Position, Symbol};

    use super::*;

    fn signal(symbol: &str, weight: Decimal) -> Signal {
        Signal { symbol: Symbol::new(symbol), target_weight: weight, reason: "test".into() }
    }

    fn market(symbol: &str, side: Side, qty: Decimal) -> (OrderId, OrderRequest) {
        let request =
            OrderRequest { symbol: Symbol::new(symbol), side, qty, order_type: OrderType::Market, reason: String::new() };
        (OrderId(1), request)
    }

    #[test]
    fn sizes_an_entry_from_equity_and_price() {
        let mut p = Portfolio::new(dec!(100_000));
        p.mark(&Symbol::new("AAPL"), dec!(200));
        let order = size_order(signal("AAPL", dec!(0.05)), &p, &[]).unwrap();
        assert_eq!((order.side, order.qty), (Side::Buy, dec!(25)));
    }

    #[test]
    fn counts_open_orders_and_reserves_their_cash() {
        let mut p = Portfolio::new(dec!(10_000));
        p.mark(&Symbol::new("AAPL"), dec!(100));
        p.mark(&Symbol::new("MSFT"), dec!(100));
        // The pending AAPL buy already covers the target.
        assert_eq!(size_order(signal("AAPL", dec!(0.5)), &p, &[market("AAPL", Side::Buy, dec!(50))]), None);
        // A pending MSFT buy of 90 shares leaves $820 of cash, 8 shares with the buffer, for AAPL.
        let order = size_order(signal("AAPL", dec!(0.5)), &p, &[market("MSFT", Side::Buy, dec!(90))]).unwrap();
        assert_eq!(order.qty, dec!(8));
    }

    #[test]
    fn exits_a_held_position_and_ignores_small_resizes() {
        let mut p = Portfolio::new(dec!(19_000));
        p.set_position(Symbol::new("AAPL"), Position { qty: dec!(10), avg_cost: dec!(90) });
        p.mark(&Symbol::new("AAPL"), dec!(100));
        let exit = size_order(signal("AAPL", Decimal::ZERO), &p, &[]).unwrap();
        assert_eq!((exit.side, exit.qty), (Side::Sell, dec!(10)));
        // Target 11 shares vs 10 held: a $100 change is under the 1% band on $20,000.
        assert_eq!(size_order(signal("AAPL", dec!(0.055)), &p, &[]), None);
    }

    #[test]
    fn needs_a_price() {
        let p = Portfolio::new(dec!(10_000));
        assert_eq!(size_order(signal("AAPL", dec!(0.1)), &p, &[]), None);
    }
}
