//! Ledger keys and TTL bumping. The delay, nonce, and owner live on the
//! instance. Queued calls and the pending delay change live in persistent
//! storage on one flat tier, and every access extends the entry. An archived
//! entry can be restored, so a queued call does not vanish.

use soroban_sdk::unwrap::UnwrapOptimized;
use soroban_sdk::{contracttype, Address, Env, Symbol, Val, Vec};

/// Every ledger key the contract reads or writes.
#[contracttype]
#[derive(Clone)]
pub enum GovKey {
    /// `u64`, instance. The current timelock delay (seconds).
    Delay,
    /// `u32`, instance. The next queue nonce.
    Nonce,
    /// `QueuedCall`, persistent. Keyed by nonce.
    Queued(u32),
    /// `PendingDelay`, persistent. At most one change is pending.
    PendingDelay,
}

/// A queued admin call. It is stored until executed or cancelled.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedCall {
    /// The contract the call invokes.
    pub target: Address,
    /// The function name on `target`.
    pub fn_name: Symbol,
    /// The invocation arguments.
    pub args: Vec<Val>,
    /// Unix timestamp (seconds) at which the call becomes executable.
    pub unlock_time: u64,
}

/// A pending delay change. It unlocks after the current delay passes.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingDelay {
    /// The delay to apply (seconds).
    pub new_delay: u64,
    /// Unix timestamp (seconds) at which the change becomes applicable.
    pub unlock_time: u64,
}

/// Ledgers per day, assuming ~5 s per ledger.
pub const ONE_DAY_LEDGERS: u32 = 17280;

// The 61-day lease outlives the 60-day delay ceiling, so a call queued on a
// fresh lease unlocks before the instance can lapse. The instance is
// restorable regardless, and the queued entry outlives the wait.
/// Remaining TTL (~60 days) below which an entry point extends the instance.
pub(crate) const LEDGER_THRESHOLD_INSTANCE: u32 = ONE_DAY_LEDGERS * 60;
/// TTL (~61 days) the instance is extended to.
pub(crate) const LEDGER_BUMP_INSTANCE: u32 = LEDGER_THRESHOLD_INSTANCE + ONE_DAY_LEDGERS;

/// Remaining TTL (~100 days) below which an access extends a queued call or
/// the pending delay change.
pub(crate) const LEDGER_THRESHOLD_QUEUED: u32 = ONE_DAY_LEDGERS * 100;
/// TTL (~120 days) a queued call or the pending delay change is extended to.
/// It outlives the 60-day delay ceiling.
pub(crate) const LEDGER_BUMP_QUEUED: u32 = ONE_DAY_LEDGERS * 120;

/// Extends the instance TTL (delay, nonce, owner) when it has dropped below
/// the threshold.
pub fn extend_instance(e: &Env) {
    e.storage()
        .instance()
        .extend_ttl(LEDGER_THRESHOLD_INSTANCE, LEDGER_BUMP_INSTANCE);
}

/// Extends the TTL of the persistent entry under `key` when it has dropped
/// below the threshold.
fn extend_entry(e: &Env, key: &GovKey) {
    e.storage()
        .persistent()
        .extend_ttl(key, LEDGER_THRESHOLD_QUEUED, LEDGER_BUMP_QUEUED);
}

/// Returns the timelock delay (seconds). Traps if the constructor never ran.
pub fn get_delay(e: &Env) -> u64 {
    e.storage()
        .instance()
        .get(&GovKey::Delay)
        .unwrap_optimized()
}

/// Returns the queued call under `nonce` and extends its TTL, or `None` if
/// absent.
pub fn get_queued(e: &Env, nonce: u32) -> Option<QueuedCall> {
    let key = GovKey::Queued(nonce);
    let queued: QueuedCall = e.storage().persistent().get(&key)?;
    extend_entry(e, &key);
    Some(queued)
}

/// Returns the pending delay change and extends its TTL, or `None` if absent.
pub fn get_pending_delay(e: &Env) -> Option<PendingDelay> {
    let key = GovKey::PendingDelay;
    let pending: PendingDelay = e.storage().persistent().get(&key)?;
    extend_entry(e, &key);
    Some(pending)
}

/// Stores the timelock delay (seconds). The caller validates the range.
pub fn set_delay(e: &Env, delay: u64) {
    e.storage().instance().set(&GovKey::Delay, &delay);
}

/// Stores a queued call under `nonce` and extends its TTL.
pub fn set_queued(e: &Env, nonce: u32, queued: &QueuedCall) {
    let key = GovKey::Queued(nonce);
    e.storage().persistent().set(&key, queued);
    extend_entry(e, &key);
}

/// Stores the pending delay change and extends its TTL. It replaces any
/// earlier pending change.
pub fn set_pending_delay(e: &Env, pending: &PendingDelay) {
    let key = GovKey::PendingDelay;
    e.storage().persistent().set(&key, pending);
    extend_entry(e, &key);
}

/// Returns the next nonce and advances the counter. The first nonce is `0`.
pub fn next_nonce(e: &Env) -> u32 {
    let key = GovKey::Nonce;
    let current: u32 = e.storage().instance().get(&key).unwrap_or(0);
    e.storage().instance().set(&key, &(current + 1));
    current
}

/// Removes the queued call under `nonce`. A missing entry is not an error.
pub fn remove_queued(e: &Env, nonce: u32) {
    e.storage().persistent().remove(&GovKey::Queued(nonce));
}

/// Removes the pending delay change. A missing entry is not an error.
pub fn remove_pending_delay(e: &Env) {
    e.storage().persistent().remove(&GovKey::PendingDelay);
}
