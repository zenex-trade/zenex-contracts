//! Oracle contract events: the owner-settable gates. `verify_price`
//! publishes nothing. The caller records the price it needs.

use soroban_sdk::contractevent;

/// The owner replaced the staleness windows through `update_staleness`.
#[contractevent]
#[derive(Clone)]
pub struct StalenessUpdate {
    /// The new strict (fill) window (seconds).
    pub trade_staleness: u64,
    /// The new protective (gap-closing) window (seconds).
    pub close_staleness: u64,
}

/// The owner replaced the spread reduction factor through
/// `update_spread_reduction_factor`.
#[contractevent]
#[derive(Clone)]
pub struct SpreadReductionUpdate {
    /// The new `SCALAR_18`-scaled narrowing toward the mid.
    pub spread_reduction_factor: i128,
}
