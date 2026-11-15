//! Price timing: every route that consumes a signed price, pinned against
//! every clock the contract keeps.
//!
//! The oracle accepts any report inside a 15-second window on either side
//! of the ledger clock, so the submitter chooses which price in that window a
//! call executes at. What bounds that choice, per route:
//!
//! ```text
//!                        age      order      position   effective price
//!                      ceiling  created_at     floor
//! accrue                  Y          -           -      newest (substitution)
//! execute_order           Y          Y (*)       Y      as submitted
//! execute_liquidation     Y          -           Y      newest (substitution)
//! update_adl_state        Y          -           -      newest (substitution)
//! execute_adl             Y          -           Y      newest (substitution)
//! execute_vault_order     Y          Y (>=,+L)   -      newest (substitution)
//! ```
//!
//! `(*)` waived when a market kind fills in its own creation ledger. This is
//! the atomic create-and-fill exemption. `(>=,+L)` price at or past the
//! commitment, plus a structural later-ledger rule.
//!
//! The clocks themselves:
//! - The **price cache** (temporary storage) holds the newest verified price
//!   the market has consumed, advanced monotonically by `Market::load`. A
//!   substitution route executes at `max(submitted, cached)`: a superseded
//!   payload is upgraded, never rejected, so keeper payload choice cannot
//!   reach back past the market's own mark. The entry outlives the close
//!   window by construction, so a lapsed cache (a day of silence) falls
//!   back to the ceiling alone with nothing left in the window to reach.
//! - `priced_at` (per position, seconds) is the price floor every fill
//!   and force-close is measured against. The `execute_order` guard keeps it
//!   monotone.
//!
//! The one deliberate residual: `execute_order` prices as submitted, so a
//! look-back inside the window still fills (triggers and bounds evaluate
//! against the payload). The window is the dial that bounds it, and the
//! base + impact fees dominate the residual band.
//!
//! # Error surface
//!
//! The oracle rejects outside the window with its own codes: `PriceStale`
//! (782) behind and `PriceAhead` (793) ahead. The clock guards in the market
//! raise `MarketError::StalePrice` (740).
//!
//! Shared anchors: 7-dec settlement token, BTC feed priced in 1e18 units (so
//! `$100k` is a raw `1e23`), oracle at `trade_staleness = 15` seconds for
//! fills / `close_staleness = 60` for the gap-closing routes,
//! `notional_lock = 30`.

use soroban_sdk::{Address, Bytes, ConversionError, Error, InvokeError};

use market::storage::DataKey;
use market::{Config, MarketError, OrderKind};
use oracle::{OracleError, PriceData};
use test_suites::chainlink_helper::ReportTimes;
use test_suites::constants::{PRICE_SCALAR, SCALAR_7};
use test_suites::setup::create_fixture_with_data;
use test_suites::test_fixture::TestFixture;
use test_suites::{default_config, SCALAR_18};

/// One dollar of the 7-decimal settlement token (token-dec).
const DOLLAR: i128 = SCALAR_7;

/// Notional for the standard position under test: `$1,000` (token-dec).
const NOTIONAL: i128 = 1_000 * DOLLAR;
/// Margin for the standard position under test: `$200` (token-dec), i.e. 5x.
const MARGIN: i128 = 200 * DOLLAR;

/// The strict trade-class staleness window in seconds (fills), and the
/// forward allowance shared by both classes.
const WINDOW: u64 = 15;

/// `notional_lock` in the default config (seconds): newly added notional cannot
/// be decreased for this long. Deliberately longer than the window, so a
/// single accepted stale price can never open and close the same size.
const NOTIONAL_LOCK: u64 = 30;

/// A report at raw `price` (dollars * 1e18) observed `age` seconds before the
/// current ledger timestamp, with `bid == ask == price`.
fn payload_aged(fixture: &TestFixture, price: i128, age: u64) -> Bytes {
    payload_at_time(fixture, price, fixture.env.ledger().timestamp() - age)
}

/// The same report observed `skew` seconds *after* the current ledger
/// timestamp. It is a price stamped in the future relative to the ledger
/// consuming it, accepted up to the window (the ledger clock trails the
/// oracle's).
fn payload_future(fixture: &TestFixture, price: i128, skew: u64) -> Bytes {
    payload_at_time(fixture, price, fixture.env.ledger().timestamp() + skew)
}

/// A report at raw `price` stamped with an absolute `published` timestamp
/// (seconds).
fn payload_at_time(fixture: &TestFixture, price: i128, published: u64) -> Bytes {
    fixture.report_at(ReportTimes::at(published), price, price, price)
}

/// Raw feed price for `dollars` whole USD at [`PRICE_SCALAR`].
fn usd(dollars: i128) -> i128 {
    dollars * PRICE_SCALAR
}

/// The price cache's publish time (seconds), or 0 when absent or lapsed.
fn market_clock(fixture: &TestFixture) -> u64 {
    fixture.env.as_contract(&fixture.market.address, || {
        fixture
            .env
            .storage()
            .temporary()
            .get::<_, PriceData>(&DataKey::PriceCache)
            .map_or(0, |p| p.publish_time)
    })
}

/// Pin a `try_` rejection to a specific contract error.
///
/// `accrue` and `update_adl_state` return `MarketData`/`AdlState`, neither of
/// which is `PartialEq`, so their results cannot be compared with `assert_eq!`
/// the way the `i128`-returning entry points are. This matches on the error arm
/// instead.
fn assert_rejected<T>(
    result: Result<Result<T, ConversionError>, Result<Error, InvokeError>>,
    expected: Error,
    context: &str,
) {
    match result {
        Err(Ok(actual)) => assert_eq!(actual, expected, "{context}"),
        Err(Err(_)) => panic!("{context}: expected {expected:?}, got a host error"),
        Ok(_) => panic!("{context}: expected {expected:?}, but the call succeeded"),
    }
}

// ---------------------------------------------------------------------------
// 1. The clocks themselves
// ---------------------------------------------------------------------------

/// The price cache's write semantics, which substitution relies on: a
/// price-consuming call advances it to that payload's publish time. The
/// advance is a maximum rather than an assignment, so a later call at an
/// older-but-valid price cannot drag it back.
///
/// `accrue` is the vehicle because it consumes a price while moving no
/// position, isolating the cache from any fill bookkeeping.
#[test]
fn price_cache_advances_to_the_newest_price_consumed_and_no_further() {
    let fixture = create_fixture_with_data();
    fixture.jump(60);

    let now = fixture.env.ledger().timestamp();
    assert!(
        market_clock(&fixture) < now,
        "the cache starts behind this ledger"
    );

    fixture
        .market
        .accrue(&payload_aged(&fixture, usd(100_000), 0));
    assert_eq!(
        market_clock(&fixture),
        now,
        "advanced to the consumed payload"
    );

    // A 10s-old payload is inside the staleness window, so the verifier accepts
    // it and the accrual lands. The cache does not follow it back.
    fixture
        .market
        .accrue(&payload_aged(&fixture, usd(100_000), 10));
    assert_eq!(market_clock(&fixture), now, "held at the newer price");
}

/// A fill stamps the position's `priced_at` with the payload's publish time,
/// not with the ledger timestamp. The two differ by exactly the payload's age,
/// which is what makes `priced_at` comparable to a later payload's publish
/// time rather than to wall clock.
#[test]
fn fill_stamps_priced_at_with_the_payload_publish_time() {
    let mut fixture = create_fixture_with_data();
    fixture.jump(60);

    let user = fixture.new_user(10_000 * DOLLAR);
    let published = fixture.env.ledger().timestamp() - 10;
    let aged = payload_aged(&fixture, usd(100_000), 10);
    fixture.open(&user, true, NOTIONAL, MARGIN, &aged);

    assert_eq!(
        fixture.position(&user, true).priced_at,
        published,
        "priced_at carries the payload's publish time, not the ledger's",
    );
}

// ---------------------------------------------------------------------------
// 2. `execute_order`: as-submitted by design, floored by `priced_at`
// ---------------------------------------------------------------------------

/// The accepted residual: a resting decrease fills at a payload the market has
/// already superseded, and pays more than the live close.
///
/// This is by design. The order's triggers and bounds evaluate against the
/// payload, so `execute_order` prices as submitted. The payload must still
/// postdate the order and the position's own floor, so the reach-back is
/// bounded by the staleness window (the config dial). The base + impact fees
/// dominate the residual band.
#[test]
fn order_decrease_look_back_inside_the_window_still_pays_more() {
    let mut fixture = create_fixture_with_data();
    let honest = fixture.new_user(10_000 * DOLLAR);
    let opportunist = fixture.new_user(10_000 * DOLLAR);

    let open_price = fixture.price(usd(100_000));
    fixture.open(&honest, true, NOTIONAL, MARGIN, &open_price);
    fixture.open(&opportunist, true, NOTIONAL, MARGIN, &open_price);

    // Past the notional lock, both rest a full close.
    fixture.jump(NOTIONAL_LOCK * 2);
    let honest_id = fixture.submit_order(
        &honest,
        true,
        OrderKind::MarketDecrease,
        i128::MAX,
        0,
        0,
        0,
        fixture.far_expiration(),
    );
    let opportunist_id = fixture.submit_order(
        &opportunist,
        true,
        OrderKind::MarketDecrease,
        i128::MAX,
        0,
        0,
        0,
        fixture.far_expiration(),
    );

    // Ten seconds later the market consumes a fresh $99k mark.
    fixture.jump(10);
    let live = payload_aged(&fixture, usd(99_000), 0);
    fixture.market.accrue(&live);

    // The honest close takes the live price.
    let honest_before = fixture.balance(&honest);
    fixture.execute_order(&fixture.keeper, &honest, honest_id, &live);
    let honest_payout = fixture.balance(&honest) - honest_before;

    // The opportunist reaches back 10 seconds for a $101k print. It postdates
    // its own order and the position's floor, and sits inside the window, so
    // it fills at the submitted price.
    let stale = payload_aged(&fixture, usd(101_000), 10);
    let opportunist_before = fixture.balance(&opportunist);
    fixture.execute_order(&fixture.keeper, &opportunist, opportunist_id, &stale);
    let stale_payout = fixture.balance(&opportunist) - opportunist_before;

    assert!(
        stale_payout > honest_payout,
        "the look-back pays more than the live close: {stale_payout} vs {honest_payout}",
    );
    assert_eq!(
        fixture.position(&opportunist, true).notional,
        0,
        "the stale close landed"
    );
}

/// The same residual on the way in: a resting increase fills at a superseded
/// payload, buying at a stale low instead of the live price. Same bound, same
/// design decision.
#[test]
fn order_increase_look_back_inside_the_window_still_lands() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(10_000 * DOLLAR);

    fixture.jump(60);
    let id = fixture.submit_order(
        &user,
        true,
        OrderKind::MarketIncrease,
        NOTIONAL,
        MARGIN,
        0,
        0,
        fixture.far_expiration(),
    );

    fixture.jump(10);
    fixture
        .market
        .accrue(&payload_aged(&fixture, usd(101_000), 0));

    // Reach back for the cheaper $99k entry: inside the window, postdating the
    // order, no position floor yet. It lands.
    let stale = payload_aged(&fixture, usd(99_000), 10);
    fixture.execute_order(&fixture.keeper, &user, id, &stale);
    assert_eq!(
        fixture.position(&user, true).notional,
        NOTIONAL,
        "the stale entry landed"
    );
}

/// `priced_at` is monotone: a fill carrying a payload older than the
/// position's own mark is rejected, so the force-close floor cannot be
/// dragged backwards through a resting increase.
#[test]
fn stale_fill_can_no_longer_drag_the_position_floor_backwards() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(20_000 * DOLLAR);
    fixture.jump(60);

    // A resting increase, created before the position exists.
    let resting = fixture.submit_order(
        &user,
        true,
        OrderKind::MarketIncrease,
        NOTIONAL,
        MARGIN,
        0,
        0,
        fixture.far_expiration(),
    );

    // Ten seconds on, open the side at a fresh price: the floor is set here.
    fixture.jump(10);
    fixture.open(
        &user,
        true,
        NOTIONAL,
        MARGIN,
        &payload_aged(&fixture, usd(100_000), 0),
    );
    let floor = fixture.position(&user, true).priced_at;

    // Filling the resting increase with a payload from before the open
    // postdates the order's creation but not the floor: rejected.
    let backdated = payload_aged(&fixture, usd(100_000), 10);
    assert_eq!(
        fixture
            .market
            .try_execute_order(&fixture.keeper, &user, &resting, &backdated),
        Err(Ok(MarketError::StalePrice.into())),
        "a fill behind the position's own mark is rejected",
    );
    assert_eq!(
        fixture.position(&user, true).priced_at,
        floor,
        "the floor did not move"
    );
}

// ---------------------------------------------------------------------------
// 3. The position floor and liquidation under substitution
// ---------------------------------------------------------------------------

/// On an empty side there is no `priced_at` to compare against: the field reads
/// zero, so the floor is vacuous and any price the verifier accepts is
/// acceptable to it. Whatever price the first fill picks becomes the floor
/// every later fill and force-close is measured against.
#[test]
fn empty_side_has_no_price_floor_so_the_first_fill_sets_it() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(10_000 * DOLLAR);
    fixture.jump(60);

    assert_eq!(
        fixture.position(&user, true).priced_at,
        0,
        "an unopened side carries a vacuous floor",
    );

    // The market has already consumed a fresh price...
    fixture
        .market
        .accrue(&payload_aged(&fixture, usd(100_000), 0));
    let clock = market_clock(&fixture);

    // ...yet the first fill may still be priced 12 seconds back (the order
    // residual above), and that becomes the floor.
    let aged = payload_aged(&fixture, usd(100_000), 12);
    let published = fixture.env.ledger().timestamp() - 12;
    fixture.open(&user, true, NOTIONAL, MARGIN, &aged);

    assert!(
        published < clock,
        "the opening mark predates the market clock"
    );
    assert_eq!(
        fixture.position(&user, true).priced_at,
        published,
        "and it is now the floor for every later fill and force-close",
    );
}

/// A keeper cannot force-close at a superseded crash print: the stale payload
/// is upgraded to the newest price the market has seen, and at that price the
/// position is healthy. A position that breached and recovered escapes on the
/// fresh evidence.
#[test]
fn liquidation_upgrades_a_stale_payload_to_the_newest_price() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(10_000 * DOLLAR);
    fixture.jump(60);

    fixture.open(
        &user,
        true,
        NOTIONAL,
        MARGIN,
        &payload_aged(&fixture, usd(100_000), 0),
    );

    // The market consumes a fresh $100k mark. A crash print from 3 seconds
    // earlier would liquidate the position if it priced the close.
    fixture.jump(5);
    fixture
        .market
        .accrue(&payload_aged(&fixture, usd(100_000), 0));
    let stale_crash = payload_aged(&fixture, usd(84_000), 3);

    assert_eq!(
        fixture
            .market
            .try_execute_liquidation(&fixture.keeper, &user, &true, &stale_crash),
        Err(Ok(MarketError::NotLiquidatable.into())),
        "the superseded crash print is upgraded, and the position is healthy at the upgrade",
    );
    assert_eq!(
        fixture.position(&user, true).notional,
        NOTIONAL,
        "the position survives"
    );
}

/// The positive control: when the submitted payload *is* the newest price the
/// market has seen, the liquidation executes at it.
#[test]
fn liquidation_lands_when_the_submitted_payload_is_the_newest() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(10_000 * DOLLAR);
    fixture.jump(60);

    fixture.open(
        &user,
        true,
        NOTIONAL,
        MARGIN,
        &payload_aged(&fixture, usd(100_000), 0),
    );

    fixture.jump(5);
    let crash = payload_aged(&fixture, usd(84_000), 0);
    fixture
        .market
        .execute_liquidation(&fixture.keeper, &user, &true, &crash);
    assert_eq!(
        fixture.position(&user, true).notional,
        0,
        "liquidated at the newest price"
    );
}

/// The intra-ledger shuffle is bounded the same way: once a crash print has
/// been consumed this ledger, a liquidation submitted with an older, kinder
/// payload still prices at the crash. Worst-price selection dies on
/// settlement as well as eligibility.
#[test]
fn same_ledger_liquidation_prices_at_the_just_consumed_mark() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(10_000 * DOLLAR);
    fixture.jump(60);

    fixture.open(
        &user,
        true,
        NOTIONAL,
        MARGIN,
        &payload_aged(&fixture, usd(100_000), 0),
    );

    // Same ledger: the market consumes the crash, then a keeper submits a 3s-old
    // $100k payload. Substitution executes the close at the crash mark.
    fixture.jump(5);
    fixture
        .market
        .accrue(&payload_aged(&fixture, usd(84_000), 0));
    let kind_but_old = payload_aged(&fixture, usd(100_000), 3);
    fixture
        .market
        .execute_liquidation(&fixture.keeper, &user, &true, &kind_but_old);
    assert_eq!(
        fixture.position(&user, true).notional,
        0,
        "the older payload was upgraded to the crash and the close landed",
    );
}

/// The cache outlives the widest close window, 120 seconds. The fixture's
/// network-minimum temporary lifetime is 16 ledgers, shorter than that
/// window, so a write that only took the minimum would lapse here and let a
/// superseded report back in.
#[test]
fn cache_survives_a_quiet_stretch_longer_than_the_network_minimum() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(10_000 * DOLLAR);
    fixture.jump(60);

    fixture.open(
        &user,
        true,
        NOTIONAL,
        MARGIN,
        &payload_aged(&fixture, usd(100_000), 0),
    );
    let marked = market_clock(&fixture);

    // 24 ledgers of silence: past the network minimum, inside the widest
    // close window.
    fixture.jump(120);
    assert_eq!(market_clock(&fixture), marked, "the cache is still live");
}

/// A lapsed cache is safe: after a day of silence the temporary entry has
/// expired, and a liquidation prices at the submitted payload under the
/// ceiling alone. The fallback never blocks a real liquidation.
#[test]
fn lapsed_cache_still_liquidates_at_the_ceiling() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(10_000 * DOLLAR);
    fixture.jump(60);

    fixture.open(
        &user,
        true,
        NOTIONAL,
        MARGIN,
        &payload_aged(&fixture, usd(100_000), 0),
    );

    // A day and change of silence: past the cache's TTL, no price consumed.
    fixture.jump(90_000);
    assert_eq!(market_clock(&fixture), 0, "the cache has lapsed");

    let crash = payload_aged(&fixture, usd(84_000), 0);
    fixture
        .market
        .execute_liquidation(&fixture.keeper, &user, &true, &crash);
    assert_eq!(
        fixture.position(&user, true).notional,
        0,
        "liquidated on the fallback path"
    );
}

// ---------------------------------------------------------------------------
// 4. Vault orders: commit-then-execute
// ---------------------------------------------------------------------------

/// The price check is inclusive: a payload stamped at the commitment instant
/// fills, because a price published at the commit ledger's close was not
/// knowable to the committer, who signed before the close.
#[test]
fn vault_fill_accepts_a_payload_at_the_commitment_instant() {
    let mut fixture = TestFixture::create();
    let user = fixture.new_user(10_000 * DOLLAR);
    fixture.jump(60);

    let created_at = fixture.env.ledger().timestamp();
    let id = fixture
        .market
        .create_vault_order(&user, &0, &(1_000 * DOLLAR), &0);

    fixture.jump(10);
    let at_creation = payload_at_time(&fixture, usd(100_000), created_at);
    assert!(
        fixture
            .market
            .try_execute_vault_order(&fixture.keeper, &user, &id, &at_creation)
            .is_ok(),
        "a payload at the commitment instant fills",
    );
    assert!(
        fixture.market.try_get_vault_order(&user, &id).is_err(),
        "the filled order is consumed",
    );
}

/// A payload published strictly before the commitment is rejected.
#[test]
fn vault_fill_rejects_a_payload_predating_the_commitment() {
    let mut fixture = TestFixture::create();
    let user = fixture.new_user(10_000 * DOLLAR);
    fixture.jump(60);

    let created_at = fixture.env.ledger().timestamp();
    let id = fixture
        .market
        .create_vault_order(&user, &0, &(1_000 * DOLLAR), &0);

    fixture.jump(10);
    let before = payload_at_time(&fixture, usd(100_000), created_at - 1);
    assert_eq!(
        fixture
            .market
            .try_execute_vault_order(&fixture.keeper, &user, &id, &before),
        Err(Ok(MarketError::StalePrice.into())),
        "a payload from before the commitment cannot price the shares",
    );
}

/// The ledger check is structural: a vault order never fills in its creation
/// ledger, whatever the payload's timestamp claims. Here the payload is
/// stamped *after* the commitment, so the price check passes.
#[test]
fn vault_fill_never_lands_in_its_creation_ledger() {
    let mut fixture = TestFixture::create();
    let user = fixture.new_user(10_000 * DOLLAR);
    fixture.jump(60);

    let id = fixture
        .market
        .create_vault_order(&user, &0, &(1_000 * DOLLAR), &0);

    // Same ledger, payload stamped one second ahead (inside the future
    // allowance): the price check passes and the ledger check still refuses.
    let ahead = payload_future(&fixture, usd(100_000), 1);
    assert_eq!(
        fixture
            .market
            .try_execute_vault_order(&fixture.keeper, &user, &id, &ahead),
        Err(Ok(MarketError::StalePrice.into())),
        "an atomic create-and-fill can never price the shares",
    );
}

/// A superseded payload does not block a vault fill: it is upgraded to the
/// newest price the market has seen and the fill lands at that mark.
#[test]
fn vault_fill_upgrades_a_superseded_payload() {
    let mut fixture = TestFixture::create();
    let user = fixture.new_user(10_000 * DOLLAR);
    fixture.jump(60);

    let id = fixture
        .market
        .create_vault_order(&user, &0, &(1_000 * DOLLAR), &0);

    // A candidate published 5 seconds back, then a newer mark consumed by the
    // market: the candidate is superseded but still postdates the order.
    fixture.jump(10);
    let candidate = payload_aged(&fixture, usd(100_000), 5);
    fixture
        .market
        .accrue(&payload_aged(&fixture, usd(100_000), 0));

    assert!(
        fixture
            .market
            .try_execute_vault_order(&fixture.keeper, &user, &id, &candidate)
            .is_ok(),
        "the superseded candidate is upgraded and the fill lands",
    );
    assert!(
        fixture.market.try_get_vault_order(&user, &id).is_err(),
        "the filled order is consumed",
    );
}

// ---------------------------------------------------------------------------
// 5. ADL under substitution
// ---------------------------------------------------------------------------

/// A fixture with the utilization caps widened, matching `test_adl.rs`: the
/// thin vault this section needs cannot clear the default 80% cap against a
/// winner marked well past its entry.
fn adl_fixture<'a>() -> TestFixture<'a> {
    TestFixture::create_with(Config {
        max_util_open: 5 * SCALAR_18,
        max_util_withdraw: 5 * SCALAR_18,
        ..default_config()
    })
}

/// Seed a thin vault and open the standard long, so a large upward move arms
/// the long side.
fn open_adl_winner(fixture: &mut TestFixture) -> Address {
    fixture.seed_vault(1_300 * DOLLAR);
    let user = fixture.new_user(2_000 * DOLLAR);
    let price = fixture.price(usd(100_000));
    fixture.open(&user, true, NOTIONAL, MARGIN, &price);
    user
}

/// Arming is measured at the newest price the market has seen, not at the
/// submitted payload: a stale $170k print cannot latch the flags once the
/// market has moved back to $100k. A fresh $170k print arms normally.
#[test]
fn adl_arming_measures_at_the_newest_price_not_the_submitted_payload() {
    let mut fixture = adl_fixture();
    open_adl_winner(&mut fixture);
    fixture.jump(60);

    // A $170k print from 5 seconds back, superseded by a fresh $100k mark.
    let stale_spike = payload_aged(&fixture, usd(170_000), 5);
    fixture
        .market
        .accrue(&payload_aged(&fixture, usd(100_000), 0));

    let state = fixture.market.update_adl_state(&stale_spike);
    assert!(
        !state.long,
        "the stale spike was upgraded to $100k: nothing to arm"
    );

    // The genuinely newest $170k print arms.
    fixture.jump(5);
    let fresh_spike = payload_aged(&fixture, usd(170_000), 0);
    assert!(
        fixture.market.update_adl_state(&fresh_spike).long,
        "the fresh spike arms"
    );
}

/// A stale payload cannot choose the forced close's price either: execution
/// substitutes the newest mark and lands at it.
#[test]
fn adl_execution_upgrades_a_stale_payload() {
    let mut fixture = adl_fixture();
    let user = open_adl_winner(&mut fixture);
    fixture.jump(60);

    // Arm the side at $170k. This consumes the price and advances the cache.
    let arm = payload_aged(&fixture, usd(170_000), 0);
    assert!(
        fixture.market.update_adl_state(&arm).long,
        "long side armed"
    );

    // A payload from before the arming measurement, still inside the window:
    // upgraded to the arming mark, and the close lands at it.
    let stale = payload_aged(&fixture, usd(170_000), 5);
    fixture
        .market
        .execute_adl(&fixture.keeper, &user, &true, &(600 * DOLLAR), &stale);
    assert_eq!(
        fixture.position(&user, true).notional,
        NOTIONAL - 600 * DOLLAR,
        "the forced close landed at the substituted mark",
    );
}

/// A payload at the cache exactly is the newest price there is: it executes
/// as submitted.
#[test]
fn adl_execution_lands_at_the_market_clock() {
    let mut fixture = adl_fixture();
    let user = open_adl_winner(&mut fixture);
    fixture.jump(60);

    let arm = payload_aged(&fixture, usd(170_000), 0);
    fixture.market.update_adl_state(&arm);

    // The very payload the cache now sits at.
    let at_clock = payload_at_time(&fixture, usd(170_000), market_clock(&fixture));
    fixture
        .market
        .execute_adl(&fixture.keeper, &user, &true, &(600 * DOLLAR), &at_clock);

    assert_eq!(
        fixture.position(&user, true).notional,
        NOTIONAL - 600 * DOLLAR,
        "the forced close landed at the clock",
    );
}

// ---------------------------------------------------------------------------
// 6. Future-dated prices
// ---------------------------------------------------------------------------

/// The future side of the window is bounded, and it is the same bound for
/// both classes. A payload inside the allowance fills (simulation runs
/// against the last closed ledger, so a freshly published price is always
/// slightly future-dated). One beyond it is rejected by the verifier with
/// `PriceAhead` (793) on every route. Only the *past* side widens for the
/// close class.
#[test]
fn future_payload_bounded_by_the_allowance() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(10_000 * DOLLAR);
    fixture.jump(60);

    // Beyond the allowance: rejected in the verifier, before any clock.
    assert_rejected(
        fixture
            .market
            .try_accrue(&payload_future(&fixture, usd(100_000), WINDOW + 1)),
        OracleError::PriceAhead.into(),
        "a payload stamped past the future allowance is rejected",
    );

    // At the allowance exactly: accepted, and it stamps the position's floor
    // ahead of wall clock by at most the window.
    let published = fixture.env.ledger().timestamp() + WINDOW;
    fixture.open(
        &user,
        true,
        NOTIONAL,
        MARGIN,
        &payload_future(&fixture, usd(100_000), WINDOW),
    );
    assert_eq!(
        fixture.position(&user, true).priced_at,
        published,
        "the floor leads wall clock by exactly the allowance",
    );
}

/// The close class shares the strict forward allowance, so a protective
/// route can never cache a future timestamp that substitution would then
/// hand to a strict fill. `accrue` rejects one second past the trade
/// allowance with `PriceAhead`. After an at-the-allowance accrual the cache
/// leads wall clock by no more than the trade window. Every substitutable
/// mark stays inside what a strict verification could have produced itself.
#[test]
fn protective_routes_cannot_cache_past_the_strict_forward_allowance() {
    let fixture = create_fixture_with_data();
    fixture.jump(60);

    assert_rejected(
        fixture
            .market
            .try_accrue(&payload_future(&fixture, usd(100_000), WINDOW + 1)),
        OracleError::PriceAhead.into(),
        "the close class keeps the strict forward allowance",
    );

    fixture
        .market
        .accrue(&payload_future(&fixture, usd(100_000), WINDOW));
    assert_eq!(
        market_clock(&fixture),
        fixture.env.ledger().timestamp() + WINDOW,
        "the cache never leads wall clock by more than the trade window",
    );
}

/// A maximally future-dated price advances the cache past wall clock, but no
/// route rejects for freshness. An honest current payload is upgraded to the
/// newer mark, so nothing wedges.
#[test]
fn future_payload_cannot_wedge_the_clock_routes() {
    let fixture = create_fixture_with_data();
    fixture.jump(60);

    // Consume a price stamped at the future allowance.
    fixture
        .market
        .accrue(&payload_future(&fixture, usd(100_000), WINDOW));
    assert_eq!(
        market_clock(&fixture),
        (fixture.env.ledger().timestamp() + WINDOW),
        "the cache leads wall clock by the allowance",
    );

    // Honest, current payloads keep landing on every substitution route.
    assert!(
        fixture
            .market
            .try_update_adl_state(&payload_aged(&fixture, usd(100_000), 0))
            .is_ok(),
        "ADL state measurement is not wedged",
    );
    assert!(
        fixture
            .market
            .try_accrue(&payload_aged(&fixture, usd(100_000), 0))
            .is_ok(),
        "accrual is not wedged",
    );
}

/// A future-dated fill parks the floor and the cache at most one allowance
/// ahead, so a liquidation is delayed by at most that long. While the cache
/// leads, honest payloads are upgraded to the healthy mark. Once wall clock
/// passes it, the next crash print liquidates.
#[test]
fn future_dated_fill_delays_liquidation_by_at_most_the_allowance() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(10_000 * DOLLAR);
    fixture.jump(60);

    // Open on a price stamped at the future allowance.
    fixture.open(
        &user,
        true,
        NOTIONAL,
        MARGIN,
        &payload_future(&fixture, usd(100_000), WINDOW),
    );

    // Inside the lead: a crash print is upgraded to the (healthy) future mark.
    fixture.jump(5);
    assert_eq!(
        fixture.market.try_execute_liquidation(
            &fixture.keeper,
            &user,
            &true,
            &payload_aged(&fixture, usd(84_000), 0)
        ),
        Err(Ok(MarketError::NotLiquidatable.into())),
        "while the cache leads, the crash print upgrades to the healthy mark",
    );

    // Past the lead: the crash print is the newest price and the close lands.
    fixture.jump(WINDOW);
    fixture.market.execute_liquidation(
        &fixture.keeper,
        &user,
        &true,
        &payload_aged(&fixture, usd(84_000), 0),
    );
    assert_eq!(
        fixture.position(&user, true).notional,
        0,
        "the delay ends with the lead"
    );
}

// ---------------------------------------------------------------------------
// 7. Second precision
// ---------------------------------------------------------------------------

/// `priced_at` is second-resolution under Data Streams: a report one second
/// behind the position's mark is rejected while one a second past it lands.
#[test]
fn priced_at_floor_binds_at_second_precision() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(20_000 * DOLLAR);
    fixture.jump(60);

    // Open at one second ahead of the ledger clock (inside the forward
    // window), so the mark sits past this ledger's own timestamp.
    let base = fixture.env.ledger().timestamp();
    fixture.open(
        &user,
        true,
        NOTIONAL,
        MARGIN,
        &payload_at_time(&fixture, usd(100_000), base + 1),
    );
    assert_eq!(fixture.position(&user, true).priced_at, base + 1);

    let resting = fixture.submit_order(
        &user,
        true,
        OrderKind::MarketIncrease,
        NOTIONAL,
        MARGIN,
        0,
        0,
        fixture.far_expiration(),
    );

    // One second behind the mark: rejected by the position floor.
    assert_eq!(
        fixture.market.try_execute_order(
            &fixture.keeper,
            &user,
            &resting,
            &payload_at_time(&fixture, usd(100_000), base)
        ),
        Err(Ok(MarketError::StalePrice.into())),
        "a report behind the position's mark is stale",
    );

    // One second past it: lands.
    fixture.execute_order(
        &fixture.keeper,
        &user,
        resting,
        &payload_at_time(&fixture, usd(100_000), base + 2),
    );
    assert_eq!(
        fixture.position(&user, true).notional,
        2 * NOTIONAL,
        "a report past the mark fills"
    );
    assert_eq!(
        fixture.position(&user, true).priced_at,
        base + 2,
        "and advances it"
    );
}
