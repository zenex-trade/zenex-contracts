//! Integration coverage for the keeper-order surface: `create_order` storage and
//! id allocation, `cancel_order`, the creation-time rejection matrix, and a
//! resting trigger order's expiration boundary at fill.
//!
//! Creation is price-free (trader auth only), so the rejection cases run on a bare
//! fixture. The fill case seeds vault liquidity first.

use soroban_sdk::Address;

use market::{MarketError, OrderKind};
use test_suites::constants::{
    BTC_PRICE, EXEC_FEE, MIN_ORDER_MARGIN, MIN_ORDER_NOTIONAL, PRICE_SCALAR,
};
use test_suites::test_fixture::TestFixture;
use test_suites::SCALAR_7;

// ---- helpers ----

/// Assert that a direct `create_order` traps with `expected`, using the generated
/// `try_` client so the typed error surfaces instead of unwinding the test.
#[allow(clippy::too_many_arguments)]
fn assert_create_rejects(
    fixture: &TestFixture,
    user: &Address,
    is_long: bool,
    kind: OrderKind,
    notional: i128,
    margin: i128,
    trigger_price: i128,
    price_bound: i128,
    expiration: u32,
    expected: MarketError,
    label: &str,
) {
    let result = fixture.market.try_create_order(
        user,
        &is_long,
        &(kind as u32),
        &notional,
        &margin,
        &trigger_price,
        &price_bound,
        &expiration,
    );
    assert_eq!(result, Err(Ok(expected.into())), "{}", label);
}

// ---- create_order storage and id allocation ----

/// Every submitted field round-trips through storage unchanged, and `created_at`
/// is stamped from the ledger clock.
#[test]
fn create_order_stores_submitted_fields() {
    let mut fixture = TestFixture::create();

    // A stop-increase (short side: fills on the downside) with distinctive,
    // mutually independent fields.
    let notional = 3 * MIN_ORDER_NOTIONAL;
    let margin = 2 * MIN_ORDER_MARGIN;
    let user = fixture.new_user(margin + EXEC_FEE);
    let trigger_price = 123_456 * PRICE_SCALAR;
    let price_bound = 110_000 * PRICE_SCALAR;
    let expiration = fixture.far_expiration();
    let created_at = fixture.env.ledger().timestamp();

    let id = fixture.submit_order(
        &user,
        false,
        OrderKind::StopIncrease,
        notional,
        margin,
        trigger_price,
        price_bound,
        expiration,
    );
    assert_eq!(id, 1, "first order id for a fresh user is 1");

    let order = fixture.market.get_order(&user, &id);
    assert!(!order.is_long, "is_long");
    assert_eq!(order.kind, OrderKind::StopIncrease as u32, "kind");
    assert_eq!(order.notional, notional, "notional");
    assert_eq!(order.margin, margin, "margin");
    assert_eq!(order.trigger_price, trigger_price, "trigger_price");
    assert_eq!(order.price_bound, price_bound, "price_bound");
    assert_eq!(order.expiration, expiration, "expiration");
    assert_eq!(
        order.created_at, created_at,
        "created_at stamped from the ledger"
    );
}

/// Ids increment per user: a user's second order is 2, and a distinct user's
/// counter starts fresh at 1. `get_order_counter` tracks the next unallocated
/// id throughout.
#[test]
fn order_ids_increment_per_user() {
    let mut fixture = TestFixture::create();
    let first_user = fixture.new_user(2 * EXEC_FEE);
    let second_user = fixture.new_user(EXEC_FEE);

    let expiration = fixture.far_expiration();
    let increase = |user: &Address| {
        fixture.submit_order(
            user,
            true,
            OrderKind::MarketIncrease,
            MIN_ORDER_NOTIONAL,
            0,
            0,
            0,
            expiration,
        )
    };

    assert_eq!(
        fixture.market.get_order_counter(&first_user),
        1,
        "a fresh user's next id is 1",
    );
    assert_eq!(increase(&first_user), 1, "first user's first id");
    assert_eq!(
        increase(&first_user),
        2,
        "first user's second id increments"
    );
    assert_eq!(
        increase(&second_user),
        1,
        "second user's counter is independent"
    );
    assert_eq!(
        fixture.market.get_order_counter(&first_user),
        3,
        "two allocations advance the next id to 3",
    );
    assert_eq!(
        fixture.market.get_order_counter(&second_user),
        2,
        "counters are per user",
    );
}

// ---- cancel_order ----

/// A cancel removes the pending order, returning the refunded escrow (the
/// exec_fee alone on a margin-free increase). The subsequent read reports
/// it gone.
#[test]
fn cancel_order_removes_the_order() {
    let mut fixture = TestFixture::create();
    let user = fixture.new_user(EXEC_FEE);

    let id = fixture.submit_order(
        &user,
        true,
        OrderKind::MarketIncrease,
        MIN_ORDER_NOTIONAL,
        0,
        0,
        0,
        fixture.far_expiration(),
    );
    let refund = fixture.market.cancel_order(&user, &id);
    assert_eq!(refund, EXEC_FEE, "the cancel returns the refunded escrow");

    assert!(
        fixture.market.try_get_order(&user, &id).is_err(),
        "the cancelled order no longer exists",
    );
}

/// Cancelling an id that was never allocated traps with `OrderNotFound` (730).
#[test]
fn cancel_missing_order_rejects_with_order_not_found() {
    let mut fixture = TestFixture::create();
    let user = fixture.new_user(0);

    let result = fixture.market.try_cancel_order(&user, &99);
    assert_eq!(result, Err(Ok(MarketError::OrderNotFound.into())));
}

// ---- creation-time rejection matrix ----

/// A negative magnitude is rejected up front with `NegativeValueNotAllowed` (710),
/// for either the notional or the margin leg.
#[test]
fn create_order_rejects_negative_magnitudes() {
    let mut fixture = TestFixture::create();
    let user = fixture.new_user(0);
    let expiration = fixture.far_expiration();

    assert_create_rejects(
        &fixture,
        &user,
        true,
        OrderKind::MarketIncrease,
        -1,
        2 * MIN_ORDER_MARGIN,
        0,
        0,
        expiration,
        MarketError::NegativeValueNotAllowed,
        "negative notional",
    );
    assert_create_rejects(
        &fixture,
        &user,
        true,
        OrderKind::MarketIncrease,
        2 * MIN_ORDER_NOTIONAL,
        -1,
        0,
        0,
        expiration,
        MarketError::NegativeValueNotAllowed,
        "negative margin",
    );
}

/// A moved value inside the open interval `(0, floor)` is dust and rejects with
/// `InvalidOrder` (732), for both the notional and the margin floor.
#[test]
fn create_order_rejects_values_below_dust_floor() {
    let mut fixture = TestFixture::create();
    let user = fixture.new_user(0);
    let expiration = fixture.far_expiration();

    assert_create_rejects(
        &fixture,
        &user,
        true,
        OrderKind::MarketIncrease,
        MIN_ORDER_NOTIONAL - 1,
        2 * MIN_ORDER_MARGIN,
        0,
        0,
        expiration,
        MarketError::InvalidOrder,
        "notional one unit below its floor",
    );
    assert_create_rejects(
        &fixture,
        &user,
        true,
        OrderKind::MarketIncrease,
        2 * MIN_ORDER_NOTIONAL,
        MIN_ORDER_MARGIN - 1,
        0,
        0,
        expiration,
        MarketError::InvalidOrder,
        "margin one unit below its floor",
    );
}

/// An order moving neither size nor margin is a no-op and rejects with
/// `InvalidOrder` (732).
#[test]
fn create_order_rejects_noop_shape() {
    let mut fixture = TestFixture::create();
    let user = fixture.new_user(0);

    assert_create_rejects(
        &fixture,
        &user,
        true,
        OrderKind::MarketIncrease,
        0,
        0,
        0,
        0,
        fixture.far_expiration(),
        MarketError::InvalidOrder,
        "no-op: both magnitudes zero",
    );
}

/// An expiration already behind the current ledger sequence rejects with
/// `OrderExpired` (731).
#[test]
fn create_order_rejects_expiration_behind_ledger() {
    let mut fixture = TestFixture::create();
    let user = fixture.new_user(0);

    // The fixture opens at sequence 100, so 99 is one ledger in the past.
    let stale_expiration = fixture.env.ledger().sequence() - 1;
    assert_create_rejects(
        &fixture,
        &user,
        true,
        OrderKind::MarketIncrease,
        2 * MIN_ORDER_NOTIONAL,
        2 * MIN_ORDER_MARGIN,
        0,
        0,
        stale_expiration,
        MarketError::OrderExpired,
        "expiration one ledger behind now",
    );
}

/// A decrease with no position rests at creation (a pre-placed TP/SL is
/// legal), and its fill rejects with `PositionNotFound` (720).
#[test]
fn decrease_without_position_rests_then_fill_rejects() {
    let mut fixture = TestFixture::create();
    let user = fixture.new_user(EXEC_FEE);
    let price = fixture.price(BTC_PRICE);

    let attempt = fixture.create_and_try_fill(
        &user,
        &user,
        true,
        OrderKind::MarketDecrease as u32,
        MIN_ORDER_NOTIONAL,
        0,
        0,
        0,
        fixture.far_expiration(),
        &price,
    );

    assert!(!attempt.filled, "the fill leg rejects without a position");
    assert_eq!(
        attempt.error,
        MarketError::PositionNotFound as u32,
        "720 at fill"
    );
    // The order itself rested and remains fillable once a position exists.
    assert_eq!(
        fixture.market.get_order(&user, &attempt.id).notional,
        MIN_ORDER_NOTIONAL,
        "the decrease order rests"
    );
}

// ---- resting trigger order expiration boundary ----

/// A resting increase trigger order fills while the ledger is at or before its
/// expiration, and a sibling order with an already-passed expiration is rejected
/// with `OrderExpired` (731) at fill.
#[test]
fn resting_trigger_order_fillable_until_expiration_then_expired() {
    let mut fixture = TestFixture::create();
    fixture.seed_vault(1_000_000 * SCALAR_7);
    let user = fixture.new_user(1_000 * SCALAR_7);

    let notional = 1_000 * SCALAR_7; // $1,000
    let margin = 200 * SCALAR_7; // $200 -> 5x
                                 // A buy-stop already crossed: a long StopIncrease fills on the upside, and
                                 // the ask ($100k) sits at or above a $90k trigger.
    let trigger_price = 90_000 * PRICE_SCALAR;

    // Order A rests three ledgers ahead of its expiration, then fills after a jump
    // that stays before it.
    let fill_expiration = fixture.env.ledger().sequence() + 3;
    let order_a = fixture.submit_order(
        &user,
        true,
        OrderKind::StopIncrease,
        notional,
        margin,
        trigger_price,
        0,
        fill_expiration,
    );
    fixture.jump(10); // +2 ledgers: sequence now fill_expiration - 1, still fillable
    assert!(
        fixture.env.ledger().sequence() <= fill_expiration,
        "the fill runs at or before expiration",
    );
    let price = fixture.price(BTC_PRICE);
    let payout = fixture.execute_order(&user, &user, order_a, &price);
    assert!(payout > 0, "the in-window fill paid the keeper");
    assert_eq!(
        fixture.position(&user, true).notional,
        notional,
        "the filled order opened the long",
    );

    // Order B is submitted fresh, then a longer jump carries the sequence past its
    // expiration before the keeper attempts the fill.
    let expire_expiration = fixture.env.ledger().sequence() + 3;
    let order_b = fixture.submit_order(
        &user,
        true,
        OrderKind::StopIncrease,
        notional,
        margin,
        trigger_price,
        0,
        expire_expiration,
    );
    fixture.jump(20); // +4 ledgers: sequence now expire_expiration + 1, past expiry
    assert!(
        fixture.env.ledger().sequence() > expire_expiration,
        "the fill runs after expiration",
    );
    let later_price = fixture.price(BTC_PRICE);
    let result = fixture
        .market
        .try_execute_order(&user, &user, &order_b, &later_price);
    assert_eq!(
        result,
        Err(Ok(MarketError::OrderExpired.into())),
        "a fill past expiration is rejected",
    );
}
