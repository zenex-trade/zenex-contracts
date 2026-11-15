//! Entry rounding at quotes and notionals that divide no whole base unit:
//! the size a fill buys covers its notional to within one base unit of
//! value, on both sides, whole or sliced.
//!
//! `test_entry_rounding` pins the direction at one quote and one notional.
//! This file pins the size of the error and where it accumulates. Every
//! quote and every notional here leaves a remainder, because a round power
//! of ten divides exactly at any scale and hides the rounding entirely.
//!
//! Every bound comes from the quote and the two decimal scales alone. No
//! expectation is computed by the conversion under test, so reverting the
//! entry-rounding direction fails these assertions rather than moving them.

use soroban_sdk::Address;

use test_suites::test_fixture::TestFixture;
use test_suites::{SCALAR_18, SCALAR_7};

/// Quotes on either side of $100,000 that divide no notional here exactly
/// (18-dec, the production feed scale).
const QUOTE_HIGH: i128 = 100_001 * SCALAR_18;
const QUOTE_LOW: i128 = 99_997 * SCALAR_18;

/// Notionals spanning four orders of magnitude, none of them round
/// (token-dec): $1.01, $137.13, $2,318.07 and $9,999.99.
const NOTIONALS: [i128; 4] = [10_100_000, 1_371_300_000, 23_180_700_000, 99_999_900_000];

/// The notional the round trips carry: $1.01 (token-dec). It is small
/// enough that the residue can outweigh the trade fees, so the direction of
/// the rounding decides the sign of the trader's balance change.
const SMALL: i128 = 10_100_000;
/// Slices one entry splits into.
const SLICES: i128 = 7;
/// The notional [`SLICES`] increases of [`SMALL`] sum to: $7.07 (token-dec).
const SLICED_TOTAL: i128 = SLICES * SMALL;

/// Vault liquidity every fixture seeds (token-dec).
const LIQUIDITY: i128 = 1_000_000 * SCALAR_7;

/// A fixture with enough liquidity to back every position in this file.
fn fixture() -> TestFixture<'static> {
    let f = TestFixture::create();
    f.seed_vault(LIQUIDITY);
    f
}

/// The entry gap of a position: `notional * SCALAR_18 - tokens * price`,
/// the notional the size fails to cover, scaled by `SCALAR_18`.
///
/// A long's gap is positive, because a long's size is worth at most what the
/// trader paid for it. A short's gap is negative, because a short owes the
/// value of its size and that value is at least the notional. Either way the
/// magnitude is under `price`, one base unit of value.
fn entry_gap(notional: i128, tokens: i128, price: i128) -> i128 {
    notional * SCALAR_18 - tokens * price
}

/// The gap converted to the settlement token, rounded up (token-dec). The
/// mark rounds against the trader on the close, so a part of a stroop costs
/// a whole one.
fn residue(gap: i128) -> i128 {
    (gap.abs() + SCALAR_18 - 1) / SCALAR_18
}

/// The residue a round trip at `price` can leave, whatever the notional
/// (token-dec). One base unit is worth `price / SCALAR_18` stroops, and the
/// value conversion rounds once more, so the residue stays under one stroop
/// past that.
fn residue_bound(price: i128) -> i128 {
    price / SCALAR_18 + 1
}

/// Open `notional` on `is_long` at `price` for a fresh trader and return the
/// position's size (base-dec). The trader posts the notional as margin, so
/// no fee can push the position under its initial margin.
fn open_at(f: &mut TestFixture<'_>, is_long: bool, notional: i128, price: i128) -> i128 {
    let user = f.new_user(2 * notional + 100 * SCALAR_7);
    f.open(&user, is_long, notional, notional, &f.price(price));
    f.position(&user, is_long).tokens
}

/// Balance changes of every party over one scenario (token-dec), with the
/// market's credit pool alongside its balance.
struct Deltas {
    user: i128,
    vault: i128,
    treasury: i128,
    market: i128,
    credit_pool: i128,
}

/// Run `scenario` for a fresh trader and return each party's balance change.
fn run(scenario: impl Fn(&TestFixture<'_>, &Address)) -> Deltas {
    let mut f = fixture();
    let user = f.new_user(1_000 * SCALAR_7);
    let before = (
        f.balance(&user),
        f.balance(&f.vault.address),
        f.balance(&f.treasury.address),
        f.balance(&f.market.address),
        f.market.get_market_data().credit_pool,
    );
    scenario(&f, &user);
    f.assert_borrowing_books([&user]);
    Deltas {
        user: f.balance(&user) - before.0,
        vault: f.balance(&f.vault.address) - before.1,
        treasury: f.balance(&f.treasury.address) - before.2,
        market: f.balance(&f.market.address) - before.3,
        credit_pool: f.market.get_market_data().credit_pool - before.4,
    }
}

/// The trader's outflows are the fees and the funding, which the vault, the
/// treasury, and the market's credit pool collect. None of them funds a
/// payout at an unchanged quote.
fn assert_no_gain(d: &Deltas) {
    assert!(
        d.user <= 0,
        "trader gained {} at an unchanged quote",
        d.user
    );
    assert!(d.vault >= 0, "vault paid out {}", d.vault);
    assert!(d.treasury >= 0, "treasury paid out {}", d.treasury);
    // A lone trader is the whole book, so funding has no receiving side and
    // settles into the credit pool. The market holds that pool and nothing
    // else once the position is closed.
    assert!(d.credit_pool >= 0, "credit pool paid out {}", d.credit_pool);
    assert_eq!(
        d.market, d.credit_pool,
        "the market kept an escrow past its credit pool after the close"
    );
    assert_eq!(
        d.user + d.vault + d.treasury + d.market,
        0,
        "tokens left the four parties"
    );
}

/// Open [`SLICES`] increases of [`SMALL`] at `price`, then close the whole
/// position at the same quote.
fn sliced_round_trip(f: &TestFixture<'_>, user: &Address, is_long: bool, price: i128) {
    for _ in 0..SLICES {
        f.open(user, is_long, SMALL, SMALL, &f.price(price));
    }
    f.jump(35);
    f.close(user, is_long, SLICED_TOTAL, 0, &f.price(price));
    assert_eq!(f.position(user, is_long).notional, 0, "position closed");
}

// --- the size of the entry error ---

/// A fill's size covers its notional to within one base unit of value, on
/// both sides and at every notional. A long's size is worth at most the
/// notional and a short's at least it, so neither entry can pay the trader.
#[test]
fn entry_covers_its_notional_to_within_one_base_unit() {
    for quote in [QUOTE_HIGH, QUOTE_LOW] {
        let mut f = fixture();
        for notional in NOTIONALS {
            let long_gap = entry_gap(notional, open_at(&mut f, true, notional, quote), quote);
            assert!(
                (0..quote).contains(&long_gap),
                "long entry gap {long_gap} outside one base unit at notional {notional}",
            );

            let short_gap = entry_gap(notional, open_at(&mut f, false, notional, quote), quote);
            assert!(
                (-quote < short_gap) && (short_gap <= 0),
                "short entry gap {short_gap} outside one base unit at notional {notional}",
            );
        }
    }
}

/// The round-trip residue stays under [`residue_bound`] at every notional,
/// on both sides. The bound depends on the quote alone: a notional four
/// orders of magnitude larger leaves no more residue behind.
#[test]
fn round_trip_residue_stays_under_its_price_bound_at_every_notional() {
    for quote in [QUOTE_HIGH, QUOTE_LOW] {
        let bound = residue_bound(quote);
        let mut f = fixture();
        for notional in NOTIONALS {
            for is_long in [true, false] {
                let tokens = open_at(&mut f, is_long, notional, quote);
                let left = residue(entry_gap(notional, tokens, quote));
                assert!(
                    left < bound,
                    "residue {left} reached the {bound} stroop bound at notional {notional}",
                );
            }
        }
    }
}

// --- where the error accumulates ---

/// Slicing a long's entry buys at most one base unit less per extra slice,
/// and never more than one increase would. Each slice floors its own size,
/// so the losses add up on the entry alone.
#[test]
fn slicing_a_long_entry_costs_at_most_one_base_unit_per_slice() {
    for quote in [QUOTE_HIGH, QUOTE_LOW] {
        let mut f = fixture();
        let whole = open_at(&mut f, true, SLICED_TOTAL, quote);

        let user = f.new_user(1_000 * SCALAR_7);
        for _ in 0..SLICES {
            f.open(&user, true, SMALL, SMALL, &f.price(quote));
        }
        let sliced = f.position(&user, true).tokens;

        assert_eq!(
            f.position(&user, true).notional,
            SLICED_TOTAL,
            "the slices sum to the whole notional",
        );
        assert!(sliced <= whole, "slicing bought {sliced} against {whole}");
        assert!(
            whole - sliced < SLICES,
            "slicing lost {} base units over {SLICES} slices",
            whole - sliced,
        );
    }
}

/// Slicing a short's entry sells at most one base unit more per extra slice,
/// and never less than one increase would. Each slice rounds its own size
/// up, and a short owes the value of its size, so the extra units cost the
/// trader.
#[test]
fn slicing_a_short_entry_costs_at_most_one_base_unit_per_slice() {
    for quote in [QUOTE_HIGH, QUOTE_LOW] {
        let mut f = fixture();
        let whole = open_at(&mut f, false, SLICED_TOTAL, quote);

        let user = f.new_user(1_000 * SCALAR_7);
        for _ in 0..SLICES {
            f.open(&user, false, SMALL, SMALL, &f.price(quote));
        }
        let sliced = f.position(&user, false).tokens;

        assert_eq!(
            f.position(&user, false).notional,
            SLICED_TOTAL,
            "the slices sum to the whole notional",
        );
        assert!(sliced >= whole, "slicing sold {sliced} against {whole}");
        assert!(
            sliced - whole < SLICES,
            "slicing added {} base units over {SLICES} slices",
            sliced - whole,
        );
    }
}

// --- the direction, end to end ---

/// A whole round trip at a non-round quote and a non-round notional never
/// pays the trader, on either side.
#[test]
fn whole_round_trip_at_a_non_round_quote_never_pays_the_trader() {
    for quote in [QUOTE_HIGH, QUOTE_LOW] {
        for is_long in [true, false] {
            let d = run(|f, user| {
                f.open(user, is_long, SMALL, SMALL, &f.price(quote));
                f.jump(35);
                f.close(user, is_long, SMALL, 0, &f.price(quote));
            });
            assert_no_gain(&d);
        }
    }
}

/// Slicing the entry cannot turn the residue around: the same notional
/// opened in [`SLICES`] increases and closed once still pays the trader
/// nothing. `close_settled` sweeps the position exactly, so only the entry
/// accumulates.
#[test]
fn sliced_entry_round_trip_at_a_non_round_quote_never_pays_the_trader() {
    for quote in [QUOTE_HIGH, QUOTE_LOW] {
        for is_long in [true, false] {
            let d = run(|f, user| sliced_round_trip(f, user, is_long, quote));
            assert_no_gain(&d);
        }
    }
}
