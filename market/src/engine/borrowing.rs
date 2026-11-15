//! The borrowing kink model and its per-side accrual,
//! [`MarketData::accrue_borrowing`].

use crate::dependencies::PriceData;
use crate::engine::config::Config;
use crate::engine::constants::SCALAR_18;
use crate::engine::market_data::MarketData;
use crate::engine::math;
use soroban_fixed_point_math::SorobanFixedPoint;
use soroban_sdk::Env;

/// Returns the per-side reserve utilization (`SCALAR_18`), rounded up: the
/// share of the side's reservable capacity that backs its open positions,
/// `side_reserved / (max_util_open * vault_balance / 2)`, clamped to
/// `[0, 1]`. Each side reserves against its own half of the vault.
fn side_utilization(
    e: &Env,
    side_reserved: i128,
    max_util_open: i128,
    vault_balance: i128,
) -> i128 {
    // No reserved tokens means no utilization, even if the vault is empty.
    if side_reserved <= 0 {
        return 0;
    }
    // The capacity floors and the quotient rounds up. Both round toward a
    // higher rate.
    let capacity = math::half_factor(e, vault_balance, max_util_open);
    // Tokens reserved against zero capacity read as full utilization.
    if capacity == 0 {
        return SCALAR_18;
    }
    math::to_ratio_ceil(e, side_reserved, capacity).min(SCALAR_18)
}

/// Returns the borrowing rate per second at `utilization` under the kink
/// model (`SCALAR_18`), rounded up.
///
/// Below the kink the rate climbs linearly with utilization:
/// `borrow_rate * utilization`. Above `target_util` an additional slope
/// applies, `(increased_borrow_rate - borrow_rate) * excess / (1 - target_util)`,
/// so the rate reaches exactly `increased_borrow_rate` at full utilization.
///
/// # Arguments
/// - `utilization`: the reserve utilization in `[0, 1]` (`SCALAR_18`).
/// - `target_util`: the kink position in `[0, 1]` (`SCALAR_18`).
/// - `borrow_rate`: the linear leg's slope, which is its rate at full
///   utilization (`SCALAR_18`, per second).
/// - `increased_borrow_rate`: the total rate at full utilization
///   (`SCALAR_18`, per second).
fn borrowing_rate(
    e: &Env,
    utilization: i128,
    target_util: i128,
    borrow_rate: i128,
    increased_borrow_rate: i128,
) -> i128 {
    // The linear leg rounds up as a charge.
    let base = math::apply_factor_ceil(e, borrow_rate, utilization);
    if utilization <= target_util {
        return base;
    }
    let excess = utilization - target_util;
    let span = SCALAR_18 - target_util;
    let rate_gap = increased_borrow_rate - borrow_rate;
    // `excess / span` ramps 0 -> 1 across the band, rounded up as a charge.
    let additional = rate_gap.fixed_mul_ceil(e, &excess, &span);
    base + additional
}

impl MarketData {
    /// Advances the borrowing accrual by `elapsed` seconds at the marked
    /// per-side utilization. The caller owns the clock and stamps
    /// `accrued_at`.
    ///
    /// The dominant side pays. The side with strictly greater base `tokens`
    /// advances its `borrowing_idx` by the kink rate at its own utilization
    /// of `vault_balance`, the backing balance (token-dec). A token tie
    /// charges both sides.
    pub fn accrue_borrowing(
        &mut self,
        e: &Env,
        elapsed: i128,
        price: &PriceData,
        config: &Config,
        vault_balance: i128,
    ) {
        if elapsed == 0 {
            return;
        }

        for is_long in [true, false] {
            // Only the strictly smaller side skips, so a token tie charges both.
            if self.tokens.get(is_long) < self.tokens.get(!is_long) {
                continue;
            }
            let utilization = side_utilization(
                e,
                self.side_reserved(e, price, is_long),
                config.max_util_open,
                vault_balance,
            );
            let rate = borrowing_rate(
                e,
                utilization,
                config.target_util,
                config.borrow_rate,
                config.increased_borrow_rate,
            );
            // A per-second SCALAR_18 rate times a plain seconds count stays
            // SCALAR_18.
            self.borrowing_idx.add(is_long, rate * elapsed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::constants::{BASE_UNIT, PRICE_SCALAR};
    use crate::engine::market_data::SidePair;

    // 7-dec token amounts: a 1000-token vault at an 80% cap gives each side a
    // 400-token capacity over its half of the vault.
    const VAULT_BALANCE: i128 = 1000_0000000;
    const MAX_UTIL_OPEN: i128 = 8 * SCALAR_18 / 10;

    #[test]
    fn side_utilization_empty_reserve_is_zero() {
        let e = Env::default();
        assert_eq!(side_utilization(&e, 0, MAX_UTIL_OPEN, VAULT_BALANCE), 0);
    }

    // 200 reserved of the side's 400 capacity is exactly half.
    #[test]
    fn side_utilization_half_capacity() {
        let e = Env::default();
        assert_eq!(
            side_utilization(&e, 200_0000000, MAX_UTIL_OPEN, VAULT_BALANCE),
            SCALAR_18 / 2
        );
    }

    // 400 reserved fills the side's 400 capacity.
    #[test]
    fn side_utilization_full_capacity() {
        let e = Env::default();
        assert_eq!(
            side_utilization(&e, 400_0000000, MAX_UTIL_OPEN, VAULT_BALANCE),
            SCALAR_18
        );
    }

    // 500 reserved of a 400 capacity clamps to full.
    #[test]
    fn side_utilization_above_capacity_clamps_to_one() {
        let e = Env::default();
        assert_eq!(
            side_utilization(&e, 500_0000000, MAX_UTIL_OPEN, VAULT_BALANCE),
            SCALAR_18
        );
    }

    // Any reserve against an empty vault is fully utilized.
    #[test]
    fn side_utilization_zero_vault_with_reserve_is_one() {
        let e = Env::default();
        assert_eq!(side_utilization(&e, 1, MAX_UTIL_OPEN, 0), SCALAR_18);
    }

    // Half of a 6-token vault is a 3-token capacity: ceil(1 * 1e18 / 3) =
    // 333_333_333_333_333_334, the quotient rounds up.
    #[test]
    fn side_utilization_quotient_rounds_up() {
        let e = Env::default();
        assert_eq!(
            side_utilization(&e, 1, SCALAR_18, 6),
            333_333_333_333_333_334
        );
    }

    // floor(3 * 0.5) = 1 capacity on the 6-token vault's half, so 1 reserved
    // is full utilization. A rounded-up capacity of 2 would have read half.
    #[test]
    fn side_utilization_capacity_rounds_down() {
        let e = Env::default();
        assert_eq!(side_utilization(&e, 1, SCALAR_18 / 2, 6), SCALAR_18);
    }

    // A 7-token vault halves to a 3-token capacity (floor), so u = ceil(2/3)
    // = 666_666_666_666_666_667. An unfloored half of 3.5 would have read 4/7.
    #[test]
    fn side_utilization_halving_rounds_down() {
        let e = Env::default();
        assert_eq!(
            side_utilization(&e, 2, SCALAR_18, 7),
            666_666_666_666_666_667
        );
    }

    // Curve fixture: slope 3e-9/s, full-utilization rate 3e-8/s, kink at 80%.
    const BORROW_RATE: i128 = 3_000_000_000;
    const INCREASED_BORROW_RATE: i128 = 30_000_000_000;
    const TARGET_UTIL: i128 = 8 * SCALAR_18 / 10;

    #[test]
    fn borrowing_rate_zero_utilization_is_zero() {
        let e = Env::default();
        assert_eq!(
            borrowing_rate(&e, 0, TARGET_UTIL, BORROW_RATE, INCREASED_BORROW_RATE),
            0
        );
    }

    // Below the kink: rate = 3e9 * 0.5 = 1.5e9.
    #[test]
    fn borrowing_rate_below_kink_is_linear() {
        let e = Env::default();
        assert_eq!(
            borrowing_rate(
                &e,
                SCALAR_18 / 2,
                TARGET_UTIL,
                BORROW_RATE,
                INCREASED_BORROW_RATE
            ),
            1_500_000_000
        );
    }

    // At the kink exactly: still the linear branch, 3e9 * 0.8 = 2.4e9.
    #[test]
    fn borrowing_rate_at_kink_is_linear() {
        let e = Env::default();
        assert_eq!(
            borrowing_rate(
                &e,
                TARGET_UTIL,
                TARGET_UTIL,
                BORROW_RATE,
                INCREASED_BORROW_RATE
            ),
            2_400_000_000
        );
    }

    // Above the kink at u = 0.9: base 3e9 * 0.9 = 2.7e9, additional
    // (30e9 - 3e9) * 0.1 / 0.2 = 13.5e9, total 16.2e9.
    #[test]
    fn borrowing_rate_above_kink_adds_ramp() {
        let e = Env::default();
        assert_eq!(
            borrowing_rate(
                &e,
                9 * SCALAR_18 / 10,
                TARGET_UTIL,
                BORROW_RATE,
                INCREASED_BORROW_RATE
            ),
            16_200_000_000
        );
    }

    // At full utilization the rate is exactly `increased_borrow_rate`:
    // 3e9 + 27e9 * 0.2 / 0.2 = 30e9.
    #[test]
    fn borrowing_rate_full_utilization_hits_increased_rate() {
        let e = Env::default();
        assert_eq!(
            borrowing_rate(
                &e,
                SCALAR_18,
                TARGET_UTIL,
                BORROW_RATE,
                INCREASED_BORROW_RATE
            ),
            INCREASED_BORROW_RATE
        );
    }

    // A flat curve (increased == base slope's full-utilization value) has no
    // kink discontinuity: at u = 1 the rate is the slope itself.
    #[test]
    fn borrowing_rate_flat_curve_has_no_ramp() {
        let e = Env::default();
        assert_eq!(
            borrowing_rate(&e, SCALAR_18, TARGET_UTIL, BORROW_RATE, BORROW_RATE),
            BORROW_RATE
        );
    }

    // Reserve fixture: 2 long base units (2e7 base-dec at the 18-dec feed
    // scale) plus 5.0 short notional. Bid and ask straddle the $10 mid.
    fn reserve_fixture() -> (MarketData, PriceData) {
        let data = MarketData {
            notional: SidePair {
                long: 20_0000000,
                short: 5_0000000,
            },
            tokens: SidePair {
                long: 2 * BASE_UNIT,
                short: 0,
            },
            ..Default::default()
        };
        let price = PriceData {
            bid: 9 * PRICE_SCALAR,
            ask: 11 * PRICE_SCALAR,
            publish_time: 0,
        };
        (data, price)
    }

    // Accrual curve: slope 3e-9/s, full-utilization rate 3e-8/s, kink at 80%,
    // each side reserving up to 80% of its half of the vault.
    fn accrual_config() -> Config {
        Config {
            max_util_open: MAX_UTIL_OPEN,
            target_util: TARGET_UTIL,
            borrow_rate: BORROW_RATE,
            increased_borrow_rate: INCREASED_BORROW_RATE,
            ..crate::engine::config::test_config()
        }
    }

    // Long tokens dominate (2e7 vs 0). The long reserve ceil(2 * $11) = 22.0
    // overruns its side capacity floor(15.0 * 0.8) = 12.0 -> u = 1 -> rate =
    // increased (3e10/s). Over 100 s the long index advances 3e12 and the
    // short index stands still.
    #[test]
    fn accrue_borrowing_charges_only_the_dominant_long() {
        let e = Env::default();
        let (mut data, price) = reserve_fixture();
        data.accrue_borrowing(&e, 100, &price, &accrual_config(), 30_0000000);
        assert_eq!(data.borrowing_idx.long, 3_000_000_000_000);
        assert_eq!(data.borrowing_idx.short, 0);
    }

    // The long reserve 22.0 against side capacity floor(55.0 * 0.8) = 44.0 is
    // u = 0.5, below the 0.8 kink -> rate = 3e9 * 0.5 = 1.5e9/s. Over 50 s
    // the long index advances 7.5e10.
    #[test]
    fn accrue_borrowing_below_kink_uses_linear_rate() {
        let e = Env::default();
        let (mut data, price) = reserve_fixture();
        data.accrue_borrowing(&e, 50, &price, &accrual_config(), 110_0000000);
        assert_eq!(data.borrowing_idx.long, 75_000_000_000);
        assert_eq!(data.borrowing_idx.short, 0);
    }

    // Short tokens dominate (2e7 vs 0). The short reserve is its entry
    // notional 22.0 against side capacity 44.0 -> u = 0.5 -> rate 1.5e9/s.
    // Over 50 s the short index advances 7.5e10 and the long index stands
    // still.
    #[test]
    fn accrue_borrowing_charges_only_the_dominant_short() {
        let e = Env::default();
        let (mut data, price) = reserve_fixture();
        data.tokens = SidePair {
            long: 0,
            short: 2 * BASE_UNIT,
        };
        data.notional = SidePair {
            long: 0,
            short: 22_0000000,
        };
        data.accrue_borrowing(&e, 50, &price, &accrual_config(), 110_0000000);
        assert_eq!(data.borrowing_idx.long, 0);
        assert_eq!(data.borrowing_idx.short, 75_000_000_000);
    }

    // A token tie (1e7 each) charges both sides, each at its own
    // utilization: the long reserve ceil(1 * $11) = 11.0 gives u = 0.25 ->
    // rate 7.5e8/s, the short reserve 22.0 gives u = 0.5 -> rate 1.5e9/s.
    // Over 100 s the long index advances 7.5e10 and the short 1.5e11.
    #[test]
    fn accrue_borrowing_tie_charges_both_sides_at_own_rates() {
        let e = Env::default();
        let (mut data, price) = reserve_fixture();
        data.tokens = SidePair {
            long: BASE_UNIT,
            short: BASE_UNIT,
        };
        data.notional.short = 22_0000000;
        data.accrue_borrowing(&e, 100, &price, &accrual_config(), 110_0000000);
        assert_eq!(data.borrowing_idx.long, 75_000_000_000);
        assert_eq!(data.borrowing_idx.short, 150_000_000_000);
    }

    // An empty book ties at zero tokens: both sides run at zero utilization,
    // so neither index moves.
    #[test]
    fn accrue_borrowing_empty_book_moves_no_index() {
        let e = Env::default();
        let (_, price) = reserve_fixture();
        let mut data = MarketData::default();
        data.accrue_borrowing(&e, 100, &price, &accrual_config(), 30_0000000);
        assert_eq!(data.borrowing_idx.long, 0);
        assert_eq!(data.borrowing_idx.short, 0);
    }

    // A zero-elapsed re-accrual charges nothing.
    #[test]
    fn accrue_borrowing_zero_elapsed_is_noop() {
        let e = Env::default();
        let (mut data, price) = reserve_fixture();
        data.accrue_borrowing(&e, 0, &price, &accrual_config(), 30_0000000);
        assert_eq!(data.borrowing_idx.long, 0);
    }
}
