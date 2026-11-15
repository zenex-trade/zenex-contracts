//! The stored [`MarketData`] record, its [`SidePair`] and [`SkewSplit`]
//! primitives, and its pure book measures.

use crate::dependencies::PriceData;
use crate::engine::constants::SCALAR_18;
use crate::engine::math;
use soroban_fixed_point_math::SorobanFixedPoint;
use soroban_sdk::{contracttype, Env};

/// Market state, stored in its own persistent entry.
///
/// The pool surplus is `credit_pool - credit_owed`.
#[derive(Clone, Debug, Default)]
#[contracttype]
pub struct MarketData {
    /// Open interest per side (token-dec).
    pub notional: SidePair,
    /// Posted margin per side (token-dec).
    pub margin: SidePair,
    /// Base size per side, `sum(notional / entry)` (base-dec).
    pub tokens: SidePair,
    /// Cumulative funding index per side (`SCALAR_18`).
    pub funding_idx: SidePair,
    /// Cumulative borrowing index per side (`SCALAR_18`).
    pub borrowing_idx: SidePair,
    /// Borrowing checkpoint weight per side: the sum over open positions of
    /// `notional * borrowing_idx` at each position's last checkpoint
    /// (token-dec times `SCALAR_18`, exact).
    pub borrowing_weight: SidePair,
    /// Signed funding rate (`SCALAR_18`, per second). Positive means longs
    /// pay.
    pub funding_rate: i128,
    /// The last accrual timestamp (seconds). Both indices share it.
    pub accrued_at: u64,
    /// The internal claimable-credit pool, including parked failed payouts
    /// (token-dec).
    pub credit_pool: i128,
    /// Total funding and parked failed payouts owed to traders (token-dec).
    pub credit_owed: i128,
}

/// A long and short pair of `i128` values.
#[derive(Clone, Debug, Default)]
#[contracttype]
pub struct SidePair {
    pub long: i128,
    pub short: i128,
}

/// A token change split by its effect on market skew.
///
/// The parts are non-negative (base-dec) and sum to the change's
/// absolute value.
#[derive(Clone, Debug)]
pub struct SkewSplit {
    /// The part that moves `|imbalance|` up (base-dec).
    pub worsening: i128,
    /// The part that moves `|imbalance|` down (base-dec).
    pub improving: i128,
}

impl SidePair {
    /// Returns the value on `is_long`'s side.
    pub fn get(&self, is_long: bool) -> i128 {
        if is_long {
            self.long
        } else {
            self.short
        }
    }

    /// Adds the signed `delta` to `is_long`'s side.
    pub fn add(&mut self, is_long: bool, delta: i128) {
        if is_long {
            self.long += delta;
        } else {
            self.short += delta;
        }
    }

    /// Returns the sum of both sides.
    pub fn total(&self) -> i128 {
        self.long + self.short
    }
}

impl MarketData {
    /// Returns the borrowing that both sides accrued but no position has
    /// settled yet (token-dec). `maximize` marks it trader-best when true,
    /// rounded down, and trader-worst when false, rounded up.
    ///
    /// Per side it is `notional * borrowing_idx - borrowing_weight`, exact
    /// until the one rounding of the sum.
    pub fn pending_borrowing(&self, e: &Env, maximize: bool) -> i128 {
        let mut pending = 0;
        for is_long in [true, false] {
            let notional = self.notional.get(is_long);
            let weight = self.borrowing_weight.get(is_long);
            // An empty side has no checkpoints left in its weight.
            debug_assert!(notional != 0 || weight == 0);
            // The release profile sets `overflow-checks`, so an overflowing
            // product traps instead of wrapping.
            let side = notional * self.borrowing_idx.get(is_long) - weight;
            // Each checkpoint index is at most the side's current index,
            // which only grows, so the exact difference is non-negative.
            debug_assert!(side >= 0);
            pending += side.max(0);
        }
        // The sum carries the index's `SCALAR_18`.
        if maximize {
            pending.fixed_mul_floor(e, &1, &SCALAR_18)
        } else {
            pending.fixed_mul_ceil(e, &1, &SCALAR_18)
        }
    }

    /// Returns the reserved value that backs `is_long`'s open positions
    /// (token-dec), rounded up.
    pub fn side_reserved(&self, e: &Env, price: &PriceData, is_long: bool) -> i128 {
        if is_long {
            // Ask-marked and rounded up: overstating is conservative for
            // the caps.
            math::to_notional_ceil(e, self.tokens.long, price.ask)
        } else {
            // A short's payout is bounded by its entry notional.
            self.notional.short
        }
    }

    /// Returns the signed pending PnL of `is_long`'s side (token-dec).
    /// `maximize` marks it trader-best when true, trader-worst when false.
    pub fn side_pnl(&self, e: &Env, price: &PriceData, is_long: bool, maximize: bool) -> i128 {
        let pnl = match (is_long, maximize) {
            // Maximized long: the ask marks its value highest.
            (true, true) => {
                math::to_notional_ceil(e, self.tokens.long, price.ask) - self.notional.long
            }
            // Minimized long: the bid marks its value lowest.
            (true, false) => {
                math::to_notional_floor(e, self.tokens.long, price.bid) - self.notional.long
            }
            // Maximized short: the bid closes it at the lowest cost.
            (false, true) => {
                self.notional.short - math::to_notional_floor(e, self.tokens.short, price.bid)
            }
            // Minimized short: the ask closes it at the highest cost.
            (false, false) => {
                self.notional.short - math::to_notional_ceil(e, self.tokens.short, price.ask)
            }
        };
        // A paper loss beyond the side's posted margin cannot realize.
        pnl.max(-self.margin.get(is_long))
    }

    /// Splits a signed token change on `is_long`'s side by its effect on
    /// skew.
    ///
    /// `signed_tokens` is the side's signed base-size change (base-dec).
    /// Positive is an increase, negative a decrease. Always
    /// `worsening + improving == signed_tokens.abs()`.
    pub fn skew_split(&self, is_long: bool, signed_tokens: i128) -> SkewSplit {
        // The signed token imbalance (base-dec). Positive means longs are
        // dominant. A pure price move never shifts it.
        let imb_pre = self.tokens.long - self.tokens.short;
        // A long change adds to the imbalance. A short change subtracts.
        let imb_post = if is_long {
            imb_pre + signed_tokens
        } else {
            imb_pre - signed_tokens
        };

        // The sign flipped: part of the change closed the old imbalance,
        // the rest opened a new one on the other side. Exact zero at
        // either end is not a flip. The one-way branch below takes it.
        if imb_pre != 0 && imb_post != 0 && imb_pre.signum() != imb_post.signum() {
            return SkewSplit {
                // The part that opened the new imbalance.
                worsening: imb_post.abs(),
                // The part that closed the old imbalance to zero.
                improving: imb_pre.abs(),
            };
        }

        // No flip: the whole change moves one way.
        if imb_post.abs() > imb_pre.abs() {
            SkewSplit {
                worsening: signed_tokens.abs(),
                improving: 0,
            }
        } else {
            SkewSplit {
                worsening: 0,
                improving: signed_tokens.abs(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::constants::{BASE_UNIT, MAX_OPEN_INTEREST, PRICE_SCALAR};

    /// Returns a `MarketData` that carries only per-side base tokens.
    /// Everything else is zero.
    fn market_with(long_tokens: i128, short_tokens: i128) -> MarketData {
        MarketData {
            tokens: SidePair {
                long: long_tokens,
                short: short_tokens,
            },
            ..Default::default()
        }
    }

    /// Asserts a split and its sum invariant, `worsening + improving == |delta|`.
    fn assert_split(split: &SkewSplit, worsening: i128, improving: i128, delta: i128) {
        assert_eq!(split.worsening, worsening, "worsening leg");
        assert_eq!(split.improving, improving, "improving leg");
        assert_eq!(
            split.worsening + split.improving,
            delta.abs(),
            "sum invariant"
        );
    }

    // Opening into a balanced book (imbalance 0 -> +100) creates skew from nothing,
    // so the whole delta worsens.
    #[test]
    fn skew_split_open_into_balanced_is_fully_worsening() {
        let split = market_with(0, 0).skew_split(true, 100);
        assert_split(&split, 100, 0, 100);
    }

    // Longs dominant (+100), adding 50 more long pushes to +150: further from
    // balance, entirely worsening.
    #[test]
    fn skew_split_increase_dominant_side_fully_worsens() {
        let split = market_with(200, 100).skew_split(true, 50);
        assert_split(&split, 50, 0, 50);
    }

    // Longs dominant (+100), trimming 50 long moves to +50: toward balance but
    // still long-dominant, entirely improving.
    #[test]
    fn skew_split_decrease_dominant_side_fully_improves() {
        let split = market_with(200, 100).skew_split(true, -50);
        assert_split(&split, 0, 50, -50);
    }

    // Longs dominant (+100), closing 150 long flips to -50. The run to zero
    // (100) improves. The overshoot (50) worsens.
    #[test]
    fn skew_split_decrease_crossing_balance_splits_both_legs() {
        let split = market_with(200, 100).skew_split(true, -150);
        assert_split(&split, 50, 100, -150);
    }

    // Longs dominant (+100), adding 50 short moves imbalance to +50: toward
    // balance via the opposite side, entirely improving.
    #[test]
    fn skew_split_increase_opposite_side_fully_improves() {
        let split = market_with(200, 100).skew_split(false, 50);
        assert_split(&split, 0, 50, 50);
    }

    // Longs dominant (+100, short 0), adding 150 short flips imbalance to -50.
    // The run to zero (100) improves. The overshoot (50) worsens.
    #[test]
    fn skew_split_increase_opposite_side_crossing_splits_both_legs() {
        let split = market_with(100, 0).skew_split(false, 150);
        assert_split(&split, 50, 100, 150);
    }

    // Shorts dominant (-100), adding 50 short pushes to -150: further from
    // balance, entirely worsening.
    #[test]
    fn skew_split_shorts_dominant_increase_fully_worsens() {
        let split = market_with(100, 200).skew_split(false, 50);
        assert_split(&split, 50, 0, 50);
    }

    // Longs dominant (+100), closing exactly 100 long lands on imbalance 0. The
    // whole leg moves to balance with no overshoot, so it is entirely
    // improving. The flip branch needs imb_post != 0, which this does not
    // satisfy.
    #[test]
    fn skew_split_close_exactly_to_balance_fully_improves() {
        let split = market_with(200, 100).skew_split(true, -100);
        assert_split(&split, 0, 100, -100);
    }

    // A balanced but non-empty book (imbalance 0) behaves like opening from zero:
    // adding 50 long creates skew, entirely worsening.
    #[test]
    fn skew_split_from_balanced_nonempty_book_fully_worsens() {
        let split = market_with(100, 100).skew_split(true, 50);
        assert_split(&split, 50, 0, 50);
    }

    // A margin-only order (delta 0) leaves skew untouched: both legs zero.
    #[test]
    fn skew_split_zero_delta_is_zero() {
        let split = market_with(200, 100).skew_split(true, 0);
        assert_split(&split, 0, 0, 0);
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

    // Per side: the long reserve is its ask-marked tokens (2 * $11 = 22.0),
    // the short reserve its entry notional (5.0).
    #[test]
    fn side_reserved_marks_longs_at_ask_and_shorts_at_notional() {
        let e = Env::default();
        let (data, price) = reserve_fixture();
        assert_eq!(data.side_reserved(&e, &price, true), 22_0000000);
        assert_eq!(data.side_reserved(&e, &price, false), 5_0000000);
    }

    // PnL fixture: spread-straddling entries. A long of 2 base units at entry
    // 20.0 with 10.0 margin, and a short of 0.5 base units at entry 5.0 with
    // 3.0 margin. Bid 9 and ask 11 sit around the $10 mid.
    fn pnl_fixture() -> (MarketData, PriceData) {
        let data = MarketData {
            notional: SidePair {
                long: 20_0000000,
                short: 5_0000000,
            },
            margin: SidePair {
                long: 10_0000000,
                short: 3_0000000,
            },
            tokens: SidePair {
                long: 2 * BASE_UNIT,
                short: BASE_UNIT / 2,
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

    // Profit marking (`maximize` true): the long values at the ask
    // (2 * 11 = 22.0 -> +2.0), the short at the bid (0.5 * 9 = 4.5 -> +0.5).
    // Both sides read as winners.
    #[test]
    fn side_pnl_marks_profit_at_adverse_prices() {
        let e = Env::default();
        let (data, price) = pnl_fixture();
        assert_eq!(data.side_pnl(&e, &price, true, true), 2_0000000);
        assert_eq!(data.side_pnl(&e, &price, false, true), 5000000);
    }

    // Loss marking (`maximize` false) mirrors it: the long values at the bid
    // (18.0 -> -2.0), the short at the ask (5.5 -> -0.5).
    #[test]
    fn side_pnl_marks_loss_at_adverse_prices() {
        let e = Env::default();
        let (data, price) = pnl_fixture();
        assert_eq!(data.side_pnl(&e, &price, true, false), -2_0000000);
        assert_eq!(data.side_pnl(&e, &price, false, false), -5000000);
    }

    // A side's loss floors at its posted margin: with the long's
    // margin cut to 1.0, its 2.0 marked loss reads -1.0.
    #[test]
    fn side_pnl_floors_loss_at_side_margin() {
        let e = Env::default();
        let (mut data, price) = pnl_fixture();
        data.margin.long = 1_0000000;
        assert_eq!(data.side_pnl(&e, &price, true, false), -1_0000000);
    }

    // The two sides sum exactly and round once. The long side holds 1,000.0
    // checkpointed at index 0.001 and 333.3333333 at 0.001500000000000333.
    // Marked at 0.002718281828459045, it owes 2.1243757712... The short side
    // holds 5.0 checkpointed at 0.01. Marked at 0.03000001, it owes exactly
    // 0.10000005. The 2.2243758212... total floors to 2.2243758 and ceils to
    // 2.2243759. Flooring each side apart would read 2.2243757.
    #[test]
    fn pending_borrowing_rounds_the_two_sides_once() {
        let e = Env::default();
        let data = MarketData {
            notional: SidePair {
                long: 1333_3333333,
                short: 5_0000000,
            },
            borrowing_idx: SidePair {
                long: 2_718_281_828_459_045,
                short: 30_000_010_000_000_000,
            },
            borrowing_weight: SidePair {
                long: 1000_0000000 * 1_000_000_000_000_000 + 333_3333333 * 1_500_000_000_000_333,
                short: 5_0000000 * (SCALAR_18 / 100),
            },
            ..Default::default()
        };
        assert_eq!(data.pending_borrowing(&e, true), 2_2243758);
        assert_eq!(data.pending_borrowing(&e, false), 2_2243759);
    }

    // A whole-unit total reads the same under both marks: 100.0
    // checkpointed at index 0.01 and marked at 0.05 owes exactly 4.0. A side
    // checkpointed at its current index, and an empty book, owe nothing.
    #[test]
    fn pending_borrowing_is_exact_on_a_whole_unit_total() {
        let e = Env::default();
        let mut data = MarketData {
            notional: SidePair {
                long: 100_0000000,
                short: 0,
            },
            borrowing_idx: SidePair {
                long: 5 * SCALAR_18 / 100,
                short: 0,
            },
            borrowing_weight: SidePair {
                long: 100_0000000 * (SCALAR_18 / 100),
                short: 0,
            },
            ..Default::default()
        };
        assert_eq!(data.pending_borrowing(&e, true), 4_0000000);
        assert_eq!(data.pending_borrowing(&e, false), 4_0000000);

        data.borrowing_weight.long = data.notional.long * data.borrowing_idx.long;
        assert_eq!(data.pending_borrowing(&e, true), 0);
        assert_eq!(data.pending_borrowing(&e, false), 0);

        assert_eq!(MarketData::default().pending_borrowing(&e, false), 0);
    }

    // At `MAX_OPEN_INTEREST` on both sides, the exact sum fits `i128` up to
    // a summed index of 170, the headroom the constant's doc states. Each
    // side here is marked at index 85 with nothing settled. A summed index
    // of 171 would not fit.
    #[test]
    fn pending_borrowing_fits_at_the_open_interest_bound() {
        let e = Env::default();
        let data = MarketData {
            notional: SidePair {
                long: MAX_OPEN_INTEREST,
                short: MAX_OPEN_INTEREST,
            },
            borrowing_idx: SidePair {
                long: 85 * SCALAR_18,
                short: 85 * SCALAR_18,
            },
            ..Default::default()
        };
        assert_eq!(data.pending_borrowing(&e, true), 170 * MAX_OPEN_INTEREST);
        assert!(MAX_OPEN_INTEREST.checked_mul(171 * SCALAR_18).is_none());
    }
}
