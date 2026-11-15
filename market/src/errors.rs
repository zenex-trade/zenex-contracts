//! `MarketError` contract error enum.
//!
//! Market errors sit in the 7xx domain. The 6xx admin domain is shared
//! with the other upgradeable contracts, same code, same meaning.

use soroban_sdk::contracterror;

#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
#[repr(u32)]
#[contracterror]
pub enum MarketError {
    // --- shared admin (6xx) ---
    /// `upgrade` was called with an `operator` that is not the contract owner.
    UpgradeNotOwner = 600,

    // --- config / construction ---
    /// A value is out of bounds, a range or ordering invariant is violated,
    /// or the feed id is not a V3 stream id.
    InvalidConfig = 700,
    /// The flat settlement price is not strictly positive.
    InvalidPrice = 701,
    /// An illegal status transition, an unknown status discriminant, or an
    /// action that requires a different operational status.
    InvalidStatus = 702,
    /// An accrual-rate parameter, borrowing or funding, changed without a
    /// same-ledger `accrue`.
    MarketNotAccrued = 703,
    /// The operational status halted the action: `Frozen`, or `Retired` on
    /// the market paths.
    MarketFrozen = 704,
    /// A size-growing increase ran while the market does not accept opens,
    /// or the target side has ADL enabled.
    IncreaseHalted = 705,
    /// Retirement was attempted while positions remain open.
    MarketNotCleared = 706,

    // --- general ---
    /// A number that must be non-negative is negative.
    NegativeValueNotAllowed = 710,

    // --- position sizing / margin ---
    /// The resulting position notional is below `min_position_notional`.
    NotionalBelowMinimum = 711,
    /// The position notional, or an increase delta, exceeds
    /// `max_position_notional`.
    NotionalAboveMaximum = 712,
    /// Margin is below the initial-margin floor after an open, increase, or
    /// withdraw.
    InsufficientMargin = 713,
    /// A side's reserved value exceeds the utilization cap of half the vault
    /// balance, after an open or a redeem.
    UtilizationExceeded = 714,
    /// A side's open interest would exceed the `max_open_interest` ceiling.
    OpenInterestExceeded = 715,
    /// An increase's notional buys no base size at the entry price.
    SizeRoundsToZero = 716,

    // --- position lifecycle ---
    /// No position exists for `(user, is_long)`.
    PositionNotFound = 720,
    /// The close exceeds the position's unlocked notional.
    NotionalLocked = 721,
    /// Liquidation was attempted while equity is at or above the maintenance
    /// margin and the delist deadline has not passed.
    NotLiquidatable = 722,
    /// Settled equity is below the maintenance margin, before a decrease or
    /// ADL, or after an open, increase, withdraw, or partial close.
    PositionLiquidatable = 723,

    // --- orders / price ---
    /// No keeper order exists for `(user, id)`.
    OrderNotFound = 730,
    /// The order's `expiration` is behind the current ledger sequence.
    OrderExpired = 731,
    /// The order is malformed: an illegal delta pair, a value under its dust
    /// or deposit floor, a zero `trigger_price` on a trigger kind, an escrow
    /// overflow, or a zero amount.
    InvalidOrder = 732,
    /// A side already holds `MAX_ORDERS_PER_SIDE` pending decrease orders.
    TooManyOrders = 733,
    /// An order or vault-order `kind` discriminant is not a known variant.
    UnknownKind = 734,
    /// The verified price predates the order's creation, or the price the
    /// position was last marked against. Also a vault fill that runs in its
    /// order's creation ledger.
    StalePrice = 740,
    /// The fill price is worse than the order's `price_bound`.
    PriceBoundExceeded = 741,
    /// The order's `trigger_price` has not been crossed at the verified
    /// price.
    TriggerNotMet = 742,

    // --- vault orders ---
    /// No vault order exists for `(user, id)`.
    VaultOrderNotFound = 750,
    /// A redeem fill ran before its `redeem_lock` cooldown elapsed.
    VaultOrderLocked = 751,
    /// A deposit fill would push the vault balance above
    /// `max_vault_balance`.
    VaultBalanceExceeded = 753,
    /// A redeem fill ran while a side's pending PnL exceeds
    /// `max_pnl_withdraw` of half the post-redeem vault balance, or a
    /// deposit fill while it exceeds `max_pnl_trader` of half the balance.
    PendingPnlExceeded = 754,
    /// A settlement's vault draw exceeds the vault's balance.
    VaultInsolvent = 755,

    // --- funding ---
    /// A claim was attempted with no claimable credit balance, or with an
    /// empty pool.
    NothingToClaim = 760,

    // --- ADL ---
    /// ADL execution was attempted on a side that is not flagged, or whose
    /// pending PnL is already at or below the clear target.
    AdlNotTriggered = 770,
    /// An ADL close left the side's pending PnL under the clear target.
    AdlOvershoot = 771,
    /// An ADL close did not reduce the side's pending PnL.
    AdlNotEligible = 772,
}
