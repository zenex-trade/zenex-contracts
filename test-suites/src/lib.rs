//! Cross-crate integration fixtures for the Zenex trading protocol.
//!
//! [`test_fixture::TestFixture`] deploys a real market + strategy-vault pair
//! from compiled WASM, including the factory, oracle, signed-report verifier,
//! and treasury, with per-invocation resource limits.

pub mod assertions;
pub mod carry;
pub mod chainlink_helper;
pub mod constants;
pub mod dependencies;
pub mod mock_chainlink;
pub mod resources;
pub mod setup;
pub mod test_fixture;
pub mod testnet_profile;
mod token;

pub use constants::{SCALAR_18, SCALAR_7};

/// Copy a `market::Config` into the identically-encoded `factory::Config`.
///
/// The two structs share their XDR layout but are distinct Rust types. The
/// factory's `deploy` takes its own type, so the fixture converts the market
/// config it hands to callers into the factory's before deploying.
pub fn to_factory_config(config: &market::Config) -> factory::Config {
    factory::Config {
        keeper_rate: config.keeper_rate,
        min_position_notional: config.min_position_notional,
        max_position_notional: config.max_position_notional,
        max_open_interest: config.max_open_interest,
        min_order_notional: config.min_order_notional,
        min_order_margin: config.min_order_margin,
        exec_fee: config.exec_fee,
        fee_dom: config.fee_dom,
        fee_non_dom: config.fee_non_dom,
        impact_scalar: config.impact_scalar,
        max_util_open: config.max_util_open,
        max_util_withdraw: config.max_util_withdraw,
        init_margin: config.init_margin,
        maintenance_margin: config.maintenance_margin,
        liq_fee: config.liq_fee,
        notional_lock: config.notional_lock,
        target_util: config.target_util,
        borrow_rate: config.borrow_rate,
        increased_borrow_rate: config.increased_borrow_rate,
        funding_increase: config.funding_increase,
        funding_decrease: config.funding_decrease,
        threshold_stable_funding: config.threshold_stable_funding,
        threshold_decrease_funding: config.threshold_decrease_funding,
        funding_min: config.funding_min,
        funding_max: config.funding_max,
        adl_max_pnl: config.adl_max_pnl,
        adl_clear_target: config.adl_clear_target,
        max_pnl_trader: config.max_pnl_trader,
        max_pnl_withdraw: config.max_pnl_withdraw,
        redeem_lock: config.redeem_lock,
        deposit_fee: config.deposit_fee,
        redeem_fee: config.redeem_fee,
        min_deposit: config.min_deposit,
        max_vault_balance: config.max_vault_balance,
    }
}

/// A valid baseline `market::Config` carrying the deployed testnet pair's
/// carry curves, known to pass `Config::check_valid`.
///
/// Ratio fields are SCALAR_18 fractions. Notional, order, and fee amounts are
/// token-dec (7-dec settlement token). Borrowing and funding rates are
/// SCALAR_18 per second.
pub fn default_config() -> market::Config {
    market::Config {
        keeper_rate: SCALAR_18 / 10, // 10%
        min_position_notional: 10_000_000,
        max_position_notional: 1_000_000_000_000,
        max_open_interest: 10_000_000_000_000,
        min_order_notional: 1_000_000,
        min_order_margin: 1_000_000,
        exec_fee: 100_000,
        fee_dom: SCALAR_18 / 200,              // 0.5%
        fee_non_dom: 3 * SCALAR_18 / 1000,     // 0.3%
        impact_scalar: 100_000_000_000_000,    // fee rate = notional / $10M, capped at 10%
        max_util_open: 8 * SCALAR_18 / 10,     // 80%
        max_util_withdraw: 9 * SCALAR_18 / 10, // 90%
        init_margin: SCALAR_18 / 10,           // 10%
        maintenance_margin: SCALAR_18 / 20,    // 5%
        liq_fee: SCALAR_18 / 100,              // 1%
        notional_lock: 30,
        target_util: 8 * SCALAR_18 / 10, // 80%
        // The carry curves are the deployed testnet pair's, so the suite runs
        // the borrowing and funding the market actually charges. Each rate is
        // SCALAR_18 per second. The yearly figures below divide it over
        // 31_536_000 seconds.
        borrow_rate: 15_854_895_992, // 50% a year at full utilization
        increased_borrow_rate: 15_854_895_992, // no kink: the rate stays linear above target
        funding_increase: 885_267,   // full skew reaches the cap in 2 hours
        funding_decrease: 36_885,    // a capped rate decays to zero in 2 days
        threshold_stable_funding: 4 * SCALAR_18 / 100, // 4% skew
        threshold_decrease_funding: 0,
        funding_min: 317_097_920,   // 1% a year floor on the charged rate
        funding_max: 6_373_921_868, // 20% a year cap
        adl_max_pnl: SCALAR_18 / 2, // 50%
        adl_clear_target: 4 * SCALAR_18 / 10, // 40%
        max_pnl_trader: 9 * SCALAR_18 / 10, // 90%
        max_pnl_withdraw: 15 * SCALAR_18 / 100, // 15%
        redeem_lock: 0,
        deposit_fee: SCALAR_18 / 1000, // 10 bps
        redeem_fee: SCALAR_18 / 1000,  // 10 bps
        min_deposit: 1_000_000,
        max_vault_balance: 10_000_000_000_000,
    }
}

/// The broad stress config.
/// It starts from `zenex-ops/deployments/testnet/2026-09-05-testnet-v2`, with
/// the deliberate sizing and margin changes below. It is neither a literal
/// deployment record nor an approved release parameter set. Tests of the recorded economics
/// instead read [`testnet_profile::TestnetProfile`] and seed real LP shares.
///
/// Ratio fields are `SCALAR_18` fractions. Notional, order, deposit, and fee
/// amounts are token-dec against the 7-decimal settlement token. The rates
/// are per second: `borrow_rate` is 50% APR, `funding_min` 1% APR, and
/// `funding_max` 20% APR.
///
/// # Divergence from the record
/// Two categories diverge, and nothing else. Apart from the margin ladder,
/// the other economic parameters are copied exactly: the borrowing and
/// funding curve, the
/// ADL and profit band, the fee rates, the utilization ratios, the timing
/// locks, and the dust floors.
///
/// - The sizing limits are 1000x the record, which keeps the record's
///   `1 : 2 : 4` proportion between `max_position_notional`,
///   `max_open_interest`, and `max_vault_balance`. The deployment sizes a
///   first market at $25,000 per position against a $100,000 vault. The
///   fixture seeds $10,000,000 so tests are never liquidity-constrained,
///   and the sizing limits rise only to admit that seeding. The deployed
///   limits are not wrong, they are small.
/// - The leverage ladder opens up to the intended 100x maximum:
///   `init_margin` is 1% instead of the record's 5%, and
///   `maintenance_margin` and `liq_fee` scale down with it at the record's
///   `5 : 2 : 0.5` proportion. `liq_fee` is a risk rate, and it moves only
///   because `check_valid` requires
///   `liq_fee < maintenance_margin < init_margin`.
///
/// Refer to [`crate::test_fixture::StackDials::deployed`] for the oracle,
/// treasury, and vault dials the same record carries outside this struct.
pub fn deployed_config() -> market::Config {
    market::Config {
        keeper_rate: 100_000_000_000_000_000,       // 10%
        min_position_notional: 10_000_000,          // $1
        max_position_notional: 250_000_000_000_000, // $25M, 1000x the record
        max_open_interest: 500_000_000_000_000,     // $50M, 1000x the record
        min_order_notional: 10_000_000,             // $1
        min_order_margin: 10_000_000,               // $1
        exec_fee: 100_000,                          // $0.01
        fee_dom: 600_000_000_000_000,               // 0.06%
        fee_non_dom: 400_000_000_000_000,           // 0.04%
        impact_scalar: 100_000_000_000_000,         // fee rate = notional / $10M
        max_util_open: 800_000_000_000_000_000,     // 80%
        max_util_withdraw: 900_000_000_000_000_000, // 90%
        init_margin: 10_000_000_000_000_000,        // 1%, the 100x ladder
        maintenance_margin: 4_000_000_000_000_000,  // 0.4%
        liq_fee: 1_000_000_000_000_000,             // 0.1%
        notional_lock: 30,                          // seconds
        target_util: 800_000_000_000_000_000,       // 80% of the reserve capacity
        borrow_rate: 15_854_895_992,                // 50% APR per second
        increased_borrow_rate: 15_854_895_992,      // 50% APR, so the kink is flat
        funding_increase: 885_267,
        funding_decrease: 36_885,
        threshold_stable_funding: 40_000_000_000_000_000, // 4% token skew
        threshold_decrease_funding: 0,
        funding_min: 317_097_920,                  // 1% APR per second
        funding_max: 6_373_921_868,                // 20% APR per second
        adl_max_pnl: 550_000_000_000_000_000,      // 55%
        adl_clear_target: 400_000_000_000_000_000, // 40%
        max_pnl_trader: 900_000_000_000_000_000,   // 90%
        max_pnl_withdraw: 150_000_000_000_000_000, // 15%
        redeem_lock: 3_600,                        // seconds
        deposit_fee: 1_000_000_000_000_000,        // 0.1%
        redeem_fee: 2_500_000_000_000_000,         // 0.25%
        min_deposit: 10_000_000,                   // $1
        max_vault_balance: 1_000_000_000_000_000,  // $100M, 1000x the record
    }
}
