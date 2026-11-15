//! Governance contract events.

use soroban_sdk::{contractevent, Address, Symbol};

/// A call was queued through `queue`, or a delay change was queued through
/// `set_delay`. A delay change carries nonce `u32::MAX`.
#[contractevent]
#[derive(Clone)]
pub struct Queued {
    #[topic]
    pub nonce: u32,
    pub target: Address,
    pub fn_name: Symbol,
    /// Unix timestamp (seconds) at which the call becomes executable.
    pub unlock_time: u64,
}

/// A queued call was executed through `execute` after its delay passed.
#[contractevent]
#[derive(Clone)]
pub struct Executed {
    #[topic]
    pub nonce: u32,
    pub target: Address,
    pub fn_name: Symbol,
}

/// The owner cancelled a queued call through `cancel`.
#[contractevent]
#[derive(Clone)]
pub struct Cancelled {
    #[topic]
    pub nonce: u32,
}

/// The owner forwarded an immediate `set_status` to the target, bypassing
/// the delay.
#[contractevent]
#[derive(Clone)]
pub struct StatusSet {
    #[topic]
    pub target: Address,
    /// The market `Status` discriminant.
    pub status: u32,
}

/// A pending delay change was applied through `apply_delay`.
#[contractevent]
#[derive(Clone)]
pub struct DelaySet {
    /// The previous delay (seconds).
    pub old_delay: u64,
    /// The delay now in force (seconds).
    pub new_delay: u64,
}
