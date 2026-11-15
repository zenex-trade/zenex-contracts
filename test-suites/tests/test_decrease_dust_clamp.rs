//! Full-close clamp on decreases sized into the dust band below the position:
//! a decrease whose survivor would sit under `min_position_notional` fills as
//! a full close instead of trapping with `NotionalBelowMinimum` forever. A
//! protective stop sized into that band therefore still fires.
//!
//! Every expected number is derived from the config and the price by the local
//! fee helpers below, never copied from contract output. The settlement token
//! is 7-decimal, so `DOLLAR` is `1e7`. The feed is 18-decimal, so a raw price
//! is dollars * 1e18.

use soroban_sdk::testutils::Address as _;
use soroban_sdk::Address;

use test_suites::carry::lone_side_carry;
use test_suites::constants::{BTC_PRICE, EXEC_FEE, PRICE_SCALAR};
use test_suites::setup::{create_fixture_with_data, DEFAULT_VAULT_LIQUIDITY};
use test_suites::test_fixture::TREASURY_RATE;
use test_suites::{default_config, SCALAR_18, SCALAR_7};

use market::{Config, OrderKind};

/// One dollar of the 7-decimal settlement token (token-dec).
const DOLLAR: i128 = SCALAR_7;

// --- price constants (feed-native, dollars * 1e18) ---

const PRICE_100K: i128 = BTC_PRICE; // $100,000
const PRICE_99K: i128 = 99_000 * PRICE_SCALAR; // $99,000
const PRICE_95K: i128 = 95_000 * PRICE_SCALAR; // $95,000

// --- independent fee / sizing math (mirrors market::engine::math) ---

/// `ceil(amount * factor / SCALAR_18)`: a charge (trade fee) rounds up.
fn factor_ceil(amount: i128, factor: i128) -> i128 {
    (amount * factor + SCALAR_18 - 1) / SCALAR_18
}

/// `floor(amount * factor / SCALAR_18)`: a cut (the keeper share) rounds down.
fn factor_floor(amount: i128, factor: i128) -> i128 {
    amount * factor / SCALAR_18
}

/// `min(ceil(notional^2 / impact_scalar), ceil(10% * notional))`: the
/// size-quadratic impact fee on a fill's full notional rounds up.
fn impact_fee(notional: i128, impact_scalar: i128) -> i128 {
    let fee = (notional * notional + impact_scalar - 1) / impact_scalar;
    fee.min((notional + 9) / 10)
}

/// Base size a long buys with `notional` at raw `price`:
/// `floor(notional * SCALAR_18 / price)` (base-dec).
fn to_tokens_floor(notional: i128, price: i128) -> i128 {
    notional * SCALAR_18 / price
}

/// Long value of `tokens` marked at `price`: `floor(tokens * price / SCALAR_18)` (token-dec).
fn to_notional_floor(tokens: i128, price: i128) -> i128 {
    tokens * price / SCALAR_18
}

/// Reserve value of `tokens` marked at `price`: `ceil(tokens * price / SCALAR_18)`
/// (token-dec). A long reserves its ask-marked value, rounded up.
fn to_notional_ceil(tokens: i128, price: i128) -> i128 {
    (tokens * price + SCALAR_18 - 1) / SCALAR_18
}

/// The vault's banked remainder of a `fee` trade fee, after the keeper's
/// `keeper_rate` and the treasury's rate (token-dec).
fn vault_fee_cut(config: &Config, fee: i128) -> i128 {
    fee - factor_floor(fee, config.keeper_rate) - factor_floor(fee, TREASURY_RATE)
}

/// The `(base, impact)` fee for a fill that fully worsens skew (an open into an
/// empty book): the whole notional pays `fee_dom` plus the impact charge.
fn worsening_fees(config: &Config, notional: i128) -> (i128, i128) {
    let base = factor_ceil(notional, config.fee_dom);
    let impact = impact_fee(notional, config.impact_scalar);
    (base, impact)
}

/// The `(base, impact)` fee for a fill that fully improves skew (reducing the
/// lone side): the whole notional pays `fee_non_dom` plus the impact charge.
fn improving_fees(config: &Config, notional: i128) -> (i128, i128) {
    let base = factor_ceil(notional, config.fee_non_dom);
    let impact = impact_fee(notional, config.impact_scalar);
    (base, impact)
}

/// The keeper payout for an order fill: the `keeper_rate` cut of the trade fee
/// plus the flat `exec_fee` every order escrows at creation and pays on fill.
fn order_keeper_payout(config: &Config, base: i128, impact: i128) -> i128 {
    factor_floor(base + impact, config.keeper_rate) + EXEC_FEE
}

/// The scenario: a $10 long with a resting StopDecrease of $9.50 whose
/// survivor ($0.50) would sit under the $1 position minimum. The triggered
/// stop clamps to a full close and fills.
#[test]
fn stop_decrease_into_dust_band_fills_as_full_close() {
    let fixture = create_fixture_with_data();
    let config = default_config();
    let keeper = fixture.keeper.clone();
    let user = Address::generate(&fixture.env);
    fixture.mint(&user, 100 * DOLLAR);

    // Open the $10 long on $3 margin at $100k.
    let notional = 10 * DOLLAR; // 100_000_000
    let margin = 3 * DOLLAR;
    fixture.open(&user, true, notional, margin, &fixture.price(PRICE_100K));
    let (open_base, open_impact) = worsening_fees(&config, notional);
    let net_margin = margin - open_base - open_impact;

    // Rest the stop loss: $9.50 of the $10 position, firing once the exit
    // (bid) falls to $99k. The $0.50 survivor is under the $1 minimum.
    let stop_notional = 95 * DOLLAR / 10; // 95_000_000
    let id = fixture.submit_order(
        &user,
        true,
        OrderKind::StopDecrease,
        stop_notional,
        0,
        PRICE_99K,
        0,
        fixture.far_expiration(),
    );

    // Past the notional lock the price crashes through the trigger.
    let held = (config.notional_lock + 5) as i128;
    fixture.jump(held as u64);
    let user_before = fixture.balance(&user);
    let payout = fixture.execute_order(&keeper, &user, id, &fixture.price(PRICE_95K));

    // The fill clamps to a full close: the whole $10 closes against the
    // settled numbers (fees priced at the full size), realizing the $0.50
    // loss. The settled remainder pays out.
    let (base, impact) = improving_fees(&config, notional);
    let tokens = to_tokens_floor(notional, PRICE_100K);
    let pnl = to_notional_floor(tokens, PRICE_95K) - notional;
    // The hold accrues carry too. The lone long reserves its ask-marked value
    // at the $95k fill mark, against the seeded liquidity plus the open's
    // vault leg. The funding charge runs at the `funding_min` floor.
    let backing = DEFAULT_VAULT_LIQUIDITY + vault_fee_cut(&config, open_base + open_impact);
    let carry = lone_side_carry(
        &config,
        notional,
        to_notional_ceil(tokens, PRICE_95K),
        backing,
        held,
    );
    let returned = net_margin - base - impact + pnl - carry.total();
    assert_eq!(
        payout,
        order_keeper_payout(&config, base, impact),
        "full-close payout"
    );
    assert_eq!(
        fixture.position(&user, true).notional,
        0,
        "the stop closed the position in full"
    );
    assert_eq!(
        fixture.balance(&user) - user_before,
        returned,
        "trader receives the settled full-close remainder",
    );
    fixture.assert_borrowing_books([&user]);
}

/// The clamp's boundary: a decrease leaving exactly `min_position_notional`
/// behind still fills as a partial, so the clamp only swallows survivors the
/// validator would reject.
#[test]
fn decrease_leaving_exact_minimum_still_fills_partially() {
    let fixture = create_fixture_with_data();
    let config = default_config();
    let keeper = fixture.keeper.clone();
    let user = Address::generate(&fixture.env);
    fixture.mint(&user, 100 * DOLLAR);

    let notional = 10 * DOLLAR;
    let margin = 3 * DOLLAR;
    fixture.open(&user, true, notional, margin, &fixture.price(PRICE_100K));
    fixture.jump(config.notional_lock + 5);

    // Close $9 of the $10 position: the $1 survivor sits exactly at the
    // position minimum, so the partial path runs.
    let close_notional = 9 * DOLLAR;
    let payout = fixture.create_and_fill(
        &keeper,
        &user,
        true,
        OrderKind::MarketDecrease as u32,
        close_notional,
        0,
        0,
        0,
        fixture.far_expiration(),
        &fixture.price(PRICE_100K),
    );

    let (base, impact) = improving_fees(&config, close_notional);
    assert_eq!(
        payout,
        order_keeper_payout(&config, base, impact),
        "partial-decrease payout"
    );
    assert_eq!(
        fixture.position(&user, true).notional,
        config.min_position_notional,
        "survivor sits exactly at the position minimum",
    );
    fixture.assert_borrowing_books([&user]);
}
