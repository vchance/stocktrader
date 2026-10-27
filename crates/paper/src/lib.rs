//! Paper trading on Alpaca, one batch per session: before the open, replay recent history through a
//! strategy, reconcile its targets with the account's real positions and open orders, and submit market
//! orders that fill at the open. Nothing is kept between runs except the journal; each run starts from what
//! the broker reports, so a missed or repeated run can't leave the runner's view out of step.

pub mod alpaca;
pub mod execute;
pub mod plan;

pub use alpaca::{PAPER_URL, Sessions, TradingClient, sessions};
pub use execute::{Journal, Outcome, submit_plan};
pub use plan::{AccountState, Plan, PlannedOrder, check_account, open_requests, plan, plan_stream, portfolio_from};
