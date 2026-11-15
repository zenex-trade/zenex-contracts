//! Fixed-point helpers over `SCALAR_18`.
//!
//! Token amounts stay in the settlement token's decimals (token-dec).
//! Base-dec is the `tokens` scale: `tokens = notional * SCALAR_18 / price`,
//! and `tokens * price / SCALAR_18` returns token-dec. [`prorate`] covers
//! proportional splits with an amount-valued denominator.

use crate::dependencies::PriceData;
use crate::engine::constants::{MAX_IMPACT_RATE, SCALAR_18};
use soroban_fixed_point_math::SorobanFixedPoint;
use soroban_sdk::Env;

/// Returns the value of `tokens` (base-dec) marked at `price`, rounded down:
/// `floor(tokens * price / SCALAR_18)` (token-dec).
pub(crate) fn to_notional_floor(e: &Env, tokens: i128, price: i128) -> i128 {
    tokens.fixed_mul_floor(e, &price, &SCALAR_18)
}

/// Returns the value of `tokens` (base-dec) marked at `price`, rounded up:
/// `ceil(tokens * price / SCALAR_18)` (token-dec).
pub(crate) fn to_notional_ceil(e: &Env, tokens: i128, price: i128) -> i128 {
    tokens.fixed_mul_ceil(e, &price, &SCALAR_18)
}

/// Returns the base size `notional` buys at `price`, rounded down:
/// `floor(notional * SCALAR_18 / price)` (base-dec).
pub(crate) fn to_tokens_floor(e: &Env, notional: i128, price: i128) -> i128 {
    notional.fixed_div_floor(e, &price, &SCALAR_18)
}

/// Returns the base size `notional` buys at `price`, rounded up:
/// `ceil(notional * SCALAR_18 / price)` (base-dec).
pub(crate) fn to_tokens_ceil(e: &Env, notional: i128, price: i128) -> i128 {
    notional.fixed_div_ceil(e, &price, &SCALAR_18)
}

/// Returns `amount` scaled by an 18-dec `factor`, rounded down:
/// `floor(amount * factor / SCALAR_18)`, in the unit of `amount`. Used for a
/// cut or a cap.
pub(crate) fn apply_factor_floor(e: &Env, amount: i128, factor: i128) -> i128 {
    amount.fixed_mul_floor(e, &factor, &SCALAR_18)
}

/// Returns `amount` scaled by an 18-dec `factor`, rounded up:
/// `ceil(amount * factor / SCALAR_18)`, in the unit of `amount`. Used for a
/// charge.
pub(crate) fn apply_factor_ceil(e: &Env, amount: i128, factor: i128) -> i128 {
    amount.fixed_mul_ceil(e, &factor, &SCALAR_18)
}

/// Returns an 18-dec `factor` of half `balance`, rounded down:
/// `floor((balance / 2) * factor / SCALAR_18)` (token-dec).
///
/// Each side of the book measures against its own half of `balance`, so
/// the two sides sum to at most `factor * balance`.
pub(crate) fn half_factor(e: &Env, balance: i128, factor: i128) -> i128 {
    apply_factor_floor(e, balance / 2, factor)
}

/// Returns the size-quadratic impact fee on a fill's `notional` (token-dec):
/// `min(ceil(notional^2 / impact_scalar), ceil(notional * MAX_IMPACT_RATE / SCALAR_18))`.
pub(crate) fn impact_fee(e: &Env, notional: i128, impact_scalar: i128) -> i128 {
    let fee = notional.fixed_mul_ceil(e, &notional, &impact_scalar);
    fee.min(apply_factor_ceil(e, notional, MAX_IMPACT_RATE))
}

/// Returns the ratio of two like-unit amounts as an 18-dec fraction, rounded
/// down: `floor(numerator * SCALAR_18 / denominator)`.
pub(crate) fn to_ratio_floor(e: &Env, numerator: i128, denominator: i128) -> i128 {
    numerator.fixed_div_floor(e, &denominator, &SCALAR_18)
}

/// Returns the ratio of two like-unit amounts as an 18-dec fraction, rounded
/// up: `ceil(numerator * SCALAR_18 / denominator)`.
pub(crate) fn to_ratio_ceil(e: &Env, numerator: i128, denominator: i128) -> i128 {
    numerator.fixed_div_ceil(e, &denominator, &SCALAR_18)
}

/// Returns the signed PnL of `tokens` (base-dec) against `notional`, marked
/// at the adverse close side `price.exit(is_long)` (token-dec). A long earns
/// `value - notional`, a short `notional - value`. The marked value rounds
/// against the trader, floor for a long and ceil for a short, so the PnL is
/// conservative for the vault.
pub(crate) fn pnl(e: &Env, tokens: i128, notional: i128, price: &PriceData, is_long: bool) -> i128 {
    let exit_price = price.exit(is_long);
    if is_long {
        to_notional_floor(e, tokens, exit_price) - notional
    } else {
        notional - to_notional_ceil(e, tokens, exit_price)
    }
}

/// Returns the pro-rata share of `amount` for `part` of `whole`, rounded
/// down: `floor(amount * part / whole)`, in the unit of `amount`.
pub(crate) fn prorate(e: &Env, amount: i128, part: i128, whole: i128) -> i128 {
    amount.fixed_mul_floor(e, &part, &whole)
}

/// Returns the cost accrued on `notional` over a cumulative-index move
/// `index_delta`: `ceil(notional * index_delta / SCALAR_18)` (token-dec). It
/// rounds up, so a payer never underpays and a receiver never over-claims.
pub(crate) fn accrued_amount(e: &Env, notional: i128, index_delta: i128) -> i128 {
    notional.fixed_mul_ceil(e, &index_delta, &SCALAR_18)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::constants::PRICE_SCALAR;

    const IMPACT_SCALAR: i128 = 1_000_000_000_000; // $100k, 7-dec

    // 100.0 notional: 1e9^2 / 1e12 = 1e6 (a 0.1% rate), far under the cap.
    #[test]
    fn impact_fee_is_quadratic_below_the_cap() {
        let e = Env::default();
        assert_eq!(impact_fee(&e, 1_000_000_000, IMPACT_SCALAR), 1_000_000);
    }

    // The quadratic term rounds up: a 0.0001 notional owes 1 unit, never 0.
    #[test]
    fn impact_fee_ceils_a_dust_fill_to_one_unit() {
        let e = Env::default();
        assert_eq!(impact_fee(&e, 1_000, IMPACT_SCALAR), 1);
    }

    // At notional = impact_scalar / 10 the quadratic rate meets the 10% cap
    // exactly. Past it the cap binds.
    #[test]
    fn impact_fee_caps_at_max_rate() {
        let e = Env::default();
        let boundary = IMPACT_SCALAR / 10;
        assert_eq!(impact_fee(&e, boundary, IMPACT_SCALAR), boundary / 10);
        assert_eq!(
            impact_fee(&e, IMPACT_SCALAR, IMPACT_SCALAR),
            IMPACT_SCALAR / 10
        );
    }

    #[test]
    fn impact_fee_is_zero_on_a_zero_notional() {
        let e = Env::default();
        assert_eq!(impact_fee(&e, 0, IMPACT_SCALAR), 0);
    }

    // 10 notional at a price of 3 buys 3.33 units: the floor gives 3, the
    // ceil 4.
    #[test]
    fn to_tokens_rounds_in_the_named_direction() {
        let e = Env::default();
        let price = 3 * PRICE_SCALAR;
        assert_eq!(to_tokens_floor(&e, 10, price), 3);
        assert_eq!(to_tokens_ceil(&e, 10, price), 4);
    }

    // An exact division has nothing to round, so both directions agree.
    #[test]
    fn to_tokens_is_exact_on_a_divisible_price() {
        let e = Env::default();
        let price = 3 * PRICE_SCALAR;
        assert_eq!(to_tokens_floor(&e, 9, price), 3);
        assert_eq!(to_tokens_ceil(&e, 9, price), 3);
    }

    // The floor can reach zero. The ceil never does on a positive notional.
    #[test]
    fn to_tokens_ceil_never_hits_zero_on_a_positive_notional() {
        let e = Env::default();
        let price = 3 * PRICE_SCALAR;
        assert_eq!(to_tokens_floor(&e, 1, price), 0);
        assert_eq!(to_tokens_ceil(&e, 1, price), 1);
    }
}
