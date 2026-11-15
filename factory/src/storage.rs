//! Ledger-key management and TTL bumping.

use soroban_sdk::{contracttype, unwrap::UnwrapOptimized, Address, BytesN, Env, Symbol};

/// Ledgers per day, assuming ~5 s per ledger.
const ONE_DAY_LEDGERS: u32 = 17280;

/// Remaining TTL (~30 days) below which an entry point extends the instance.
const LEDGER_THRESHOLD_INSTANCE: u32 = ONE_DAY_LEDGERS * 30;
/// TTL (~31 days) the instance is extended to.
const LEDGER_BUMP_INSTANCE: u32 = LEDGER_THRESHOLD_INSTANCE + ONE_DAY_LEDGERS;

/// Remaining TTL (~100 days) below which a read or write extends a
/// deployed-market entry.
const LEDGER_THRESHOLD_POOL: u32 = ONE_DAY_LEDGERS * 100;
/// TTL (~120 days) a deployed-market entry is extended to.
const LEDGER_BUMP_POOL: u32 = LEDGER_THRESHOLD_POOL + 20 * ONE_DAY_LEDGERS;

/// Persistent ledger keys. No entry point or event reaches this key, so spec
/// shaking strips it from the exported spec. The SDK marks `export = false`
/// a deprecated no-op.
#[derive(Clone)]
#[contracttype(export = false)]
pub enum FactoryDataKey {
    /// `bool`, persistent. Present and `true` for every market this factory
    /// deployed.
    Pools(Address),
}

/// Deployment inputs for the markets this factory creates. The owner replaces
/// them through `set_init_meta`.
#[derive(Clone)]
#[contracttype]
pub struct FactoryInitMeta {
    /// The installed market contract WASM hash.
    pub market_hash: BytesN<32>,
    /// The installed strategy-vault WASM hash.
    pub vault_hash: BytesN<32>,
    /// The treasury address wired into every market deploy.
    pub treasury: Address,
}

/// Extends the instance TTL when it has dropped below the threshold.
pub fn extend_instance(e: &Env) {
    e.storage()
        .instance()
        .extend_ttl(LEDGER_THRESHOLD_INSTANCE, LEDGER_BUMP_INSTANCE);
}

/// Returns the deployment inputs. Traps if the constructor never ran.
pub fn get_init_meta(e: &Env) -> FactoryInitMeta {
    e.storage()
        .instance()
        .get::<Symbol, FactoryInitMeta>(&Symbol::new(e, "InitMeta"))
        .unwrap_optimized()
}

/// Stores the deployment inputs.
pub fn set_init_meta(e: &Env, meta: &FactoryInitMeta) {
    e.storage()
        .instance()
        .set::<Symbol, FactoryInitMeta>(&Symbol::new(e, "InitMeta"), meta);
}

/// Returns whether `pool_id` is a recorded deployment. A hit also extends the
/// entry's TTL, so a market that is looked up stays recorded.
pub fn is_deployed(e: &Env, pool_id: &Address) -> bool {
    let key = FactoryDataKey::Pools(pool_id.clone());
    if let Some(result) = e.storage().persistent().get::<FactoryDataKey, bool>(&key) {
        e.storage()
            .persistent()
            .extend_ttl(&key, LEDGER_THRESHOLD_POOL, LEDGER_BUMP_POOL);
        result
    } else {
        false
    }
}

/// Records `pool_id` as a deployment and sets the entry's TTL.
pub fn set_deployed(e: &Env, pool_id: &Address) {
    let key = FactoryDataKey::Pools(pool_id.clone());
    e.storage()
        .persistent()
        .set::<FactoryDataKey, bool>(&key, &true);
    e.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_THRESHOLD_POOL, LEDGER_BUMP_POOL);
}
