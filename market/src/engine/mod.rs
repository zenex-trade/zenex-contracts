//! The market engine. The entry layer in `contract.rs` handles owner auth
//! and the instance TTL. It then delegates here. The trader entry bodies
//! check `user` auth. Each module owns its type and the entry bodies for it,
//! and [`keeper`] owns the permissionless price-bearing fills.

pub mod adl;
pub mod borrowing;
pub mod config;
pub mod constants;
pub mod funding;
pub mod keeper;
pub mod market;
pub mod market_data;
pub(crate) mod math;
pub mod order;
pub mod position;
pub mod settlement;
pub mod status;
pub mod vault_order;

pub use adl::{execute_adl, update_adl_state};
pub use config::set_config;
pub use funding::claim_credit;
pub use keeper::{accrue, execute_liquidation, execute_order, execute_vault_order};
pub use order::{cancel_order, create_order};
pub use status::{set_status, set_terminal_price};
pub use vault_order::{cancel_vault_order, create_vault_order};
