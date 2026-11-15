//! Oracle-wide constants.

/// The 18-decimal fixed-point unit: the scale of `spread_reduction_factor`.
/// Report prices keep the feed's native precision.
pub const SCALAR_18: i128 = 1_000_000_000_000_000_000;

/// Upper bound on `trade_staleness` (seconds). The market's
/// `MIN_NOTIONAL_LOCK` is sized to this ceiling, so the two move together.
pub const MAX_TRADE_STALENESS_SECONDS: u64 = 15;

/// Upper bound on `close_staleness` (seconds), the protective window for the
/// gap-closing calls. It is wider than the trade ceiling by design. Closing
/// at an old price beats not closing at all. The market's price cache and
/// per-position `priced_at` floor keep the window from becoming a
/// price-selection surface.
pub const MAX_CLOSE_STALENESS_SECONDS: u64 = 120;

/// Lower bound on both staleness windows (seconds), a degenerate-value guard.
pub const MIN_STALENESS_SECONDS: u64 = 3;
