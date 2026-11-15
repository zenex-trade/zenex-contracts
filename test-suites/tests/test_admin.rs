//! Integration coverage for the owner/admin surface of the market contract:
//! `set_config` (valid apply, owner gating, and one rejection per
//! invariant class), `set_status` (the live transition matrix plus the
//! behavioral effects of `Frozen` and `OnIce`), and a live-behavior check that
//! a raised `min_position_notional` bites new orders while leaving an open
//! position untouched.
//!
//! The delist-to-retirement wind-down (grace windows, terminal price, retire
//! clear-out) lives in `test_retirement.rs`. This suite covers only the plain
//! transition matrix and direct admin actions.

use market::{Config, MarketError, OrderKind, Status};
use test_suites::constants::BTC_PRICE;
use test_suites::setup::create_fixture_with_data;
use test_suites::test_fixture::TestFixture;
use test_suites::{default_config, SCALAR_18, SCALAR_7};

// ---- set_config: valid apply, owner gating ----

/// A valid single-field tweak lands and `get_config` reflects it, with the
/// other fields left untouched.
///
/// The published `ConfigUpdate` event is not asserted.
#[test]
fn set_config_valid_tweak_lands() {
    let fixture = TestFixture::create();

    // Raise the max position notional (still below the open-interest ceiling).
    // No other invariant bounds it from above, so the config stays valid.
    let raised_max = 2 * default_config().max_position_notional;
    let tweaked = Config {
        max_position_notional: raised_max,
        ..default_config()
    };
    fixture.market.set_config(&tweaked);

    let stored = fixture.market.get_config();
    assert_eq!(
        stored.max_position_notional, raised_max,
        "the tweaked field applied"
    );
    assert_eq!(
        stored.min_position_notional,
        default_config().min_position_notional,
        "an untouched field is unchanged",
    );
}

/// `set_config` is owner-gated: with the mocked auths cleared, the call traps on
/// the missing owner authorization.
#[test]
fn set_config_requires_owner_auth() {
    let fixture = TestFixture::create();
    let tweaked = Config {
        max_position_notional: 2 * default_config().max_position_notional,
        ..default_config()
    };

    // Drop the fixture's blanket auth mock: the `#[only_owner]` require_auth now
    // has nothing to satisfy it, so the call fails on authorization (a host auth
    // error, not a typed MarketError).
    fixture.env.set_auths(&[]);
    let result = fixture.market.try_set_config(&tweaked);
    assert!(result.is_err(), "set_config without owner auth is rejected");
    assert_eq!(
        fixture.market.get_config().max_position_notional,
        default_config().max_position_notional,
        "config unchanged after the rejected call",
    );
}

// ---- set_config: one rejection per invariant class (each mutating one field) ----

/// Assert a `set_config` with `config` traps with `expected`, via the generated
/// `try_` client so the typed error surfaces.
fn assert_set_config_rejects(
    fixture: &TestFixture,
    config: Config,
    expected: MarketError,
    label: &str,
) {
    let result = fixture.market.try_set_config(&config);
    assert_eq!(result, Err(Ok(expected.into())), "{}", label);
}

/// A profit-ladder ratio at the SCALAR_18 insolvency line (where strictly below
/// is required) rejects with `InvalidConfig` (700).
#[test]
fn set_config_rejects_ratio_at_scalar_ceiling() {
    let fixture = TestFixture::create();
    assert_set_config_rejects(
        &fixture,
        Config {
            max_pnl_trader: SCALAR_18,
            ..default_config()
        },
        MarketError::InvalidConfig,
        "max_pnl_trader at the insolvency line",
    );
}

/// A broken solvency ladder (`adl_max_pnl` above `max_pnl_trader`) rejects
/// with `InvalidConfig` (700).
#[test]
fn set_config_rejects_broken_profit_ladder() {
    let fixture = TestFixture::create();
    // ADL trigger 95% above the 90% trader haircut threshold.
    assert_set_config_rejects(
        &fixture,
        Config {
            adl_max_pnl: 95 * SCALAR_18 / 100,
            ..default_config()
        },
        MarketError::InvalidConfig,
        "ADL trigger above the trader haircut threshold",
    );
}

/// A redeem gate above the ADL clear target (`max_pnl_withdraw >
/// adl_clear_target`) rejects with `InvalidConfig` (700).
#[test]
fn set_config_rejects_redeem_gate_above_trader_haircut() {
    let fixture = TestFixture::create();
    // Redeem gate 95% above the 40% ADL clear target.
    assert_set_config_rejects(
        &fixture,
        Config {
            max_pnl_withdraw: 95 * SCALAR_18 / 100,
            ..default_config()
        },
        MarketError::InvalidConfig,
        "redeem gate above the trader haircut threshold",
    );
}

/// A vault fill fee above the shared fee-rate bound rejects with
/// `InvalidConfig` (700), on either the deposit or the redeem rate.
#[test]
fn set_config_rejects_vault_fill_fee_above_bound() {
    let fixture = TestFixture::create();
    // 2% sits above the 1% MAX_FEE_RATE bound.
    assert_set_config_rejects(
        &fixture,
        Config {
            deposit_fee: SCALAR_18 / 50,
            ..default_config()
        },
        MarketError::InvalidConfig,
        "deposit fill fee above the fee-rate bound",
    );
    assert_set_config_rejects(
        &fixture,
        Config {
            redeem_fee: SCALAR_18 / 50,
            ..default_config()
        },
        MarketError::InvalidConfig,
        "redeem fill fee above the fee-rate bound",
    );
}

/// A `min_deposit` above `max_vault_balance / 100` rejects with `InvalidConfig`
/// (700).
#[test]
fn set_config_rejects_min_deposit_above_vault_cap_fraction() {
    let fixture = TestFixture::create();
    // One unit past the default cap: 10_000_000_000_000 / 100 = 100_000_000_000.
    assert_set_config_rejects(
        &fixture,
        Config {
            min_deposit: 100_000_000_001,
            ..default_config()
        },
        MarketError::InvalidConfig,
        "min_deposit above the vault-cap fraction",
    );
}

/// A negative rate rejects with `NegativeValueNotAllowed` (710).
#[test]
fn set_config_rejects_negative_value() {
    let fixture = TestFixture::create();
    assert_set_config_rejects(
        &fixture,
        Config {
            keeper_rate: -1,
            ..default_config()
        },
        MarketError::NegativeValueNotAllowed,
        "negative keeper rate",
    );
}

// ---- set_status: transition matrix ----

/// The live statuses `Active`, `OnIce`, and `Frozen` interchange freely, and
/// `get_status` reflects each set.
#[test]
fn set_status_live_statuses_interchange() {
    let fixture = TestFixture::create();
    assert_eq!(
        fixture.market.get_status(),
        Status::Active as u32,
        "starts Active"
    );

    fixture.market.set_status(&(Status::OnIce as u32));
    assert_eq!(
        fixture.market.get_status(),
        Status::OnIce as u32,
        "Active -> OnIce"
    );

    fixture.market.set_status(&(Status::Active as u32));
    assert_eq!(
        fixture.market.get_status(),
        Status::Active as u32,
        "OnIce -> Active"
    );

    fixture.market.set_status(&(Status::Frozen as u32));
    assert_eq!(
        fixture.market.get_status(),
        Status::Frozen as u32,
        "Active -> Frozen"
    );

    fixture.market.set_status(&(Status::Active as u32));
    assert_eq!(
        fixture.market.get_status(),
        Status::Active as u32,
        "Frozen -> Active"
    );
}

/// A same-status set is an operator error and rejects with `InvalidStatus`
/// (702).
#[test]
fn set_status_same_status_rejects_invalid_status() {
    let fixture = TestFixture::create();
    let result = fixture.market.try_set_status(&(Status::Active as u32));
    assert_eq!(
        result,
        Err(Ok(MarketError::InvalidStatus.into())),
        "Active -> Active is rejected"
    );
}

// ---- set_status: behavioral effects ----

/// While `Frozen`, `create_order` traps with `MarketFrozen` (704).
#[test]
fn frozen_blocks_create_order() {
    let mut fixture = TestFixture::create();
    let user = fixture.new_user(0);

    fixture.market.set_status(&(Status::Frozen as u32));

    // A well-shaped decrease order still cannot be created on a frozen market.
    let result = fixture.market.try_create_order(
        &user,
        &true,
        &(OrderKind::MarketDecrease as u32),
        &default_config().min_order_notional,
        &0,
        &0,
        &0,
        &fixture.far_expiration(),
    );
    assert_eq!(
        result,
        Err(Ok(MarketError::MarketFrozen.into())),
        "create blocked while Frozen"
    );
}

/// While `OnIce`, an increase fill is halted (`IncreaseHalted`, 705) but a
/// decrease on an open position still fills.
#[test]
fn on_ice_blocks_opens_but_allows_closes() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(1_000 * SCALAR_7);

    // Open a long while the market is Active.
    let notional = 1_000 * SCALAR_7; // $1,000
    let margin = 200 * SCALAR_7; // $200 -> 5x
    let price = fixture.price(BTC_PRICE);
    fixture.open(&user, true, notional, margin, &price);
    assert_eq!(
        fixture.position(&user, true).notional,
        notional,
        "long opened"
    );

    fixture.market.set_status(&(Status::OnIce as u32));

    // Opens are halted: a fresh increase order can rest, but its fill traps.
    // Its margin was already escrowed when the order was created.
    let increase_id = fixture.submit_order(
        &user,
        true,
        OrderKind::MarketIncrease,
        notional,
        margin,
        0,
        0,
        fixture.far_expiration(),
    );
    let halted = fixture
        .market
        .try_execute_order(&user, &user, &increase_id, &price);
    assert_eq!(
        halted,
        Err(Ok(MarketError::IncreaseHalted.into())),
        "opens halted while OnIce"
    );

    // Closes still work: past the decrease lock, a partial close reduces the
    // position.
    fixture.jump(60);
    let close_notional = 500 * SCALAR_7;
    let later_price = fixture.price(BTC_PRICE);
    fixture.close(&user, true, close_notional, 0, &later_price);
    assert_eq!(
        fixture.position(&user, true).notional,
        notional - close_notional,
        "close fills while OnIce",
    );
}

// ---- config change applies to live behavior ----

/// Raising `min_position_notional` rejects a new order landing below the new
/// floor (`NotionalBelowMinimum`, 711) while leaving an already-open position
/// untouched.
#[test]
fn raising_min_position_notional_rejects_new_small_order_leaves_existing_untouched() {
    let mut fixture = create_fixture_with_data();
    let holder = fixture.new_user(1_000 * SCALAR_7);

    // An open position sized above the default floor but below the new one.
    let held_notional = 1_000 * SCALAR_7; // $1,000
    let price = fixture.price(BTC_PRICE);
    fixture.open(&holder, true, held_notional, 200 * SCALAR_7, &price);
    assert_eq!(
        fixture.position(&holder, true).notional,
        held_notional,
        "position opened"
    );

    // Raise the floor to $5,000: valid (still below max_position_notional and
    // above min_order_notional).
    let raised_floor = 5_000 * SCALAR_7;
    fixture.market.set_config(&Config {
        min_position_notional: raised_floor,
        ..default_config()
    });

    // The open position is untouched by the config change.
    assert_eq!(
        fixture.position(&holder, true).notional,
        held_notional,
        "existing position unaffected by the raised floor",
    );

    // A new $2,000 order clears the dust and margin gates but its resulting
    // position sits under the new floor, so the fill rejects.
    let newcomer = fixture.new_user(1_000 * SCALAR_7);
    let small_notional = 2_000 * SCALAR_7; // $2,000, under the $5,000 floor
    let small_id = fixture.submit_order(
        &newcomer,
        true,
        OrderKind::MarketIncrease,
        small_notional,
        400 * SCALAR_7, // $400 -> 5x, well above the 10% init margin
        0,
        0,
        fixture.far_expiration(),
    );
    let rejected = fixture
        .market
        .try_execute_order(&newcomer, &newcomer, &small_id, &price);
    assert_eq!(
        rejected,
        Err(Ok(MarketError::NotionalBelowMinimum.into())),
        "a new order below the raised floor is rejected",
    );
}
