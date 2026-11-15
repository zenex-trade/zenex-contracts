//! Oracle-gate integration tests: the oracle's two-tier staleness bound and
//! spread reduction as the market enforces them per route, and the owner-gated
//! setters that move them.
//!
//! Reports are signed through [`TestFixture::report_at`] with explicit
//! [`ReportTimes`], allowing stale and future observations. The fixture configures the oracle with the strict trade
//! window `TRADE_STALENESS = 15` seconds (fills: `execute_order`,
//! `execute_vault_order`), the protective close window `CLOSE_STALENESS =
//! 60` seconds (gap-closing calls: `execute_liquidation`, `execute_adl`,
//! `update_adl_state`, `accrue`), and spread reduction off. Both bounds are
//! inclusive (reject on `age > window`), so each is pinned from both sides:
//! one second inside passes, one second outside rejects. The band between
//! the windows is where the routes split.
//!
//! # Error surface
//! The market calls `verify_price` with a plain (non-`try_`) cross-contract call
//! inside `Market::load`. An oracle rejection is never caught, and the host
//! propagates the callee's own contract error out through the market WASM
//! frame unchanged. Every gate therefore surfaces on the market's `try_` clients
//! as `Err(Ok(OracleError::X.into()))`: the oracle's code (782), not a
//! `MarketError` and not an opaque host trap.
//!
//! Shared anchors: 7-dec settlement token, BTC feed priced in 1e18 units, so
//! `$100k` is a feed-native price of `1e23`.

use soroban_sdk::testutils::{Address as _, MockAuth, MockAuthInvoke};
use soroban_sdk::{Address, Bytes, IntoVal};

use market::OrderKind;
use oracle::OracleError;
use test_suites::assertions::auth_error;
use test_suites::carry::lone_side_carry;
use test_suites::chainlink_helper::ReportTimes;
use test_suites::constants::{BTC_PRICE, PRICE_SCALAR, SCALAR_18, SCALAR_7};
use test_suites::default_config;
use test_suites::setup::{create_fixture_with_data, DEFAULT_VAULT_LIQUIDITY};
use test_suites::test_fixture::TestFixture;

/// One dollar of the 7-decimal settlement token (token-dec).
const DOLLAR: i128 = SCALAR_7;

/// Notional every resting order under test carries: `$10,000` (token-dec).
const NOTIONAL: i128 = 10_000 * DOLLAR;
/// Margin every resting order under test posts: `$2,000` (token-dec), i.e. 5x.
const MARGIN: i128 = 2_000 * DOLLAR;

/// Keeper payout for one of those fills (token-dec). Opening into an empty (or
/// long-dominant) book fully worsens skew, so the whole notional pays
/// `fee_dom` 0.5% = `5e8` plus the size-quadratic impact
/// `notional^2 / impact_scalar` = `(1e11)^2 / 1e14` = `1e8`. The keeper takes
/// `keeper_rate` 10% of that `6e8` trade fee and the flat `exec_fee` `1e5`.
const FILL_PAYOUT: i128 = 60_100_000;

/// Base size a filled order buys at `$100k` (base-dec):
/// `floor(notional * 1e18 / price)` = `1e11 * 1e18 / 1e23`.
const FILL_TOKENS: i128 = 1_000_000;

/// Feed-native price for `$84,000`, the mark that makes the liquidation
/// test's 5x long eligible for a force close.
const PRICE_84K: i128 = 84_000 * PRICE_SCALAR;

/// A flat (`bid == ask == price`) report at `$100k`, observed `age` seconds
/// before the current ledger timestamp.
fn report(fixture: &TestFixture, age: u64) -> Bytes {
    report_at(fixture, BTC_PRICE, age)
}

/// A flat report at feed-native `price` (dollars * 1e18), observed `age`
/// seconds ago.
fn report_at(fixture: &TestFixture, price: i128, age: u64) -> Bytes {
    let observed = fixture.env.ledger().timestamp() - age;
    fixture.report_at(ReportTimes::at(observed), price, price, price)
}

/// Fund a fresh trader and rest a market-increase long on it, returning the
/// trader and the order id. The order is left unfilled for a price-gated
/// `execute_order` to attempt.
fn rest_long(fixture: &TestFixture) -> (Address, u32) {
    let user = Address::generate(&fixture.env);
    fixture.mint(&user, 100_000 * DOLLAR);
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
    (user, id)
}

// --- staleness gate ---

/// A report observed 16 seconds before the fill ledger is one second past the
/// oracle's 15-second window, so `execute_order` rejects with `PriceStale`
/// (782) and the order is left exactly as it rested.
#[test]
fn stale_report_rejects_order_fill_and_leaves_it_resting() {
    let fixture = create_fixture_with_data();
    let (user, id) = rest_long(&fixture);

    // Move well past creation so a 16s-old report can exist at all.
    fixture.jump(60);
    let stale = report(&fixture, 16);

    let result = fixture
        .market
        .try_execute_order(&fixture.keeper, &user, &id, &stale);

    assert_eq!(
        result,
        Err(Ok(OracleError::PriceStale.into())),
        "a 16s-old report is rejected by the 15s staleness bound",
    );
    // The rejected fill reverted whole: no position, and the order still rests
    // with its original terms.
    assert_eq!(
        fixture.position(&user, true).notional,
        0,
        "no position opened"
    );
    let order = fixture.market.get_order(&user, &id);
    assert_eq!(order.notional, NOTIONAL, "resting order notional untouched");
    assert_eq!(order.margin, MARGIN, "resting order margin untouched");
    assert_eq!(
        fixture.balance(&fixture.keeper),
        0,
        "keeper paid nothing for the rejected fill"
    );
}

/// The positive control: a report observed exactly 15 seconds ago (the
/// inclusive staleness limit) fills, so the rejection above is not the bound
/// being off by one.
#[test]
fn report_at_the_staleness_bound_fills() {
    let fixture = create_fixture_with_data();
    let (user, id) = rest_long(&fixture);

    fixture.jump(60);
    let at_bound = report(&fixture, 15);

    let payout = fixture.execute_order(&fixture.keeper, &user, id, &at_bound);

    assert_eq!(payout, FILL_PAYOUT, "keeper payout on the accepted fill");
    let position = fixture.position(&user, true);
    assert_eq!(position.notional, NOTIONAL, "position opened at the bound");
    assert_eq!(
        position.tokens, FILL_TOKENS,
        "tokens bought at the $100k ask"
    );
    assert!(
        fixture.market.try_get_order(&user, &id).is_err(),
        "the filled order is consumed"
    );
}

// --- liquidation path ---

/// The liquidation route verifies under the protective 60s window, not the
/// 15s fill window. A report past the close window still rejects with
/// `PriceStale` and the position survives untouched. The same price in the
/// band between the windows, old enough that no fill could use it,
/// force-closes. That proves the wider window alone is what admits it.
#[test]
fn stale_report_rejects_liquidation_and_position_survives() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(1_000 * DOLLAR);

    // A 5x long: $1,000 notional on $200 margin. The open fully worsens the
    // empty book, so it pays base `fee_dom` 0.5% of 1e10 = 5e7 plus impact
    // `(1e10)^2 / 1e14` = 1e6, leaving margin 1_949_000_000.
    let notional = 1_000 * DOLLAR;
    let margin = 200 * DOLLAR;
    fixture.open(&user, true, notional, margin, &fixture.price(BTC_PRICE));
    let opened = fixture.position(&user, true);
    assert_eq!(opened.margin, 1_949_000_000, "margin net of the open fees");

    // At $84k the position is liquidatable: the long's mark loss
    // (`floor(tokens 1e5 * 8.4e22 / 1e18)` - notional = -1_600_000_000)
    // drops equity under the 5e8 maintenance margin. The report carrying that
    // price, though, is observed 61s before the fill ledger, one second past
    // even the protective close window.
    fixture.jump(70);
    let stale = report_at(&fixture, PRICE_84K, 61);

    let result = fixture
        .market
        .try_execute_liquidation(&fixture.keeper, &user, &true, &stale);

    assert_eq!(
        result,
        Err(Ok(OracleError::PriceStale.into())),
        "a 61s-old report cannot force-close a position",
    );
    let survived = fixture.position(&user, true);
    assert_eq!(survived.notional, notional, "notional untouched");
    assert_eq!(survived.margin, opened.margin, "margin untouched");
    assert_eq!(survived.tokens, opened.tokens, "tokens untouched");

    // The same price aged 20s sits between the windows: too old for any fill,
    // inside the close window. The liquidation lands. Keeper reward is
    // `keeper_rate` 10% of the closing trade fee (`fee_non_dom` 0.3% of 1e10
    // = 3e7 plus the 1e6 impact) plus 10% of the full 1e8 liquidation fee
    // = 13_100_000.
    let in_window = report_at(&fixture, PRICE_84K, 20);
    let reward = fixture
        .market
        .execute_liquidation(&fixture.keeper, &user, &true, &in_window);
    assert_eq!(reward, 13_100_000, "keeper reward inside the close window");
    assert_eq!(
        fixture.position(&user, true).notional,
        0,
        "position liquidated"
    );
}

// --- the per-class split ---

/// One report aged into the band between the windows (16s: past the 15s
/// trade window, inside the 60s close window) splits the routes in the same
/// ledger. `execute_order` rejects it with `PriceStale`. `accrue`, a
/// close-class call, accepts it and advances the accrual clock.
#[test]
fn between_the_windows_fills_reject_while_accrue_lands() {
    let fixture = create_fixture_with_data();
    let (user, id) = rest_long(&fixture);

    fixture.jump(60);
    let aged = report(&fixture, 16);

    let result = fixture
        .market
        .try_execute_order(&fixture.keeper, &user, &id, &aged);
    assert_eq!(
        result,
        Err(Ok(OracleError::PriceStale.into())),
        "a 16s-old report cannot fill an order",
    );

    let data = fixture.market.accrue(&aged);
    assert_eq!(
        data.accrued_at,
        fixture.env.ledger().timestamp(),
        "the same report drives a close-class accrual",
    );
}

/// A vault-order fill is a voluntary user fill, so it keeps the strict trade
/// window even though it executes at the newest price: the same
/// between-the-windows report rejects `execute_vault_order` with
/// `PriceStale`.
#[test]
fn between_the_windows_vault_fills_stay_strict() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(100_000 * DOLLAR);
    let id = fixture.market.create_vault_order(
        &user,
        &(market::VaultOrderKind::Deposit as u32),
        &(10_000 * DOLLAR),
        &0,
    );

    fixture.jump(60);
    let aged = report(&fixture, 16);

    let result = fixture
        .market
        .try_execute_vault_order(&fixture.keeper, &user, &id, &aged);
    assert_eq!(
        result,
        Err(Ok(OracleError::PriceStale.into())),
        "a 16s-old report cannot fill a vault order",
    );
}

// --- self-liquidation through the wide window ---

/// A permissionless self-liquidation at a between-the-windows price is
/// strictly value-destroying, so the wide close window opens no profitable
/// open-then-liquidate-yourself round trip. Even collecting the keeper leg
/// themselves, the trader receives less than the position's mark equity at
/// the very price they chose. The non-keeper 90% of the close fees and of the
/// liquidation fee are burned against them, on top of whatever the
/// maintenance-margin breach already cost. Cache substitution (a stale
/// payload upgrades to the newest mark) and the per-position `priced_at`
/// floor bound the price choice itself. This test pins the economics of the
/// choice that remains.
#[test]
fn self_liquidation_in_the_wide_window_destroys_value() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(1_000 * DOLLAR);

    // The 5x long from the liquidation test: $1,000 notional on $200 margin,
    // margin 1_949_000_000 net of the open fees.
    let notional = 1_000 * DOLLAR;
    let margin = 200 * DOLLAR;
    fixture.open(&user, true, notional, margin, &fixture.price(BTC_PRICE));

    // The trader picks the price: $84k aged 20s (past the fill window, inside
    // the close window) and liquidates themselves, naming themselves keeper.
    let held: i128 = 70;
    fixture.jump(held as u64);
    let picked = report_at(&fixture, PRICE_84K, 20);
    let before = fixture.balance(&user);
    let reward = fixture
        .market
        .execute_liquidation(&user, &user, &true, &picked);
    let received = fixture.balance(&user) - before;

    // Mark equity at the picked price: margin 1_949_000_000 plus the long's
    // 84k loss of -1_600_000_000.
    let equity = 349_000_000;
    // The 70 second hold also accrues carry, settled on the liquidation: the
    // lone long reserves its ask-marked value at the picked $84k mark
    // (1e5 tokens * $84k), against the seeded liquidity plus the open's vault
    // leg (the 5.1e7 open fee less the keeper's 10% and the treasury's 5%).
    let open_fee = 51_000_000;
    let carry = lone_side_carry(
        &default_config(),
        notional,
        100_000 * 84_000,
        DEFAULT_VAULT_LIQUIDITY + open_fee - open_fee / 10 - open_fee / 20,
        held,
    );
    // What actually comes back: equity less the close fees (base `fee_non_dom`
    // 0.3% of 1e10 = 3e7 plus 1e6 impact), the full 1e8 liquidation fee, and
    // the carry, plus the self-kept keeper cut (10% of the fees) =
    // 231_100_000 - carry. The equity stays far above the 1e8 fee cap, so the
    // liquidation fee is unsaturated and the keeper cut is unchanged.
    assert_eq!(
        reward, 13_100_000,
        "the self-liquidator keeps only the keeper cut"
    );
    assert_eq!(
        received,
        231_100_000 - carry.total(),
        "trader refund plus self-kept keeper cut"
    );
    assert!(
        received < equity,
        "self-liquidation returns less than mark equity at the chosen price",
    );
}

// --- spread reduction through the fill path ---

/// The market consumes the oracle's spread reduction: a report with a real
/// bid/ask spread fills a long at the raw ask while reduction is off, and an
/// identical report fills at the collapsed mid once the owner sets the factor
/// to `SCALAR_18`. That proves the reduced price is what `Market::load` sees.
#[test]
fn full_spread_reduction_moves_the_fill_price_to_the_mid() {
    let fixture = create_fixture_with_data();
    let (first_user, first_id) = rest_long(&fixture);
    let (second_user, second_id) = rest_long(&fixture);

    // $99,900 bid / $100,100 ask around a $100k mid.
    let bid: i128 = 99_900 * PRICE_SCALAR;
    let ask: i128 = 100_100 * PRICE_SCALAR;
    let spread = |fixture: &TestFixture| {
        fixture.report_at(
            ReportTimes::at(fixture.env.ledger().timestamp()),
            BTC_PRICE,
            bid,
            ask,
        )
    };

    // Reduction off: the long opens at the raw $100,100 ask.
    fixture.execute_order(&fixture.keeper, &first_user, first_id, &spread(&fixture));
    // `floor(notional 1e11 * 1e18 / ask 1.001e23)`.
    assert_eq!(
        fixture.position(&first_user, true).tokens,
        999_000,
        "tokens bought at the raw ask",
    );

    fixture.oracle.update_spread_reduction_factor(&SCALAR_18);

    // Full reduction: the same spread collapses to its $100k mid, so the
    // second fill buys exactly the flat-$100k token count.
    fixture.execute_order(&fixture.keeper, &second_user, second_id, &spread(&fixture));
    assert_eq!(
        fixture.position(&second_user, true).tokens,
        FILL_TOKENS,
        "tokens bought at the collapsed mid",
    );
}

// --- owner-gated bounds ---

/// `update_staleness` (seconds) takes effect on the next fill: a
/// 10-second-old report fills under the default 15s trade window, and an
/// identical report rejects with `PriceStale` once the owner tightens the
/// trade window to 5 seconds.
#[test]
fn tightened_staleness_rejects_a_report_that_just_filled() {
    let fixture = create_fixture_with_data();
    let (first_user, first_id) = rest_long(&fixture);
    let (second_user, second_id) = rest_long(&fixture);

    fixture.jump(60);

    // Baseline: 10s old is inside the 15s trade window.
    let payout = fixture.execute_order(
        &fixture.keeper,
        &first_user,
        first_id,
        &report(&fixture, 10),
    );
    assert_eq!(
        payout, FILL_PAYOUT,
        "10s-old report fills under the default window"
    );

    fixture.oracle.update_staleness(&5, &60);
    assert_eq!(
        fixture.oracle.trade_staleness(),
        5,
        "the new trade window is stored"
    );
    assert_eq!(
        fixture.oracle.close_staleness(),
        60,
        "the close window is unchanged"
    );

    // A below-floor update rejects and leaves the stored pair untouched, as
    // does a close window narrower than the trade window.
    assert!(
        fixture.oracle.try_update_staleness(&2, &60).is_err(),
        "below the 3s floor rejects"
    );
    assert!(
        fixture.oracle.try_update_staleness(&10, &9).is_err(),
        "close < trade rejects"
    );
    assert_eq!(
        fixture.oracle.trade_staleness(),
        5,
        "the rejected updates changed nothing"
    );
    assert_eq!(
        fixture.oracle.close_staleness(),
        60,
        "the rejected updates changed nothing"
    );

    let result = fixture.market.try_execute_order(
        &fixture.keeper,
        &second_user,
        &second_id,
        &report(&fixture, 10),
    );

    assert_eq!(
        result,
        Err(Ok(OracleError::PriceStale.into())),
        "the same 10s age is stale once the trade window is 5s",
    );
    assert_eq!(
        fixture.position(&second_user, true).notional,
        0,
        "the second order did not fill"
    );
    assert!(
        fixture
            .market
            .try_get_order(&second_user, &second_id)
            .is_ok(),
        "the second order still rests",
    );
}

/// Both setters are `#[only_owner]`: with just a stranger's auth mocked
/// neither call can authorize, and both values read back unchanged.
///
/// The rejection is an auth failure rather than a contract error. The
/// `#[only_owner]` macro calls `require_auth` on the stored owner, which no
/// stranger signature can satisfy, so the `try_` client surfaces the host's
/// `Error(Context, InvalidAction)`, pinned here through [`auth_error`].
#[test]
fn non_owner_cannot_move_the_price_bounds() {
    let fixture = create_fixture_with_data();
    let stranger = Address::generate(&fixture.env);
    let oracle_address = fixture.oracle.address.clone();

    fixture.env.mock_auths(&[MockAuth {
        address: &stranger,
        invoke: &MockAuthInvoke {
            contract: &oracle_address,
            fn_name: "update_staleness",
            args: (5_u64, 60_u64).into_val(&fixture.env),
            sub_invokes: &[],
        },
    }]);
    assert_eq!(
        fixture.oracle.try_update_staleness(&5, &60),
        Err(Ok(auth_error())),
        "a non-owner cannot tighten the staleness windows",
    );

    fixture.env.mock_auths(&[MockAuth {
        address: &stranger,
        invoke: &MockAuthInvoke {
            contract: &oracle_address,
            fn_name: "update_spread_reduction_factor",
            args: (SCALAR_18,).into_val(&fixture.env),
            sub_invokes: &[],
        },
    }]);
    assert_eq!(
        fixture
            .oracle
            .try_update_spread_reduction_factor(&SCALAR_18),
        Err(Ok(auth_error())),
        "a non-owner cannot move the spread reduction",
    );

    // Views need no auth, so the narrowed mock does not hide the state: all
    // values are still the fixture's.
    assert_eq!(
        fixture.oracle.trade_staleness(),
        15,
        "trade window unchanged"
    );
    assert_eq!(
        fixture.oracle.close_staleness(),
        60,
        "close window unchanged"
    );
    assert_eq!(
        fixture.oracle.spread_reduction_factor(),
        0,
        "spread reduction unchanged"
    );
}
