//! Contract entry points.

use soroban_sdk::{contract, contractimpl, Address, Env, MuxedAddress, String};
use stellar_tokens::{
    fungible::{Base, FungibleToken},
    vault::Vault,
};

use crate::{
    storage,
    strategy::{StrategyVault, DECIMALS_OFFSET},
};

/// Collateral vault for a single market. Shares are an OpenZeppelin fungible
/// token, and LPs route through the market's vault orders.
#[contract]
pub struct StrategyVaultContract;

#[contractimpl]
impl StrategyVaultContract {
    /// Initializes the vault with its underlying asset, share metadata, and
    /// strategy address.
    ///
    /// # Arguments
    ///
    /// * `e` - Access to the Soroban environment.
    /// * `name` - The share token's name.
    /// * `symbol` - The share token's symbol.
    /// * `asset` - The underlying collateral token.
    /// * `strategy` - The market contract, the only address that may deposit,
    ///   redeem, or withdraw. Immutable.
    ///
    /// # Notes
    ///
    /// * The share decimals are the asset's plus the fixed
    ///   [`DECIMALS_OFFSET`].
    pub fn __constructor(e: Env, name: String, symbol: String, asset: Address, strategy: Address) {
        Vault::set_asset(&e, asset);
        // Stored too, so the library's own share math reads the same offset.
        Vault::set_decimals_offset(&e, DECIMALS_OFFSET);
        Base::set_metadata(&e, Vault::decimals(&e), name, symbol);

        storage::set_strategy(&e, &strategy);
        storage::extend_instance(&e);
    }

    /// Deposits `assets` from `from`, mints their uPnL-priced shares to
    /// `receiver`, and returns the shares minted (share decimals). Shares
    /// round down, in the vault's favor.
    ///
    /// # Arguments
    ///
    /// * `e` - Access to the Soroban environment.
    /// * `assets` - The assets to deposit (token-dec).
    /// * `receiver` - The share recipient.
    /// * `from` - The asset provider. Unless it is the strategy, it must have
    ///   approved the strategy on the asset.
    /// * `net_pnl` - Signed pending trader PnL marked against the vault
    ///   (token-dec). Positive is profit the vault owes.
    ///
    /// # Errors
    ///
    /// * `StrategyVaultError::InvalidAmount` - If `assets` is not positive.
    /// * `StrategyVaultError::PnlExceedsAssets` - If the pending profit
    ///   exceeds the asset balance.
    /// * `StrategyVaultError::SupplyLimitExceeded` - If the shares would lift
    ///   the supply above `i128::MAX` less the virtual shares.
    ///
    /// # Events
    ///
    /// * topics - `["deposit", operator: Address, from: Address, receiver:
    ///   Address]`
    /// * data - `[assets: i128, shares: i128]`
    ///
    /// # Notes
    ///
    /// * Authorization for the strategy is required.
    pub fn strategy_deposit(
        e: Env,
        assets: i128,
        receiver: Address,
        from: Address,
        net_pnl: i128,
    ) -> i128 {
        StrategyVault::require_strategy(&e);
        let shares = StrategyVault::deposit(&e, assets, &receiver, &from, net_pnl);
        storage::extend_instance(&e);
        shares
    }

    /// Redeems `shares` from `owner`, pays their uPnL-priced assets to
    /// `receiver`, and returns the assets paid (token-dec), rounded down.
    ///
    /// # Arguments
    ///
    /// * `e` - Access to the Soroban environment.
    /// * `shares` - The shares to burn (share decimals).
    /// * `receiver` - The asset recipient.
    /// * `owner` - The share holder.
    /// * `net_pnl` - Signed pending trader PnL (token-dec).
    ///
    /// # Errors
    ///
    /// * `StrategyVaultError::InvalidAmount` - If `shares` is not positive.
    /// * `StrategyVaultError::PnlExceedsAssets` - If the pending profit
    ///   exceeds the asset balance.
    /// * `StrategyVaultError::PayoutExceedsAssets` - If the payout exceeds
    ///   the asset balance.
    /// * `FungibleTokenError::InsufficientAllowance` - If `owner` is not the
    ///   strategy and approved it for fewer than `shares`.
    /// * `FungibleTokenError::InsufficientBalance` - If `owner` holds fewer
    ///   than `shares`.
    ///
    /// # Events
    ///
    /// * topics - `["withdraw", operator: Address, receiver: Address, owner:
    ///   Address]`
    /// * data - `[assets: i128, shares: i128]`
    ///
    /// # Notes
    ///
    /// * Authorization for the strategy is required.
    pub fn strategy_redeem(
        e: Env,
        shares: i128,
        receiver: Address,
        owner: Address,
        net_pnl: i128,
    ) -> i128 {
        StrategyVault::require_strategy(&e);
        let assets = StrategyVault::redeem(&e, shares, &receiver, &owner, net_pnl);
        storage::extend_instance(&e);
        assets
    }

    /// Withdraws `amount` of the asset to the strategy, which uses it to pay
    /// winning positions. This lowers `total_assets` and therefore the share
    /// price.
    ///
    /// # Arguments
    ///
    /// * `e` - Access to the Soroban environment.
    /// * `amount` - The assets to withdraw to the strategy (token-dec).
    ///
    /// # Errors
    ///
    /// * `StrategyVaultError::InvalidAmount` - If `amount` is not positive.
    ///
    /// # Events
    ///
    /// * topics - `["strategy_withdraw", strategy: Address]`
    /// * data - `[amount: i128]`
    ///
    /// # Notes
    ///
    /// * Authorization for the strategy is required.
    pub fn strategy_withdraw(e: Env, amount: i128) {
        StrategyVault::require_strategy(&e);
        StrategyVault::withdraw(&e, amount);
        storage::extend_instance(&e);
    }

    /// Returns the shares (share decimals) a deposit of `assets` would mint
    /// at the uPnL-marked share price, rounded down.
    ///
    /// # Arguments
    ///
    /// * `e` - Access to the Soroban environment.
    /// * `assets` - The assets to quote (token-dec).
    /// * `net_pnl` - Signed pending trader PnL marked against the vault
    ///   (token-dec).
    ///
    /// # Errors
    ///
    /// * `StrategyVaultError::InvalidAmount` - If `assets` is negative.
    /// * `StrategyVaultError::PnlExceedsAssets` - If the pending profit
    ///   exceeds the asset balance.
    pub fn preview_deposit(e: Env, assets: i128, net_pnl: i128) -> i128 {
        StrategyVault::assets_to_shares(&e, assets, net_pnl)
    }

    /// Returns the assets (token-dec) a redemption of `shares` would pay at
    /// the uPnL-marked share price, rounded down.
    ///
    /// # Arguments
    ///
    /// * `e` - Access to the Soroban environment.
    /// * `shares` - The shares to quote (share decimals).
    /// * `net_pnl` - Signed pending trader PnL marked against the vault
    ///   (token-dec).
    ///
    /// # Errors
    ///
    /// * `StrategyVaultError::InvalidAmount` - If `shares` is negative.
    /// * `StrategyVaultError::PnlExceedsAssets` - If the pending profit
    ///   exceeds the asset balance.
    pub fn preview_redeem(e: Env, shares: i128, net_pnl: i128) -> i128 {
        StrategyVault::shares_to_assets(&e, shares, net_pnl)
    }

    /// Returns the underlying asset the vault manages.
    ///
    /// # Arguments
    ///
    /// * `e` - Access to the Soroban environment.
    pub fn query_asset(e: Env) -> Address {
        Vault::query_asset(&e)
    }

    /// Returns the vault's raw balance of the underlying asset (token-dec),
    /// before any pending-PnL mark.
    ///
    /// # Arguments
    ///
    /// * `e` - Access to the Soroban environment.
    pub fn total_assets(e: Env) -> i128 {
        Vault::total_assets(&e)
    }

    /// Returns the registered strategy, the market contract.
    ///
    /// # Arguments
    ///
    /// * `e` - Access to the Soroban environment.
    pub fn get_strategy(e: Env) -> Address {
        storage::get_strategy(&e)
    }
}

/// Default share transfers. Redeem locks are enforced on the market, not
/// here. The upstream defaults extend the balance and allowance entries they
/// touch, and this contract's own entry points carry the instance lease.
#[contractimpl(contracttrait)]
impl FungibleToken for StrategyVaultContract {
    type ContractType = Vault;
}
