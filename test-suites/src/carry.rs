//! Hand-derived borrowing and funding charges, mirroring
//! `market::engine::borrowing` and `market::engine::funding`.
//!
//! The integration suites derive every expected amount from the config.
//! [`crate::default_config`] runs the deployed carry curves, so a position
//! held across a jump owes carry that the trade-fee math alone does not
//! account for. [`lone_side_carry`] covers the shape those suites build: one
//! accrual window over a one-sided book. Nothing here reads contract output.

use crate::constants::SCALAR_18;
use market::Config;

/// `ceil(amount * factor / SCALAR_18)`: a charge rounds up.
fn factor_ceil(amount: i128, factor: i128) -> i128 {
    (amount * factor + SCALAR_18 - 1) / SCALAR_18
}

/// `floor(amount * factor / SCALAR_18)`: a capacity rounds down.
fn factor_floor(amount: i128, factor: i128) -> i128 {
    amount * factor / SCALAR_18
}

/// The carry one position owes over an accrual window (token-dec). The two
/// legs settle to different places, so they stay separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Carry {
    /// The borrowing fee. It splits like a trade fee, except that the keeper
    /// takes no cut: the treasury takes its rate and the vault banks the rest.
    pub borrowing: i128,
    /// The funding charge. A one-sided book has no receiver, so it settles
    /// into the market's `credit_pool` and the market holds the tokens.
    pub funding: i128,
}

impl Carry {
    /// The whole charge debited from the position (token-dec).
    pub fn total(&self) -> i128 {
        self.borrowing + self.funding
    }
}

/// Returns the per-side reserve utilization of `reserve` against
/// `vault_balance` (`SCALAR_18`), clamped to `[0, 1]`.
///
/// Each side measures its reserve against its own half of the vault. The
/// capacity floors and the quotient ceils, so both round toward a higher rate.
fn utilization(config: &Config, reserve: i128, vault_balance: i128) -> i128 {
    if reserve <= 0 {
        return 0;
    }
    let capacity = factor_floor(vault_balance / 2, config.max_util_open);
    if capacity == 0 {
        return SCALAR_18;
    }
    ((reserve * SCALAR_18 + capacity - 1) / capacity).min(SCALAR_18)
}

/// Returns the borrowing index move over `seconds` for a side holding
/// `reserve` against a `vault_balance` backing (`SCALAR_18`).
///
/// Only the side with at least as many base tokens as the other accrues, so
/// the caller passes the dominant side's reserve. A long reserves its
/// ask-marked value at the accrual price, a short its entry notional.
///
/// # Arguments
/// - `config`: the market config the fixture deployed.
/// - `reserve`: the side's reserved value (token-dec).
/// - `vault_balance`: the vault's `total_assets` at the accrual (token-dec).
/// - `seconds`: the accrual window.
pub fn borrowing_index(config: &Config, reserve: i128, vault_balance: i128, seconds: i128) -> i128 {
    let util = utilization(config, reserve, vault_balance);
    let base = factor_ceil(config.borrow_rate, util);
    let rate = if util <= config.target_util {
        base
    } else {
        let excess = util - config.target_util;
        let span = SCALAR_18 - config.target_util;
        let gap = config.increased_borrow_rate - config.borrow_rate;
        base + (gap * excess + span - 1) / span
    };
    rate * seconds
}

/// Returns the funding index move over `seconds` on a book whose open
/// interest sits entirely on one side (`SCALAR_18`).
///
/// One side means a token skew of 1, so the rate ramps at the full
/// `funding_increase` and the whole window charges at the evolved rate. The
/// charged magnitude is floored at `funding_min` and capped at `funding_max`.
/// The caller must hold the book one-sided and the saved rate at zero for the
/// whole window, which one `jump` between an open and a close gives.
pub fn funding_index(config: &Config, seconds: i128) -> i128 {
    let ramped = (config.funding_increase * seconds).min(config.funding_max);
    ramped.max(config.funding_min) * seconds
}

/// Returns the carry a position of `notional` owes over `seconds` on a
/// one-sided book (token-dec).
///
/// `reserve` is the whole side's reserved value, which equals `notional` only
/// when the position is alone on its side. Both legs round up: a payer never
/// underpays.
///
/// # Arguments
/// - `config`: the market config the fixture deployed.
/// - `notional`: the position's own notional (token-dec).
/// - `reserve`: the side's reserved value at the accrual price (token-dec).
/// - `vault_balance`: the vault's `total_assets` at the accrual (token-dec).
/// - `seconds`: the accrual window.
pub fn lone_side_carry(
    config: &Config,
    notional: i128,
    reserve: i128,
    vault_balance: i128,
    seconds: i128,
) -> Carry {
    Carry {
        borrowing: factor_ceil(
            notional,
            borrowing_index(config, reserve, vault_balance, seconds),
        ),
        funding: factor_ceil(notional, funding_index(config, seconds)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::default_config;

    // A $1,000 long alone against a $10M vault reserves its own notional at a
    // flat mark: utilization is 1e10 / floor(80% * 5e13) = 2.5e-4 of capacity.
    #[test]
    fn utilization_measures_the_reserve_against_half_the_vault() {
        let config = default_config();
        assert_eq!(
            utilization(&config, 10_000_000_000, 100_000_000_000_000),
            250_000_000_000_000
        );
    }

    // An empty side reserves nothing, so it owes nothing.
    #[test]
    fn utilization_of_an_empty_side_is_zero() {
        assert_eq!(utilization(&default_config(), 0, 100_000_000_000_000), 0);
    }

    // `default_config` sets `increased_borrow_rate` equal to `borrow_rate`,
    // so a utilization past the kink adds nothing: the rate at full
    // utilization is `borrow_rate` itself.
    #[test]
    fn borrowing_index_is_flat_past_the_kink_on_the_deployed_curve() {
        let config = default_config();
        let balance = 100_000_000_000_000;
        let full = factor_floor(balance / 2, config.max_util_open);
        assert_eq!(
            borrowing_index(&config, full, balance, 1),
            config.borrow_rate
        );
    }

    // Over a short window the ramp stays under `funding_min`, so the floor
    // sets the charged rate: 35 seconds charge 35 * funding_min.
    #[test]
    fn funding_index_charges_the_floor_over_a_short_window() {
        let config = default_config();
        assert_eq!(funding_index(&config, 35), config.funding_min * 35);
    }

    // Over a long window the ramp saturates at `funding_max`: a week of full
    // skew accelerates far past the cap.
    #[test]
    fn funding_index_saturates_at_the_cap_over_a_long_window() {
        let config = default_config();
        assert_eq!(
            funding_index(&config, 604_800),
            config.funding_max * 604_800
        );
    }
}
