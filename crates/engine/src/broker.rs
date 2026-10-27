use std::collections::BTreeMap;

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use st_core::{
    Event, EventKind, ExecutionAdapter, ExecutionError, Fill, OrderId, OrderRequest, OrderType, Side, Timestamp,
};

#[derive(Clone, Debug)]
pub struct FillModel {
    /// Adverse price move applied to market orders, in basis points.
    pub slippage_bps: Decimal,
    pub commission_per_share: Decimal,
    pub min_commission: Decimal,
}

impl Default for FillModel {
    fn default() -> Self {
        Self { slippage_bps: dec!(5), commission_per_share: Decimal::ZERO, min_commission: Decimal::ZERO }
    }
}

struct PendingOrder {
    request: OrderRequest,
    submitted_at: Timestamp,
}

/// Fills orders against later bars: an order is only eligible for a bar that opened after it was
/// submitted, so a signal computed from a bar's close fills at the next bar's open, never the same bar.
pub struct SimulatedBroker {
    model: FillModel,
    next_id: u64,
    pending: BTreeMap<OrderId, PendingOrder>,
}

impl SimulatedBroker {
    pub fn new(model: FillModel) -> Self {
        Self { model, next_id: 1, pending: BTreeMap::new() }
    }

    fn commission(&self, qty: Decimal) -> Decimal {
        (qty * self.model.commission_per_share).max(self.model.min_commission)
    }
}

impl ExecutionAdapter for SimulatedBroker {
    fn submit(&mut self, order: OrderRequest, now: Timestamp) -> Result<OrderId, ExecutionError> {
        if order.qty <= Decimal::ZERO {
            return Err(ExecutionError::Rejected(format!("non-positive quantity {}", order.qty)));
        }
        let id = OrderId(self.next_id);
        self.next_id += 1;
        self.pending.insert(id, PendingOrder { request: order, submitted_at: now });
        Ok(id)
    }

    fn cancel(&mut self, id: OrderId) -> Result<(), ExecutionError> {
        self.pending.remove(&id).map(|_| ()).ok_or(ExecutionError::UnknownOrder(id))
    }

    fn open_orders(&self) -> Vec<(OrderId, OrderRequest)> {
        self.pending.iter().map(|(id, p)| (*id, p.request.clone())).collect()
    }

    fn on_market_event(&mut self, event: &Event) -> Vec<Fill> {
        let EventKind::Bar(bar) = &event.kind else {
            return Vec::new();
        };
        let slip = self.model.slippage_bps / dec!(10_000);
        let mut fills = Vec::new();

        let eligible: Vec<OrderId> = self
            .pending
            .iter()
            .filter(|(_, p)| p.request.symbol == bar.symbol && p.submitted_at <= bar.start)
            .map(|(id, _)| *id)
            .collect();

        for id in eligible {
            let req = &self.pending[&id].request;
            let price = match (req.order_type, req.side) {
                (OrderType::Market, Side::Buy) => Some(bar.open * (Decimal::ONE + slip)),
                (OrderType::Market, Side::Sell) => Some(bar.open * (Decimal::ONE - slip)),
                (OrderType::Limit(limit), Side::Buy) => (bar.low <= limit).then(|| bar.open.min(limit)),
                (OrderType::Limit(limit), Side::Sell) => (bar.high >= limit).then(|| bar.open.max(limit)),
            };
            let Some(price) = price else { continue };
            let order = self.pending.remove(&id).expect("eligible order is pending").request;
            fills.push(Fill {
                order_id: id,
                commission: self.commission(order.qty),
                symbol: order.symbol,
                side: order.side,
                qty: order.qty,
                price: price.round_dp(4),
                at: bar.start,
            });
        }
        fills
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};
    use st_core::{Bar, Symbol};

    use super::*;

    fn bar_event(day: i64, open: Decimal) -> Event {
        let start = Utc.with_ymd_and_hms(2026, 1, 5, 14, 30, 0).unwrap() + Duration::days(day);
        Event::bar(Bar {
            symbol: Symbol::new("ABC"),
            start,
            end: start + Duration::minutes(390),
            open,
            high: open + dec!(1),
            low: open - dec!(1),
            close: open,
            volume: dec!(1000),
            traded_close: None,
        })
    }

    #[test]
    fn market_order_fills_at_next_bar_open_with_slippage() {
        let mut broker = SimulatedBroker::new(FillModel::default());
        let today = bar_event(0, dec!(100));
        let order = OrderRequest {
            symbol: Symbol::new("ABC"),
            side: Side::Buy,
            qty: dec!(10),
            order_type: OrderType::Market,
            reason: "test".into(),
        };
        broker.submit(order, today.available_at).unwrap();

        // Re-delivering the bar the decision was based on must not fill.
        assert!(broker.on_market_event(&today).is_empty());

        let fills = broker.on_market_event(&bar_event(1, dec!(200)));
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].price, dec!(200.1));
        assert!(broker.open_orders().is_empty());
    }
}
