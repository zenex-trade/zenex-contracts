#![no_std]

//! Zenex governance timelock contract.
//!
//! The owner queues admin calls. Anyone executes them after a mandatory delay.
//! [`Governance::set_status`] bypasses the delay for emergency halts. The delay
//! itself changes only through the same timelock, so nobody can shorten it
//! instantly.

use soroban_sdk::{
    contract, contractclient, contractimpl, panic_with_error, Address, Env, IntoVal, Symbol, Val,
    Vec,
};
use stellar_access::ownable::{self, Ownable};
use stellar_macros::only_owner;

mod errors;
mod events;
mod storage;

pub use errors::GovernanceError;
pub use storage::QueuedCall;

/// Timelock for deferred admin calls.
#[contract]
pub struct GovernanceContract;

/// The governance contract's cross-contract surface.
#[contractclient(name = "GovernanceClient")]
pub trait Governance {
    /// Queues an arbitrary contract call for execution after the delay and
    /// returns its nonce. Nonces increase by one per call and never repeat.
    ///
    /// # Authorization
    /// - Owner only (`#[only_owner]`).
    ///
    /// # Arguments
    /// - `target`: the contract to call.
    /// - `fn_name`: the function to invoke on `target`.
    /// - `args`: the call arguments.
    ///
    /// # Events
    /// - `Queued`: the nonce, target, function, and unlock time.
    fn queue(e: Env, target: Address, fn_name: Symbol, args: Vec<Val>) -> u32;

    /// Cancels a queued call before anyone executes it.
    ///
    /// # Authorization
    /// - Owner only (`#[only_owner]`).
    ///
    /// # Arguments
    /// - `nonce`: the queued call to cancel.
    ///
    /// # Errors
    /// - [`GovernanceError::NotQueued`] if no call is queued under `nonce`.
    ///
    /// # Events
    /// - `Cancelled`: the nonce.
    fn cancel(e: Env, nonce: u32);

    /// Executes a queued call once its delay has passed.
    ///
    /// The entry is removed before the call, so a re-entrant `execute` with
    /// the same nonce traps.
    ///
    /// # Authorization
    /// - Permissionless.
    ///
    /// # Arguments
    /// - `nonce`: the queued call to execute.
    ///
    /// # Errors
    /// - [`GovernanceError::NotQueued`] if no call is queued under `nonce`.
    /// - [`GovernanceError::NotUnlocked`] if the unlock time is still ahead of
    ///   the ledger clock.
    ///
    /// # Events
    /// - `Executed`: the nonce, target, and function.
    fn execute(e: Env, nonce: u32);

    /// Calls `set_status` on `target` at once, bypassing the delay.
    ///
    /// # Authorization
    /// - Owner only (`#[only_owner]`).
    ///
    /// # Arguments
    /// - `target`: the market contract.
    /// - `status`: the new status, as the market `Status` `u32` discriminant.
    ///
    /// # Events
    /// - `StatusSet`: the target and status.
    fn set_status(e: Env, target: Address, status: u32);

    /// Queues a delay change.
    ///
    /// The new delay takes effect only after the current delay passes, so
    /// the owner cannot shorten the timelock instantly. A second call
    /// replaces a pending change that has not been applied yet.
    ///
    /// # Authorization
    /// - Owner only (`#[only_owner]`).
    ///
    /// # Arguments
    /// - `new_delay`: the new delay (seconds), in `(0, 60 days]`.
    ///
    /// # Errors
    /// - [`GovernanceError::InvalidDelay`] if `new_delay` is zero or above 60 days.
    ///
    /// # Events
    /// - `Queued`: with nonce `u32::MAX`, this contract as the target, and the
    ///   unlock time of the pending change.
    fn set_delay(e: Env, new_delay: u64);

    /// Applies the pending delay change once the current delay has passed.
    ///
    /// # Authorization
    /// - Permissionless.
    ///
    /// # Errors
    /// - [`GovernanceError::NotQueued`] if no delay change is pending.
    /// - [`GovernanceError::NotUnlocked`] if the unlock time is still ahead of
    ///   the ledger clock.
    ///
    /// # Events
    /// - `DelaySet`: the old and new delay.
    fn apply_delay(e: Env);

    /// Returns the current delay (seconds).
    fn get_delay(e: Env) -> u64;

    /// Returns the queued [`QueuedCall`] under `nonce`.
    ///
    /// # Errors
    /// - [`GovernanceError::NotQueued`] if no call is queued under `nonce`.
    fn get_queued(e: Env, nonce: u32) -> QueuedCall;
}

#[contractimpl]
impl GovernanceContract {
    /// Initializes the governance contract with an owner and a delay.
    ///
    /// # Arguments
    ///
    /// * `e` - Access to the Soroban environment.
    /// * `owner` - The contract owner for the admin entries.
    /// * `delay` - The mandatory wait (seconds) before a queued call can
    ///   execute, in `(0, 60 days]`.
    ///
    /// # Errors
    ///
    /// * [`GovernanceError::InvalidDelay`] - If `delay` is zero or above 60
    ///   days.
    pub fn __constructor(e: Env, owner: Address, delay: u64) {
        if delay == 0 || delay > 60 * 24 * 3600 {
            panic_with_error!(&e, GovernanceError::InvalidDelay);
        }
        ownable::set_owner(&e, &owner);
        storage::set_delay(&e, delay);
        storage::extend_instance(&e);
    }
}

#[contractimpl]
impl Governance for GovernanceContract {
    #[only_owner]
    fn queue(e: Env, target: Address, fn_name: Symbol, args: Vec<Val>) -> u32 {
        storage::extend_instance(&e);
        let delay = storage::get_delay(&e);
        let unlock_time = e.ledger().timestamp() + delay;
        let nonce = storage::next_nonce(&e);
        let queued = QueuedCall {
            target: target.clone(),
            fn_name: fn_name.clone(),
            args,
            unlock_time,
        };
        storage::set_queued(&e, nonce, &queued);
        events::Queued {
            nonce,
            target,
            fn_name,
            unlock_time,
        }
        .publish(&e);
        nonce
    }

    #[only_owner]
    fn cancel(e: Env, nonce: u32) {
        storage::extend_instance(&e);
        if storage::get_queued(&e, nonce).is_none() {
            panic_with_error!(&e, GovernanceError::NotQueued);
        }
        storage::remove_queued(&e, nonce);
        events::Cancelled { nonce }.publish(&e);
    }

    fn execute(e: Env, nonce: u32) {
        storage::extend_instance(&e);
        let queued = storage::get_queued(&e, nonce)
            .unwrap_or_else(|| panic_with_error!(&e, GovernanceError::NotQueued));

        if queued.unlock_time > e.ledger().timestamp() {
            panic_with_error!(&e, GovernanceError::NotUnlocked);
        }

        // The entry is removed before the external call, so the forwarded
        // call cannot execute it again.
        let target = queued.target.clone();
        let fn_name = queued.fn_name.clone();
        storage::remove_queued(&e, nonce);

        e.invoke_contract::<Val>(&target, &fn_name, queued.args);

        events::Executed {
            nonce,
            target,
            fn_name,
        }
        .publish(&e);
    }

    #[only_owner]
    fn set_status(e: Env, target: Address, status: u32) {
        storage::extend_instance(&e);
        let args: Vec<Val> = Vec::from_array(&e, [status.into_val(&e)]);
        e.invoke_contract::<Val>(&target, &Symbol::new(&e, "set_status"), args);
        events::StatusSet { target, status }.publish(&e);
    }

    #[only_owner]
    fn set_delay(e: Env, new_delay: u64) {
        storage::extend_instance(&e);
        if new_delay == 0 || new_delay > 60 * 24 * 3600 {
            panic_with_error!(&e, GovernanceError::InvalidDelay);
        }
        let current_delay = storage::get_delay(&e);
        let unlock_time = e.ledger().timestamp() + current_delay;
        let pending = storage::PendingDelay {
            new_delay,
            unlock_time,
        };
        storage::set_pending_delay(&e, &pending);
        events::Queued {
            nonce: u32::MAX,
            target: e.current_contract_address(),
            fn_name: Symbol::new(&e, "set_delay"),
            unlock_time,
        }
        .publish(&e);
    }

    fn apply_delay(e: Env) {
        storage::extend_instance(&e);
        let pending = storage::get_pending_delay(&e)
            .unwrap_or_else(|| panic_with_error!(&e, GovernanceError::NotQueued));

        if pending.unlock_time > e.ledger().timestamp() {
            panic_with_error!(&e, GovernanceError::NotUnlocked);
        }

        let old_delay = storage::get_delay(&e);
        storage::remove_pending_delay(&e);
        storage::set_delay(&e, pending.new_delay);
        events::DelaySet {
            old_delay,
            new_delay: pending.new_delay,
        }
        .publish(&e);
    }

    fn get_delay(e: Env) -> u64 {
        storage::get_delay(&e)
    }

    fn get_queued(e: Env, nonce: u32) -> QueuedCall {
        storage::get_queued(&e, nonce)
            .unwrap_or_else(|| panic_with_error!(&e, GovernanceError::NotQueued))
    }
}

#[contractimpl(contracttrait)]
impl Ownable for GovernanceContract {}

#[cfg(test)]
mod test;
