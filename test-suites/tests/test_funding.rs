//! Integration coverage for the two accrual indices and the funding pool.
//!
//! Funding (velocity) and borrowing (kink model) both advance in the same
//! `Market::load` path, so they share this file. Each test isolates one
//! index and derives its value by hand, so every config builder here pins both
//! engines rather than inheriting the default carry curves. The rates are
//! chosen so the accrued amounts stay clean and the posted margins survive the
//! jumps.
//!
//! Expected values come from independent math derived in the comment above each
//! assert, never from contract output. Prices anchor at BTC $100k with a 7-dec
//! settlement token and an 18-dec feed, so `tokens = notional * 1e18 / price`
//! puts a $1,000 notional at 1e5 base-dec (0.01 BTC).

use soroban_sdk::token::TokenClient;
use soroban_sdk::Address;

use market::{Config, MarketError};
use test_suites::constants::BTC_PRICE;
use test_suites::test_fixture::TestFixture;
use test_suites::{default_config, SCALAR_7};

// ---- config builders ----

/// Funding velocity turned on, borrowing left at zero.
///
/// Acceleration 1e6 per second^2, decay 5e5, a hold band between 2% and 5%
/// token skew, no charge floor, and a 1e11 saved-rate cap (below the ~3.17e11
/// per-second `MAX_FUNDING_RATE` bound). A one-sided or wide-skew book ramps
/// the rate by `funding_increase * skew` per second.
fn funding_config() -> Config {
    Config {
        funding_increase: 1_000_000,
        funding_decrease: 500_000,
        threshold_stable_funding: 5 * test_suites::SCALAR_18 / 100,
        threshold_decrease_funding: 2 * test_suites::SCALAR_18 / 100,
        funding_min: 0,
        funding_max: 100_000_000_000,
        // Borrowing off, so the funding indices and the credit pool carry the
        // only charge these tests derive.
        borrow_rate: 0,
        increased_borrow_rate: 0,
        ..default_config()
    }
}

/// Borrowing kink curve turned on, funding left at zero.
///
/// Below-kink slope 4e7 per second, full-utilization rate 4e8. The kink sits at
/// the default 80% utilization, well above the low utilizations these tests
/// drive, so the linear branch `rate = ceil(borrow_rate * utilization)` applies.
fn borrowing_config() -> Config {
    Config {
        borrow_rate: 40_000_000,
        increased_borrow_rate: 400_000_000,
        // Funding off, so the borrowing index carries the only charge these
        // tests derive.
        funding_increase: 0,
        funding_decrease: 0,
        threshold_stable_funding: 0,
        funding_min: 0,
        funding_max: 0,
        ..default_config()
    }
}

/// Both indices turned on together, for the tests that only assert the accrue
/// return shape and the zero-elapsed no-op rather than exact index values.
fn both_config() -> Config {
    Config {
        borrow_rate: 40_000_000,
        increased_borrow_rate: 400_000_000,
        ..funding_config()
    }
}

// ---- shared setup ----

/// A dollar amount in the 7-dec settlement token.
fn usd(amount: i128) -> i128 {
    amount * SCALAR_7
}

/// Deploy a funding-config market with a long ($3,000) and a smaller short
/// ($1,000) open, jump an hour, then touch each side with a margin-only
/// increase so the payer settles its charge and the receiver banks its credit.
///
/// The long dominates by tokens (3e5 vs 1e5, skew 0.5), so longs pay: the
/// long's touch debits its funding into the pool, the short's touch credits the
/// short's claimable balance. Returns the fixture and the two traders.
fn skewed_book_settled<'a>() -> (TestFixture<'a>, Address, Address) {
    let mut fixture = TestFixture::create_with(funding_config());
    fixture.seed_vault(usd(10_000_000));
    let long_user = fixture.new_user(usd(100_000));
    let short_user = fixture.new_user(usd(100_000));

    let price = fixture.price(BTC_PRICE);
    // Long $3,000 on $1,000 (3x), short $1,000 on $500 (2x). Opened in the same
    // ledger as construction, so neither open accrues (elapsed 0).
    fixture.open(&long_user, true, usd(3_000), usd(1_000), &price);
    fixture.open(&short_user, false, usd(1_000), usd(500), &price);

    // One hour of one-directional skew.
    fixture.jump(3_600);

    // A fresh price for the post-jump fills: the pre-jump payload is now past
    // the verifier's staleness window.
    let price = fixture.price(BTC_PRICE);
    // Margin-only increases ($1 each) trigger settlement without moving size
    // or skew. The long touch accrues the hour. The short touch runs at the same
    // ledger (elapsed 0) and only reads the already-advanced index.
    fixture.open(&long_user, true, 0, usd(1), &price);
    fixture.open(&short_user, false, 0, usd(1), &price);

    (fixture, long_user, short_user)
}

// ---- funding accrual ----

/// A lone long ramps the funding rate from zero and moves only the paying
/// (long) side's index. With no short notional the credit is never distributed.
#[test]
fn funding_rate_ramps_with_a_skewed_book() {
    let mut fixture = TestFixture::create_with(funding_config());
    fixture.seed_vault(usd(10_000_000));
    let user = fixture.new_user(usd(100_000));

    let price = fixture.price(BTC_PRICE);
    // Lone long $1,000: tokens = 1e10 * 1e18 / 1e23 = 1e5, skew = 1.
    fixture.open(&user, true, usd(1_000), usd(500), &price);

    let elapsed: i128 = 3_600;
    fixture.jump(elapsed as u64);
    let price = fixture.price(BTC_PRICE);
    let data = fixture.market.accrue(&price);

    // Fresh rate, skew 1: rate = funding_increase * skew * elapsed =
    // 1e6 * 1 * 3600 = 3.6e9 per second (SCALAR_18).
    let expected_rate = 1_000_000 * elapsed;
    assert_eq!(
        data.funding_rate, expected_rate,
        "funding rate after one hour"
    );
    assert_eq!(data.funding_rate, 3_600_000_000);

    // Longs pay: the long index rises by rate * elapsed = 3.6e9 * 3600 = 1.296e13.
    let expected_pay = expected_rate * elapsed;
    assert_eq!(data.funding_idx.long, expected_pay, "paying-side index");
    assert_eq!(data.funding_idx.long, 12_960_000_000_000);
    // No short notional, so the receiver credit is skipped and its index holds.
    assert_eq!(data.funding_idx.short, 0, "empty receiver side stays put");

    // The accrue persisted what it returned.
    let stored = fixture.market.get_market_data();
    assert_eq!(stored.funding_idx.long, data.funding_idx.long);
    assert_eq!(stored.funding_rate, data.funding_rate);
    fixture.assert_borrowing_books(&fixture.users);
}

/// Funding is peer to peer: the paying side debits its charge into the pool on
/// its next touch, and the receiving side banks an equal claimable credit.
#[test]
fn funding_is_peer_to_peer_between_the_two_sides() {
    let (fixture, long_user, short_user) = skewed_book_settled();

    let data = fixture.market.get_market_data();

    // Skew 0.5 (|3e5 - 1e5| / 4e5): rate = floor(1e6 * 0.5) * 3600 =
    // 500000 * 3600 = 1.8e9 per second.
    assert_eq!(data.funding_rate, 1_800_000_000, "rate at skew 0.5");
    // Longs pay: long index up by rate * elapsed = 1.8e9 * 3600 = 6.48e12.
    assert_eq!(
        data.funding_idx.long, 6_480_000_000_000,
        "paying long index"
    );
    // Short credit spreads the paid total over the short notional: the index
    // falls by pay_delta * long_notional / short_notional = 6.48e12 * 3 = 1.944e13.
    assert_eq!(
        data.funding_idx.short, -19_440_000_000_000,
        "earning short index"
    );

    // Long pays ceil($3,000 * 6.48e12 / 1e18) = ceil(3e10 * 6.48e12 / 1e18) =
    // 194400 token-dec into the pool.
    assert_eq!(data.credit_pool, 194_400, "pool holds the long's payment");
    assert_eq!(data.credit_owed, 194_400, "owed matches the credited total");

    // Short earns ceil($1,000 * 1.944e13 / 1e18) = ceil(1e10 * 1.944e13 / 1e18) =
    // 194400, conserving the long's payment exactly.
    assert_eq!(
        fixture.market.get_claimable_credit(&short_user),
        194_400,
        "receiver banked the payer's charge",
    );
    assert_eq!(
        fixture.market.get_claimable_credit(&long_user),
        0,
        "the paying side is owed nothing",
    );
    fixture.assert_borrowing_books(&fixture.users);
}

// ---- funding pool: claim ----

/// A claim transfers the full claimable balance out, zeroes the entry, and
/// shrinks both pool totals by the paid amount.
#[test]
fn claim_credit_pays_out_and_zeroes_the_balance() {
    let (fixture, _long_user, short_user) = skewed_book_settled();
    let token = TokenClient::new(&fixture.env, &fixture.token_address);

    let before = token.balance(&short_user);
    let paid = fixture.market.claim_credit(&short_user);
    assert_eq!(paid, 194_400, "claim pays the full banked credit");
    assert_eq!(
        token.balance(&short_user) - before,
        194_400,
        "the tokens landed in the trader's wallet",
    );

    assert_eq!(
        fixture.market.get_claimable_credit(&short_user),
        0,
        "the claimable entry is cleared",
    );
    let data = fixture.market.get_market_data();
    assert_eq!(data.credit_pool, 0, "the pool drained by the payout");
    assert_eq!(data.credit_owed, 0, "nothing remains owed");
    fixture.assert_borrowing_books(&fixture.users);
}

/// Claiming with no banked balance is rejected with `NothingToClaim` (760).
#[test]
fn claim_credit_with_no_balance_rejects() {
    let mut fixture = TestFixture::create_with(funding_config());
    let user = fixture.new_user(0);

    let result = fixture.market.try_claim_credit(&user);
    assert_eq!(result, Err(Ok(MarketError::NothingToClaim.into())));
}

// ---- borrowing accrual ----

/// Only the dominant side's borrowing index advances, and closing the
/// position banks the accrued borrowing fee to the vault.
#[test]
fn borrowing_index_advances_and_close_settles_the_fee_to_the_vault() {
    let mut fixture = TestFixture::create_with(borrowing_config());
    // $1M vault so the lone long sits well below the kink.
    fixture.seed_vault(usd(1_000_000));
    let user = fixture.new_user(usd(100_000));

    let price = fixture.price(BTC_PRICE);
    // Long $100,000 on $50,000 (2x): tokens = 1e12 * 1e18 / 1e23 = 1e7,
    // long-side reserve = ceil(1e7 * 1e23 / 1e18) = 1e12.
    fixture.open(&user, true, usd(100_000), usd(50_000), &price);

    let elapsed: i128 = 86_400;
    fixture.jump(elapsed as u64);
    let price = fixture.price(BTC_PRICE);
    let data = fixture.market.accrue(&price);

    // The open's fee (base 5e9 plus the (1e12)^2 / 1e14 = 1e10 impact) left
    // its 85% vault share of 12_750_000_000 behind, so the backing is
    // 10_012_750_000_000. The long side's capacity is 80% of half the
    // backing: floor(0.8 * 5_006_375_000_000) = 4_005_100_000_000.
    // utilization = ceil(1e12 * 1e18 / capacity) = 249_681_655_888_741_855,
    // below the 80% kink. rate = ceil(4e7 * utilization / 1e18) = 9_987_267
    // per second, and index = rate * elapsed.
    let expected_index = 9_987_267 * elapsed;
    assert_eq!(data.borrowing_idx.long, expected_index, "long index");
    assert_eq!(data.borrowing_idx.long, 862_899_868_800);
    // The short side holds fewer tokens (none), so it accrues nothing.
    assert_eq!(data.borrowing_idx.short, 0, "short index stands still");
    // Funding is off in this config, so its index never moves.
    assert_eq!(
        data.funding_idx.long, 0,
        "borrowing config leaves funding at zero"
    );

    // Closing at the same price realizes no PnL. The vault banks the borrowing
    // fee ceil(1e12 * idx / 1e18) = 862_900 less the treasury's 5% (43_145),
    // plus the close fee remainder: base ceil(0.3% * 1e12) = 3e9 plus the 1e10
    // impact, less the 10% keeper cut and the 5% treasury cut.
    // Total 819_755 + 11_050_000_000 = 11_050_819_755.
    let vault_before = fixture.vault.total_assets();
    fixture.close(&user, true, i128::MAX, 0, &price);
    assert_eq!(
        fixture.vault.total_assets() - vault_before,
        11_050_819_755,
        "the vault banked the borrowing fee and the close-fee remainder",
    );
    fixture.assert_borrowing_books(&fixture.users);
}

/// A token tie charges both sides, each at the rate from its own per-side
/// utilization. With equal reserves the two rates match.
#[test]
fn borrowing_charges_a_token_balanced_book_on_both_sides() {
    let mut fixture = TestFixture::create_with(borrowing_config());
    fixture.seed_vault(usd(1_000_000));
    let long_user = fixture.new_user(usd(100_000));
    let short_user = fixture.new_user(usd(100_000));

    let price = fixture.price(BTC_PRICE);
    // Equal $1,000 notionals at the same price hold equal tokens (1e5 each):
    // a token-balanced book.
    fixture.open(&long_user, true, usd(1_000), usd(500), &price);
    fixture.open(&short_user, false, usd(1_000), usd(500), &price);

    fixture.jump(3_600);
    let price = fixture.price(BTC_PRICE);
    let data = fixture.market.accrue(&price);

    // Each side reserves 1e10: the long's ask-marked tokens
    // ceil(1e5 * 1e23 / 1e18), the short's $1,000 entry notional. The two
    // opens' fees (each base plus the 1e6 impact) left 69_700_000 behind, so
    // each side's capacity is floor(0.8 * 5_000_034_850_000) =
    // 4_000_027_880_000: utilization = ceil(1e10 * 1e18 / capacity) =
    // 2_499_982_575_121_452, rate = ceil(4e7 * utilization / 1e18) =
    // 100_000/s. Over 3600 s both indices advance 3.6e8.
    let expected_index = 100_000 * 3_600;
    assert_eq!(data.borrowing_idx.long, expected_index, "long side pays");
    assert_eq!(data.borrowing_idx.short, expected_index, "short side pays");
    assert_eq!(
        data.accrued_at,
        fixture.env.ledger().timestamp(),
        "the accrual timestamp advanced",
    );
    fixture.assert_borrowing_books(&fixture.users);
}

// ---- accrue behavior shared by both indices ----

/// A second accrue in the same ledger moves neither index: the elapsed is zero.
#[test]
fn zero_elapsed_accrue_is_a_noop_on_both_indices() {
    let mut fixture = TestFixture::create_with(both_config());
    fixture.seed_vault(usd(1_000_000));
    let user = fixture.new_user(usd(100_000));

    let price = fixture.price(BTC_PRICE);
    // A lone long makes both indices accrue: skewed for funding, dominant for borrowing.
    fixture.open(&user, true, usd(1_000), usd(500), &price);

    fixture.jump(3_600);
    let price = fixture.price(BTC_PRICE);
    let first = fixture.market.accrue(&price);
    // Both indices are nonzero after the first accrue, so a no-op is meaningful.
    assert!(
        first.funding_idx.long > 0,
        "funding accrued on the first call"
    );
    assert!(
        first.borrowing_idx.long > 0,
        "borrowing accrued on the first call"
    );

    // Same ledger, elapsed 0.
    let second = fixture.market.accrue(&price);
    assert_eq!(
        second.funding_idx.long, first.funding_idx.long,
        "funding index held"
    );
    assert_eq!(
        second.borrowing_idx.long, first.borrowing_idx.long,
        "borrowing index held"
    );
    assert_eq!(second.funding_rate, first.funding_rate, "funding rate held");
    fixture.assert_borrowing_books(&fixture.users);
}

/// `accrue` returns the same market data it persists.
#[test]
fn accrue_returns_the_freshly_persisted_market_data() {
    let mut fixture = TestFixture::create_with(both_config());
    fixture.seed_vault(usd(1_000_000));
    let user = fixture.new_user(usd(100_000));

    let price = fixture.price(BTC_PRICE);
    fixture.open(&user, true, usd(1_000), usd(500), &price);

    fixture.jump(3_600);
    let price = fixture.price(BTC_PRICE);
    let returned = fixture.market.accrue(&price);
    let stored = fixture.market.get_market_data();

    assert_eq!(
        returned.funding_idx.long, stored.funding_idx.long,
        "funding index"
    );
    assert_eq!(
        returned.borrowing_idx.long, stored.borrowing_idx.long,
        "borrowing index"
    );
    assert_eq!(returned.funding_rate, stored.funding_rate, "funding rate");
    assert_eq!(returned.accrued_at, stored.accrued_at, "accrual timestamp");
    fixture.assert_borrowing_books(&fixture.users);
}
