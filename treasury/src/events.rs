//! Treasury contract events: the owner's two actions. `get_rate` publishes
//! nothing. The market reads the rate at each settlement.

use soroban_sdk::{contractevent, Address};

/// The owner replaced the protocol fee rate through `set_rate`.
#[contractevent]
#[derive(Clone)]
pub struct RateUpdate {
    /// The new fee rate (`SCALAR_18` fraction).
    pub rate: i128,
}

/// The owner withdrew collected fees through `withdraw`.
#[contractevent]
#[derive(Clone)]
pub struct Withdraw {
    /// The token withdrawn.
    #[topic]
    pub token: Address,
    /// The recipient.
    #[topic]
    pub to: Address,
    /// The amount moved (token-dec).
    pub amount: i128,
}
