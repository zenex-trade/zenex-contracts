#![no_std]
#![allow(clippy::too_many_arguments)]

//! Zenex factory contract. It deploys an isolated strategy-vault and market
//! pair atomically from the WASM hashes in [`FactoryInitMeta`] and records
//! each market for [`Factory::is_deployed`]. [`Factory::deploy`] is
//! permissionless. The owner controls only what future deploys get, the
//! [`FactoryInitMeta`] and the factory's own code, so a deployer trusts the
//! owner for them. Deployed markets own their upgrade authority.

mod errors;
pub mod events;
mod storage;

#[cfg(test)]
mod test;

pub use errors::FactoryError;
use events::{Deploy, InitMetaUpdate};
pub use storage::FactoryInitMeta;

use soroban_sdk::{
    contract, contractclient, contractimpl, contracttype, panic_with_error, Address, Bytes, BytesN,
    Env, String,
};
use stellar_access::ownable::{self as ownable, Ownable};
use stellar_contract_utils::upgradeable::{self as upgradeable, Upgradeable};
use stellar_macros::only_owner;

/// Mirrors the market contract's `Config` with the same XDR encoding, so the
/// market constructor decodes it directly. The field docs are the market's.
#[contracttype]
#[derive(Clone, Debug)]
pub struct Config {
    // --- sizing and fees ---
    /// Keeper share of trade, liquidation and vault fill fees (`SCALAR_18`).
    pub keeper_rate: i128,
    /// Minimum position notional (token-dec).
    pub min_position_notional: i128,
    /// Maximum position notional (token-dec).
    pub max_position_notional: i128,
    /// Per-side open-interest ceiling (token-dec). At least
    /// `max_position_notional`.
    pub max_open_interest: i128,
    /// Minimum `|notional|` per order, the dust floor (token-dec). At most
    /// `min_position_notional`.
    pub min_order_notional: i128,
    /// Minimum posted margin per order, the dust floor (token-dec).
    pub min_order_margin: i128,
    /// Flat keeper execution fee, escrowed per order at creation (token-dec).
    /// At most `min_order_margin`.
    pub exec_fee: i128,
    /// Dominant-side trade fee rate (`SCALAR_18`).
    pub fee_dom: i128,
    /// Non-dominant-side trade fee rate (`SCALAR_18`).
    pub fee_non_dom: i128,
    /// Impact fee denominator (token-dec). Every fill pays
    /// `notional * min(notional / impact_scalar, MAX_IMPACT_RATE)`.
    pub impact_scalar: i128,

    // --- utilization caps (SCALAR_18, util = side reserve / (vault / 2)) ---
    /// Utilization above which size increases on that side are blocked
    /// (`SCALAR_18`). Also each side's borrow-reserve denominator: capacity is
    /// `max_util_open * vault / 2`.
    pub max_util_open: i128,
    /// Utilization above which withdrawals are blocked (`SCALAR_18`), so the
    /// vault keeps at least `reserved / max_util_withdraw` of backing. At
    /// least `max_util_open`.
    pub max_util_withdraw: i128,

    // --- margin ---
    /// Initial margin rate (`SCALAR_18`). Max leverage is `1 / init_margin`.
    pub init_margin: i128,
    /// Hard liquidation floor (`SCALAR_18`). Below `init_margin`.
    pub maintenance_margin: i128,
    /// Liquidation fee rate (`SCALAR_18`). Every liquidation charges
    /// `min(max(equity, 0), ceil(liq_fee * notional))`.
    pub liq_fee: i128,

    // --- position lifecycle ---
    /// Decrease lock on newly added notional (seconds).
    pub notional_lock: u64,

    // --- borrowing (kink model over reserve utilization) ---
    /// Kink utilization on the normalized `[0, 1]` reserve curve
    /// (`SCALAR_18`). Below 1.
    pub target_util: i128,
    /// Borrowing-rate slope below the kink (`SCALAR_18`, per second). The
    /// rate there is `borrow_rate * utilization`.
    pub borrow_rate: i128,
    /// Borrowing rate at full utilization (`SCALAR_18`, per second). In
    /// `[borrow_rate, MAX_BORROW_RATE]`.
    pub increased_borrow_rate: i128,

    // --- funding velocity ---
    /// Velocity acceleration as skew widens (`SCALAR_18`, per second^2).
    pub funding_increase: i128,
    /// Velocity decay inside the decay band (`SCALAR_18`, per second^2).
    pub funding_decrease: i128,
    /// Skew band within which the rate holds (`SCALAR_18`).
    pub threshold_stable_funding: i128,
    /// Skew band within which the rate decays (`SCALAR_18`). At most
    /// `threshold_stable_funding`.
    pub threshold_decrease_funding: i128,
    /// Charged-rate floor (`SCALAR_18`, per second).
    pub funding_min: i128,
    /// Saved-rate hard cap (`SCALAR_18`, per second).
    pub funding_max: i128,

    // --- risk / ADL ---
    /// ADL trigger (`SCALAR_18`). A side arms when its pending PnL over half
    /// the vault exceeds this. In `[MIN_ADL_TRIGGER, max_pnl_trader]`, below 1.
    pub adl_max_pnl: i128,
    /// ADL clear target on the same measure (`SCALAR_18`). In
    /// `[MIN_ADL_CLEAR, adl_max_pnl]`.
    pub adl_clear_target: i128,
    /// Realized-profit haircut threshold (`SCALAR_18`), below 1. While a
    /// side's pending PnL exceeds this fraction of half the vault, close
    /// payouts scale by `allowance / side PnL`. Also the per-side profit cap
    /// in share-pricing PnL.
    pub max_pnl_trader: i128,
    /// Redeem gate (`SCALAR_18`). Redeems are blocked while a side's pending
    /// PnL exceeds this fraction of half the post-redeem balance. In
    /// `(0, adl_clear_target]`.
    pub max_pnl_withdraw: i128,

    // --- vault orders ---
    /// Redeem cooldown from a vault order's `created_at` (seconds). An
    /// operator liquidity-stickiness dial. `0` fills as soon as a
    /// post-creation price exists.
    pub redeem_lock: u64,
    /// Deposit fill fee rate on moved assets (`SCALAR_18`).
    pub deposit_fee: i128,
    /// Redeem fill fee rate on moved assets (`SCALAR_18`).
    pub redeem_fee: i128,
    /// Minimum assets per deposit order (token-dec).
    pub min_deposit: i128,
    /// Vault balance ceiling enforced on deposit fills (token-dec).
    pub max_vault_balance: i128,
}

/// Deploys an isolated market and vault pair per market.
#[contract]
pub struct FactoryContract;

/// The factory's cross-contract surface.
#[contractclient(name = "FactoryClient")]
pub trait Factory {
    /// Deploys a strategy-vault and market pair atomically and returns the
    /// `(market, vault)` addresses.
    ///
    /// The market registers the vault as its collateral vault. The vault
    /// registers the market as its immutable strategy. Both addresses derive
    /// from `admin` and the salts, so another deployer cannot front-run a salt.
    ///
    /// # Authorization
    /// - `admin` must authorize the call and both deployments. It becomes the
    ///   owner of the new market contract.
    ///
    /// # Arguments
    /// - `admin`: the deployment authorizer and owner of the new market.
    /// - `salt`: the salt for the market address. The vault salt derives from it.
    /// - `token`: the collateral token, the settlement token of both contracts.
    /// - `oracle`: the oracle contract.
    /// - `feed_id`: the market's price stream id. Immutable on the market.
    /// - `config`: the initial market [`Config`].
    /// - `vault_name`: the vault share-token name.
    /// - `vault_symbol`: the vault share-token symbol.
    ///
    /// # Errors
    /// - Traps in the host if a contract already exists at either derived
    ///   address, that is, on a repeated `(admin, salt)`.
    /// - Traps if either constructor rejects its inputs, including invalid
    ///   market `config` / `feed_id` or incompatible token/share metadata.
    ///   The host wraps constructor failures as `Error(Context, InvalidAction)`.
    ///   The underlying error codes remain in diagnostics, not in the caller's
    ///   result.
    ///
    /// # Events
    /// - [`Deploy`]: the deployed market and vault addresses.
    fn deploy(
        e: Env,
        admin: Address,
        salt: BytesN<32>,
        token: Address,
        oracle: Address,
        feed_id: BytesN<32>,
        config: Config,
        vault_name: String,
        vault_symbol: String,
    ) -> (Address, Address);

    /// Returns whether this factory deployed `market`.
    ///
    /// # Arguments
    /// - `market`: the market contract to look up.
    fn is_deployed(e: Env, market: Address) -> bool;

    /// Replaces the WASM hashes and treasury that future deploys use.
    ///
    /// The hashes live in instance storage, not in code, so upgrading the
    /// factory's own WASM does not refresh them. After a market or vault
    /// upgrade, this call stops new markets from deploying superseded code.
    /// Deployed markets are untouched.
    ///
    /// # Authorization
    /// - Owner only (`#[only_owner]`).
    ///
    /// # Arguments
    /// - `init_meta`: the replacement [`FactoryInitMeta`].
    ///
    /// # Events
    /// - [`InitMetaUpdate`]: the replacement metadata.
    fn set_init_meta(e: Env, init_meta: FactoryInitMeta);

    /// Returns the WASM hashes and treasury that future deploys use.
    fn get_init_meta(e: Env) -> FactoryInitMeta;
}

#[contractimpl]
impl FactoryContract {
    /// Initializes the factory with the installed WASM hashes and the
    /// treasury address.
    ///
    /// # Arguments
    ///
    /// * `e` - Access to the Soroban environment.
    /// * `owner` - The contract owner. It controls `set_init_meta` and
    ///   `upgrade`.
    /// * `init_meta` - The `FactoryInitMeta` for future deploys.
    pub fn __constructor(e: Env, owner: Address, init_meta: FactoryInitMeta) {
        ownable::set_owner(&e, &owner);
        // The baseline for future migrations. Refer to the `Upgradeable` impl.
        upgradeable::set_schema_version(&e, 1);
        storage::set_init_meta(&e, &init_meta);
    }
}

#[contractimpl]
impl Factory for FactoryContract {
    fn deploy(
        e: Env,
        admin: Address,
        salt: BytesN<32>,
        token: Address,
        oracle: Address,
        feed_id: BytesN<32>,
        config: Config,
        vault_name: String,
        vault_symbol: String,
    ) -> (Address, Address) {
        admin.require_auth();
        storage::extend_instance(&e);
        let init_meta = storage::get_init_meta(&e);

        // The vault salt is domain-separated by hashing, so one salt cannot
        // land both roles. A bijective transform, like a bit flip, would pair
        // salts that occupy each other's addresses.
        let mut vault_salt_preimage = Bytes::from_array(&e, &salt.to_array());
        vault_salt_preimage.extend_from_slice(b"vault");
        let vault_salt: BytesN<32> = e.crypto().sha256(&vault_salt_preimage).to_bytes();

        // Both addresses derive from the admin, not the factory, so the salt
        // alone cannot be front-run. The host requires `admin` to authorize
        // each deployment.
        let market_deployer = e.deployer().with_address(admin.clone(), salt);
        let vault_deployer = e.deployer().with_address(admin.clone(), vault_salt);
        let market_address = market_deployer.deployed_address();
        let vault_address = vault_deployer.deployed_address();

        // The vault deploys first. Its constructor does not call the market.
        vault_deployer.deploy_v2(
            init_meta.vault_hash,
            (
                vault_name,
                vault_symbol,
                token.clone(),
                market_address.clone(),
            ),
        );

        // The market deploys second. Its constructor only stores the vault
        // address, so the order does not matter to it.
        market_deployer.deploy_v2(
            init_meta.market_hash,
            (
                admin.clone(),
                token,
                vault_address.clone(),
                oracle,
                init_meta.treasury,
                feed_id,
                config,
            ),
        );

        storage::set_deployed(&e, &market_address);

        Deploy {
            trading: market_address.clone(),
            vault: vault_address.clone(),
        }
        .publish(&e);
        (market_address, vault_address)
    }

    fn is_deployed(e: Env, market: Address) -> bool {
        storage::extend_instance(&e);
        storage::is_deployed(&e, &market)
    }

    #[only_owner]
    fn set_init_meta(e: Env, init_meta: FactoryInitMeta) {
        storage::extend_instance(&e);
        storage::set_init_meta(&e, &init_meta);
        InitMetaUpdate { init_meta }.publish(&e);
    }

    fn get_init_meta(e: Env) -> FactoryInitMeta {
        storage::extend_instance(&e);
        storage::get_init_meta(&e)
    }
}

#[contractimpl(contracttrait)]
impl Ownable for FactoryContract {}

// `contracttrait` is deliberately absent: `Upgradeable` carries no default
// bodies, so all authorization is ours to enforce.
#[contractimpl]
impl Upgradeable for FactoryContract {
    /// Replaces the contract WASM. Storage is kept as is, not migrated. It
    /// does not change what future deploys install. `set_init_meta` does.
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
    /// * [`FactoryError::UpgradeNotOwner`] - If `operator` is not the owner.
    ///
    /// # Notes
    ///
    /// * Authorization for the owner is required, and `operator` must be the
    ///   owner.
    #[only_owner]
    fn upgrade(e: &Env, new_wasm_hash: BytesN<32>, operator: Address) {
        // `operator` is inert under `ownable`, so it is pinned to the owner.
        if ownable::get_owner(e).as_ref() != Some(&operator) {
            panic_with_error!(e, FactoryError::UpgradeNotOwner);
        }
        // The call most likely to arrive after a long quiet stretch, so the
        // TTL is extended here too.
        storage::extend_instance(e);
        upgradeable::upgrade(e, &new_wasm_hash);
    }
}
