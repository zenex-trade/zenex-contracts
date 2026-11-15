//! Entry-size rounding: a round trip at an unchanged quote never pays the
//! trader, on either side, whole or sliced.
//!
//! The quote is not a round power of ten, the case a round fixture price
//! hides. At `$100,001 * 1e18` a $1 position buys 99.999 base units. A
//! floored short size would be worth less than its notional, so closing at
//! the same quote would pay the trader up to one base unit, about a cent,
//! from the vault.

use test_suites::test_fixture::TestFixture;
use test_suites::{SCALAR_18, SCALAR_7};

/// A non-divisible 18-decimal quote: $100,001.
const QUOTE: i128 = 100_001 * SCALAR_18;

/// Balance deltas of every party after a scenario, token-dec, with the
/// market's credit pool alongside its balance.
struct Deltas {
    user: i128,
    vault: i128,
    treasury: i128,
    market: i128,
    credit_pool: i128,
}

/// Runs `scenario` for a fresh trader and returns each party's balance
/// change.
fn run(scenario: impl Fn(&TestFixture, &soroban_sdk::Address)) -> Deltas {
    let mut f = TestFixture::create();
    f.seed_vault(1_000_000 * SCALAR_7);
    let user = f.new_user(100 * SCALAR_7);
    let before = (
        f.balance(&user),
        f.balance(&f.vault.address),
        f.balance(&f.treasury.address),
        f.balance(&f.market.address),
        f.market.get_market_data().credit_pool,
    );
    scenario(&f, &user);
    Deltas {
        user: f.balance(&user) - before.0,
        vault: f.balance(&f.vault.address) - before.1,
        treasury: f.balance(&f.treasury.address) - before.2,
        market: f.balance(&f.market.address) - before.3,
        credit_pool: f.market.get_market_data().credit_pool - before.4,
    }
}

/// Fees and conservative rounding may debit the trader. The vault never
/// funds a net payout at an unchanged quote.
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

/// A $1 short at $100,001 buys 100 base units (99.999 rounded up), and a
/// $1 long buys 99. Each side's size is worth at most its notional in the
/// trader's favour.
#[test]
fn entry_size_rounds_against_the_trader_on_both_sides() {
    let mut f = TestFixture::create();
    f.seed_vault(1_000_000 * SCALAR_7);
    let long = f.new_user(100 * SCALAR_7);
    let short = f.new_user(100 * SCALAR_7);

    f.open(&long, true, SCALAR_7, SCALAR_7, &f.price(QUOTE));
    f.open(&short, false, SCALAR_7, SCALAR_7, &f.price(QUOTE));

    assert_eq!(f.position(&long, true).tokens, 99);
    assert_eq!(f.position(&short, false).tokens, 100);
}

/// The two-scale probe: a $1 short opened and closed at $100,001, with an
/// 8-decimal control and an 18-decimal case. Each scale starts with its own
/// market and balances.
#[test]
fn short_round_trip_at_both_report_scales_never_pays_the_trader() {
    for price_scale in [100_000_000_i128, SCALAR_18] {
        let quote = 100_001 * price_scale;
        let d = run(|f, user| {
            f.open(user, false, SCALAR_7, SCALAR_7, &f.price(quote));
            f.jump(35);
            f.close(user, false, SCALAR_7, 0, &f.price(quote));
        });
        println!(
            "price_scale={price_scale}, trader={}, vault={}, treasury={}, market={}",
            d.user, d.vault, d.treasury, d.market
        );
        assert_no_gain(&d);
    }
}

/// The long mirror of the probe. A long floors on entry and on the close
/// mark, so it is safe. The test pins that it stays so.
#[test]
fn long_round_trip_at_an_unchanged_quote_never_pays_the_trader() {
    let d = run(|f, user| {
        f.open(user, true, SCALAR_7, SCALAR_7, &f.price(QUOTE));
        f.jump(35);
        f.close(user, true, SCALAR_7, 0, &f.price(QUOTE));
    });
    assert_no_gain(&d);
}

/// Slicing a short's exit into $1 closes cannot recover the residue. Each
/// closed chunk's value rounds up, and a sum of ceilings is at least the
/// ceiling of the sum, so the lifetime PnL stays at or below zero.
#[test]
fn sliced_short_exit_at_an_unchanged_quote_never_pays_the_trader() {
    let d = run(|f, user| {
        f.open(user, false, 5 * SCALAR_7, 5 * SCALAR_7, &f.price(QUOTE));
        f.jump(35);
        for _ in 0..4 {
            f.close(user, false, SCALAR_7, 0, &f.price(QUOTE));
        }
        f.close(user, false, SCALAR_7, 0, &f.price(QUOTE));
        assert_eq!(f.position(user, false).notional, 0, "position closed");
    });
    assert_no_gain(&d);
}

/// The long mirror of the sliced exit: floors on both legs sum to at most
/// the floor of the whole.
#[test]
fn sliced_long_exit_at_an_unchanged_quote_never_pays_the_trader() {
    let d = run(|f, user| {
        f.open(user, true, 5 * SCALAR_7, 5 * SCALAR_7, &f.price(QUOTE));
        f.jump(35);
        for _ in 0..4 {
            f.close(user, true, SCALAR_7, 0, &f.price(QUOTE));
        }
        f.close(user, true, SCALAR_7, 0, &f.price(QUOTE));
        assert_eq!(f.position(user, true).notional, 0, "position closed");
    });
    assert_no_gain(&d);
}
