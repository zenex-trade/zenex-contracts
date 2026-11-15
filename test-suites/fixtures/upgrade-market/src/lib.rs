#![no_std]

//! Test-only changed-code market for upgrade acceptance. Every production
//! module is compiled directly from current market source. The sole addition
//! is an observable revision entry. There is no schema change or migration.
//! Never deploy this fixture as a release contract.
//! The Rust upgrade tests check the compiled interface and state preservation.

#[path = "../../../../market/src/contract.rs"]
mod contract;
#[path = "../../../../market/src/dependencies/mod.rs"]
mod dependencies;
#[path = "../../../../market/src/engine/mod.rs"]
pub mod engine;
#[path = "../../../../market/src/errors.rs"]
mod errors;
#[path = "../../../../market/src/events.rs"]
pub mod events;
#[path = "../../../../market/src/storage.rs"]
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

#[soroban_sdk::contractimpl]
impl MarketContract {
    /// Proves an invocation reached the replacement executable.
    pub fn audit_revision() -> u32 {
        2
    }
}
