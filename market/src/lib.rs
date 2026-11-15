#![no_std]

//! Zenex market contract.
//!
//! One perpetual futures market per contract. Traders create orders.
//! Keepers fill them at oracle-verified prices.
//!
//! Entry points: [`Market`]. Logic: [`engine`].

mod contract;
mod dependencies;
pub mod engine;
mod errors;
pub mod events;
pub mod storage;

pub use contract::*;
pub use engine::adl::AdlState;
pub use engine::config::Config;
pub use engine::market_data::{MarketData, SidePair};
pub use engine::order::{Order, OrderKind};
pub use engine::position::Position;
pub use engine::status::Status;
pub use engine::vault_order::{VaultOrder, VaultOrderKind};
pub use errors::MarketError;
