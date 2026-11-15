//! Strategy-vault client. The market reads the vault balance for
//! utilization, routes vault deposit and redeem orders at the uPnL-marked
//! share price, and withdraws tokens to pay winning positions.

use soroban_sdk::{contractclient, Address, Env};

/// The strategy-vault contract interface.
#[allow(dead_code)] // consumed by the generated `VaultClient`
#[contractclient(name = "VaultClient")]
pub trait VaultInterface {
    /// Returns the raw underlying asset balance the vault holds (token-dec),
    /// before any pending-PnL mark.
    fn total_assets(e: Env) -> i128;

    /// Pulls `assets` (token-dec) from `from` and mints shares to `receiver`,
    /// priced against the backing marked with `net_pnl`, the signed pending
    /// trader PnL (token-dec). Returns the shares minted (share decimals).
    /// Only the strategy, this contract, may call it.
    fn strategy_deposit(
        e: Env,
        assets: i128,
        receiver: Address,
        from: Address,
        net_pnl: i128,
    ) -> i128;

    /// Burns `shares` (share decimals) from `owner` and pays assets to
    /// `receiver`, priced against the backing marked with `net_pnl`, the
    /// signed pending trader PnL (token-dec). Returns the assets paid
    /// (token-dec). Only the strategy, this contract, may call it.
    fn strategy_redeem(
        e: Env,
        shares: i128,
        receiver: Address,
        owner: Address,
        net_pnl: i128,
    ) -> i128;

    /// Returns the shares (share decimals) a deposit of `assets` (token-dec)
    /// would mint at the `net_pnl`-marked exchange rate.
    fn preview_deposit(e: Env, assets: i128, net_pnl: i128) -> i128;

    /// Returns the assets (token-dec) a redemption of `shares` (share
    /// decimals) would pay at the `net_pnl`-marked exchange rate.
    fn preview_redeem(e: Env, shares: i128, net_pnl: i128) -> i128;

    /// Returns the vault share balance of `account` (share decimals).
    fn balance(e: Env, account: Address) -> i128;

    /// Transfers `amount` (share decimals) of vault shares from `from` to
    /// `to`. `from` must authorize the call.
    fn transfer(e: Env, from: Address, to: Address, amount: i128);

    /// Moves `amount` (token-dec) from the vault to the registered strategy,
    /// this contract. It lowers `total_assets`. Only the strategy may call
    /// it.
    fn strategy_withdraw(e: Env, amount: i128);
}
