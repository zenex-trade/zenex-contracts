//! Storage for the oracle: the instance-tier config, with TTL bumping, and
//! the temporary-tier memo of verified report bodies.

use soroban_sdk::unwrap::UnwrapOptimized;
use soroban_sdk::{contracttype, Address, Bytes, BytesN, Env};

/// Ledgers per day, assuming ~5 s per ledger.
const ONE_DAY_LEDGERS: u32 = 17280;
/// Remaining TTL (~30 days) below which an entry point extends the instance.
const LEDGER_THRESHOLD_INSTANCE: u32 = ONE_DAY_LEDGERS * 30;
/// TTL (~31 days) the instance is extended to.
const LEDGER_BUMP_INSTANCE: u32 = LEDGER_THRESHOLD_INSTANCE + ONE_DAY_LEDGERS;

/// Every key the contract reads or writes. The constructor stores the four
/// instance keys, so their getters below trap only on an uninitialized
/// contract. The temporary key is lazy.
#[contracttype]
pub enum DataKey {
    // --- instance ---
    /// `Address`. Chainlink's deployed verifier contract. The constructor sets
    /// it, and no entry point changes it.
    Verifier,
    /// `u64`. Max report age for strict (fill) verifications (seconds).
    TradeStaleness,
    /// `u64`. Max report age for protective (gap-closing) verifications
    /// (seconds).
    CloseStaleness,
    /// `i128`. The `SCALAR_18`-scaled spread narrowing factor.
    SpreadReductionFactor,
    // --- temporary ---
    /// `Bytes`. The verifier's returned body for the signed report whose
    /// `sha256` is the key. Lazy. It lapses with the temporary tier.
    VerifiedReport(BytesN<32>),
}

/// Extends the instance TTL when it has dropped below the threshold.
pub fn extend_instance(e: &Env) {
    e.storage()
        .instance()
        .extend_ttl(LEDGER_THRESHOLD_INSTANCE, LEDGER_BUMP_INSTANCE);
}

/// Returns the verifier contract address.
pub fn get_verifier(e: &Env) -> Address {
    e.storage()
        .instance()
        .get(&DataKey::Verifier)
        .unwrap_optimized()
}

/// Stores the verifier contract address. Only the constructor calls it.
pub fn set_verifier(e: &Env, verifier: &Address) {
    e.storage().instance().set(&DataKey::Verifier, verifier);
}

/// Returns the strict (fill) staleness window (seconds).
pub fn get_trade_staleness(e: &Env) -> u64 {
    e.storage()
        .instance()
        .get(&DataKey::TradeStaleness)
        .unwrap_optimized()
}

/// Stores the strict (fill) staleness window (seconds). The caller validates
/// the range.
pub fn set_trade_staleness(e: &Env, seconds: u64) {
    e.storage()
        .instance()
        .set(&DataKey::TradeStaleness, &seconds);
}

/// Returns the protective (gap-closing) staleness window (seconds).
pub fn get_close_staleness(e: &Env) -> u64 {
    e.storage()
        .instance()
        .get(&DataKey::CloseStaleness)
        .unwrap_optimized()
}

/// Stores the protective (gap-closing) staleness window (seconds). The
/// caller validates the range.
pub fn set_close_staleness(e: &Env, seconds: u64) {
    e.storage()
        .instance()
        .set(&DataKey::CloseStaleness, &seconds);
}

/// Returns the spread reduction factor (`SCALAR_18`-scaled).
pub fn get_spread_reduction_factor(e: &Env) -> i128 {
    e.storage()
        .instance()
        .get(&DataKey::SpreadReductionFactor)
        .unwrap_optimized()
}

/// Stores the spread reduction factor (`SCALAR_18`-scaled). The caller
/// validates the range.
pub fn set_spread_reduction_factor(e: &Env, factor: i128) {
    e.storage()
        .instance()
        .set(&DataKey::SpreadReductionFactor, &factor);
}

// --- temporary: verified report memo ---

/// Returns the verifier's returned body for the signed report with
/// `sha256` `digest`, or `None` when never verified or lapsed.
pub fn get_verified_report(e: &Env, digest: &BytesN<32>) -> Option<Bytes> {
    e.storage()
        .temporary()
        .get(&DataKey::VerifiedReport(digest.clone()))
}

/// Stores the verifier's returned `body` for the signed report with
/// `sha256` `digest`. The caller writes a digest only when no live entry
/// holds it.
pub fn set_verified_report(e: &Env, digest: &BytesN<32>, body: &Bytes) {
    // No `extend_ttl`: a lapsed entry is only a miss (re-verified), so the
    // network-minimum temporary lifetime is enough even where it undercuts
    // `close_staleness`.
    e.storage()
        .temporary()
        .set(&DataKey::VerifiedReport(digest.clone()), body);
}
