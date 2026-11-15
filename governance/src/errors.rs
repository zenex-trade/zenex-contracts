//! `GovernanceError` contract error enum.
//!
//! Governance owns the 81x domain. `Unauthorized` publishes as discriminant
//! 1, outside that domain, to mirror the shared access-control convention.

use soroban_sdk::contracterror;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum GovernanceError {
    /// The caller is not the contract owner. No entry point raises it.
    Unauthorized = 1,
    /// No call is queued under the nonce, or no pending delay change exists.
    NotQueued = 810,
    /// The timelock delay has not passed yet.
    NotUnlocked = 811,
    /// The delay is zero or above the 60-day ceiling.
    InvalidDelay = 812,
}
