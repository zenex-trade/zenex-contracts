//! Treasury integration coverage: protocol fees accruing to the treasury
//! through real fills, the owner-gated withdrawal path, and the rate surface
//! (`get_rate` / `set_rate`) that the market reads at every settlement.
//!
//! The market pays the treasury cut inline at fill time, so the accrual tests
//! drive an open and a close through the fixture's position helpers. They derive
//! the expected legs from the config fee math (mirrored locally, never copied
//! from contract output). The fixture starts with
//! `mock_all_auths_allowing_non_root_auth`. The auth-rejection tests narrow
//! that to a single mocked authorizer.

use soroban_sdk::testutils::{Address as _, MockAuth, MockAuthInvoke};
use soroban_sdk::{Address, IntoVal};

use test_suites::carry::lone_side_carry;
use test_suites::constants::BTC_PRICE;
use test_suites::setup::{create_fixture_with_data, DEFAULT_VAULT_LIQUIDITY};
use test_suites::test_fixture::TREASURY_RATE;
use test_suites::{default_config, SCALAR_18, SCALAR_7};

use treasury::TreasuryError;

/// One dollar of the 7-decimal settlement token (token-dec).
const DOLLAR: i128 = SCALAR_7;

/// The fixture's flat $100,000 mark (feed-native, dollars * 1e18).
const PRICE_100K: i128 = BTC_PRICE;

/// The highest rate the treasury accepts: 50% of `SCALAR_18`.
const MAX_RATE: i128 = SCALAR_18 / 2;

// --- independent fee math (mirrors market::engine::math) ---

/// `ceil(amount * factor / SCALAR_18)`: a charge (trade fee) rounds up.
fn factor_ceil(amount: i128, factor: i128) -> i128 {
    (amount * factor + SCALAR_18 - 1) / SCALAR_18
}

/// `floor(amount * factor / SCALAR_18)`: a cut (the treasury share) rounds down.
fn factor_floor(amount: i128, factor: i128) -> i128 {
    amount * factor / SCALAR_18
}

/// `min(ceil(notional^2 / impact_scalar), ceil(10% * notional))`: the
/// size-quadratic impact fee on a fill's full notional rounds up.
fn impact_fee(notional: i128, impact_scalar: i128) -> i128 {
    let fee = (notional * notional + impact_scalar - 1) / impact_scalar;
    fee.min((notional + 9) / 10)
}

/// The total trade fee of a fill that fully worsens skew (an open into an empty
/// book): `fee_dom` on the whole notional plus the impact charge.
fn worsening_fee(notional: i128) -> i128 {
    let config = default_config();
    factor_ceil(notional, config.fee_dom) + impact_fee(notional, config.impact_scalar)
}

/// The total trade fee of a fill that fully improves skew (closing the lone
/// side): `fee_non_dom` on the whole notional plus the impact charge.
fn improving_fee(notional: i128) -> i128 {
    let config = default_config();
    factor_ceil(notional, config.fee_non_dom) + impact_fee(notional, config.impact_scalar)
}

/// The treasury's cut of a `fee` trade fee at `rate` (a floor: the treasury
/// leg rounds down, the vault banks the remainder).
fn treasury_leg(fee: i128, rate: i128) -> i128 {
    factor_floor(fee, rate)
}

/// The vault's cut of a `fee` trade fee at treasury `rate`: what the keeper's
/// `keeper_rate` and the treasury leave behind (token-dec).
fn vault_leg(fee: i128, rate: i128) -> i128 {
    fee - factor_floor(fee, default_config().keeper_rate) - factor_floor(fee, rate)
}

/// A round trip through the fixture ($10,000 notional on $2,000 margin at a
/// flat $100k) pays the treasury its rate cut of both fills. The owner
/// withdraws the accrued balance in two steps that reconcile exactly.
///
/// Derivation at `default_config` and [`TREASURY_RATE`] (5%):
/// - open: base = ceil(0.5% * $10,000) = $50, impact = ceil($10,000^2 / $10M) =
///   $10, so the fee is $60 and the treasury leg is floor(5% * $60) = $3.
/// - close: the lone long improves skew, so base = ceil(0.3% * $10,000) = $30
///   plus the same $10 impact, a $40 fee and a floor(5% * $40) = $2 leg.
/// - the close also settles the 35 seconds of borrowing the position ran, a
///   139 stroop fee whose treasury cut is floor(5% * 139) = 6 stroops.
/// - total accrual $5.0000006, withdrawn as a half and then the remainder.
#[test]
fn fills_accrue_treasury_fees_and_the_owner_withdraws_them() {
    let mut fixture = create_fixture_with_data();
    let config = default_config();
    let treasury_address = fixture.treasury.address.clone();
    let user = fixture.new_user(100_000 * DOLLAR);

    assert_eq!(
        fixture.balance(&treasury_address),
        0,
        "the treasury starts empty",
    );

    let notional = 10_000 * DOLLAR;
    let margin = 2_000 * DOLLAR; // 5x
    fixture.open(&user, true, notional, margin, &fixture.price(PRICE_100K));

    let open_leg = treasury_leg(worsening_fee(notional), TREASURY_RATE);
    assert_eq!(
        fixture.balance(&treasury_address),
        open_leg,
        "the open's treasury leg landed",
    );

    // Past the notional lock the position can be closed in full.
    let held: i128 = (config.notional_lock + 5) as i128;
    fixture.jump(held as u64);
    fixture.close(&user, true, notional, 0, &fixture.price(PRICE_100K));

    let close_leg = treasury_leg(improving_fee(notional), TREASURY_RATE);
    // The borrowing the position ran settles on the close, and the treasury
    // cuts it too (the funding leg banks into the market's credit pool, which
    // the treasury never sees). The vault backs the position with the seeded
    // liquidity plus the open's vault leg, and the lone long's ask-marked
    // reserve at the flat $100k mark is its own notional.
    let backing = DEFAULT_VAULT_LIQUIDITY + vault_leg(worsening_fee(notional), TREASURY_RATE);
    let borrowing = lone_side_carry(&config, notional, notional, backing, held).borrowing;
    let borrowing_leg = treasury_leg(borrowing, TREASURY_RATE);
    let accrued = open_leg + close_leg + borrowing_leg;
    assert_eq!(
        fixture.balance(&treasury_address),
        accrued,
        "both fills accrued to the treasury",
    );

    // The owner withdraws part of the balance to a fresh recipient.
    let recipient = Address::generate(&fixture.env);
    let partial = accrued / 2;
    fixture
        .treasury
        .withdraw(&fixture.token_address, &recipient, &partial);
    assert_eq!(
        fixture.balance(&recipient),
        partial,
        "recipient banked the partial withdrawal"
    );
    assert_eq!(
        fixture.balance(&treasury_address),
        accrued - partial,
        "the treasury keeps the un-withdrawn remainder",
    );

    // ...and then the remainder, draining it exactly.
    fixture
        .treasury
        .withdraw(&fixture.token_address, &recipient, &(accrued - partial));
    assert_eq!(
        fixture.balance(&recipient),
        accrued,
        "recipient holds the whole accrual"
    );
    assert_eq!(
        fixture.balance(&treasury_address),
        0,
        "the treasury is drained"
    );
}

/// `withdraw` is owner-gated: with only a stranger's auth mocked the call
/// fails and the treasury balance is untouched.
#[test]
fn withdraw_by_non_owner_rejects_and_leaves_the_balance() {
    let fixture = create_fixture_with_data();
    let treasury_address = fixture.treasury.address.clone();

    // Fund the treasury under the fixture's blanket mock before narrowing auth.
    let funded = 1_000 * DOLLAR;
    fixture.mint(&treasury_address, funded);

    let non_owner = Address::generate(&fixture.env);
    fixture.env.mock_auths(&[MockAuth {
        address: &non_owner,
        invoke: &MockAuthInvoke {
            contract: &treasury_address,
            fn_name: "withdraw",
            args: (fixture.token_address.clone(), non_owner.clone(), funded).into_val(&fixture.env),
            sub_invokes: &[],
        },
    }]);

    let result = fixture
        .treasury
        .try_withdraw(&fixture.token_address, &non_owner, &funded);
    assert!(result.is_err(), "a non-owner cannot withdraw protocol fees");
    assert_eq!(
        fixture.balance(&non_owner),
        0,
        "the stranger received nothing"
    );
    assert_eq!(
        fixture.balance(&treasury_address),
        funded,
        "the treasury balance is unchanged after the rejected withdrawal",
    );
}

/// Withdrawing more than the treasury holds traps in the token transfer (the
/// contract carries no balance check of its own), leaving both sides untouched.
#[test]
fn withdraw_above_the_balance_traps_and_leaves_the_balance() {
    let fixture = create_fixture_with_data();
    let treasury_address = fixture.treasury.address.clone();

    let funded = 1_000 * DOLLAR;
    fixture.mint(&treasury_address, funded);
    let recipient = Address::generate(&fixture.env);

    // One stroop over the balance: the SAC transfer rejects, so the whole
    // owner-authorized call unwinds.
    let result = fixture
        .treasury
        .try_withdraw(&fixture.token_address, &recipient, &(funded + 1));
    assert!(result.is_err(), "an over-withdrawal cannot settle");
    assert_eq!(fixture.balance(&recipient), 0, "recipient received nothing");
    assert_eq!(
        fixture.balance(&treasury_address),
        funded,
        "the treasury balance is unchanged after the trapped withdrawal",
    );

    // The balance itself is still withdrawable, so nothing was left in a bad state.
    fixture
        .treasury
        .withdraw(&fixture.token_address, &recipient, &funded);
    assert_eq!(
        fixture.balance(&recipient),
        funded,
        "the exact balance withdraws fine"
    );
}

/// The deployed rate is [`TREASURY_RATE`]. The owner replaces it, including at
/// the `SCALAR_18 / 2` upper bound, and `get_rate` reflects each change.
#[test]
fn set_rate_by_owner_replaces_the_protocol_rate() {
    let fixture = create_fixture_with_data();

    assert_eq!(
        fixture.treasury.get_rate(),
        TREASURY_RATE,
        "the fixture deploys the treasury at 5%",
    );

    let new_rate = SCALAR_18 / 10; // 10%
    fixture.treasury.set_rate(&new_rate);
    assert_eq!(
        fixture.treasury.get_rate(),
        new_rate,
        "the owner's rate took effect"
    );

    // The inclusive upper bound is accepted.
    fixture.treasury.set_rate(&MAX_RATE);
    assert_eq!(fixture.treasury.get_rate(), MAX_RATE, "50% is a valid rate");
}

/// The market reads `get_rate` at every settlement, so a rate change moves the
/// treasury leg of later fills. At 10% the same $10,000 open that pays a $60
/// trade fee sends floor(10% * $60) = $6 instead of the $3 charged at 5%.
#[test]
fn set_rate_changes_the_treasury_leg_of_later_fills() {
    let mut fixture = create_fixture_with_data();
    let treasury_address = fixture.treasury.address.clone();
    let user = fixture.new_user(100_000 * DOLLAR);

    let new_rate = SCALAR_18 / 10; // 10%
    fixture.treasury.set_rate(&new_rate);

    let notional = 10_000 * DOLLAR;
    let margin = 2_000 * DOLLAR;
    fixture.open(&user, true, notional, margin, &fixture.price(PRICE_100K));

    let fee = worsening_fee(notional);
    assert_eq!(
        fixture.balance(&treasury_address),
        treasury_leg(fee, new_rate),
        "the fill paid the new rate's cut",
    );
    assert_ne!(
        treasury_leg(fee, new_rate),
        treasury_leg(fee, TREASURY_RATE),
        "the new rate is observably different from the deployed one",
    );
}

/// A rate above `SCALAR_18 / 2` rejects [`TreasuryError::InvalidRate`] and the
/// stored rate is untouched. A negative rate rejects the same way.
#[test]
fn set_rate_above_half_rejects_invalid_rate() {
    let fixture = create_fixture_with_data();

    let result = fixture.treasury.try_set_rate(&(MAX_RATE + 1));
    assert_eq!(
        result,
        Err(Ok(TreasuryError::InvalidRate.into())),
        "one stroop above 50% is out of range",
    );
    assert_eq!(
        fixture.treasury.get_rate(),
        TREASURY_RATE,
        "the rate is unchanged after the rejected raise",
    );

    let negative = fixture.treasury.try_set_rate(&-1);
    assert_eq!(
        negative,
        Err(Ok(TreasuryError::InvalidRate.into())),
        "a negative rate is out of range",
    );
    assert_eq!(
        fixture.treasury.get_rate(),
        TREASURY_RATE,
        "the rate is unchanged after the rejected negative rate",
    );
}

/// `set_rate` is owner-gated: with only a stranger's auth mocked the call
/// fails and the stored rate is unchanged.
#[test]
fn set_rate_by_non_owner_rejects_and_leaves_the_rate() {
    let fixture = create_fixture_with_data();
    let treasury_address = fixture.treasury.address.clone();
    let non_owner = Address::generate(&fixture.env);
    let new_rate = SCALAR_18 / 10;

    fixture.env.mock_auths(&[MockAuth {
        address: &non_owner,
        invoke: &MockAuthInvoke {
            contract: &treasury_address,
            fn_name: "set_rate",
            args: (new_rate,).into_val(&fixture.env),
            sub_invokes: &[],
        },
    }]);

    let result = fixture.treasury.try_set_rate(&new_rate);
    assert!(
        result.is_err(),
        "a non-owner cannot change the protocol rate"
    );
    assert_eq!(
        fixture.treasury.get_rate(),
        TREASURY_RATE,
        "the rate is unchanged after the rejected call",
    );
}
