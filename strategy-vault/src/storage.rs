//! Instance storage for the strategy address, with TTL bumping.

use soroban_sdk::{contracttype, unwrap::UnwrapOptimized, Address, Env};

/// Ledgers per day, assuming ~5 s per ledger.
const ONE_DAY_LEDGERS: u32 = 17280;

// The instance tier matches the market's 31-day convention. The market reads
// the vault inside `Market::load` on every keeper path, so the vault lease
// must be no shorter than the market lease.
/// Remaining TTL (~30 days) below which an entry point extends the instance.
pub(crate) const LEDGER_THRESHOLD_INSTANCE: u32 = ONE_DAY_LEDGERS * 30;
/// TTL (~31 days) the instance is extended to.
pub(crate) const LEDGER_BUMP_INSTANCE: u32 = LEDGER_THRESHOLD_INSTANCE + ONE_DAY_LEDGERS;

/// The vault's own instance keys. The share token's keys belong to the
/// OpenZeppelin modules.
#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub enum StrategyStorageKey {
    /// `Address`. The registered strategy, the market contract. Immutable.
    Strategy,
}

/// Extends the instance TTL when it has dropped below the threshold.
pub fn extend_instance(e: &Env) {
    e.storage()
        .instance()
        .extend_ttl(LEDGER_THRESHOLD_INSTANCE, LEDGER_BUMP_INSTANCE);
}

/// Returns the registered strategy. Traps if the constructor never ran.
pub fn get_strategy(e: &Env) -> Address {
    e.storage()
        .instance()
        .get::<StrategyStorageKey, Address>(&StrategyStorageKey::Strategy)
        .unwrap_optimized()
}

/// Stores the registered strategy. Only the constructor calls it.
pub fn set_strategy(e: &Env, strategy: &Address) {
    e.storage()
        .instance()
        .set::<StrategyStorageKey, Address>(&StrategyStorageKey::Strategy, strategy);
}
