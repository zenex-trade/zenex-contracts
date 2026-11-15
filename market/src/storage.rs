//! Ledger-key management, persistence, and TTL bumping.
//!
//! [`MarketData`] is a singleton. [`Position`]s are netted, keyed
//! `(user, is_long)`. An on-chain read of [`MarketData`], a position, or an
//! order extends the entry's TTL. A simulated call leaves no footprint.

use crate::dependencies::PriceData;
use crate::engine::adl::AdlState;
use crate::engine::config::Config;
use crate::engine::market_data::MarketData;
use crate::engine::order::Order;
use crate::engine::position::Position;
use crate::engine::status::Status;
use crate::engine::vault_order::VaultOrder;
use crate::errors::MarketError;
use soroban_sdk::{contracttype, panic_with_error, unwrap::UnwrapOptimized, Address, BytesN, Env};

// Three persistent TTL tiers, at Blend parity and assuming ~5 s per ledger:
// - Instance (30/31 d): small read-mostly state (config, addresses, status,
//   ADL flags), stored on the contract instance itself. It loads whole with
//   every invocation and is rent-bumped each transaction.
// - Shared (45/46 d): singleton state most transactions touch (MarketData).
// - User (100/120 d): per-user entries. Positions, each carrying its side's
//   decrease-order list, trade and vault orders, the order counter, and
//   claimable credit. These are long-lived. An archived position still
//   counts in the market totals and must be restored before it can be closed
//   or liquidated. An archived order is restored, keeper-paid, at fill.
/// Ledgers per day, assuming ~5 s per ledger.
const ONE_DAY_LEDGERS: u32 = 17280;

/// Remaining TTL (~30 days) below which an entry point extends the instance.
const LEDGER_THRESHOLD_INSTANCE: u32 = ONE_DAY_LEDGERS * 30;
/// TTL (~31 days) the instance is extended to.
const LEDGER_BUMP_INSTANCE: u32 = LEDGER_THRESHOLD_INSTANCE + ONE_DAY_LEDGERS;

/// Remaining TTL (~45 days) below which an access extends a shared-tier
/// entry.
const LEDGER_THRESHOLD_SHARED: u32 = ONE_DAY_LEDGERS * 45;
/// TTL (~46 days) a shared-tier entry is extended to.
const LEDGER_BUMP_SHARED: u32 = LEDGER_THRESHOLD_SHARED + ONE_DAY_LEDGERS;

/// Remaining TTL (~100 days) below which an access extends a user-tier
/// entry.
const LEDGER_THRESHOLD_USER: u32 = ONE_DAY_LEDGERS * 100;
/// TTL (~120 days) a user-tier entry is extended to.
pub(crate) const LEDGER_BUMP_USER: u32 = ONE_DAY_LEDGERS * 120;

// The threshold must exceed the oracle's widest close window (120 s). A
// shorter one lets a report the cache has superseded outlive the cache and
// price a protective call again.
/// Remaining TTL (~1 hour) below which a write extends the price cache.
const LEDGER_THRESHOLD_PRICE_CACHE: u32 = 720;
/// TTL (~1 day) the price cache is extended to.
pub(crate) const LEDGER_BUMP_PRICE_CACHE: u32 = ONE_DAY_LEDGERS;

/// Every ledger key the contract reads or writes. Each variant's doc gives
/// the stored value and the tier. A "lazy" key is absent during normal life
/// and is created only when the relevant state first occurs.
#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    // --- instance (bumped every tx, small read-mostly state) ---
    /// `Config`. The global market parameters. A mutable singleton.
    Config,
    /// `BytesN<32>`. The price stream id. Immutable, constructor-set.
    FeedId,
    /// `u32`. The operational `Status` discriminant. The status gate reads
    /// it without loading `Config`.
    Status,
    /// `Address`. The strategy-vault contract.
    Vault,
    /// `Address`. The settlement token, the collateral asset.
    Token,
    /// `Address`. The oracle contract.
    Oracle,
    /// `Address`. The treasury contract, the protocol fee sink.
    Treasury,
    /// `u64`. The first-delist timestamp. It is the grace and deadline
    /// anchor and the sticky-delist marker. Lazy: absent unless delisted.
    DelistedAt,
    /// `i128`. The flat settlement price, in the feed's native precision.
    /// Lazy: absent until set after the delist grace window.
    TerminalPrice,
    /// `AdlState`. The per-side ADL flags. A zeroed default until first
    /// written. Every increase fill reads it.
    Adl,
    // --- persistent, shared tier ---
    /// `MarketData`. The hot per-market state: open interest, margin,
    /// tokens, indices. A singleton in its own entry.
    MarketData,
    // --- temporary ---
    /// `PriceData`. The newest verified price the market has consumed,
    /// monotonic on `publish_time`. Lazy, and it lapses harmlessly.
    PriceCache,
    // --- persistent, user tier ---
    /// `(user, is_long)` to `Position`. The netted position. It carries
    /// the side's pending decrease order ids. Long and short are
    /// independent positions.
    Position(Address, bool),
    /// `(user, id)` to `VaultOrder`. A pending vault deposit or
    /// redemption.
    VaultOrder(Address, u32),
    /// `(user, id)` to `Order`. A pending keeper trade order.
    Order(Address, u32),
    /// `user` to `u32`. The next order id, shared by trade and vault orders,
    /// allocated from 1.
    OrderCounter(Address),
    /// `user` to `i128`. The funding owed to the user, plus any payout whose
    /// direct transfer failed (token-dec).
    ClaimableCredit(Address),
}

/// Extends the instance TTL when it has dropped below the threshold.
pub fn extend_instance(e: &Env) {
    e.storage()
        .instance()
        .extend_ttl(LEDGER_THRESHOLD_INSTANCE, LEDGER_BUMP_INSTANCE);
}

// --- instance: config and addresses ---

/// Returns the global market parameters.
pub fn get_config(e: &Env) -> Config {
    e.storage()
        .instance()
        .get(&DataKey::Config)
        .unwrap_optimized()
}

/// Stores the global market parameters.
pub fn set_config(e: &Env, config: &Config) {
    e.storage().instance().set(&DataKey::Config, config);
}

/// Returns the operational status.
pub fn get_status(e: &Env) -> Status {
    let raw: u32 = e
        .storage()
        .instance()
        .get(&DataKey::Status)
        .unwrap_optimized();
    Status::from_u32(e, raw)
}

/// Stores the operational status.
pub fn set_status(e: &Env, status: &Status) {
    e.storage()
        .instance()
        .set(&DataKey::Status, &(*status as u32));
}

/// Returns whether a flat settlement price is set. It is the pricing and
/// accrual switch.
pub fn has_terminal_price(e: &Env) -> bool {
    e.storage().instance().has(&DataKey::TerminalPrice)
}

/// Returns the flat settlement price, in the feed's native precision.
pub fn get_terminal_price(e: &Env) -> i128 {
    e.storage()
        .instance()
        .get(&DataKey::TerminalPrice)
        .unwrap_optimized()
}

/// Stores the flat settlement price, in the feed's native precision.
pub fn set_terminal_price(e: &Env, price: i128) {
    e.storage().instance().set(&DataKey::TerminalPrice, &price);
}

/// Returns the first-delist timestamp.
pub fn get_delisted_at(e: &Env) -> Option<u64> {
    e.storage().instance().get(&DataKey::DelistedAt)
}

/// Stores the first-delist timestamp.
pub fn set_delisted_at(e: &Env, timestamp: u64) {
    e.storage().instance().set(&DataKey::DelistedAt, &timestamp);
}

/// Deletes the delist marker.
pub fn remove_delisted_at(e: &Env) {
    e.storage().instance().remove(&DataKey::DelistedAt);
}

/// Returns the per-side ADL flags, or a zeroed default until first written.
pub fn get_adl(e: &Env) -> AdlState {
    e.storage()
        .instance()
        .get(&DataKey::Adl)
        .unwrap_or_default()
}

/// Stores the ADL state.
pub fn set_adl(e: &Env, state: &AdlState) {
    e.storage().instance().set(&DataKey::Adl, state);
}

/// Returns the strategy-vault contract address.
pub fn get_vault(e: &Env) -> Address {
    e.storage()
        .instance()
        .get(&DataKey::Vault)
        .unwrap_optimized()
}

/// Stores the strategy-vault contract address.
pub fn set_vault(e: &Env, vault: &Address) {
    e.storage().instance().set(&DataKey::Vault, vault);
}

/// Returns the settlement token contract address.
pub fn get_token(e: &Env) -> Address {
    e.storage()
        .instance()
        .get(&DataKey::Token)
        .unwrap_optimized()
}

/// Stores the settlement token contract address.
pub fn set_token(e: &Env, token: &Address) {
    e.storage().instance().set(&DataKey::Token, token);
}

/// Returns the oracle contract address.
pub fn get_oracle(e: &Env) -> Address {
    e.storage()
        .instance()
        .get(&DataKey::Oracle)
        .unwrap_optimized()
}

/// Stores the oracle contract address.
pub fn set_oracle(e: &Env, oracle: &Address) {
    e.storage().instance().set(&DataKey::Oracle, oracle);
}

/// Returns the treasury contract address.
pub fn get_treasury(e: &Env) -> Address {
    e.storage()
        .instance()
        .get(&DataKey::Treasury)
        .unwrap_optimized()
}

/// Stores the treasury contract address.
pub fn set_treasury(e: &Env, treasury: &Address) {
    e.storage().instance().set(&DataKey::Treasury, treasury);
}

// --- instance: immutable feed anchors (constructor-set, never updated) ---

/// Returns the price stream id.
pub fn get_feed_id(e: &Env) -> BytesN<32> {
    e.storage()
        .instance()
        .get(&DataKey::FeedId)
        .unwrap_optimized()
}

/// Stores the price stream id. Only the constructor calls it.
pub fn set_feed_id(e: &Env, feed_id: &BytesN<32>) {
    e.storage().instance().set(&DataKey::FeedId, feed_id);
}

// --- persistent, shared tier: MarketData (singleton) ---

/// Returns the market state singleton and extends its TTL.
pub fn get_market_data(e: &Env) -> MarketData {
    let key = DataKey::MarketData;
    let market_data = e.storage().persistent().get(&key).unwrap_optimized();
    e.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_THRESHOLD_SHARED, LEDGER_BUMP_SHARED);
    market_data
}

/// Stores the market state singleton and extends its TTL.
pub fn set_market_data(e: &Env, market_data: &MarketData) {
    let key = DataKey::MarketData;
    e.storage().persistent().set(&key, market_data);
    e.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_THRESHOLD_SHARED, LEDGER_BUMP_SHARED);
}

// --- temporary: price cache ---

/// Returns the newest verified price the market has consumed, or `None`
/// when never written or lapsed.
pub fn get_price_cache(e: &Env) -> Option<PriceData> {
    e.storage().temporary().get(&DataKey::PriceCache)
}

/// Stores the newest consumed price. The caller keeps it monotonic on
/// `publish_time`.
pub fn set_price_cache(e: &Env, price: &PriceData) {
    let key = DataKey::PriceCache;
    e.storage().temporary().set(&key, price);
    // Rewriting a live temporary entry does not refresh its TTL.
    e.storage()
        .temporary()
        .extend_ttl(&key, LEDGER_THRESHOLD_PRICE_CACHE, LEDGER_BUMP_PRICE_CACHE);
}

// --- persistent, user tier: positions ---

/// Returns the netted position for `(user, is_long)` and extends its TTL,
/// or `None` when no row is stored.
pub fn find_position(e: &Env, user: &Address, is_long: bool) -> Option<Position> {
    let key = DataKey::Position(user.clone(), is_long);
    let position = e.storage().persistent().get::<_, Position>(&key);
    if position.is_some() {
        e.storage()
            .persistent()
            .extend_ttl(&key, LEDGER_THRESHOLD_USER, LEDGER_BUMP_USER);
    }
    position
}

/// Returns the netted position for `(user, is_long)` and extends its TTL,
/// or the zeroed position when none is stored. A miss writes nothing. Zero
/// notional means no open position.
pub fn get_position(e: &Env, user: &Address, is_long: bool) -> Position {
    find_position(e, user, is_long).unwrap_or_else(|| Position::zeroed(e))
}

/// Stores the netted position for `(user, is_long)` and extends its TTL.
pub fn set_position(e: &Env, user: &Address, is_long: bool, position: &Position) {
    let key = DataKey::Position(user.clone(), is_long);
    e.storage().persistent().set(&key, position);
    e.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_THRESHOLD_USER, LEDGER_BUMP_USER);
}

// --- persistent, user tier: vault orders ---

/// Returns the vault order `(user, id)` and extends its TTL.
///
/// # Errors
/// - [`MarketError::VaultOrderNotFound`] if no such order exists.
pub fn get_vault_order(e: &Env, user: &Address, id: u32) -> VaultOrder {
    let key = DataKey::VaultOrder(user.clone(), id);
    let order = e
        .storage()
        .persistent()
        .get::<_, VaultOrder>(&key)
        .unwrap_or_else(|| panic_with_error!(e, MarketError::VaultOrderNotFound));
    e.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_THRESHOLD_USER, LEDGER_BUMP_USER);
    order
}

/// Stores the vault order `(user, id)` and extends its TTL.
pub fn set_vault_order(e: &Env, user: &Address, id: u32, order: &VaultOrder) {
    let key = DataKey::VaultOrder(user.clone(), id);
    e.storage().persistent().set(&key, order);
    e.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_THRESHOLD_USER, LEDGER_BUMP_USER);
}

/// Deletes the vault order `(user, id)`. A missing entry is not an error.
pub fn remove_vault_order(e: &Env, user: &Address, id: u32) {
    e.storage()
        .persistent()
        .remove(&DataKey::VaultOrder(user.clone(), id));
}

// --- persistent, user tier: per-user order counter ---

/// Returns `user`'s next unallocated order id without touching storage
/// state. `1` means none is allocated yet.
pub fn get_order_counter(e: &Env, user: &Address) -> u32 {
    e.storage()
        .persistent()
        .get(&DataKey::OrderCounter(user.clone()))
        .unwrap_or(1)
}

/// Allocates and returns the next order id for `user`, starting at 1. Id
/// `0` is reserved as the instant-execution return of a retired-market
/// redeem. Trade and vault orders share the counter.
pub fn next_order_id(e: &Env, user: &Address) -> u32 {
    let key = DataKey::OrderCounter(user.clone());
    let current: u32 = e.storage().persistent().get(&key).unwrap_or(1);
    e.storage().persistent().set(&key, &(current + 1));
    e.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_THRESHOLD_USER, LEDGER_BUMP_USER);
    current
}

// --- persistent, user tier: claimable credit ---

/// Returns the funding owed to `user`, plus any payout whose direct transfer
/// failed (token-dec), or `0` until the user is first credited.
pub fn get_claimable_credit(e: &Env, user: &Address) -> i128 {
    e.storage()
        .persistent()
        .get(&DataKey::ClaimableCredit(user.clone()))
        .unwrap_or(0)
}

/// Adds the signed `amount` (token-dec) to `user`'s claimable credit
/// balance and extends its TTL.
pub fn add_claimable_credit(e: &Env, user: &Address, amount: i128) {
    let key = DataKey::ClaimableCredit(user.clone());
    let current: i128 = e.storage().persistent().get(&key).unwrap_or(0);
    e.storage().persistent().set(&key, &(current + amount));
    e.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_THRESHOLD_USER, LEDGER_BUMP_USER);
}

// --- persistent, user tier: keeper trade orders ---
//
// `expiration` is a pure validity gate decoupled from the TTL. Every
// removal runs through our code, so the escrow is always resolved.

/// Returns the keeper order `(user, id)` and extends its TTL.
///
/// # Errors
/// - [`MarketError::OrderNotFound`] if no such order exists.
pub fn get_order(e: &Env, user: &Address, id: u32) -> Order {
    let key = DataKey::Order(user.clone(), id);
    let order = e
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| panic_with_error!(e, MarketError::OrderNotFound));
    e.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_THRESHOLD_USER, LEDGER_BUMP_USER);
    order
}

/// Stores the keeper order `(user, id)` and extends its TTL.
pub fn set_order(e: &Env, user: &Address, id: u32, order: &Order) {
    let key = DataKey::Order(user.clone(), id);
    e.storage().persistent().set(&key, order);
    e.storage()
        .persistent()
        .extend_ttl(&key, LEDGER_THRESHOLD_USER, LEDGER_BUMP_USER);
}

/// Deletes the keeper order `(user, id)`. A missing entry is not an error.
pub fn remove_order(e: &Env, user: &Address, id: u32) {
    e.storage()
        .persistent()
        .remove(&DataKey::Order(user.clone(), id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{contract, Env};

    #[contract]
    struct Harness;

    // The loader reads a miss as the zeroed row and writes nothing.
    #[test]
    fn get_position_reads_a_miss_without_storing_it() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        let user = Address::generate(&e);
        e.as_contract(&contract_id, || {
            let position = get_position(&e, &user, true);
            assert_eq!(position.notional, 0, "absent row reads as zeroed");
            assert!(
                find_position(&e, &user, true).is_none(),
                "a read leaves the row absent",
            );
            assert!(
                !e.storage()
                    .persistent()
                    .has(&DataKey::Position(user.clone(), true)),
                "the loader stores nothing",
            );
        });
    }

    // Each tier extends to its own horizon at 17_280 ledgers a day: the
    // instance to 31 days (535_680 ledgers) and the shared MarketData entry
    // to 46 days (794_880 ledgers).
    #[test]
    fn instance_and_shared_tiers_extend_to_their_horizons() {
        use soroban_sdk::testutils::storage::{Instance as _, Persistent as _};
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            extend_instance(&e);
            assert_eq!(e.storage().instance().get_ttl(), 535_680);
            set_market_data(&e, &MarketData::default());
            assert_eq!(
                e.storage().persistent().get_ttl(&DataKey::MarketData),
                794_880
            );
        });
    }
}
