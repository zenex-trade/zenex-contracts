//! Scalars, thresholds, and config-validation bounds.

/// The single fixed-point scalar (18 decimals).
pub const SCALAR_18: i128 = 1_000_000_000_000_000_000;
/// One whole settlement token in token-dec: `10^TOKEN_DECIMALS`.
pub const SCALAR_7: i128 = 10_000_000;
/// The settlement token's decimals, checked at construction. Every Stellar
/// asset contract has 7, and the bounds below are sized for it.
pub const TOKEN_DECIMALS: u32 = 7;

// --- Config validation bounds (checked in `Config::check_valid`) and thresholds ---

/// Max keeper share of trade, liquidation and vault fill fees: 50%.
pub const MAX_KEEPER_RATE: i128 = SCALAR_18 / 2;
/// Max per-side trade fee: 1% of notional.
pub const MAX_FEE_RATE: i128 = SCALAR_18 / 100;
/// Global utilization cap ceiling: 1000%.
pub const MAX_UTIL: i128 = 10 * SCALAR_18;
/// Impact-fee rate ceiling: 10% of notional.
pub const MAX_IMPACT_RATE: i128 = SCALAR_18 / 10;
/// Ceiling on a minimum-size chunk's impact rate,
/// `min_position_notional * SCALAR_18 / impact_scalar`. `check_valid`
/// floors `impact_scalar` against it.
pub const MIN_CHUNK_IMPACT_CAP: i128 = SCALAR_18 / 1000;
/// Max initial margin: 50% = 2x min leverage.
pub const MAX_MARGIN: i128 = SCALAR_18 / 2;
/// Min initial margin: 0.1% = 1000x max leverage.
pub const MIN_MARGIN: i128 = SCALAR_18 / 1000;
/// Max liquidation fee: 25%.
pub const MAX_LIQ_FEE: i128 = SCALAR_18 / 4;
/// ADL trigger floor: 45% of half the vault balance. A config change
/// cannot arm ADL against modest open winners.
pub const MIN_ADL_TRIGGER: i128 = 45 * SCALAR_18 / 100;
/// ADL clear-target floor: 40% of half the vault balance. It bounds how
/// deep a config change can let ADL cut.
pub const MIN_ADL_CLEAR: i128 = 40 * SCALAR_18 / 100;
/// Seconds in a 365-day year, the APR basis for per-second rate bounds.
pub const SECONDS_PER_YEAR: i128 = 31_536_000;
/// Max borrowing rate at full utilization: 1000% APR, per second.
pub const MAX_BORROW_RATE: i128 = 10 * SCALAR_18 / SECONDS_PER_YEAR;
/// Max funding-rate magnitude: 1000% APR, per second.
pub const MAX_FUNDING_RATE: i128 = 10 * SCALAR_18 / SECONDS_PER_YEAR;
/// Min decrease lock on newly added notional (seconds). Sized to the
/// oracle's trade-class staleness ceiling,
/// `MAX_TRADE_STALENESS_SECONDS`: it blocks an open-then-decrease round
/// trip inside one accepted payload's validity.
pub const MIN_NOTIONAL_LOCK: u64 = 15;
/// Max decrease lock on newly added notional: 1 day (seconds).
pub const MAX_NOTIONAL_LOCK: u64 = 86_400;
/// Delist grace window: 1 day (seconds). Un-delisting stays open until it
/// elapses. After that the delist is permanent and the owner may set the
/// flat settlement price.
pub const DELIST_GRACE: u64 = 86_400;
/// Delist deadline: 7 days (seconds) from the delist. Traders may close
/// voluntarily until it passes. After that, forced liquidation unlocks.
pub const DELIST_DEADLINE: u64 = 7 * 86_400;
/// Max redeem cooldown on vault orders: 30 days (seconds).
pub const MAX_REDEEM_LOCK: u64 = 2_592_000;
/// Bounds the vault-order fill floor to a fraction of `max_vault_balance`:
/// `min_deposit <= max_vault_balance / MIN_DEPOSIT_DIVISOR`.
pub const MIN_DEPOSIT_DIVISOR: i128 = 100;
/// Per-side open-interest ceiling bound: `10^11` tokens. It keeps the
/// borrowing weight, `notional * borrowing_idx` summed over both sides,
/// inside `i128` while the summed index stays below 170 (170x notional in
/// cumulative borrowing). At `MAX_BORROW_RATE` on both sides that is 8.5
/// years; at one side or a realistic rate it is far longer.
pub const MAX_OPEN_INTEREST: i128 = 100_000_000_000 * SCALAR_7;
/// Max pending decrease orders per `(user, is_long)`.
pub const MAX_ORDERS_PER_SIDE: u32 = 8;

// --- Unit-test scales ---

/// Price scale of every unit-test price: `10^18`, the scale a production
/// Chainlink Data Streams V3 report carries. Every test price derives from
/// this constant. A fixture at any other scale exercises a base-size
/// granularity the deployment never sees.
#[cfg(test)]
pub const PRICE_SCALAR: i128 = SCALAR_18;

/// One whole unit of the traded asset in base-dec at [`PRICE_SCALAR`]:
/// `10^7`. A unit worth `p` dollars costs `p * 10^7` token-dec against the
/// 7-decimal settlement token, and `notional * SCALAR_18 / price` returns
/// `10^7` base-dec for it.
#[cfg(test)]
pub const BASE_UNIT: i128 = SCALAR_7;
