//! Shared scale and market constants for the integration suites.

/// Settlement-token unit: the SAC is 7-decimal, so `1 * SCALAR_7` is one token.
pub const SCALAR_7: i128 = 10_000_000;

/// The single dimensionless fixed-point scalar (rates, fees, ratios, margins).
pub const SCALAR_18: i128 = 1_000_000_000_000_000_000;

/// Price scale of every fixture price: `10^18`, the scale a production
/// Chainlink Data Streams V3 report carries. Precision does not cancel: the
/// entry base size floors or ceils at the price's resolution. A fixture
/// priced at any other scale therefore exercises a granularity the deployment
/// never sees. Build every price from this constant, never from a raw literal.
pub const PRICE_SCALAR: i128 = SCALAR_18;

/// Stream id the fixture deploys its market against: a V3-prefixed
/// (`0x0003…`) test id.
pub fn feed_btc(env: &soroban_sdk::Env) -> soroban_sdk::BytesN<32> {
    let mut id = [0u8; 32];
    id[1] = 0x03;
    id[31] = 1;
    soroban_sdk::BytesN::from_array(env, &id)
}

/// The fixture's BTC mark, $100,000 at [`PRICE_SCALAR`]. It exceeds the
/// `i64` range, which is why prices are `i128` throughout.
pub const BTC_PRICE: i128 = 100_000 * PRICE_SCALAR;

/// Time constants (seconds).
pub const SECONDS_IN_HOUR: u64 = 3_600;
pub const SECONDS_PER_WEEK: u64 = 604_800;

// Config values mirrored from `crate::default_config` so tests can use them in
// const positions. `mirrored_config_constants_match_default_config` (below)
// guards against drift.

/// Flat keeper execution fee escrowed per order at creation (token-dec).
pub const EXEC_FEE: i128 = 100_000;
/// Minimum |notional| per order, the dust floor (token-dec).
pub const MIN_ORDER_NOTIONAL: i128 = 1_000_000;
/// Minimum posted margin per order, the dust floor (token-dec).
pub const MIN_ORDER_MARGIN: i128 = 1_000_000;

#[cfg(test)]
mod tests {
    use super::*;

    /// The mirrored config constants stay in sync with `default_config`. A
    /// change there must land here too, or the tests that use them assert
    /// stale values.
    #[test]
    fn mirrored_config_constants_match_default_config() {
        let config = crate::default_config();
        assert_eq!(EXEC_FEE, config.exec_fee);
        assert_eq!(MIN_ORDER_NOTIONAL, config.min_order_notional);
        assert_eq!(MIN_ORDER_MARGIN, config.min_order_margin);
    }
}
