//! Market contract events: order and vault-order receipts, fill receipts,
//! and admin updates.

use crate::engine::config::Config;
use crate::engine::order::Order;
use crate::engine::vault_order::VaultOrder;
use soroban_sdk::{contractevent, Address};

/// A trader created an order through `create_order`. The row is immutable
/// while pending.
#[contractevent]
#[derive(Clone)]
pub struct CreateOrder {
    #[topic]
    pub user: Address,
    #[topic]
    pub id: u32,
    /// The stored order row, as `get_order` returns it.
    pub order: Order,
}

/// A pending order was cancelled. `user` cancelled it through
/// `cancel_order`, or the closure sweep cancelled it when its side's
/// position closed.
#[contractevent]
#[derive(Clone)]
pub struct CancelOrder {
    #[topic]
    pub user: Address,
    #[topic]
    pub id: u32,
    /// The escrow this cancel returned, `Order::escrow_amount` (token-dec).
    pub refund: i128,
}

/// A trader created a vault deposit or redeem order through
/// `create_vault_order`. The row is immutable while pending.
#[contractevent]
#[derive(Clone)]
pub struct CreateVaultOrder {
    #[topic]
    pub user: Address,
    #[topic]
    pub id: u32,
    /// The stored vault-order row, as `get_vault_order` returns it.
    pub order: VaultOrder,
}

/// The trader cancelled a pending vault order through `cancel_vault_order`.
#[contractevent]
#[derive(Clone)]
pub struct CancelVaultOrder {
    #[topic]
    pub user: Address,
    #[topic]
    pub id: u32,
}

/// A keeper filled a deposit order through `execute_vault_order`.
#[contractevent]
#[derive(Clone)]
pub struct DepositFill {
    #[topic]
    pub user: Address,
    #[topic]
    pub id: u32,
    /// The reward recipient the fill's caller named.
    pub keeper: Address,
    /// The gross assets deposited from escrow (token-dec). The vault receives
    /// `assets - fee`.
    pub assets: i128,
    /// The vault shares minted to the user (share decimals).
    pub shares: i128,
    /// The vault fee charged (token-dec).
    pub fee: i128,
    /// The NAV adjustment the share mint priced against, signed (token-dec).
    /// It includes pending borrowing income.
    pub net_pnl: i128,
}

/// A keeper filled a redeem order through `execute_vault_order`.
#[contractevent]
#[derive(Clone)]
pub struct RedeemFill {
    #[topic]
    pub user: Address,
    /// The vault order id. `0` is a retired-market instant redeem, executed
    /// at creation.
    #[topic]
    pub id: u32,
    /// The reward recipient the fill's caller named. On a retired-market
    /// instant redeem it is the redeeming user.
    pub keeper: Address,
    /// The vault shares burned from escrow (share decimals).
    pub shares: i128,
    /// The gross assets redeemed from the vault (token-dec). The user
    /// receives `assets - fee`, parked as claimable credit if the transfer
    /// fails.
    pub assets: i128,
    /// The vault fee charged (token-dec).
    pub fee: i128,
    /// The NAV adjustment the share burn priced against, signed (token-dec).
    /// It includes pending borrowing income.
    pub net_pnl: i128,
}

/// A keeper rejected a vault order through `execute_vault_order`: the fill
/// would have paid less than the order's `min_out`. The order's `amount`
/// returns to the user and its `exec_fee` pays the keeper. Both are in the
/// order's `CreateVaultOrder` row. A deposit refund the user cannot receive
/// parks as claimable credit.
#[contractevent]
#[derive(Clone)]
pub struct RejectVaultOrder {
    #[topic]
    pub user: Address,
    #[topic]
    pub id: u32,
    /// The reward recipient the fill's caller named.
    pub keeper: Address,
    /// What the fill would have paid, below `min_out`: the shares a deposit
    /// would mint (share decimals), or a redeem's assets net of the vault
    /// fee (token-dec).
    pub quoted: i128,
    /// The NAV adjustment the quote priced against, signed (token-dec).
    /// It includes pending borrowing income.
    pub net_pnl: i128,
}

/// A user claimed their credit balance through `claim_credit`.
#[contractevent]
#[derive(Clone)]
pub struct ClaimCredit {
    #[topic]
    pub user: Address,
    /// The claimable balance paid (token-dec).
    pub amount: i128,
}

/// A keeper recomputed the ADL flags through `update_adl_state`.
#[contractevent]
#[derive(Clone)]
pub struct AdlUpdate {
    /// Long-side ADL is enabled. Size-growing long increases are blocked.
    pub long: bool,
    /// Short-side ADL is enabled. Size-growing short increases are blocked.
    pub short: bool,
}

/// A keeper advanced the accrual indices through `accrue`. The event only
/// marks the poke. The post-accrual market state is read from
/// `get_market_data`.
#[contractevent]
#[derive(Clone)]
pub struct AccrualUpdate {}

/// The owner changed the operational status through `set_status`.
#[contractevent]
#[derive(Clone)]
pub struct StatusUpdate {
    /// The new operational status, as the `Status` discriminant.
    pub status: u32,
}

/// The owner replaced the global configuration through `set_config`.
#[contractevent]
#[derive(Clone)]
pub struct ConfigUpdate {
    /// The new global market configuration.
    pub config: Config,
}

/// The owner set or refreshed the flat settlement price through
/// `set_terminal_price`.
#[contractevent]
#[derive(Clone)]
pub struct TerminalPriceUpdate {
    /// The flat settlement price, in the feed's native precision.
    pub price: i128,
}

/// A keeper fill opened the position. The side was empty before the fill.
///
/// The payload is `IncreaseFill`'s minus `funding` and `borrowing`.
/// Accruals settle over the pre-fill notional, so both are zero on an open.
#[contractevent]
#[derive(Clone)]
pub struct OpenFill {
    #[topic]
    pub user: Address,
    #[topic]
    pub id: u32,
    #[topic]
    pub is_long: bool,
    /// The reward recipient the fill's caller named.
    pub keeper: Address,
    /// The entry-side execution price, the ask for a long and the bid for a
    /// short, in the feed's native precision.
    pub price: i128,
    /// The size opened (token-dec).
    pub notional: i128,
    /// The base size bought (base-dec).
    pub tokens: i128,
    /// The margin posted, escrowed at order creation (token-dec).
    pub margin: i128,
    /// The trade fee charged (token-dec).
    pub base_fee: i128,
    /// The impact fee charged (token-dec).
    pub impact_fee: i128,
}

/// A keeper filled an increase order on an already-open position. An order
/// that opens the side emits `OpenFill` instead.
#[contractevent]
#[derive(Clone)]
pub struct IncreaseFill {
    #[topic]
    pub user: Address,
    #[topic]
    pub id: u32,
    #[topic]
    pub is_long: bool,
    /// The reward recipient the fill's caller named.
    pub keeper: Address,
    /// The entry-side execution price, the ask for a long and the bid for a
    /// short, in the feed's native precision.
    pub price: i128,
    /// The size added (token-dec).
    pub notional: i128,
    /// The base size bought (base-dec).
    pub tokens: i128,
    /// The margin posted, escrowed at order creation (token-dec).
    pub margin: i128,
    /// The trade fee charged (token-dec).
    pub base_fee: i128,
    /// The impact fee charged (token-dec).
    pub impact_fee: i128,
    /// The settled funding (token-dec). Positive is paid from margin.
    /// Negative is credited as claimable.
    pub funding: i128,
    /// The settled borrowing fee (token-dec).
    pub borrowing: i128,
}

/// A keeper filled a partial decrease. The position survives the fill.
///
/// `notional` and `tokens` are the closed fraction at entry pricing.
/// `price` is the exit price. `margin` and `pnl` are gross of the itemized
/// fees.
#[contractevent]
#[derive(Clone)]
pub struct DecreaseFill {
    #[topic]
    pub user: Address,
    /// The filled order id. `0` is a forced ADL close through `execute_adl`.
    #[topic]
    pub id: u32,
    #[topic]
    pub is_long: bool,
    /// The reward recipient the fill's caller named.
    pub keeper: Address,
    /// The exit-side execution price, the bid for a long and the ask for a
    /// short, in the feed's native precision.
    pub price: i128,
    /// The closed size (token-dec).
    pub notional: i128,
    /// The base size closed (base-dec).
    pub tokens: i128,
    /// The requested withdrawal (token-dec).
    pub margin: i128,
    /// The realized PnL on the closed fraction, post-haircut, gross of the
    /// settled costs (token-dec).
    pub pnl: i128,
    /// The trade fee charged (token-dec).
    pub base_fee: i128,
    /// The impact fee charged (token-dec).
    pub impact_fee: i128,
    /// The settled funding (token-dec). Positive is paid from margin.
    /// Negative is credited as claimable.
    pub funding: i128,
    /// The settled borrowing fee (token-dec).
    pub borrowing: i128,
    /// The withdrawal plus the profit the fees did not consume, paid to the
    /// trader or parked as claimable credit (token-dec).
    pub returned: i128,
}

/// A keeper fill closed the whole position. The stored row zeroes.
///
/// `notional` and `tokens` are the full closed size at entry pricing.
/// `price` is the exit price. `margin` and `pnl` are gross of the itemized
/// fees.
#[contractevent]
#[derive(Clone)]
pub struct CloseFill {
    #[topic]
    pub user: Address,
    /// The filled order id. `0` is a forced ADL close through `execute_adl`.
    #[topic]
    pub id: u32,
    #[topic]
    pub is_long: bool,
    /// The reward recipient the fill's caller named.
    pub keeper: Address,
    /// The exit-side execution price, the bid for a long and the ask for a
    /// short, in the feed's native precision.
    pub price: i128,
    /// The full closed size (token-dec).
    pub notional: i128,
    /// The base size closed (base-dec).
    pub tokens: i128,
    /// The freed margin, gross of the itemized fees (token-dec).
    pub margin: i128,
    /// The realized PnL on the closed size, post-haircut, gross of the
    /// settled costs (token-dec).
    pub pnl: i128,
    /// The trade fee charged (token-dec).
    pub base_fee: i128,
    /// The impact fee charged (token-dec).
    pub impact_fee: i128,
    /// The settled funding (token-dec). Positive is paid from margin.
    /// Negative is credited as claimable.
    pub funding: i128,
    /// The settled borrowing fee (token-dec).
    pub borrowing: i128,
    /// The fees and losses past the freed margin, which the vault absorbs
    /// (token-dec).
    pub bad_debt: i128,
    /// The post-fee equity for the trader, paid or parked as claimable
    /// credit (token-dec).
    pub returned: i128,
}

/// A keeper liquidated the position. The full size is force-closed.
///
/// `margin` and `pnl` are gross of the itemized fees. The liquidation fee
/// splits like a trade fee between the keeper, the treasury, and the vault.
#[contractevent]
#[derive(Clone)]
pub struct Liquidation {
    #[topic]
    pub user: Address,
    #[topic]
    pub is_long: bool,
    /// The reward recipient the fill's caller named.
    pub keeper: Address,
    /// The exit-side execution price, the bid for a long and the ask for a
    /// short, in the feed's native precision.
    pub price: i128,
    /// The force-closed size (token-dec).
    pub notional: i128,
    /// The base size closed (base-dec).
    pub tokens: i128,
    /// The freed margin, gross of the itemized fees (token-dec).
    pub margin: i128,
    /// The realized PnL on the closed size, post-haircut, gross of the
    /// settled costs (token-dec).
    pub pnl: i128,
    /// The trade fee charged (token-dec).
    pub base_fee: i128,
    /// The impact fee charged (token-dec).
    pub impact_fee: i128,
    /// The settled funding (token-dec). Positive is paid from margin.
    /// Negative is credited as claimable.
    pub funding: i128,
    /// The settled borrowing fee (token-dec).
    pub borrowing: i128,
    /// The fees and losses past the freed margin, which the vault absorbs
    /// (token-dec).
    pub bad_debt: i128,
    /// The remainder for the trader, net of the liquidation fee, paid or
    /// parked as claimable credit (token-dec).
    pub returned: i128,
    /// The liquidation fee charged (token-dec):
    /// `min(max(equity, 0), ceil(Config::liq_fee * notional))`.
    pub liq_fee: i128,
}
