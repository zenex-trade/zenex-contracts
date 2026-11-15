//! `OracleError` contract error enum. The oracle owns the 78x/79x domain.
//! Gaps are retired codes.

use soroban_sdk::contracterror;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum OracleError {
    // --- shared admin (6xx) ---
    /// `upgrade` was called with an `operator` that is not the contract owner.
    /// The discriminant is shared: market and factory raise the same code for
    /// the same condition.
    UpgradeNotOwner = 600,

    /// The verified report body failed decoding, or the feed id is not a V3
    /// stream id (`0x0003` prefix).
    InvalidData = 780,
    /// A non-positive benchmark or price side, a crossed book (`bid > ask`),
    /// or an `int192` value that overflows `i128`.
    InvalidPrice = 781,
    /// The observation is older than the selected staleness window.
    PriceStale = 782,
    /// A staleness pair that violates `MIN_STALENESS_SECONDS <= trade <=
    /// MAX_TRADE_STALENESS_SECONDS` or `trade <= close <=
    /// MAX_CLOSE_STALENESS_SECONDS`.
    InvalidStaleness = 783,
    /// The ledger clock has passed the report's own `expiresAt`.
    ReportExpired = 784,
    /// A `spread_reduction_factor` outside `[0, SCALAR_18]`.
    InvalidSpreadReduction = 785,

    /// The report prices a different stream than the caller's feed anchor.
    FeedMismatch = 790,
    /// The report's validity window opens, or its observation sits, more
    /// than the forward allowance ahead of the ledger clock. The forward
    /// allowance is the trade window for both classes.
    PriceAhead = 793,
}
