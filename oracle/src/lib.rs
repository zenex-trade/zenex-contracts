#![no_std]

//! Zenex oracle contract: a Chainlink Data Streams adapter for the market.
//!
//! Chainlink's deployed verifier checks the DON signatures. This contract
//! decodes the V3 report. It enforces every protocol gate (feed identity,
//! validity window, two-tier staleness, price sanity, spread reduction)
//! before it exposes the [`PriceData`].

mod constants;
mod dependencies;
mod error;
pub mod events;
mod report;
mod storage;
mod verify;

use soroban_sdk::{
    contract, contractimpl, contracttype, panic_with_error, Address, Bytes, BytesN, Env,
};
use stellar_access::ownable::{self as ownable, Ownable};
use stellar_contract_utils::upgradeable::{self as upgradeable, Upgradeable};
use stellar_macros::only_owner;

use crate::constants::SCALAR_18;
use crate::dependencies::VerifierClient;
use crate::events::{SpreadReductionUpdate, StalenessUpdate};

pub use crate::constants::{
    MAX_CLOSE_STALENESS_SECONDS, MAX_TRADE_STALENESS_SECONDS, MIN_STALENESS_SECONDS,
};
pub use crate::error::OracleError;
/// Decodes a verified body in integration fixtures using the contract's own decoder.
#[cfg(feature = "testutils")]
pub use crate::report::decode as decode_report;
pub use crate::report::ReportDataV3;

/// Verified price data returned by the oracle. Prices keep the feed's native
/// precision, and no exponent travels with them: one market's prices all
/// come from one stream, which the anchored `feed_id` pins.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceData {
    /// Best bid after spread reduction, in the feed's native precision.
    pub bid: i128,
    /// Best ask after spread reduction, in the same precision as `bid`.
    pub ask: i128,
    /// The report's observation time, `observationsTimestamp`, as a Unix
    /// timestamp (seconds).
    pub publish_time: u64,
}

/// Chainlink Data Streams price adapter.
#[contract]
pub struct Oracle;

#[contractimpl]
impl Oracle {
    /// Initializes the oracle.
    ///
    /// # Arguments
    ///
    /// * `env` - Access to the Soroban environment.
    /// * `owner` - The address that may update the staleness windows and the
    ///   spread reduction.
    /// * `verifier` - Chainlink's deployed Data Streams verifier contract. No
    ///   entry point changes it. Only an owner upgrade can, and the upgrade
    ///   keeps the oracle address each market stores.
    /// * `trade_staleness` - The maximum age of a price observation for
    ///   strict (fill) verifications (seconds).
    /// * `close_staleness` - The maximum age for protective (gap-closing)
    ///   verifications (seconds). At least `trade_staleness`.
    /// * `spread_reduction_factor` - Symmetric bid/ask narrowing toward their
    ///   midpoint, `SCALAR_18`-scaled. `0` is off, and `SCALAR_18` collapses
    ///   both onto the mid.
    ///
    /// # Errors
    ///
    /// * [`OracleError::InvalidStaleness`] - If the staleness pair is out of
    ///   bounds.
    /// * [`OracleError::InvalidSpreadReduction`] - If the factor is outside
    ///   `[0, SCALAR_18]`.
    pub fn __constructor(
        env: Env,
        owner: Address,
        verifier: Address,
        trade_staleness: u64,
        close_staleness: u64,
        spread_reduction_factor: i128,
    ) {
        require_valid_staleness(&env, trade_staleness, close_staleness);
        if spread_reduction_factor < 0 || spread_reduction_factor > SCALAR_18 {
            panic_with_error!(&env, OracleError::InvalidSpreadReduction);
        }
        ownable::set_owner(&env, &owner);
        // The baseline for future migrations. Refer to the `Upgradeable` impl.
        upgradeable::set_schema_version(&env, 1);
        storage::set_verifier(&env, &verifier);
        storage::set_trade_staleness(&env, trade_staleness);
        storage::set_close_staleness(&env, close_staleness);
        storage::set_spread_reduction_factor(&env, spread_reduction_factor);
        storage::extend_instance(&env);
    }

    /// Verifies a signed Data Streams report and returns the gated
    /// `PriceData` for the caller's stream.
    ///
    /// A still-cached verified report skips the verifier call.
    ///
    /// # Arguments
    ///
    /// * `env` - Access to the Soroban environment.
    /// * `report` - The signed report blob.
    /// * `feed_id` - The caller's stream anchor. It must equal the report's
    ///   `feedId`.
    /// * `protective` - Uses `close_staleness` instead of `trade_staleness` as
    ///   the age window.
    ///
    /// # Errors
    ///
    /// * [`OracleError::InvalidData`] - If the body fails decoding or the
    ///   `feedId` is not a V3 stream id.
    /// * [`OracleError::FeedMismatch`] - If the report prices another stream.
    /// * [`OracleError::PriceAhead`] - If the validity start or the
    ///   observation is over `trade_staleness` ahead of the ledger.
    /// * [`OracleError::ReportExpired`] - If the ledger has passed
    ///   `expiresAt`.
    /// * [`OracleError::PriceStale`] - If the observation is older than the
    ///   age window.
    /// * [`OracleError::InvalidPrice`] - If the benchmark or a side is not
    ///   positive, the book is crossed, or an `int192` overflows `i128`.
    pub fn verify_price(
        env: Env,
        report: Bytes,
        feed_id: BytesN<32>,
        protective: bool,
    ) -> PriceData {
        storage::extend_instance(&env);
        let max_ahead = storage::get_trade_staleness(&env);
        let max_staleness = if protective {
            storage::get_close_staleness(&env)
        } else {
            max_ahead
        };
        let digest: BytesN<32> = env.crypto().sha256(&report).into();
        // Every gate below runs on both paths, so a hit never widens a time
        // window. Known and accepted: a config Chainlink deactivates stays
        // replayable through the memo until its observation ages past
        // `close_staleness`. That can be up to `trade_staleness` plus
        // `close_staleness` after first acceptance. The verifier exposes no
        // config version to key on.
        let body = match storage::get_verified_report(&env, &digest) {
            Some(body) => body,
            None => {
                let verifier = VerifierClient::new(&env, &storage::get_verifier(&env));
                let body = verifier.verify(&report, &env.current_contract_address());
                storage::set_verified_report(&env, &digest, &body);
                body
            }
        };
        let decoded = report::decode(&env, &body);
        verify::to_price_data(
            &env,
            decoded,
            &feed_id,
            max_staleness,
            max_ahead,
            storage::get_spread_reduction_factor(&env),
        )
    }

    /// Updates both staleness windows.
    ///
    /// One call sets the pair atomically, so the `trade <= close` ordering
    /// is always validated against the new pair.
    ///
    /// # Arguments
    ///
    /// * `env` - Access to the Soroban environment.
    /// * `trade_staleness` - The maximum report age for strict (fill)
    ///   verifications (seconds).
    /// * `close_staleness` - The maximum report age for protective
    ///   (gap-closing) verifications (seconds). At least `trade_staleness`.
    ///
    /// # Errors
    ///
    /// * `OwnableError::OwnerNotSet` - If ownership was renounced.
    /// * [`OracleError::InvalidStaleness`] - If the staleness pair is out of
    ///   bounds.
    ///
    /// # Events
    ///
    /// * topics - `["staleness_update"]`
    /// * data - `[trade_staleness: u64, close_staleness: u64]`
    ///
    /// # Notes
    ///
    /// * Authorization for the owner is required.
    #[only_owner]
    pub fn update_staleness(env: Env, trade_staleness: u64, close_staleness: u64) {
        require_valid_staleness(&env, trade_staleness, close_staleness);
        storage::extend_instance(&env);
        storage::set_trade_staleness(&env, trade_staleness);
        storage::set_close_staleness(&env, close_staleness);
        StalenessUpdate {
            trade_staleness,
            close_staleness,
        }
        .publish(&env);
    }

    /// Updates the spread reduction factor.
    ///
    /// # Arguments
    ///
    /// * `env` - Access to the Soroban environment.
    /// * `spread_reduction_factor` - The `SCALAR_18`-scaled narrowing toward
    ///   the mid. `0` is off, and `SCALAR_18` collapses both sides onto the
    ///   mid.
    ///
    /// # Errors
    ///
    /// * `OwnableError::OwnerNotSet` - If ownership was renounced.
    /// * [`OracleError::InvalidSpreadReduction`] - If the factor is outside
    ///   `[0, SCALAR_18]`.
    ///
    /// # Events
    ///
    /// * topics - `["spread_reduction_update"]`
    /// * data - `[spread_reduction_factor: i128]`
    ///
    /// # Notes
    ///
    /// * Authorization for the owner is required.
    #[only_owner]
    pub fn update_spread_reduction_factor(env: Env, spread_reduction_factor: i128) {
        if spread_reduction_factor < 0 || spread_reduction_factor > SCALAR_18 {
            panic_with_error!(&env, OracleError::InvalidSpreadReduction);
        }
        storage::extend_instance(&env);
        storage::set_spread_reduction_factor(&env, spread_reduction_factor);
        SpreadReductionUpdate {
            spread_reduction_factor,
        }
        .publish(&env);
    }

    /// Returns the Chainlink verifier contract address.
    ///
    /// # Arguments
    ///
    /// * `env` - Access to the Soroban environment.
    pub fn verifier(env: Env) -> Address {
        storage::get_verifier(&env)
    }

    /// Returns the strict (fill) staleness window (seconds).
    ///
    /// # Arguments
    ///
    /// * `env` - Access to the Soroban environment.
    pub fn trade_staleness(env: Env) -> u64 {
        storage::get_trade_staleness(&env)
    }

    /// Returns the protective (gap-closing) staleness window (seconds).
    ///
    /// # Arguments
    ///
    /// * `env` - Access to the Soroban environment.
    pub fn close_staleness(env: Env) -> u64 {
        storage::get_close_staleness(&env)
    }

    /// Returns the current spread reduction factor (`SCALAR_18`-scaled).
    ///
    /// # Arguments
    ///
    /// * `env` - Access to the Soroban environment.
    pub fn spread_reduction_factor(env: Env) -> i128 {
        storage::get_spread_reduction_factor(&env)
    }
}

/// Traps unless the staleness pair is valid: the trade window within
/// `[MIN_STALENESS_SECONDS, MAX_TRADE_STALENESS_SECONDS]` and the close
/// window within `[trade_staleness, MAX_CLOSE_STALENESS_SECONDS]`.
///
/// # Errors
/// - [`OracleError::InvalidStaleness`] if any bound fails.
fn require_valid_staleness(env: &Env, trade_staleness: u64, close_staleness: u64) {
    if trade_staleness < MIN_STALENESS_SECONDS
        || trade_staleness > MAX_TRADE_STALENESS_SECONDS
        || close_staleness < trade_staleness
        || close_staleness > MAX_CLOSE_STALENESS_SECONDS
    {
        panic_with_error!(env, OracleError::InvalidStaleness);
    }
}

#[contractimpl(contracttrait)]
impl Ownable for Oracle {}

// `contracttrait` is deliberately absent: `Upgradeable` carries no default
// bodies, so all authorization is ours to enforce.
#[contractimpl]
impl Upgradeable for Oracle {
    /// Replaces the contract WASM. Storage is kept as is, not migrated.
    ///
    /// # Arguments
    ///
    /// * `e` - Access to the Soroban environment.
    /// * `new_wasm_hash` - The hash of an uploaded WASM blob.
    /// * `operator` - The owner.
    ///
    /// # Errors
    ///
    /// * `OwnableError::OwnerNotSet` - If ownership was renounced.
    /// * [`OracleError::UpgradeNotOwner`] - If `operator` is not the owner.
    ///
    /// # Notes
    ///
    /// * Authorization for the owner is required, and `operator` must be the
    ///   owner.
    #[only_owner]
    fn upgrade(e: &Env, new_wasm_hash: BytesN<32>, operator: Address) {
        // `operator` is inert under `ownable`, so it is pinned to the owner.
        if ownable::get_owner(e).as_ref() != Some(&operator) {
            panic_with_error!(e, OracleError::UpgradeNotOwner);
        }
        // The call most likely to arrive after a long quiet stretch, so the
        // TTL is extended here too.
        storage::extend_instance(e);
        upgradeable::upgrade(e, &new_wasm_hash);
    }
}

#[cfg(test)]
mod test;
