//! Contract entry points.
//!
//! [`Market`] is the interface and carries the docs of every entry. Each
//! mutating entry extends the instance TTL and delegates to
//! [`crate::engine`]. The getters read without extending it.

#![allow(clippy::too_many_arguments)]

use crate::engine;
use crate::engine::adl::AdlState;
use crate::engine::config::Config;
use crate::engine::constants::TOKEN_DECIMALS;
use crate::engine::market_data::MarketData;
use crate::engine::order::Order;
use crate::engine::position::Position;
use crate::engine::status::Status;
use crate::engine::vault_order::VaultOrder;
use crate::errors::MarketError;
use crate::storage;
use soroban_sdk::{
    contract, contractclient, contractimpl, panic_with_error, token, Address, Bytes, BytesN, Env,
};
use stellar_access::ownable::{self as ownable, Ownable};
use stellar_contract_utils::upgradeable::{self as upgradeable, Upgradeable};
use stellar_macros::only_owner;

/// One perpetual futures market.
#[contract]
pub struct MarketContract;

/// The market contract interface.
///
/// A priced entry runs at its effective price. [`Market::execute_order`]
/// uses the verified report. The other priced entries use the newer of the
/// verified report and the market's cached price. Once a terminal price is
/// stored, every entry uses it and verifies no report.
#[contractclient(name = "MarketClient")]
pub trait Market {
    // --- admin (auth = owner) ---

    /// Replaces the global market configuration.
    ///
    /// # Authorization
    /// - Owner only (`#[only_owner]`).
    ///
    /// # Arguments
    /// - `config`: the new global market configuration.
    ///
    /// # Errors
    /// - [`MarketError::InvalidConfig`] if a bound or range check fails.
    /// - [`MarketError::NegativeValueNotAllowed`] if a fee, margin,
    ///   borrowing, funding, ADL, or PnL field, `min_order_margin`,
    ///   `min_deposit`, or `exec_fee` is negative. Any other negative field
    ///   fails its own bound as [`MarketError::InvalidConfig`].
    /// - [`MarketError::MarketNotAccrued`] if a borrowing or funding rate
    ///   parameter changed without a same-ledger accrual. Waived while
    ///   [`Status::Frozen`].
    ///
    /// # Events
    /// - [`ConfigUpdate`](crate::events::ConfigUpdate): the new
    ///   configuration.
    fn set_config(e: Env, config: Config);

    /// Sets the contract operational status.
    ///
    /// Entering [`Status::Retired`] sweeps the credit-pool surplus to the
    /// vault.
    ///
    /// # Authorization
    /// - Owner only (`#[only_owner]`).
    ///
    /// # Arguments
    /// - `status`: the target operational status, as its [`Status`] `u32`
    ///   discriminant.
    ///
    /// # Errors
    /// - [`MarketError::InvalidStatus`] on an unknown status value or an
    ///   invalid transition.
    /// - [`MarketError::MarketNotCleared`] on [`Status::Retired`] while any
    ///   position remains open.
    ///
    /// # Events
    /// - [`StatusUpdate`](crate::events::StatusUpdate): the new status.
    fn set_status(e: Env, status: u32);

    /// Sets or refreshes the flat settlement price of a delisted market.
    ///
    /// From the first set on, every fill and accrual prices flat at the
    /// stored value. Submitted price bytes are ignored.
    ///
    /// # Authorization
    /// - Owner only (`#[only_owner]`).
    ///
    /// # Arguments
    /// - `price`: the flat settlement price, in the feed's native precision.
    ///
    /// # Errors
    /// - [`MarketError::InvalidStatus`] unless the status is
    ///   [`Status::Delisted`] and the grace window has expired.
    /// - [`MarketError::InvalidPrice`] if `price` is not strictly positive.
    ///
    /// # Events
    /// - [`TerminalPriceUpdate`](crate::events::TerminalPriceUpdate): the
    ///   new price.
    fn set_terminal_price(e: Env, price: i128);

    // --- trader (auth = user, price-free) ---

    /// Creates an [`Order`] for the keeper to fill and returns its id.
    ///
    /// An increase escrows `margin + exec_fee`. A decrease escrows
    /// `exec_fee`. The `exec_fee` pays the keeper at fill, and the escrow
    /// refunds on cancel.
    ///
    /// # Authorization
    /// - `user.require_auth()`.
    ///
    /// # Arguments
    /// - `user`: the trader who owns the order.
    /// - `is_long`: the position side the order targets.
    /// - `kind`: the [`OrderKind`](crate::engine::order::OrderKind) discriminant.
    /// - `notional`: the size change magnitude, non-negative (token-dec). A
    ///   decrease at or above the position size, or one that leaves less
    ///   than `min_position_notional` behind, clamps to a full close at
    ///   fill. `i128::MAX` signals a full close.
    /// - `margin`: the margin change magnitude, non-negative (token-dec).
    /// - `trigger_price`: the crossing level for a limit or stop kind, in
    ///   the feed's native precision. A market kind does not read it, but a
    ///   negative value still traps.
    /// - `price_bound`: the fill slippage limit, in the feed's native
    ///   precision. `0` means unbounded.
    /// - `expiration`: the last ledger sequence the order is fillable at.
    ///
    /// # Errors
    /// - [`MarketError::MarketFrozen`] if the market status is
    ///   [`Status::Frozen`] or [`Status::Retired`].
    /// - [`MarketError::UnknownKind`] if `kind` is not a known discriminant.
    /// - [`MarketError::NegativeValueNotAllowed`] if a magnitude,
    ///   `trigger_price`, or `price_bound` is negative.
    /// - [`MarketError::InvalidOrder`] if any of these holds:
    ///   - the shape is a no-op;
    ///   - a moved value is below a dust floor;
    ///   - a limit or stop kind carries a zero `trigger_price`;
    ///   - an increase's `margin + exec_fee` overflows.
    /// - [`MarketError::NotionalAboveMaximum`] if an increase's `notional`
    ///   exceeds `max_position_notional`.
    /// - [`MarketError::OrderExpired`] if `expiration` is already behind the
    ///   current ledger.
    /// - [`MarketError::TooManyOrders`] if a decrease targets a side that
    ///   already holds the maximum pending decrease orders.
    ///
    /// # Events
    /// - [`CreateOrder`](crate::events::CreateOrder): the stored order row.
    fn create_order(
        e: Env,
        user: Address,
        is_long: bool,
        kind: u32,
        notional: i128,
        margin: i128,
        trigger_price: i128,
        price_bound: i128,
        expiration: u32,
    ) -> u32;

    /// Cancels the pending [`Order`] `(user, id)` and returns the refunded
    /// escrow (token-dec).
    ///
    /// The refund is an increase's `margin + exec_fee`, or a decrease's
    /// `exec_fee`.
    ///
    /// # Authorization
    /// - `user.require_auth()`.
    ///
    /// # Arguments
    /// - `user`: the trader who owns the order.
    /// - `id`: the order id.
    ///
    /// # Errors
    /// - [`MarketError::MarketFrozen`] if the market status is
    ///   [`Status::Frozen`].
    /// - [`MarketError::OrderNotFound`] if no order `(user, id)` exists.
    ///
    /// # Events
    /// - [`CancelOrder`](crate::events::CancelOrder): the cancelled
    ///   `(user, id)` and the refund.
    fn cancel_order(e: Env, user: Address, id: u32) -> i128;

    /// Creates a [`VaultOrder`] for the keeper to fill and returns its id.
    ///
    /// The deposit assets or redeem shares are escrowed in the market
    /// contract at creation. A redeem on a [`Status::Retired`] market skips
    /// the order and pays the assets at once, without `min_out`. Its
    /// returned id is `0`.
    ///
    /// # Authorization
    /// - `user.require_auth()`.
    ///
    /// # Arguments
    /// - `user`: the depositor or redeemer.
    /// - `kind`: the
    ///   [`VaultOrderKind`](crate::engine::vault_order::VaultOrderKind)
    ///   discriminant.
    /// - `amount`: the assets to deposit (token-dec), or the shares to redeem
    ///   (share decimals). Shares carry 13 decimals: the token's 7 plus
    ///   the vault's fixed offset of 6.
    /// - `min_out`: the slippage bound, the minimum received at fill net of
    ///   the vault fee. Shares for a deposit (share decimals), or assets for
    ///   a redeem (token-dec). `0` is unset. A fill quoted below it rejects
    ///   the order rather than leaving it to wait.
    ///
    /// # Errors
    /// - [`MarketError::NegativeValueNotAllowed`] if `amount` or `min_out` is
    ///   negative.
    /// - [`MarketError::UnknownKind`] if `kind` is not a known discriminant.
    /// - [`MarketError::MarketFrozen`] if the market status is
    ///   [`Status::Frozen`].
    /// - [`MarketError::InvalidStatus`] if a deposit is created on a
    ///   [`Status::Retired`] market.
    /// - [`MarketError::InvalidOrder`] if `amount` is zero, a deposit falls
    ///   under `min_deposit`, or its `amount + exec_fee` escrow sum
    ///   overflows.
    ///
    /// # Events
    /// - [`CreateVaultOrder`](crate::events::CreateVaultOrder): the stored
    ///   order row.
    /// - [`RedeemFill`](crate::events::RedeemFill) with id `0`: the receipt
    ///   of a [`Status::Retired`] market's direct redeem.
    fn create_vault_order(e: Env, user: Address, kind: u32, amount: i128, min_out: i128) -> u32;

    /// Cancels the pending [`VaultOrder`] `(user, id)` and returns the
    /// refunded principal.
    ///
    /// The principal is assets for a deposit (token-dec), or shares for a
    /// redeem (share decimals). The escrowed `exec_fee` refunds as well and
    /// is not part of the return.
    ///
    /// # Authorization
    /// - `user.require_auth()`.
    ///
    /// # Arguments
    /// - `user`: the trader who owns the vault order.
    /// - `id`: the vault order id.
    ///
    /// # Errors
    /// - [`MarketError::MarketFrozen`] if the market status is
    ///   [`Status::Frozen`].
    /// - [`MarketError::VaultOrderNotFound`] if no vault order `(user, id)`
    ///   exists.
    ///
    /// # Events
    /// - [`CancelVaultOrder`](crate::events::CancelVaultOrder): the
    ///   cancelled `(user, id)`.
    fn cancel_vault_order(e: Env, user: Address, id: u32) -> i128;

    /// Pays `user`'s claimable credit balance from the pool and returns the
    /// amount paid (token-dec).
    ///
    /// The balance holds earned funding, plus any payout whose direct
    /// transfer failed. The payout is the claimable balance capped at the
    /// pool's holdings. An unpaid remainder stays claimable.
    ///
    /// # Authorization
    /// - `user.require_auth()`.
    ///
    /// # Arguments
    /// - `user`: the trader who claims the balance.
    ///
    /// # Errors
    /// - [`MarketError::MarketFrozen`] if the market status is
    ///   [`Status::Frozen`].
    /// - [`MarketError::NothingToClaim`] if `user` has no claimable balance,
    ///   or the pool holds nothing to pay it with.
    ///
    /// # Events
    /// - [`ClaimCredit`](crate::events::ClaimCredit): the paid amount.
    fn claim_credit(e: Env, user: Address) -> i128;

    // --- keeper (permissionless, price-bearing) ---

    /// Fills the pending [`Order`] `(user, id)` at the effective price and
    /// returns the keeper's payout (token-dec).
    ///
    /// # Authorization
    /// - Permissionless. `keeper` is named by the caller and
    ///   unauthenticated.
    ///
    /// # Arguments
    /// - `keeper`: the reward recipient.
    /// - `user`: the trader who owns the order.
    /// - `id`: the order id.
    /// - `price`: the keeper's serialized price update, verified for the
    ///   contract's feed.
    ///
    /// # Errors
    /// - [`MarketError::MarketFrozen`] if the market status is
    ///   [`Status::Frozen`] or [`Status::Retired`].
    /// - [`MarketError::OrderNotFound`] if no order `(user, id)` exists.
    /// - [`MarketError::IncreaseHalted`] if a size-growing increase runs
    ///   while the status does not accept opens or the target side has ADL
    ///   enabled.
    /// - [`MarketError::OrderExpired`] if the order expired before the fill.
    /// - [`MarketError::StalePrice`] if the effective price predates the
    ///   order, except for a market order filled in its creation ledger, or
    ///   predates the price the position was last marked against.
    /// - [`MarketError::TriggerNotMet`] if the fill price does not cross the
    ///   order's trigger.
    /// - [`MarketError::PriceBoundExceeded`] if the fill price is worse than
    ///   `price_bound`.
    /// - [`MarketError::PositionNotFound`] if a decrease targets an absent
    ///   position.
    /// - [`MarketError::PositionLiquidatable`] if a decrease targets a
    ///   position whose settled equity is below the maintenance margin, or
    ///   any fill leaves the position's settled equity below it.
    /// - [`MarketError::NotionalLocked`] if the close exceeds the position's
    ///   unlocked notional.
    /// - [`MarketError::NotionalBelowMinimum`] if the resulting position
    ///   falls under the size floor.
    /// - [`MarketError::NotionalAboveMaximum`] if the resulting position
    ///   exceeds the size ceiling.
    /// - [`MarketError::InsufficientMargin`] if margin falls below the
    ///   initial-margin requirement.
    /// - [`MarketError::OpenInterestExceeded`] if a size-growing increase
    ///   would take the side's open interest above `max_open_interest`.
    /// - [`MarketError::SizeRoundsToZero`] if an increase's notional buys no
    ///   base size at the entry price.
    /// - [`MarketError::UtilizationExceeded`] if a size-growing increase
    ///   would take the side's reserved value above the utilization cap.
    /// - [`MarketError::VaultInsolvent`] if a decrease settlement's vault
    ///   draw exceeds the vault balance.
    ///
    /// # Events
    /// - [`OpenFill`](crate::events::OpenFill) when the fill opens the
    ///   position, [`IncreaseFill`](crate::events::IncreaseFill) when it
    ///   grows an already-open one,
    ///   [`DecreaseFill`](crate::events::DecreaseFill) on a partial close,
    ///   or [`CloseFill`](crate::events::CloseFill) when the whole position
    ///   closes: the itemized fill receipt.
    /// - [`CancelOrder`](crate::events::CancelOrder) per swept decrease
    ///   order when the position closes.
    fn execute_order(e: Env, keeper: Address, user: Address, id: u32, price: Bytes) -> i128;

    /// Force-closes the [`Position`] `(user, is_long)` at the effective price
    /// and returns the keeper's payout (token-dec).
    ///
    /// # Authorization
    /// - Permissionless. `keeper` is named by the caller and
    ///   unauthenticated.
    ///
    /// # Arguments
    /// - `keeper`: the reward recipient.
    /// - `user`: the trader who owns the position.
    /// - `is_long`: the position side.
    /// - `price`: the keeper's serialized price update, verified for the
    ///   contract's feed.
    ///
    /// # Errors
    /// - [`MarketError::MarketFrozen`] if the market status is
    ///   [`Status::Frozen`] or [`Status::Retired`].
    /// - [`MarketError::PositionNotFound`] if no position exists for
    ///   `(user, is_long)`.
    /// - [`MarketError::StalePrice`] if the effective price is older than
    ///   the price the position was last marked against.
    /// - [`MarketError::NotLiquidatable`] if equity still covers the
    ///   maintenance margin. Waived once the market is [`Status::Delisted`]
    ///   and [`DELIST_DEADLINE`](crate::engine::constants::DELIST_DEADLINE)
    ///   has passed since `delisted_at`.
    /// - [`MarketError::VaultInsolvent`] if the settlement's vault draw
    ///   exceeds the vault balance.
    ///
    /// # Events
    /// - [`Liquidation`](crate::events::Liquidation): the itemized receipt.
    /// - [`CancelOrder`](crate::events::CancelOrder) per swept decrease
    ///   order.
    fn execute_liquidation(
        e: Env,
        keeper: Address,
        user: Address,
        is_long: bool,
        price: Bytes,
    ) -> i128;

    /// Recomputes both sides' pending PnL at the effective price, sets or
    /// clears the [`AdlState`] flags, and returns the result.
    ///
    /// A flagged side rejects increases that add notional and is eligible
    /// for [`Market::execute_adl`]. A margin-only increase still fills.
    ///
    /// # Authorization
    /// - Permissionless.
    ///
    /// # Arguments
    /// - `price`: the keeper's serialized price update, verified for the
    ///   contract's feed.
    ///
    /// # Errors
    /// - [`MarketError::MarketFrozen`] if the market status is
    ///   [`Status::Frozen`] or [`Status::Retired`].
    ///
    /// # Events
    /// - [`AdlUpdate`](crate::events::AdlUpdate): the per-side flags.
    fn update_adl_state(e: Env, price: Bytes) -> AdlState;

    /// Deleverages the winning [`Position`] `(user, is_long)` by `amount` at
    /// the effective price and returns the keeper's payout (token-dec).
    ///
    /// The close reduces the side's pending PnL toward `adl_clear_target` of
    /// half the vault balance. Each side measures against its own half. The
    /// payout is the `keeper_rate` cut of the trade fee.
    ///
    /// # Authorization
    /// - Permissionless. `keeper` is named by the caller and
    ///   unauthenticated.
    ///
    /// # Arguments
    /// - `keeper`: the reward recipient.
    /// - `user`: the trader who owns the position.
    /// - `is_long`: the position side.
    /// - `amount`: the notional to close (token-dec). A request at or above
    ///   the position's notional closes it in full. So does one whose
    ///   remainder would sit below `min_position_notional`.
    /// - `price`: the keeper's serialized price update, verified for the
    ///   contract's feed.
    ///
    /// # Errors
    /// - [`MarketError::MarketFrozen`] if the market status is
    ///   [`Status::Frozen`] or [`Status::Retired`].
    /// - [`MarketError::InvalidOrder`] if `amount` is below
    ///   `min_order_notional`.
    /// - [`MarketError::AdlNotTriggered`] if the side is not flagged for
    ///   deleveraging, or its pending PnL is already at or below
    ///   `adl_clear_target` of half the vault balance.
    /// - [`MarketError::StalePrice`] if the effective price is older than
    ///   the price the position was last marked against.
    /// - [`MarketError::PositionNotFound`] if no position exists for
    ///   `(user, is_long)`.
    /// - [`MarketError::PositionLiquidatable`] if the position's settled
    ///   equity is below the maintenance margin, before the close or for the
    ///   remainder of a partial close. A liquidatable position must be
    ///   liquidated, not deleveraged.
    /// - [`MarketError::NotionalLocked`] if the close touches notional still
    ///   under the decrease lock.
    /// - [`MarketError::NotionalAboveMaximum`] if the remainder of a partial
    ///   close exceeds `max_position_notional`.
    /// - [`MarketError::VaultInsolvent`] if the settlement's vault draw
    ///   exceeds the vault balance.
    /// - [`MarketError::AdlNotEligible`] if the close does not reduce the
    ///   side's pending PnL.
    /// - [`MarketError::AdlOvershoot`] if the close lands the side under the
    ///   re-measured clear allowance.
    ///
    /// # Events
    /// - [`DecreaseFill`](crate::events::DecreaseFill) on a partial close,
    ///   or [`CloseFill`](crate::events::CloseFill) when the whole position
    ///   closes: the itemized receipt, with id `0`.
    /// - [`CancelOrder`](crate::events::CancelOrder) per swept decrease
    ///   order when the position closes.
    fn execute_adl(
        e: Env,
        keeper: Address,
        user: Address,
        is_long: bool,
        amount: i128,
        price: Bytes,
    ) -> i128;

    /// Fills the [`VaultOrder`] `(user, id)` at the effective price and
    /// returns the keeper's payout (token-dec).
    ///
    /// The whole order fills at once and is removed. The fill deducts the
    /// `deposit_fee` or `redeem_fee` cut of the moved assets, split between
    /// the keeper, the treasury, and the vault. The payout is the
    /// `keeper_rate` cut of that fee plus the order's escrowed `exec_fee`.
    ///
    /// # Authorization
    /// - Permissionless. `keeper` is named by the caller and
    ///   unauthenticated.
    ///
    /// # Arguments
    /// - `keeper`: the reward recipient.
    /// - `user`: the trader who owns the vault order.
    /// - `id`: the vault order id.
    /// - `price`: the keeper's serialized price update, verified for the
    ///   contract's feed.
    ///
    /// # Errors
    /// - [`MarketError::MarketFrozen`] if the market status is
    ///   [`Status::Frozen`] or [`Status::Retired`].
    /// - [`MarketError::VaultOrderNotFound`] if no vault order `(user, id)`
    ///   exists.
    /// - [`MarketError::StalePrice`] if the effective price predates the
    ///   order's `created_at`, or the fill runs in the order's creation
    ///   ledger.
    /// - [`MarketError::VaultOrderLocked`] if a redeem's `redeem_lock`
    ///   cooldown from `created_at` has not elapsed.
    /// - [`MarketError::VaultBalanceExceeded`] if a deposit would push the
    ///   vault above `max_vault_balance`.
    /// - [`MarketError::UtilizationExceeded`] if a redeem would leave the
    ///   vault under-reserved.
    /// - [`MarketError::PendingPnlExceeded`] if a redeem would leave a side's
    ///   pending PnL above `max_pnl_withdraw` of half the remaining balance,
    ///   or a deposit runs while it is above `max_pnl_trader` of half the
    ///   balance.
    ///
    /// # Events
    /// - [`DepositFill`](crate::events::DepositFill) or
    ///   [`RedeemFill`](crate::events::RedeemFill): the itemized fill
    ///   receipt.
    /// - [`RejectVaultOrder`](crate::events::RejectVaultOrder): the fill
    ///   would pay less than the order's `min_out`. The principal returns to
    ///   the user, or parks as claimable credit when a deposit refund fails.
    ///   The `exec_fee` pays the keeper, and the order is removed.
    ///   Capacity checks trap instead and leave the order.
    fn execute_vault_order(e: Env, keeper: Address, user: Address, id: u32, price: Bytes) -> i128;

    // --- maintenance (permissionless) ---

    /// Advances the borrowing and funding indices to the current timestamp
    /// at the effective price and returns the accrued [`MarketData`].
    ///
    /// # Authorization
    /// - Permissionless.
    ///
    /// # Arguments
    /// - `price`: the keeper's serialized price update, verified for the
    ///   contract's feed.
    ///
    /// # Errors
    /// - [`MarketError::MarketFrozen`] if the market status is
    ///   [`Status::Frozen`] or [`Status::Retired`].
    ///
    /// # Events
    /// - [`AccrualUpdate`](crate::events::AccrualUpdate): the accrual
    ///   marker.
    fn accrue(e: Env, price: Bytes) -> MarketData;

    // --- views ---

    /// Returns the current global [`Config`].
    fn get_config(e: Env) -> Config;

    /// Returns the stored [`MarketData`], as of its last accrual.
    fn get_market_data(e: Env) -> MarketData;

    /// Returns the netted [`Position`] for `(user, is_long)`, or a zeroed
    /// one if no row is stored. A miss stores nothing.
    ///
    /// # Arguments
    /// - `user`: the trader who owns the position.
    /// - `is_long`: the position side.
    fn get_position(e: Env, user: Address, is_long: bool) -> Position;

    /// Returns the pending keeper [`Order`] `(user, id)`.
    ///
    /// # Arguments
    /// - `user`: the trader who owns the order.
    /// - `id`: the order id.
    ///
    /// # Errors
    /// - [`MarketError::OrderNotFound`] if no such order exists.
    fn get_order(e: Env, user: Address, id: u32) -> Order;

    /// Returns the operational status as its [`Status`] `u32` discriminant.
    fn get_status(e: Env) -> u32;

    /// Returns the pending [`VaultOrder`] `(user, id)`.
    ///
    /// # Arguments
    /// - `user`: the trader who owns the vault order.
    /// - `id`: the vault order id.
    ///
    /// # Errors
    /// - [`MarketError::VaultOrderNotFound`] if no such vault order exists.
    fn get_vault_order(e: Env, user: Address, id: u32) -> VaultOrder;

    /// Returns `user`'s next unallocated order id.
    ///
    /// Ids `1..counter` are the already-allocated trade and vault order ids.
    /// `1` means none yet.
    ///
    /// # Arguments
    /// - `user`: the trader who owns the orders.
    fn get_order_counter(e: Env, user: Address) -> u32;

    /// Returns the [`AdlState`], zeroed until the first recompute.
    fn get_adl(e: Env) -> AdlState;

    /// Returns the funding owed to `user`, plus any payout whose direct
    /// transfer failed (token-dec), or `0` if none.
    ///
    /// # Arguments
    /// - `user`: the trader to read.
    fn get_claimable_credit(e: Env, user: Address) -> i128;

    /// Returns the settlement token, the asset every token-dec amount is
    /// denominated in.
    fn get_token(e: Env) -> Address;

    /// Returns the strategy-vault that backs this market.
    fn get_vault(e: Env) -> Address;

    /// Returns the treasury, the protocol fee sink.
    fn get_treasury(e: Env) -> Address;

    /// Returns the oracle that verifies submitted price bytes.
    fn get_oracle(e: Env) -> Address;

    /// Returns the wind-down anchors `(terminal_price, delisted_at)`, or
    /// `None` if the market was never delisted or the owner reverted its
    /// last delist.
    ///
    /// `terminal_price` is in the feed's native precision and is `0` until a
    /// flat settlement price is set. `delisted_at` is a Unix timestamp
    /// (seconds).
    fn get_retirement(e: Env) -> Option<(i128, u64)>;

    /// Returns the constructor-set price stream id.
    fn get_feed(e: Env) -> BytesN<32>;
}

#[contractimpl]
impl MarketContract {
    /// Initializes the market contract.
    ///
    /// `MarketData` starts zeroed with its accrual timestamp at `now`. The
    /// status starts `Status::Active`.
    ///
    /// # Arguments
    ///
    /// * `e` - Access to the Soroban environment.
    /// * `owner` - The contract owner for the admin entries.
    /// * `token` - The settlement token.
    /// * `vault` - The strategy-vault contract.
    /// * `oracle` - The oracle contract.
    /// * `treasury` - The protocol fee sink.
    /// * `feed_id` - The price stream id. Immutable.
    /// * `config` - The initial global market configuration.
    ///
    /// # Errors
    ///
    /// * [`MarketError::InvalidConfig`] - If `feed_id` is not a V3 stream id
    ///   (`0x0003` prefix), or a bound or range check fails.
    /// * [`MarketError::NegativeValueNotAllowed`] - If a fee, margin,
    ///   borrowing, funding, ADL, or PnL field, `min_order_margin`,
    ///   `min_deposit`, or `exec_fee` is negative. Any other negative field
    ///   fails its own bound as [`MarketError::InvalidConfig`].
    ///
    /// # Notes
    ///
    /// * Called once at deployment. It sets `owner` as the contract owner.
    pub fn __constructor(
        e: Env,
        owner: Address,
        token: Address,
        vault: Address,
        oracle: Address,
        treasury: Address,
        feed_id: BytesN<32>,
        config: Config,
    ) {
        // Only a V3 stream (`0x0003` prefix) carries the report shape the
        // oracle decodes.
        let feed = feed_id.to_array();
        if feed[0] != 0x00 || feed[1] != 0x03 {
            panic_with_error!(&e, MarketError::InvalidConfig);
        }
        config.require_valid(&e);
        // The bounds and the vault's share decimals assume the token's.
        if token::TokenClient::new(&e, &token).decimals() != TOKEN_DECIMALS {
            panic_with_error!(&e, MarketError::InvalidConfig);
        }

        ownable::set_owner(&e, &owner);
        // `get_schema_version` reports 0 when unset. Stamp 1 so a migration
        // can tell schema version 1 from unset.
        upgradeable::set_schema_version(&e, 1);
        storage::set_token(&e, &token);
        storage::set_vault(&e, &vault);
        storage::set_oracle(&e, &oracle);
        storage::set_treasury(&e, &treasury);
        storage::set_feed_id(&e, &feed_id);
        storage::set_config(&e, &config);

        let now = e.ledger().timestamp();
        storage::set_market_data(
            &e,
            &MarketData {
                accrued_at: now,
                ..Default::default()
            },
        );

        storage::set_status(&e, &Status::Active);
        storage::extend_instance(&e);
    }
}

#[contractimpl]
impl Market for MarketContract {
    #[only_owner]
    fn set_config(e: Env, config: Config) {
        storage::extend_instance(&e);
        engine::set_config(&e, &config);
    }

    #[only_owner]
    fn set_status(e: Env, status: u32) {
        storage::extend_instance(&e);
        engine::set_status(&e, &Status::from_u32(&e, status));
    }

    #[only_owner]
    fn set_terminal_price(e: Env, price: i128) {
        storage::extend_instance(&e);
        engine::set_terminal_price(&e, price);
    }

    fn create_order(
        e: Env,
        user: Address,
        is_long: bool,
        kind: u32,
        notional: i128,
        margin: i128,
        trigger_price: i128,
        price_bound: i128,
        expiration: u32,
    ) -> u32 {
        storage::extend_instance(&e);
        engine::create_order(
            &e,
            &user,
            is_long,
            kind,
            notional,
            margin,
            trigger_price,
            price_bound,
            expiration,
        )
    }

    fn cancel_order(e: Env, user: Address, id: u32) -> i128 {
        storage::extend_instance(&e);
        engine::cancel_order(&e, &user, id)
    }

    fn create_vault_order(e: Env, user: Address, kind: u32, amount: i128, min_out: i128) -> u32 {
        storage::extend_instance(&e);
        engine::create_vault_order(&e, &user, kind, amount, min_out)
    }

    fn cancel_vault_order(e: Env, user: Address, id: u32) -> i128 {
        storage::extend_instance(&e);
        engine::cancel_vault_order(&e, &user, id)
    }

    fn claim_credit(e: Env, user: Address) -> i128 {
        storage::extend_instance(&e);
        engine::claim_credit(&e, &user)
    }

    fn execute_order(e: Env, keeper: Address, user: Address, id: u32, price: Bytes) -> i128 {
        storage::extend_instance(&e);
        engine::execute_order(&e, &keeper, &user, id, &price)
    }

    fn execute_liquidation(
        e: Env,
        keeper: Address,
        user: Address,
        is_long: bool,
        price: Bytes,
    ) -> i128 {
        storage::extend_instance(&e);
        engine::execute_liquidation(&e, &keeper, &user, is_long, &price)
    }

    fn update_adl_state(e: Env, price: Bytes) -> AdlState {
        storage::extend_instance(&e);
        engine::update_adl_state(&e, &price)
    }

    fn execute_adl(
        e: Env,
        keeper: Address,
        user: Address,
        is_long: bool,
        amount: i128,
        price: Bytes,
    ) -> i128 {
        storage::extend_instance(&e);
        engine::execute_adl(&e, &keeper, &user, is_long, amount, &price)
    }

    fn execute_vault_order(e: Env, keeper: Address, user: Address, id: u32, price: Bytes) -> i128 {
        storage::extend_instance(&e);
        engine::execute_vault_order(&e, &keeper, &user, id, &price)
    }

    fn accrue(e: Env, price: Bytes) -> MarketData {
        storage::extend_instance(&e);
        engine::accrue(&e, &price)
    }

    fn get_config(e: Env) -> Config {
        storage::get_config(&e)
    }

    fn get_market_data(e: Env) -> MarketData {
        storage::get_market_data(&e)
    }

    fn get_position(e: Env, user: Address, is_long: bool) -> Position {
        storage::get_position(&e, &user, is_long)
    }

    fn get_order(e: Env, user: Address, id: u32) -> Order {
        storage::get_order(&e, &user, id)
    }

    fn get_status(e: Env) -> u32 {
        storage::get_status(&e) as u32
    }

    fn get_vault_order(e: Env, user: Address, id: u32) -> VaultOrder {
        storage::get_vault_order(&e, &user, id)
    }

    fn get_order_counter(e: Env, user: Address) -> u32 {
        storage::get_order_counter(&e, &user)
    }

    fn get_adl(e: Env) -> AdlState {
        storage::get_adl(&e)
    }

    fn get_claimable_credit(e: Env, user: Address) -> i128 {
        storage::get_claimable_credit(&e, &user)
    }

    fn get_token(e: Env) -> Address {
        storage::get_token(&e)
    }

    fn get_vault(e: Env) -> Address {
        storage::get_vault(&e)
    }

    fn get_treasury(e: Env) -> Address {
        storage::get_treasury(&e)
    }

    fn get_oracle(e: Env) -> Address {
        storage::get_oracle(&e)
    }

    fn get_retirement(e: Env) -> Option<(i128, u64)> {
        storage::get_delisted_at(&e).map(|delisted_at| {
            let price = if storage::has_terminal_price(&e) {
                storage::get_terminal_price(&e)
            } else {
                0
            };
            (price, delisted_at)
        })
    }

    fn get_feed(e: Env) -> BytesN<32> {
        storage::get_feed_id(&e)
    }
}

#[contractimpl(contracttrait)]
impl Ownable for MarketContract {}

// No `contracttrait`: `Upgradeable` has no default bodies, so authorization
// is ours.
#[contractimpl]
impl Upgradeable for MarketContract {
    /// Replaces the contract WASM.
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
    /// * [`MarketError::UpgradeNotOwner`] - If `operator` is not the owner.
    ///
    /// # Notes
    ///
    /// * Authorization for the owner is required, and `operator` must be the
    ///   owner.
    #[only_owner]
    fn upgrade(e: &Env, new_wasm_hash: BytesN<32>, operator: Address) {
        // `operator` is inert under `ownable`, so it is pinned to the owner.
        if ownable::get_owner(e).as_ref() != Some(&operator) {
            panic_with_error!(e, MarketError::UpgradeNotOwner);
        }
        // `upgrade` is the call most likely to arrive after a long quiet
        // stretch, so the TTL is extended here too.
        storage::extend_instance(e);
        upgradeable::upgrade(e, &new_wasm_hash);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};

    /// Returns a V3 stream id: the `0x0003` prefix, then filler bytes.
    fn v3_feed() -> [u8; 32] {
        let mut feed = [7u8; 32];
        feed[0] = 0x00;
        feed[1] = 0x03;
        feed
    }

    /// Registers the market at ledger timestamp 1_000 with `feed` as its
    /// feed id and the unit-test config, and returns its address.
    fn deploy(e: &Env, feed: [u8; 32]) -> Address {
        // A Stellar asset contract, so the constructor's decimals read sees 7.
        let token = e.register_stellar_asset_contract_v2(Address::generate(e));
        deploy_with_token(e, feed, &token.address())
    }

    /// Registers the market as [`deploy`] does, with `token` as the
    /// collateral, and returns its address.
    fn deploy_with_token(e: &Env, feed: [u8; 32], token: &Address) -> Address {
        e.ledger().with_mut(|ledger| ledger.timestamp = 1_000);
        e.register(
            MarketContract,
            (
                Address::generate(e),
                token.clone(),
                Address::generate(e),
                Address::generate(e),
                Address::generate(e),
                BytesN::from_array(e, &feed),
                crate::engine::config::test_config(),
            ),
        )
    }

    // The constructor starts the shared accrual clock at the deploy
    // timestamp, not at zero, so the first window spans only the time since
    // deployment.
    #[test]
    fn constructor_stamps_the_accrual_clock_at_deploy() {
        let e = Env::default();
        let market = deploy(&e, v3_feed());
        e.as_contract(&market, || {
            assert_eq!(storage::get_market_data(&e).accrued_at, 1_000);
            assert_eq!(storage::get_status(&e), Status::Active);
        });
    }

    // A V2 stream id (`0x0002`) keeps the zero leading byte but is still not
    // a V3 id, so one wrong prefix byte rejects.
    #[test]
    #[should_panic(expected = "#700")]
    fn constructor_rejects_a_v2_feed_id() {
        let e = Env::default();
        let mut feed = v3_feed();
        feed[1] = 0x02;
        deploy(&e, feed);
    }

    // The `0x03` second byte alone does not make a V3 id. A non-zero leading
    // byte rejects.
    #[test]
    #[should_panic(expected = "#700")]
    fn constructor_rejects_a_nonzero_leading_feed_byte() {
        let e = Env::default();
        let mut feed = v3_feed();
        feed[0] = 0x01;
        deploy(&e, feed);
    }

    // Reports the constructor-set decimals and nothing else.
    #[soroban_sdk::contract]
    struct DecimalsToken;

    #[soroban_sdk::contractimpl]
    impl DecimalsToken {
        pub fn __constructor(e: Env, decimals: u32) {
            e.storage()
                .instance()
                .set(&soroban_sdk::symbol_short!("decimals"), &decimals);
        }

        pub fn decimals(e: Env) -> u32 {
            e.storage()
                .instance()
                .get(&soroban_sdk::symbol_short!("decimals"))
                .unwrap()
        }
    }

    // The collateral must have exactly `TOKEN_DECIMALS` (7). Its neighbors,
    // 6 and 8, reject.
    #[test]
    #[should_panic(expected = "#700")]
    fn constructor_rejects_a_six_decimal_token() {
        let e = Env::default();
        let token = e.register(DecimalsToken, (6_u32,));
        deploy_with_token(&e, v3_feed(), &token);
    }

    #[test]
    #[should_panic(expected = "#700")]
    fn constructor_rejects_an_eight_decimal_token() {
        let e = Env::default();
        let token = e.register(DecimalsToken, (8_u32,));
        deploy_with_token(&e, v3_feed(), &token);
    }
}
