#![no_std]

//! Zenex treasury contract.
//!
//! Holds the protocol fee rate and the collected fees. The market reads the
//! rate with [`Treasury::get_rate`] and pays the treasury cut directly at
//! settlement. The owner withdraws the accumulated balance.

pub mod events;
mod storage;

#[cfg(test)]
mod test;

use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, panic_with_error, token::TokenClient,
    Address, Env,
};
use stellar_access::ownable::{self as ownable, Ownable};
use stellar_macros::only_owner;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum TreasuryError {
    /// The fee rate is outside `[0, SCALAR_18 / 2]` (0% to 50%).
    InvalidRate = 900,
}

/// Holds the protocol fee rate and the collected fees.
#[contract]
pub struct TreasuryContract;

/// Fixed-point scale of the fee rate: `SCALAR_18` is 100%.
const SCALAR_18: i128 = 1_000_000_000_000_000_000;

/// The treasury's cross-contract surface.
#[contractclient(name = "TreasuryClient")]
pub trait Treasury {
    /// Returns the current protocol fee rate (`SCALAR_18` fraction).
    fn get_rate(e: Env) -> i128;

    /// Sets the protocol fee rate.
    ///
    /// # Authorization
    /// - Owner only (`#[only_owner]`).
    ///
    /// # Arguments
    /// - `rate`: the new fee rate as a `SCALAR_18` fraction, in
    ///   `[0, SCALAR_18 / 2]`. For example, `1e17` is 10%.
    ///
    /// # Errors
    /// - [`TreasuryError::InvalidRate`] if `rate` is outside `[0, SCALAR_18 / 2]`.
    ///
    /// # Events
    /// - [`events::RateUpdate`]: the new rate.
    fn set_rate(e: Env, rate: i128);

    /// Withdraws collected protocol fees.
    ///
    /// # Authorization
    /// - Owner only (`#[only_owner]`).
    ///
    /// # Arguments
    /// - `token`: the token contract to withdraw from.
    /// - `to`: the recipient address.
    /// - `amount`: the amount to withdraw (token-dec).
    ///
    /// # Events
    /// - [`events::Withdraw`]: the token, recipient, and amount.
    fn withdraw(e: Env, token: Address, to: Address, amount: i128);
}

#[contractimpl]
impl TreasuryContract {
    /// Initializes the treasury with an owner and a fee rate.
    ///
    /// # Arguments
    ///
    /// * `e` - Access to the Soroban environment.
    /// * `owner` - The address that may change the rate and withdraw fees.
    /// * `rate` - The protocol fee rate as a `SCALAR_18` fraction, in
    ///   `[0, SCALAR_18 / 2]`.
    ///
    /// # Errors
    ///
    /// * [`TreasuryError::InvalidRate`] - If `rate` is outside
    ///   `[0, SCALAR_18 / 2]`.
    pub fn __constructor(e: Env, owner: Address, rate: i128) {
        if !(0..=SCALAR_18 / 2).contains(&rate) {
            panic_with_error!(&e, TreasuryError::InvalidRate);
        }
        ownable::set_owner(&e, &owner);
        storage::set_rate(&e, rate);
    }
}

#[contractimpl]
impl Treasury for TreasuryContract {
    fn get_rate(e: Env) -> i128 {
        storage::extend_instance(&e);
        storage::get_rate(&e)
    }

    #[only_owner]
    fn set_rate(e: Env, rate: i128) {
        storage::extend_instance(&e);
        if !(0..=SCALAR_18 / 2).contains(&rate) {
            panic_with_error!(&e, TreasuryError::InvalidRate);
        }
        storage::set_rate(&e, rate);
        events::RateUpdate { rate }.publish(&e);
    }

    #[only_owner]
    fn withdraw(e: Env, token: Address, to: Address, amount: i128) {
        storage::extend_instance(&e);
        let token_client = TokenClient::new(&e, &token);
        // The token traps on an amount past the balance. Nothing is published
        // for a transfer that did not happen.
        token_client.transfer(&e.current_contract_address(), &to, &amount);
        events::Withdraw { token, to, amount }.publish(&e);
    }
}

#[contractimpl(contracttrait)]
impl Ownable for TreasuryContract {}
