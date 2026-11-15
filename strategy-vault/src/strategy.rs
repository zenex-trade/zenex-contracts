//! Strategy integration for the vault: the strategy gate, the
//! uPnL-marked share pricing, and the deposit, redeem, and withdraw cores.

use soroban_fixed_point_math::SorobanFixedPoint;
use soroban_sdk::{contracterror, contractevent, panic_with_error, token, Address, Env};
use stellar_tokens::{
    fungible::Base,
    vault::{emit_deposit, emit_withdraw, Vault},
};

use crate::storage;

/// Extra share decimals on top of the asset's, the same for every vault. A
/// 7-decimal asset gives 13-decimal shares. The `10^DECIMALS_OFFSET` virtual
/// shares make an inflation attack cost about that multiple of the victim's
/// loss.
pub const DECIMALS_OFFSET: u32 = 6;

/// The virtual shares, `10^DECIMALS_OFFSET`.
const VIRTUAL_SHARES: i128 = 1_000_000;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum StrategyVaultError {
    /// An amount argument is negative, or zero where a positive value is
    /// required.
    InvalidAmount = 800,
    /// The pending trader profit exceeds the vault's asset balance.
    PnlExceedsAssets = 801,
    /// The deposit would lift the share supply above `i128::MAX` less the
    /// virtual shares.
    SupplyLimitExceeded = 802,
    /// A redeem's payout exceeds the vault's asset balance.
    PayoutExceedsAssets = 803,
}

/// The strategy withdrew assets from the vault through `strategy_withdraw`.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StrategyWithdraw {
    /// The registered strategy that took the assets.
    #[topic]
    pub strategy: Address,
    /// The assets withdrawn (token-dec).
    pub amount: i128,
}

/// The vault's strategy-facing core. The entry points on
/// [`StrategyVaultContract`](crate::StrategyVaultContract) gate and delegate
/// to it.
pub struct StrategyVault;

impl StrategyVault {
    /// Requires the registered strategy, the market contract, to authorize
    /// the call. The strategy-facing contract entries run it first.
    pub fn require_strategy(e: &Env) {
        storage::get_strategy(e).require_auth();
    }

    /// Returns the backing available to shareholders (token-dec): the asset
    /// balance less `net_pnl`, the strategy's pending trader PnL marked
    /// against the vault.
    ///
    /// A zero result is valid. The exchange rate's `+1` floor prices it, which
    /// covers the empty vault's bootstrap deposit.
    ///
    /// # Arguments
    /// - `net_pnl`: signed pending trader PnL (token-dec). Positive is profit
    ///   the vault still owes. Negative is loss it still collects.
    ///
    /// # Errors
    /// - [`StrategyVaultError::PnlExceedsAssets`] if the pending profit
    ///   exceeds the asset balance.
    pub fn effective_assets(e: &Env, net_pnl: i128) -> i128 {
        let effective = Vault::total_assets(e) - net_pnl;
        if effective < 0 {
            panic_with_error!(e, StrategyVaultError::PnlExceedsAssets);
        }
        effective
    }

    /// Returns the shares `assets` mints, priced against the uPnL-marked
    /// backing and rounded down.
    ///
    /// # Errors
    /// - [`StrategyVaultError::InvalidAmount`] if `assets` is negative.
    /// - Refer to [`StrategyVault::effective_assets`] errors.
    pub fn assets_to_shares(e: &Env, assets: i128, net_pnl: i128) -> i128 {
        if assets < 0 {
            panic_with_error!(e, StrategyVaultError::InvalidAmount);
        }
        let (supply, effective_assets) = Self::exchange_basis(e, net_pnl);
        // shares = assets * (supply + VIRTUAL_SHARES) / (effective_assets + 1)
        assets.fixed_mul_floor(e, &supply, &effective_assets)
    }

    /// Returns the assets `shares` redeems for, priced against the
    /// uPnL-marked backing and rounded down.
    ///
    /// # Errors
    /// - [`StrategyVaultError::InvalidAmount`] if `shares` is negative.
    /// - Refer to [`StrategyVault::effective_assets`] errors.
    pub fn shares_to_assets(e: &Env, shares: i128, net_pnl: i128) -> i128 {
        if shares < 0 {
            panic_with_error!(e, StrategyVaultError::InvalidAmount);
        }
        let (supply, effective_assets) = Self::exchange_basis(e, net_pnl);
        // assets = shares * (effective_assets + 1) / (supply + VIRTUAL_SHARES)
        shares.fixed_mul_floor(e, &effective_assets, &supply)
    }

    /// Returns the exchange-rate basis `(supply, assets)`: the share supply
    /// plus the virtual shares, and the uPnL-marked backing plus one. That
    /// is the ERC-4626 inflation mitigation, and it matches the library's
    /// raw-balance rate.
    fn exchange_basis(e: &Env, net_pnl: i128) -> (i128, i128) {
        let supply = Base::total_supply(e) + VIRTUAL_SHARES;
        (supply, Self::effective_assets(e, net_pnl) + 1)
    }

    /// Pulls `assets` from `from`, mints the uPnL-priced shares to
    /// `receiver`, and returns the shares minted.
    ///
    /// If `from` is the strategy, the vault pulls with a direct transfer.
    /// Otherwise it pulls with `transfer_from`, so `from` must have approved
    /// the strategy on the asset. The market always passes itself as `from`.
    ///
    /// # Security Warning
    /// Assumes the caller ran [`StrategyVault::require_strategy`].
    ///
    /// # Errors
    /// - [`StrategyVaultError::InvalidAmount`] if `assets` is not positive.
    /// - [`StrategyVaultError::SupplyLimitExceeded`] if the shares would lift
    ///   the supply above `i128::MAX` less the virtual shares.
    /// - Refer to [`StrategyVault::assets_to_shares`] errors.
    ///
    /// # Events
    /// - [`Deposit`](stellar_tokens::vault::Deposit): the assets taken and
    ///   the shares minted.
    pub fn deposit(
        e: &Env,
        assets: i128,
        receiver: &Address,
        from: &Address,
        net_pnl: i128,
    ) -> i128 {
        if assets <= 0 {
            panic_with_error!(e, StrategyVaultError::InvalidAmount);
        }
        // The shares are priced before the assets land, so the deposit does
        // not count itself as backing.
        let shares = Self::assets_to_shares(e, assets, net_pnl);
        // Pricing adds the virtual shares to the supply. A supply within
        // them of `i128::MAX` would trap every later deposit and redeem, and
        // only a redeem burns shares.
        if shares > i128::MAX - VIRTUAL_SHARES - Base::total_supply(e) {
            panic_with_error!(e, StrategyVaultError::SupplyLimitExceeded);
        }
        let operator = storage::get_strategy(e);
        Vault::deposit_internal(e, receiver, assets, shares, from, &operator);
        emit_deposit(e, &operator, from, receiver, assets, shares);
        shares
    }

    /// Burns `shares` from `owner`, pays the uPnL-priced assets to
    /// `receiver`, and returns the assets paid.
    ///
    /// If `owner` is not the strategy, the vault spends `owner`'s share
    /// allowance for the strategy first. The market always passes itself as
    /// `owner`, after it moved the shares to itself.
    ///
    /// # Security Warning
    /// Assumes the caller ran [`StrategyVault::require_strategy`].
    ///
    /// # Errors
    /// - [`StrategyVaultError::InvalidAmount`] if `shares` is not positive.
    /// - [`StrategyVaultError::PayoutExceedsAssets`] if the payout exceeds
    ///   the asset balance.
    /// - Refer to [`StrategyVault::shares_to_assets`] errors.
    ///
    /// # Events
    /// - [`Withdraw`](stellar_tokens::vault::Withdraw): the shares burned and
    ///   the assets paid.
    pub fn redeem(
        e: &Env,
        shares: i128,
        receiver: &Address,
        owner: &Address,
        net_pnl: i128,
    ) -> i128 {
        if shares <= 0 {
            panic_with_error!(e, StrategyVaultError::InvalidAmount);
        }
        let assets = Self::shares_to_assets(e, shares, net_pnl);
        // A negative `net_pnl` counts collections the vault does not hold
        // yet, so the payout can exceed the balance. It traps here with a
        // typed error instead of in the token transfer.
        if assets > Vault::total_assets(e) {
            panic_with_error!(e, StrategyVaultError::PayoutExceedsAssets);
        }
        let operator = storage::get_strategy(e);
        Vault::withdraw_internal(e, receiver, owner, assets, shares, &operator);
        emit_withdraw(e, &operator, receiver, owner, assets, shares);
        assets
    }

    /// Transfers `amount` of the underlying asset to the registered
    /// strategy. This lowers `total_assets` and therefore the share price.
    ///
    /// # Security Warning
    /// Assumes the caller ran [`StrategyVault::require_strategy`].
    ///
    /// # Errors
    /// - [`StrategyVaultError::InvalidAmount`] if `amount` is not positive.
    ///
    /// # Events
    /// - [`StrategyWithdraw`]: the assets the strategy took.
    pub fn withdraw(e: &Env, amount: i128) {
        if amount <= 0 {
            panic_with_error!(e, StrategyVaultError::InvalidAmount);
        }

        let strategy = storage::get_strategy(e);
        let asset = Vault::query_asset(e);
        let token_client = token::Client::new(e, &asset);
        token_client.transfer(&e.current_contract_address(), &strategy, &amount);

        StrategyWithdraw { strategy, amount }.publish(e);
    }
}
