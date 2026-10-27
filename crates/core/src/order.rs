use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::types::{Price, Qty, Symbol, Timestamp};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    /// +1 for buys, -1 for sells.
    pub fn sign(self) -> Decimal {
        match self {
            Side::Buy => Decimal::ONE,
            Side::Sell => Decimal::NEGATIVE_ONE,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum OrderType {
    Market,
    Limit(Price),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OrderRequest {
    pub symbol: Symbol,
    pub side: Side,
    pub qty: Qty,
    pub order_type: OrderType,
    /// Why the order exists, kept for the audit trail.
    pub reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OrderId(pub u64);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub order_id: OrderId,
    pub symbol: Symbol,
    pub side: Side,
    pub qty: Qty,
    pub price: Price,
    pub commission: Decimal,
    pub at: Timestamp,
}

/// A strategy's desired portfolio weight for a symbol: 0.05 means 5% of equity, 0 means flat.
/// Strategies express intent as weights; the engine and risk manager turn that into sized orders.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Signal {
    pub symbol: Symbol,
    pub target_weight: Decimal,
    pub reason: String,
}
