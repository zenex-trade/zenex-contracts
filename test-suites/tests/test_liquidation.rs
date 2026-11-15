//! Integration coverage for the keeper liquidation path (`execute_liquidation`).
//!
//! Each test opens a real position on the market, moves the verified price,
//! and force-closes it, asserting the always-charged (equity-capped) liquidation
//! fee, the keeper payout, and the eligibility and price-anti-replay guards. Every expected
//! amount is derived from the default config by hand. The derivations live in the
//! per-test comments.
//!
//! Shared anchors (default config, 7-dec token, BTC feed at 18 decimals):
//! - Position: notional `$1,000` on `$200` margin (5x), tokens `1e5`.
//! - Every fill pays the size-quadratic impact `notional^2 / impact_scalar`
//!   = `(1e10)^2 / 1e14` = `1e6` on top of its base fee.
//! - Open into an empty book is dominant: base `fee_dom` 0.5% = `5e7` plus
//!   the `1e6` impact, leaving margin `1_949_000_000`.
//! - The liquidating full close is improving (it shrinks the lone side): base
//!   `fee_non_dom` 0.3% = `3e7` plus the `1e6` impact, so the settled
//!   remainder before PnL is `1_949_000_000 - 31_000_000 = 1_918_000_000`.
//! - maintenance margin 5% of notional = `5e8`. Liquidation fee `liq_fee`
//!   1% of notional = `1e8`, charged on every liquidation capped at the
//!   remaining equity.
//! - Keeper reward on the close: `keeper_rate` 10% of the `3.1e7` trade fee =
//!   `3_100_000`, plus 10% of the charged liquidation fee. The fixture
//!   treasury takes 5% of every fee leg, and the vault banks the remainders.

use soroban_sdk::Address;

use market::MarketError;
use test_suites::constants::{BTC_PRICE, SCALAR_7};
use test_suites::setup::create_fixture_with_data;
use test_suites::test_fixture::TestFixture;

/// Notional opened by every position under test: `$1,000` (token-dec).
const NOTIONAL: i128 = 1_000 * SCALAR_7;
/// Margin posted by every position under test: `$200` (token-dec), i.e. 5x.
const MARGIN: i128 = 200 * SCALAR_7;

/// Margin remaining after the liquidating close settles its own fees
/// (token-dec): open leaves `2e9 - base_dom(5e7) - impact(1e6) = 1_949_000_000`,
/// then the close debits `base_non_dom(3e7) + impact(1e6)`.
const SETTLED_MARGIN: i128 = 1_918_000_000;
/// Keeper cut of the close's trade fee (token-dec): `keeper_rate(10%) * 3.1e7`.
const KEEPER_TRADE_CUT: i128 = 3_100_000;
/// The uncapped liquidation fee (token-dec): `liq_fee(1%) * notional`.
const LIQ_FEE_FULL: i128 = 100_000_000;
/// Keeper reward when the full liquidation fee is collectable:
/// the trade-fee cut plus `keeper_rate(10%) * 1e8`.
const KEEPER_PAYOUT_FULL_FEE: i128 = KEEPER_TRADE_CUT + 10_000_000;

/// The full BTC price at 18 decimals ($100k) used for opens.
const PRICE_OPEN: i128 = BTC_PRICE;

/// Open a fresh 5x position on `is_long` at $100k and return its user.
fn open_five_x(fixture: &mut TestFixture, is_long: bool) -> Address {
    let user = fixture.new_user(1_000 * SCALAR_7);
    let open_price = fixture.price(PRICE_OPEN);
    fixture.open(&user, is_long, NOTIONAL, MARGIN, &open_price);
    user
}

/// A healthy position at the mark is not liquidatable: closing at $100k leaves
/// equity `1_918_000_000` (no PnL), far above the `5e8` maintenance margin.
#[test]
fn liquidate_healthy_long_rejects_not_liquidatable() {
    let mut fixture = create_fixture_with_data();
    let user = open_five_x(&mut fixture, true);

    let price = fixture.price_usd(100_000);
    let result = fixture
        .market
        .try_execute_liquidation(&fixture.keeper, &user, &true, &price);

    assert_eq!(result, Err(Ok(MarketError::NotLiquidatable.into())));
    // The rejected call traps: the position is still open.
    assert_eq!(fixture.position(&user, true).notional, NOTIONAL);
    fixture.assert_borrowing_books(&fixture.users);
}

/// A drop to $84k leaves equity above the fee cap: the full `1e8` liquidation
/// fee is charged and the trader keeps the net remainder.
#[test]
fn liquidate_long_charges_fee_and_returns_net_remainder() {
    let mut fixture = create_fixture_with_data();
    let user = open_five_x(&mut fixture, true);

    // pnl = floor(tokens 1e5 * bid / 1e18) - notional = floor(1e5 * 8.4e22 / 1e18) - 1e10
    //     = 8_400_000_000 - 10_000_000_000 = -1_600_000_000.
    let pnl = -1_600_000_000_i128;
    // equity = settled remainder + pnl = 1_918_000_000 - 1_600_000_000 = 318_000_000.
    let equity = SETTLED_MARGIN + pnl;
    // 1e8 (fee cap) <= 318_000_000 < 5e8 (maintenance): eligible, and the
    // full liquidation fee is collectable.
    assert!(
        (100_000_000..500_000_000).contains(&equity),
        "full-fee band"
    );

    let trader_before = fixture.balance(&user);
    let keeper_before = fixture.balance(&fixture.keeper);

    let price = fixture.price_usd(84_000);
    let reward = fixture
        .market
        .execute_liquidation(&fixture.keeper, &user, &true, &price);

    assert_eq!(reward, KEEPER_PAYOUT_FULL_FEE, "keeper reward");
    // The trader keeps the remainder net of the liquidation fee. The keeper
    // is paid its cut of the trade fee and the liquidation fee.
    assert_eq!(
        fixture.balance(&user) - trader_before,
        equity - LIQ_FEE_FULL,
        "trader net remainder"
    );
    assert_eq!(
        fixture.balance(&fixture.keeper) - keeper_before,
        KEEPER_PAYOUT_FULL_FEE,
        "keeper credited"
    );
    // The position row is zeroed on a full liquidation.
    let position = fixture.position(&user, true);
    assert_eq!(position.notional, 0, "notional cleared");
    assert_eq!(position.margin, 0, "margin cleared");
    assert_eq!(position.tokens, 0, "tokens cleared");
    assert_eq!(
        fixture.market.get_market_data().notional.long,
        0,
        "long OI cleared"
    );
    fixture.assert_borrowing_books(&fixture.users);
}

/// A drop to $81k pushes equity below the fee cap: the liquidation fee
/// saturates on the whole remainder and the trader receives nothing.
#[test]
fn liquidate_long_fee_saturates_on_remainder() {
    let mut fixture = create_fixture_with_data();
    let user = open_five_x(&mut fixture, true);

    // pnl = floor(1e5 * 8.1e22 / 1e18) - 1e10 = 8_100_000_000 - 10_000_000_000 = -1_900_000_000.
    let pnl = -1_900_000_000_i128;
    // equity = 1_918_000_000 - 1_900_000_000 = 18_000_000, below the 1e8 fee
    // cap: the fee saturates on the remainder.
    let equity = SETTLED_MARGIN + pnl;
    assert!(
        equity < 100_000_000 && equity > 0,
        "saturated band, still solvent"
    );

    let trader_before = fixture.balance(&user);
    let keeper_before = fixture.balance(&fixture.keeper);
    let vault_before = fixture.vault.total_assets();

    let price = fixture.price_usd(81_000);
    let reward = fixture
        .market
        .execute_liquidation(&fixture.keeper, &user, &true, &price);

    // The saturated 18_000_000 fee pays the keeper its 10% (1_800_000) on top
    // of the trade-fee cut. The trader is paid nothing.
    assert_eq!(reward, KEEPER_TRADE_CUT + 1_800_000, "keeper reward");
    assert_eq!(fixture.balance(&user), trader_before, "trader gets nothing");
    assert_eq!(
        fixture.balance(&fixture.keeper) - keeper_before,
        KEEPER_TRADE_CUT + 1_800_000,
        "keeper credited"
    );
    // Vault banks the trader's loss, the saturated fee net of the keeper's
    // 10% (1_800_000) and the treasury's 5% (900_000) = 15_300_000, and the
    // close-fee remainder net of the keeper and treasury cuts:
    // 1_900_000_000 + 15_300_000 + 26_350_000 = 1_941_650_000.
    assert_eq!(
        fixture.vault.total_assets() - vault_before,
        1_941_650_000,
        "vault banks loss + liq fee + fee remainder"
    );
    assert_eq!(
        fixture.position(&user, true).notional,
        0,
        "position cleared"
    );
    fixture.assert_borrowing_books(&fixture.users);
}

/// Bad-debt floor: a drop to $78k drives the raw loss past the settled
/// margin, so equity floors at zero and no extra funds are pulled from the
/// trader. The keeper is still paid.
#[test]
fn liquidate_long_bad_debt_floors_equity_without_pulling_trader() {
    let mut fixture = create_fixture_with_data();
    let user = open_five_x(&mut fixture, true);

    // Raw pnl = floor(1e5 * 7.8e22 / 1e18) - 1e10 = 7_800_000_000 - 10_000_000_000
    //         = -2_200_000_000. The 282_000_000 past the settled 1_918_000_000
    // remainder is bad debt the vault absorbs, and equity floors at zero.

    let trader_before = fixture.balance(&user);
    let keeper_before = fixture.balance(&fixture.keeper);
    let vault_before = fixture.vault.total_assets();

    let price = fixture.price_usd(78_000);
    let reward = fixture
        .market
        .execute_liquidation(&fixture.keeper, &user, &true, &price);

    // Zero equity: the capped liquidation fee collects nothing, so the keeper
    // earns only its trade-fee cut.
    assert_eq!(reward, KEEPER_TRADE_CUT, "keeper reward");
    // The floor means the trader is never charged beyond the posted margin:
    // its wallet is untouched by the liquidation.
    assert_eq!(
        fixture.balance(&user),
        trader_before,
        "no extra pull from trader"
    );
    assert_eq!(
        fixture.balance(&fixture.keeper) - keeper_before,
        KEEPER_TRADE_CUT,
        "keeper credited"
    );
    // The vault nets the raw loss less the absorbed bad debt, plus the
    // close-fee remainder after the keeper and treasury cuts:
    // 2_200_000_000 - 282_000_000 + 26_350_000 = 1_944_350_000. That is the
    // settled margin it could actually collect plus its fee share.
    assert_eq!(
        fixture.vault.total_assets() - vault_before,
        1_944_350_000,
        "vault banks the capped loss and its fee share"
    );
    assert_eq!(
        fixture.position(&user, true).notional,
        0,
        "position cleared"
    );
    fixture.assert_borrowing_books(&fixture.users);
}

/// A liquidation of a `(user, is_long)` with no open position rejects with
/// `PositionNotFound` (the price verifies, but no position exists to close).
#[test]
fn liquidate_absent_position_rejects_position_not_found() {
    let mut fixture = create_fixture_with_data();
    let stranger = fixture.new_user(0);

    let price = fixture.price_usd(100_000);
    let result = fixture
        .market
        .try_execute_liquidation(&fixture.keeper, &stranger, &true, &price);

    assert_eq!(result, Err(Ok(MarketError::PositionNotFound.into())));
    fixture.assert_borrowing_books(&fixture.users);
}

/// A liquidation price that predates the position's last entry change cannot
/// force-close it: the payload is upgraded to the newest consumed mark (the
/// open's own fill price), where the position is healthy.
#[test]
fn liquidate_with_price_predating_entry_upgrades_to_the_newest_mark() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(1_000 * SCALAR_7);

    // Build the crash payload before the position exists (publish_time = T0),
    // then let time pass and open at $100k, so the open's mark (T0+10)
    // strictly postdates the payload. A further jump leaves the fill ledger,
    // while the 12s total age stays inside the verifier's staleness window, so
    // the payload parses and substitution is what prices the call.
    let stale_price = fixture.price_usd(84_000);
    fixture.jump(10);
    let open_price = fixture.price(PRICE_OPEN);
    fixture.open(&user, true, NOTIONAL, MARGIN, &open_price);
    fixture.jump(2);

    let result =
        fixture
            .market
            .try_execute_liquidation(&fixture.keeper, &user, &true, &stale_price);

    assert_eq!(result, Err(Ok(MarketError::NotLiquidatable.into())));
    // The rejected call traps: the position is untouched.
    assert_eq!(fixture.position(&user, true).notional, NOTIONAL);
    fixture.assert_borrowing_books(&fixture.users);
}

/// Short mirror: a rally to $116k leaves a short's equity above the fee cap,
/// so the full fee is charged and the trader keeps the net remainder.
#[test]
fn liquidate_short_charges_fee_and_returns_net_remainder() {
    let mut fixture = create_fixture_with_data();
    let user = open_five_x(&mut fixture, false);

    // pnl = notional - ceil(tokens 1e5 * ask / 1e18) = 1e10 - ceil(1e5 * 1.16e23 / 1e18)
    //     = 10_000_000_000 - 11_600_000_000 = -1_600_000_000.
    let pnl = -1_600_000_000_i128;
    // equity = 1_918_000_000 - 1_600_000_000 = 318_000_000: the full fee is
    // collectable.
    let equity = SETTLED_MARGIN + pnl;
    assert!(
        (100_000_000..500_000_000).contains(&equity),
        "full-fee band"
    );

    let trader_before = fixture.balance(&user);
    let keeper_before = fixture.balance(&fixture.keeper);

    let price = fixture.price_usd(116_000);
    let reward = fixture
        .market
        .execute_liquidation(&fixture.keeper, &user, &false, &price);

    assert_eq!(reward, KEEPER_PAYOUT_FULL_FEE, "keeper reward");
    assert_eq!(
        fixture.balance(&user) - trader_before,
        equity - LIQ_FEE_FULL,
        "trader net remainder"
    );
    assert_eq!(
        fixture.balance(&fixture.keeper) - keeper_before,
        KEEPER_PAYOUT_FULL_FEE,
        "keeper credited"
    );
    let position = fixture.position(&user, false);
    assert_eq!(position.notional, 0, "notional cleared");
    assert_eq!(position.margin, 0, "margin cleared");
    assert_eq!(
        fixture.market.get_market_data().notional.short,
        0,
        "short OI cleared"
    );
    fixture.assert_borrowing_books(&fixture.users);
}

/// A keeper liquidates two positions at the same price in sequence: an
/// eligible position and a healthy one. The healthy call rejects with
/// `NotLiquidatable` without reverting the eligible one.
#[test]
fn keeper_isolates_healthy_from_eligible_liquidation() {
    let mut fixture = create_fixture_with_data();

    // Eligible: a 5x long, liquidatable at $84k (equity 318_000_000, above
    // the fee cap so the full fee is charged).
    let eligible = open_five_x(&mut fixture, true);

    // Healthy: a heavily over-marginized long ($2,000 on $1,000 notional),
    // whose equity stays far above the maintenance margin at $84k.
    let healthy = fixture.new_user(3_000 * SCALAR_7);
    let open_price = fixture.price(PRICE_OPEN);
    fixture.open(&healthy, true, NOTIONAL, 2_000 * SCALAR_7, &open_price);

    let price = fixture.price_usd(84_000);
    let liquidate = |user: &Address| {
        fixture
            .market
            .try_execute_liquidation(&fixture.keeper, user, &true, &price)
    };

    // The eligible liquidation lands and returns the keeper payout.
    assert_eq!(
        liquidate(&eligible),
        Ok(Ok(KEEPER_PAYOUT_FULL_FEE)),
        "eligible keeper payout"
    );

    // The healthy liquidation rejects with NotLiquidatable (722).
    assert_eq!(
        liquidate(&healthy),
        Err(Ok(MarketError::NotLiquidatable.into())),
        "722 code"
    );

    // The failure rolled back only itself: the eligible position is closed, the
    // healthy one is still open.
    assert_eq!(
        fixture.position(&eligible, true).notional,
        0,
        "eligible closed"
    );
    assert_eq!(
        fixture.position(&healthy, true).notional,
        NOTIONAL,
        "healthy still open"
    );
    fixture.assert_borrowing_books(&fixture.users);
}

/// Two liquidations priced by the same report blob reach the Chainlink
/// verifier once: the oracle memoises the verified body per signed report,
/// and the second call is a hit. Inspect each invocation before any getters
/// replace its verifier events.
#[test]
fn two_liquidations_with_the_same_report_call_the_verifier_once() {
    let mut fixture = create_fixture_with_data();
    let first = open_five_x(&mut fixture, true);
    let second = open_five_x(&mut fixture, true);

    let price = fixture.price_usd(84_000);
    fixture
        .market
        .execute_liquidation(&fixture.keeper, &first, &true, &price);
    assert_eq!(
        fixture.verified_count(),
        1,
        "first liquidation verifies the report"
    );
    fixture
        .market
        .execute_liquidation(&fixture.keeper, &second, &true, &price);

    assert_eq!(
        fixture.verified_count(),
        0,
        "the second liquidation is a memo hit"
    );
    assert_eq!(fixture.position(&first, true).notional, 0, "first closed");
    assert_eq!(fixture.position(&second, true).notional, 0, "second closed");
    fixture.assert_borrowing_books(&fixture.users);
}

/// A liquidation after the borrowing index has moved settles the position's
/// borrowing and removes its whole contribution from the side's weight. A
/// second long, opened an hour later at a higher index, keeps its own.
#[test]
fn liquidation_after_accrual_removes_only_the_liquidated_weight() {
    let mut fixture = create_fixture_with_data();
    let user = open_five_x(&mut fixture, true);
    fixture.jump(3_600);
    let other = open_five_x(&mut fixture, true);
    let checkpoint = fixture.position(&other, true).borrowing_idx;
    assert!(checkpoint > 0, "the second long opens at a moved index");

    fixture.jump(3_600);
    let price = fixture.price_usd(84_000);
    fixture
        .market
        .execute_liquidation(&fixture.keeper, &user, &true, &price);

    assert_eq!(fixture.position(&user, true).notional, 0, "liquidated");
    let data = fixture.market.get_market_data();
    assert!(data.borrowing_idx.long > checkpoint);
    assert_eq!(data.borrowing_weight.long, NOTIONAL * checkpoint);
    fixture.assert_borrowing_books(&fixture.users);
}
