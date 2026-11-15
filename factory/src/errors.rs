//! `FactoryError` contract error enum.
//!
//! The factory defines only the shared admin `6xx` domain. A deployment
//! failure traps in the host. The host wraps constructor failures as
//! `Error(Context, InvalidAction)`. Their underlying contract error remains
//! in host diagnostics and does not propagate directly to the caller.

use soroban_sdk::contracterror;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum FactoryError {
    /// `upgrade` was called with an `operator` that is not the contract owner.
    /// The discriminant is shared: market and oracle raise the same code for
    /// the same condition.
    UpgradeNotOwner = 600,
}
