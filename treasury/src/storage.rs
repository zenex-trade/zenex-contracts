//! Instance storage for the fee rate, with TTL bumping.

use soroban_sdk::{Env, Symbol};

/// Ledgers per day, assuming ~5 s per ledger.
const ONE_DAY_LEDGERS: u32 = 17280;

/// Remaining TTL (~30 days) below which an entry point extends the instance.
const LEDGER_THRESHOLD_INSTANCE: u32 = ONE_DAY_LEDGERS * 30;
/// TTL (~31 days) the instance is extended to.
const LEDGER_BUMP_INSTANCE: u32 = LEDGER_THRESHOLD_INSTANCE + ONE_DAY_LEDGERS;

/// Extends the instance TTL when it has dropped below the threshold.
pub fn extend_instance(e: &Env) {
    e.storage()
        .instance()
        .extend_ttl(LEDGER_THRESHOLD_INSTANCE, LEDGER_BUMP_INSTANCE);
}

/// Returns the fee rate (`SCALAR_18` fraction).
///
/// The constructor always stores a rate, so the `0` fallback is unreachable
/// on an initialized contract.
pub fn get_rate(e: &Env) -> i128 {
    e.storage()
        .instance()
        .get::<Symbol, i128>(&Symbol::new(e, "Rate"))
        .unwrap_or(0)
}

/// Stores the fee rate (`SCALAR_18` fraction). The caller validates the range.
pub fn set_rate(e: &Env, rate: i128) {
    e.storage()
        .instance()
        .set::<Symbol, i128>(&Symbol::new(e, "Rate"), &rate);
}
