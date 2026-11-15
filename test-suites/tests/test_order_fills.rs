//! Keeper order-fill integration tests: the full open / increase / decrease /
//! close lifecycle, PnL realization, and the core fill rejections (price
//! bound, trigger, notional lock, margin) plus failed-fill isolation.
//!
//! Every expected number is derived from the config and the price by the local
//! fee helpers below, never copied from contract output. The settlement token
//! is 7-decimal, so `DOLLAR` is `1e7`. The feed is 18-decimal, so a raw price
//! is dollars * 1e18 and `$100k` is `1e23`.

use soroban_sdk::testutils::Address as _;
use soroban_sdk::{Address, Bytes};

use test_suites::carry::{lone_side_carry, Carry};
use test_suites::constants::{BTC_PRICE, EXEC_FEE, PRICE_SCALAR};
use test_suites::setup::{create_fixture_with_data, DEFAULT_VAULT_LIQUIDITY};
use test_suites::test_fixture::{TestFixture, TREASURY_RATE};
use test_suites::{default_config, SCALAR_18, SCALAR_7};

use market::{Config, MarketError, OrderKind};

/// One dollar of the 7-decimal settlement token (token-dec). Notionals and
/// margin are written as `N * DOLLAR` so the scale never has to be counted.
const DOLLAR: i128 = SCALAR_7;

// --- price constants (feed-native, dollars * 1e18) ---

const PRICE_100K: i128 = BTC_PRICE; // $100,000
const PRICE_110K: i128 = 110_000 * PRICE_SCALAR; // $110,000
const PRICE_95K: i128 = 95_000 * PRICE_SCALAR; // $95,000
const PRICE_99K: i128 = 99_000 * PRICE_SCALAR; // $99,000
const PRICE_99_999: i128 = 99_999 * PRICE_SCALAR; // $99,999
const PRICE_90K: i128 = 90_000 * PRICE_SCALAR; // $90,000

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

/// Base size a short buys with `notional` at raw `price`:
/// `ceil(notional * SCALAR_18 / price)` (base-dec).
fn to_tokens_ceil(notional: i128, price: i128) -> i128 {
    (notional * SCALAR_18 + price - 1) / price
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

/// The `(base, impact)` fee for a fill that fully worsens skew (an open into an
/// empty or same-side-dominant book): the whole notional pays `fee_dom` plus the
/// size-quadratic impact charge.
fn worsening_fees(config: &Config, notional: i128) -> (i128, i128) {
    let base = factor_ceil(notional, config.fee_dom);
    let impact = impact_fee(notional, config.impact_scalar);
    (base, impact)
}

/// The `(base, impact)` fee for a fill that fully improves skew (reducing the
/// lone side): the whole notional pays `fee_non_dom` plus the size-quadratic
/// impact charge.
fn improving_fees(config: &Config, notional: i128) -> (i128, i128) {
    let base = factor_ceil(notional, config.fee_non_dom);
    let impact = impact_fee(notional, config.impact_scalar);
    (base, impact)
}

/// The keeper payout for a fill whose trade fee is `base + impact`: the
/// `keeper_rate` cut of the trade fee.
fn keeper_payout(config: &Config, base: i128, impact: i128) -> i128 {
    factor_floor(base + impact, config.keeper_rate)
}

/// The keeper payout for an order fill: the trade-fee cut plus the flat
/// `exec_fee` every order escrows at creation and pays out on fill.
fn order_keeper_payout(config: &Config, base: i128, impact: i128) -> i128 {
    keeper_payout(config, base, impact) + EXEC_FEE
}

/// The treasury leg for a fill: the fixture treasury's rate cut of the trade fee.
fn treasury_leg(base: i128, impact: i128) -> i128 {
    factor_floor(base + impact, TREASURY_RATE)
}

/// The vault's banked remainder of the trade fee after the keeper and treasury
/// cuts.
fn vault_fee_cut(config: &Config, base: i128, impact: i128) -> i128 {
    base + impact - keeper_payout(config, base, impact) - treasury_leg(base, impact)
}

/// The vault's banked remainder of a settled `carry`: the borrowing fee less
/// the treasury's cut. The keeper takes no cut of borrowing, and the funding
/// leg banks into the market's credit pool instead, so the vault never sees it.
fn vault_carry_cut(carry: &Carry) -> i128 {
    carry.borrowing - factor_floor(carry.borrowing, TREASURY_RATE)
}

// --- helpers ---

/// Open (or grow) a position with a distinct `keeper`, so the
/// trader's wallet delta is the posted margin plus `EXEC_FEE` (which pays the
/// keeper rather than round-tripping back to a self-filling trader).
#[allow(clippy::too_many_arguments)]
fn open_with_keeper(
    fixture: &TestFixture,
    keeper: &Address,
    user: &Address,
    is_long: bool,
    notional: i128,
    margin: i128,
    price: &Bytes,
) -> i128 {
    fixture.create_and_fill(
        keeper,
        user,
        is_long,
        OrderKind::MarketIncrease as u32,
        notional,
        margin,
        0,
        0,
        fixture.far_expiration(),
        price,
    )
}

/// Decrease (or close) a position with a distinct `keeper`.
#[allow(clippy::too_many_arguments)]
fn decrease_with_keeper(
    fixture: &TestFixture,
    keeper: &Address,
    user: &Address,
    is_long: bool,
    notional: i128,
    margin: i128,
    price: &Bytes,
) -> i128 {
    fixture.create_and_fill(
        keeper,
        user,
        is_long,
        OrderKind::MarketDecrease as u32,
        notional,
        margin,
        0,
        0,
        fixture.far_expiration(),
        price,
    )
}

/// The full open / increase / partial-decrease / close cycle at a flat $100k
/// (no PnL, so every wallet move is fees and the withdrawal). Asserts the
/// netted position, the keeper payout, and the trader wallet delta at each step
/// against independently derived fee math.
#[test]
fn full_lifecycle_open_increase_partial_decrease_close() {
    let fixture = create_fixture_with_data();
    let config = default_config();
    let keeper = fixture.keeper.clone();
    let user = Address::generate(&fixture.env);
    fixture.mint(&user, 100_000 * DOLLAR);

    let price = fixture.price(PRICE_100K);
    let raw = PRICE_100K;

    // --- Step A: open long $10,000 on $3,000 margin (3.33x) ---
    let notional_a = 10_000 * DOLLAR;
    let margin_a = 3_000 * DOLLAR;
    let (base_a, impact_a) = worsening_fees(&config, notional_a); // fresh book: fully worsening
    let user_before = fixture.balance(&user);
    let keeper_before = fixture.balance(&keeper);

    let payout_a = open_with_keeper(&fixture, &keeper, &user, true, notional_a, margin_a, &price);

    let net_margin_a = margin_a - base_a - impact_a;
    assert_eq!(
        payout_a,
        order_keeper_payout(&config, base_a, impact_a),
        "open payout"
    );
    let position = fixture.position(&user, true);
    assert_eq!(position.notional, notional_a, "notional after open");
    assert_eq!(
        position.tokens,
        to_tokens_floor(notional_a, raw),
        "tokens after open"
    );
    assert_eq!(position.margin, net_margin_a, "margin net of open fees");
    assert_eq!(
        fixture.balance(&user) - user_before,
        -(margin_a + EXEC_FEE),
        "trader pays margin plus exec_fee on open"
    );
    assert_eq!(
        fixture.balance(&keeper) - keeper_before,
        payout_a,
        "keeper paid open payout"
    );

    // --- Step B: increase the long by $10,000 on $2,000 (same ledger, dominant side worsens) ---
    let notional_b = 10_000 * DOLLAR;
    let margin_b = 2_000 * DOLLAR;
    let (base_b, impact_b) = worsening_fees(&config, notional_b);
    let user_before = fixture.balance(&user);

    let payout_b = open_with_keeper(&fixture, &keeper, &user, true, notional_b, margin_b, &price);

    let net_margin_b = net_margin_a + margin_b - base_b - impact_b;
    assert_eq!(
        payout_b,
        order_keeper_payout(&config, base_b, impact_b),
        "increase payout"
    );
    let position = fixture.position(&user, true);
    assert_eq!(
        position.notional,
        notional_a + notional_b,
        "notional after increase"
    );
    assert_eq!(
        position.tokens,
        to_tokens_floor(notional_a, raw) + to_tokens_floor(notional_b, raw),
        "tokens after increase"
    );
    assert_eq!(position.margin, net_margin_b, "margin net of increase fees");
    assert_eq!(
        fixture.balance(&user) - user_before,
        -(margin_b + EXEC_FEE),
        "trader pays added margin plus exec_fee"
    );

    fixture.assert_borrowing_books([&user]);

    // The whole notional is locked by the two same-window increases. Jump past
    // the 30s lock so a partial decrease is allowed.
    let held = (config.notional_lock + 5) as i128;
    fixture.jump(held as u64);
    let price = fixture.price(PRICE_100K);

    // --- Step C: partial decrease $5,000 of notional, withdraw $1,000 ---
    let notional_total = notional_a + notional_b; // $20,000
    let tokens_total = to_tokens_floor(notional_a, raw) + to_tokens_floor(notional_b, raw);
    let notional_c = 5_000 * DOLLAR;
    let withdraw_c = 1_000 * DOLLAR;
    let (base_c, impact_c) = improving_fees(&config, notional_c); // reducing the lone long: fully improving
    let closed_tokens_c = tokens_total * notional_c / notional_total; // pro-rata, floor
    let user_before = fixture.balance(&user);

    let payout_c = decrease_with_keeper(
        &fixture, &keeper, &user, true, notional_c, withdraw_c, &price,
    );

    // The jump accrued carry on the whole open position: the lone long
    // reserves its ask-marked value at the flat $100k mark, against the
    // seeded liquidity plus the two opens' vault legs. It settles on this
    // first touch after the jump.
    let backing = DEFAULT_VAULT_LIQUIDITY
        + vault_fee_cut(&config, base_a, impact_a)
        + vault_fee_cut(&config, base_b, impact_b);
    let carry = lone_side_carry(
        &config,
        notional_total,
        to_notional_ceil(tokens_total, raw),
        backing,
        held,
    );

    // Flat price: realized PnL is 0, so no profit covers the close fee. The
    // fee debits the surviving margin, never the withdrawal, so the margin
    // drops by the withdrawal, the fee, and the settled carry.
    let net_margin_c = net_margin_b - withdraw_c - base_c - impact_c - carry.total();
    assert_eq!(
        payout_c,
        order_keeper_payout(&config, base_c, impact_c),
        "partial-decrease payout"
    );
    let position = fixture.position(&user, true);
    assert_eq!(
        position.notional,
        notional_total - notional_c,
        "notional after partial"
    );
    assert_eq!(
        position.tokens,
        tokens_total - closed_tokens_c,
        "tokens after partial"
    );
    assert_eq!(
        position.margin, net_margin_c,
        "margin net of decrease fees and withdrawal"
    );
    assert_eq!(
        fixture.balance(&user) - user_before,
        withdraw_c - EXEC_FEE,
        "trader receives the withdrawal in full, less the order's exec_fee"
    );

    fixture.assert_borrowing_books([&user]);

    // --- Step D: full close of the remaining $15,000 ---
    let notional_d = notional_total - notional_c; // $15,000
    let (base_d, impact_d) = improving_fees(&config, notional_d);
    let user_before = fixture.balance(&user);

    let payout_d = decrease_with_keeper(&fixture, &keeper, &user, true, notional_d, 0, &price);

    // Full close pays out the margin remaining after this close's fees.
    let returned_d = net_margin_c - base_d - impact_d;
    assert_eq!(
        payout_d,
        order_keeper_payout(&config, base_d, impact_d),
        "close payout"
    );
    assert_eq!(fixture.position(&user, true).notional, 0, "position closed");
    assert_eq!(
        fixture.balance(&user) - user_before,
        returned_d - EXEC_FEE,
        "trader receives the close remainder net of the order's exec_fee"
    );

    // Whole cycle at a flat price nets to the total fees paid: the four trade
    // fees, the flat exec_fee escrowed by each of the four orders, and the
    // carry settled on the partial decrease. Step D runs in the same ledger
    // as step C, so it accrues no further carry.
    let total_fees = (base_a + impact_a)
        + (base_b + impact_b)
        + (base_c + impact_c)
        + (base_d + impact_d)
        + 4 * EXEC_FEE
        + carry.total();
    let net_wallet = fixture.balance(&user) - 100_000 * DOLLAR;
    assert_eq!(
        net_wallet, -total_fees,
        "trader net loss equals total fees at a flat price"
    );
    fixture.assert_borrowing_books([&user]);
}

/// A lone long open: the position row, the keeper payout, the treasury leg, and
/// the trader/keeper/treasury wallet deltas, all from the config fee math.
#[test]
fn open_long_records_position_and_settles_fees() {
    let fixture = create_fixture_with_data();
    let config = default_config();
    let keeper = fixture.keeper.clone();
    let treasury = fixture.treasury.address.clone();
    let user = Address::generate(&fixture.env);
    fixture.mint(&user, 100_000 * DOLLAR);

    let notional = 10_000 * DOLLAR;
    let margin = 2_000 * DOLLAR; // 5x
    let price = fixture.price(PRICE_100K);
    let (base, impact) = worsening_fees(&config, notional);

    let vault_before = fixture.vault.total_assets();
    let user_before = fixture.balance(&user);
    let treasury_before = fixture.balance(&treasury);

    let payout = open_with_keeper(&fixture, &keeper, &user, true, notional, margin, &price);

    let position = fixture.position(&user, true);
    assert_eq!(position.notional, notional, "position notional");
    assert_eq!(
        position.tokens,
        to_tokens_floor(notional, PRICE_100K),
        "position tokens"
    );
    assert_eq!(
        position.margin,
        margin - base - impact,
        "margin net of fees"
    );
    assert_eq!(
        payout,
        order_keeper_payout(&config, base, impact),
        "keeper payout"
    );
    assert_eq!(fixture.balance(&keeper), payout, "keeper wallet");
    assert_eq!(
        fixture.balance(&treasury) - treasury_before,
        treasury_leg(base, impact),
        "treasury leg"
    );
    assert_eq!(
        user_before - fixture.balance(&user),
        margin + EXEC_FEE,
        "trader pays margin plus exec_fee"
    );
    assert_eq!(
        fixture.vault.total_assets() - vault_before,
        vault_fee_cut(&config, base, impact),
        "vault banks the fee remainder",
    );
    assert_eq!(
        fixture.market.get_market_data().notional.long,
        notional,
        "long open interest"
    );
    fixture.assert_borrowing_books([&user]);
}

/// A lone short open mirrors the long: the short buys base at the bid, fully
/// worsens the fresh book, and the short open interest reflects it.
#[test]
fn open_short_records_position_and_settles_fees() {
    let fixture = create_fixture_with_data();
    let config = default_config();
    let keeper = fixture.keeper.clone();
    let user = Address::generate(&fixture.env);
    fixture.mint(&user, 100_000 * DOLLAR);

    let notional = 10_000 * DOLLAR;
    let margin = 2_000 * DOLLAR; // 5x
    let price = fixture.price(PRICE_100K); // bid == ask == mid
    let (base, impact) = worsening_fees(&config, notional);

    let payout = open_with_keeper(&fixture, &keeper, &user, false, notional, margin, &price);

    let position = fixture.position(&user, false);
    assert_eq!(position.notional, notional, "short notional");
    assert_eq!(
        position.tokens,
        to_tokens_ceil(notional, PRICE_100K),
        "short tokens at the bid"
    );
    assert_eq!(
        position.margin,
        margin - base - impact,
        "short margin net of fees"
    );
    assert_eq!(
        payout,
        order_keeper_payout(&config, base, impact),
        "short open payout"
    );
    let market = fixture.market.get_market_data();
    assert_eq!(market.notional.short, notional, "short open interest");
    assert_eq!(market.notional.long, 0, "no long interest");
    fixture.assert_borrowing_books([&user]);
}

/// A long opened at $100k and closed at $110k realizes $1,000 of profit funded
/// by the vault. The trader wallet nets the profit minus round-trip fees.
#[test]
fn close_long_realizes_profit_when_price_rises() {
    let fixture = create_fixture_with_data();
    let config = default_config();
    let keeper = fixture.keeper.clone();
    let user = Address::generate(&fixture.env);
    fixture.mint(&user, 100_000 * DOLLAR);

    let notional = 10_000 * DOLLAR;
    let margin = 2_000 * DOLLAR;
    let (base_open, impact_open) = worsening_fees(&config, notional);
    let tokens = to_tokens_floor(notional, PRICE_100K);

    let user_start = fixture.balance(&user);
    let vault_start = fixture.vault.total_assets();
    open_with_keeper(
        &fixture,
        &keeper,
        &user,
        true,
        notional,
        margin,
        &fixture.price(PRICE_100K),
    );

    let held = (config.notional_lock + 5) as i128;
    fixture.jump(held as u64);
    let (base_close, impact_close) = improving_fees(&config, notional); // lone long fully improves on close
    let payout = decrease_with_keeper(
        &fixture,
        &keeper,
        &user,
        true,
        notional,
        0,
        &fixture.price(PRICE_110K),
    );

    // Long PnL = tokens * bid - notional. At $110k flat the bid is $110k.
    let pnl = to_notional_floor(tokens, PRICE_110K) - notional;
    assert_eq!(pnl, 1_000 * DOLLAR, "expected $1,000 profit");
    // The hold accrues carry, settled on the close: the lone long reserves
    // its ask-marked value at the $110k close mark, against the seeded
    // liquidity plus the open's vault leg.
    let backing = DEFAULT_VAULT_LIQUIDITY + vault_fee_cut(&config, base_open, impact_open);
    let carry = lone_side_carry(
        &config,
        notional,
        to_notional_ceil(tokens, PRICE_110K),
        backing,
        held,
    );
    let net_margin = margin - base_open - impact_open - base_close - impact_close;
    let returned = net_margin + pnl - carry.total();

    assert_eq!(
        payout,
        order_keeper_payout(&config, base_close, impact_close),
        "close payout"
    );
    assert_eq!(fixture.position(&user, true).notional, 0, "position closed");
    // Both the open and the close escrow the flat exec_fee from the trader's
    // wallet, on top of the trade-fee legs already folded into `returned`.
    assert_eq!(
        fixture.balance(&user) - user_start,
        returned - margin - 2 * EXEC_FEE,
        "trader nets profit minus fees"
    );
    // The vault funds the realized profit, offset by the fee remainders it
    // banked on the open and the close and by its share of the borrowing fee.
    let vault_banked = vault_fee_cut(&config, base_open, impact_open)
        + vault_fee_cut(&config, base_close, impact_close)
        + vault_carry_cut(&carry);
    assert_eq!(
        vault_start - fixture.vault.total_assets(),
        pnl - vault_banked,
        "vault funded the profit net of banked fees",
    );
    fixture.assert_borrowing_books([&user]);
}

/// A long opened at $100k and closed at $95k realizes a $500 loss banked by the
/// vault. The trader wallet nets the loss plus round-trip fees.
#[test]
fn close_long_realizes_loss_when_price_falls() {
    let fixture = create_fixture_with_data();
    let config = default_config();
    let keeper = fixture.keeper.clone();
    let user = Address::generate(&fixture.env);
    fixture.mint(&user, 100_000 * DOLLAR);

    let notional = 10_000 * DOLLAR;
    let margin = 2_000 * DOLLAR;
    let (base_open, impact_open) = worsening_fees(&config, notional);
    let tokens = to_tokens_floor(notional, PRICE_100K);

    let user_start = fixture.balance(&user);
    let vault_start = fixture.vault.total_assets();
    open_with_keeper(
        &fixture,
        &keeper,
        &user,
        true,
        notional,
        margin,
        &fixture.price(PRICE_100K),
    );

    let held = (config.notional_lock + 5) as i128;
    fixture.jump(held as u64);
    let (base_close, impact_close) = improving_fees(&config, notional);
    let payout = decrease_with_keeper(
        &fixture,
        &keeper,
        &user,
        true,
        notional,
        0,
        &fixture.price(PRICE_95K),
    );

    let pnl = to_notional_floor(tokens, PRICE_95K) - notional;
    assert_eq!(pnl, -500 * DOLLAR, "expected $500 loss");
    // The hold accrues carry, settled on the close: the lone long reserves
    // its ask-marked value at the $95k close mark, against the seeded
    // liquidity plus the open's vault leg.
    let backing = DEFAULT_VAULT_LIQUIDITY + vault_fee_cut(&config, base_open, impact_open);
    let carry = lone_side_carry(
        &config,
        notional,
        to_notional_ceil(tokens, PRICE_95K),
        backing,
        held,
    );
    let net_margin = margin - base_open - impact_open - base_close - impact_close;
    let returned = net_margin + pnl - carry.total();

    assert_eq!(
        payout,
        order_keeper_payout(&config, base_close, impact_close),
        "close payout"
    );
    assert_eq!(fixture.position(&user, true).notional, 0, "position closed");
    // Both the open and the close escrow the flat exec_fee from the trader's
    // wallet, on top of the trade-fee legs already folded into `returned`.
    assert_eq!(
        fixture.balance(&user) - user_start,
        returned - margin - 2 * EXEC_FEE,
        "trader nets loss plus fees"
    );
    // The vault banks the forfeited loss, the fee remainders of both fills,
    // and its share of the borrowing fee.
    let vault_banked = vault_fee_cut(&config, base_open, impact_open)
        + vault_fee_cut(&config, base_close, impact_close)
        + vault_carry_cut(&carry);
    assert_eq!(
        fixture.vault.total_assets() - vault_start,
        -pnl + vault_banked,
        "vault banked the loss and the fee remainders",
    );
    fixture.assert_borrowing_books([&user]);
}

/// An oversized decrease (`i128::MAX` notional) clamps to a full close and
/// removes the position.
#[test]
fn full_close_via_max_notional_clears_position() {
    let fixture = create_fixture_with_data();
    let config = default_config();
    let keeper = fixture.keeper.clone();
    let user = Address::generate(&fixture.env);
    fixture.mint(&user, 100_000 * DOLLAR);

    let notional = 10_000 * DOLLAR;
    let margin = 2_000 * DOLLAR;
    open_with_keeper(
        &fixture,
        &keeper,
        &user,
        true,
        notional,
        margin,
        &fixture.price(PRICE_100K),
    );

    fixture.jump(config.notional_lock + 5);
    let (base_close, impact_close) = improving_fees(&config, notional);
    let payout = decrease_with_keeper(
        &fixture,
        &keeper,
        &user,
        true,
        i128::MAX,
        0,
        &fixture.price(PRICE_100K),
    );

    assert_eq!(
        payout,
        order_keeper_payout(&config, base_close, impact_close),
        "close payout on the full position"
    );
    let position = fixture.position(&user, true);
    assert_eq!(position.notional, 0, "notional cleared");
    assert_eq!(position.tokens, 0, "tokens cleared");
    assert_eq!(position.margin, 0, "margin cleared");
    assert_eq!(
        fixture.market.get_market_data().notional.long,
        0,
        "open interest cleared"
    );
    fixture.assert_borrowing_books([&user]);
}

/// A create-and-try-fill whose `price_bound` is tighter than the ask reports
/// `PriceBoundExceeded` and leaves the order resting. A later direct
/// `execute_order` at a price inside the bound fills it.
#[test]
fn violated_price_bound_rests_order_then_direct_execute_fills() {
    let fixture = create_fixture_with_data();
    let config = default_config();
    let keeper = fixture.keeper.clone();
    let user = Address::generate(&fixture.env);
    fixture.mint(&user, 100_000 * DOLLAR);

    let notional = 10_000 * DOLLAR;
    let margin = 2_000 * DOLLAR;
    // A long enters at the ask ($100k). Bound at $99,999 is below it, so the
    // fill at $100k must reject.
    let attempt = fixture.create_and_try_fill(
        &keeper,
        &user,
        true,
        OrderKind::MarketIncrease as u32,
        notional,
        margin,
        0,
        PRICE_99_999,
        fixture.far_expiration(),
        &fixture.price(PRICE_100K),
    );
    assert!(!attempt.filled, "fill kicked back over the bound");
    assert_eq!(
        attempt.error,
        MarketError::PriceBoundExceeded as u32,
        "reports 741"
    );
    assert_eq!(
        fixture.position(&user, true).notional,
        0,
        "order rests, no position"
    );

    // At $99,000 the ask sits inside the $99,999 bound, so a direct fill lands.
    let (base, impact) = worsening_fees(&config, notional); // fresh book still fully worsening
    let payout = fixture.execute_order(&keeper, &user, attempt.id, &fixture.price(PRICE_99K));
    assert_eq!(
        payout,
        order_keeper_payout(&config, base, impact),
        "fill payout inside the bound"
    );
    let position = fixture.position(&user, true);
    assert_eq!(
        position.notional, notional,
        "position opened on the second attempt"
    );
    assert_eq!(
        position.tokens,
        to_tokens_floor(notional, PRICE_99K),
        "tokens at the $99k ask"
    );
    fixture.assert_borrowing_books([&user]);
}

/// A resting long limit order (trigger below spot, fires when the ask falls to
/// it) rejects with `TriggerNotMet` until the price crosses, then fills.
#[test]
fn resting_trigger_order_rejects_until_price_crosses() {
    let fixture = create_fixture_with_data();
    let config = default_config();
    let keeper = fixture.keeper.clone();
    let user = Address::generate(&fixture.env);
    fixture.mint(&user, 100_000 * DOLLAR);

    let notional = 10_000 * DOLLAR;
    let margin = 2_000 * DOLLAR;
    // Limit buy (LimitIncrease): fires once the entry (ask) drops to $95k.
    let id = fixture.submit_order(
        &user,
        true,
        OrderKind::LimitIncrease,
        notional,
        margin,
        PRICE_95K,
        0,
        fixture.far_expiration(),
    );

    // At $100k the ask is above the $95k trigger: not crossed. execute_order
    // returns a plain i128, so its try_ error side is a raw host error.
    let not_met = fixture
        .market
        .try_execute_order(&keeper, &user, &id, &fixture.price(PRICE_100K));
    match not_met {
        Err(Ok(err)) => assert_eq!(
            err,
            MarketError::TriggerNotMet.into(),
            "trigger not crossed rejects with 742"
        ),
        other => panic!("expected TriggerNotMet, got {other:?}"),
    }
    assert_eq!(
        fixture.position(&user, true).notional,
        0,
        "order still resting"
    );

    // Move to $95k: the ask reaches the trigger and the order fills.
    fixture.jump(10);
    let (base, impact) = worsening_fees(&config, notional);
    let payout = fixture.execute_order(&keeper, &user, id, &fixture.price(PRICE_95K));
    assert_eq!(
        payout,
        order_keeper_payout(&config, base, impact),
        "fill payout on the cross"
    );
    assert_eq!(
        fixture.position(&user, true).notional,
        notional,
        "position opened on the cross"
    );
    fixture.assert_borrowing_books([&user]);
}

/// A decrease inside the notional lock reports `NotionalLocked`. The same order
/// fills once the lock elapses.
#[test]
fn decrease_inside_notional_lock_rejects_then_passes_after_jump() {
    let fixture = create_fixture_with_data();
    let config = default_config();
    let keeper = fixture.keeper.clone();
    let user = Address::generate(&fixture.env);
    fixture.mint(&user, 100_000 * DOLLAR);

    let notional = 10_000 * DOLLAR;
    let margin = 2_000 * DOLLAR;
    fixture.open(&user, true, notional, margin, &fixture.price(PRICE_100K));

    // Immediately: the whole notional is locked, so a partial decrease rejects.
    let notional_close = 5_000 * DOLLAR;
    let attempt = fixture.create_and_try_fill(
        &keeper,
        &user,
        true,
        OrderKind::MarketDecrease as u32,
        notional_close,
        0,
        0,
        0,
        fixture.far_expiration(),
        &fixture.price(PRICE_100K),
    );
    assert!(!attempt.filled, "locked decrease did not fill");
    assert_eq!(
        attempt.error,
        MarketError::NotionalLocked as u32,
        "reports 721"
    );
    assert_eq!(
        fixture.position(&user, true).notional,
        notional,
        "position unchanged while locked"
    );

    // Past the lock the resting order fills.
    fixture.jump(config.notional_lock + 5);
    let (base, impact) = improving_fees(&config, notional_close);
    let payout = fixture.execute_order(&keeper, &user, attempt.id, &fixture.price(PRICE_100K));
    assert_eq!(
        payout,
        order_keeper_payout(&config, base, impact),
        "partial-decrease payout after the lock"
    );
    assert_eq!(
        fixture.position(&user, true).notional,
        notional - notional_close,
        "notional reduced after the lock"
    );
    fixture.assert_borrowing_books([&user]);
}

/// A keeper fills two orders in sequence and a failure stays isolated: one
/// order fills, the other reports `TriggerNotMet`, and the failure does not
/// roll back the success.
#[test]
fn keeper_isolates_failed_fill_from_success() {
    let fixture = create_fixture_with_data();
    let config = default_config();
    let env = fixture.env.clone();
    let keeper = fixture.keeper.clone();

    let notional = 10_000 * DOLLAR;
    let margin = 2_000 * DOLLAR;

    // User A: a market increase (no trigger) that will fill at $100k.
    let user_a = Address::generate(&env);
    fixture.mint(&user_a, 100_000 * DOLLAR);
    let id_a = fixture.submit_order(
        &user_a,
        true,
        OrderKind::MarketIncrease,
        notional,
        margin,
        0,
        0,
        fixture.far_expiration(),
    );

    // User B: a limit buy with the trigger below $100k, so it will not cross.
    let user_b = Address::generate(&env);
    fixture.mint(&user_b, 100_000 * DOLLAR);
    let id_b = fixture.submit_order(
        &user_b,
        true,
        OrderKind::LimitIncrease,
        notional,
        margin,
        PRICE_90K,
        0,
        fixture.far_expiration(),
    );

    // Fill both in sequence a ledger later at $100k.
    fixture.jump(10);
    let price = fixture.price(PRICE_100K);
    let (base, impact) = worsening_fees(&config, notional);
    assert_eq!(
        fixture
            .market
            .try_execute_order(&keeper, &user_a, &id_a, &price),
        Ok(Ok(order_keeper_payout(&config, base, impact))),
        "user A payout"
    );
    assert_eq!(
        fixture
            .market
            .try_execute_order(&keeper, &user_b, &id_b, &price),
        Err(Ok(MarketError::TriggerNotMet.into())),
        "user B reports 742"
    );

    // The failure did not poison the success: A is open, B still rests.
    assert_eq!(
        fixture.position(&user_a, true).notional,
        notional,
        "user A position opened"
    );
    assert_eq!(
        fixture.position(&user_b, true).notional,
        0,
        "user B position not opened"
    );
    fixture.assert_borrowing_books([&user_a, &user_b]);
}

/// An open whose margin falls short of the initial margin (leverage above
/// the 10x cap) reports `InsufficientMargin` and rests without a position.
#[test]
fn open_beyond_max_leverage_rejects_insufficient_margin() {
    let fixture = create_fixture_with_data();
    let keeper = fixture.keeper.clone();
    let user = Address::generate(&fixture.env);
    fixture.mint(&user, 100_000 * DOLLAR);

    // init_margin is 10%, so $10,000 notional needs >= $1,000 margin. $100
    // clears the order dust floor but fails the margin gate at fill.
    let notional = 10_000 * DOLLAR;
    let margin = 100 * DOLLAR; // 100x

    let attempt = fixture.create_and_try_fill(
        &keeper,
        &user,
        true,
        OrderKind::MarketIncrease as u32,
        notional,
        margin,
        0,
        0,
        fixture.far_expiration(),
        &fixture.price(PRICE_100K),
    );
    assert!(!attempt.filled, "under-margined open did not fill");
    assert_eq!(
        attempt.error,
        MarketError::InsufficientMargin as u32,
        "reports 713"
    );
    assert_eq!(
        fixture.position(&user, true).notional,
        0,
        "no position opened"
    );
}

/// An open that rides two resting exit triggers (a take-profit
/// limit-decrease and a stop-loss stop-decrease) created in the same ledger:
/// the market open is created first, the two triggers rest as ids 2 and 3,
/// then only the open fills, and its payout matches a lone open.
#[test]
fn open_fills_while_exit_triggers_rest() {
    let fixture = create_fixture_with_data();
    let config = default_config();
    let env = fixture.env.clone();
    let keeper = fixture.keeper.clone();
    let user = Address::generate(&env);
    fixture.mint(&user, 100_000 * DOLLAR);

    let notional = 10_000 * DOLLAR;
    let margin = 2_000 * DOLLAR;
    let expiration = fixture.far_expiration();

    // The market open (the fillable order).
    let open = fixture.submit_order(
        &user,
        true,
        OrderKind::MarketIncrease,
        notional,
        margin,
        0,
        0,
        expiration,
    );
    // Take-profit. A long limit-decrease fires when the exit rises to $110k.
    // It rests before the position exists and never fills here.
    fixture.submit_order(
        &user,
        true,
        OrderKind::LimitDecrease,
        notional,
        0,
        PRICE_110K,
        0,
        expiration,
    );
    // Stop-loss. A long stop-decrease fires when the exit drops to $95k.
    fixture.submit_order(
        &user,
        true,
        OrderKind::StopDecrease,
        notional,
        0,
        PRICE_95K,
        0,
        expiration,
    );

    let payout = fixture.execute_order(&keeper, &user, open, &fixture.price(PRICE_100K));

    // The open filled at the lone-open payout. The triggers did not touch it.
    let (base, impact) = worsening_fees(&config, notional);
    assert_eq!(
        payout,
        order_keeper_payout(&config, base, impact),
        "open payout matches a lone open"
    );
    let position = fixture.position(&user, true);
    assert_eq!(position.notional, notional, "the market open filled");
    assert_eq!(
        position.tokens,
        to_tokens_floor(notional, PRICE_100K),
        "opened tokens at the $100k ask"
    );

    // Both exit triggers rest (ids 2 and 3, after the open's id 1) for a later
    // keeper fill, readable from the side's decrease orders.
    let tp = fixture.market.get_order(&user, &2);
    assert_eq!(tp.notional, notional, "take-profit notional");
    assert_eq!(tp.kind, OrderKind::LimitDecrease as u32, "take-profit kind");
    let sl = fixture.market.get_order(&user, &3);
    assert_eq!(sl.notional, notional, "stop-loss notional");
    assert_eq!(sl.kind, OrderKind::StopDecrease as u32, "stop-loss kind");
    fixture.assert_borrowing_books([&user]);
}

// --- no per-position profit ceiling ---

/// Semantics pin for the accepted netted-side state: a
/// profit larger than the `max_pnl_trader` allowance passes through IN FULL
/// when a later long's pending loss nets the side aggregate under the
/// allowance. The haircut and ADL both read the netted measure and stay
/// asleep. The claim is margin-backed by the loser, so this is the intended
/// payout, not a bug. If this test starts failing, the accepted semantics
/// changed.
///
/// Construction: whale longs $200k at $100k against a $1M vault. A second LP
/// triples the vault before the move (rising marks would otherwise block any
/// new open on the side, and the deposit gate would refuse the deposit), and
/// a loser longs $300k at the $500k top. At $450k the whale
/// is +$700k, the loser -$30k: the side nets ~$670k, under the ~$677k
/// allowance (22.5% of the ~$3M vault), while the whale's own claim exceeds
/// it. That is the disarmed state exactly.
///
/// The pnl config floor (`MIN_ADL_TRIGGER` 45%) forces `max_pnl_trader` to
/// its 45% minimum. The position and vault-balance caps are raised for the
/// $200k positions and the $3M book.
#[test]
fn close_payout_on_a_netted_side_passes_the_raw_profit_through() {
    let mut config = default_config();
    config.max_pnl_trader = 45 * SCALAR_18 / 100;
    config.adl_max_pnl = 45 * SCALAR_18 / 100;
    config.adl_clear_target = 40 * SCALAR_18 / 100;
    config.max_pnl_withdraw = 40 * SCALAR_18 / 100;
    config.max_position_notional = 10_000_000_000_000;
    config.max_vault_balance = 1_000_000_000_000_000;
    // The whale marked at $500k plus the top-buyer exceed the default open
    // cap (the reserve marks longs at the ask). Widen both utilization caps
    // so the netted construction fits inside side capacity.
    config.max_util_open = SCALAR_18;
    config.max_util_withdraw = SCALAR_18;
    // Carry off. The whale is held across two accrual windows whose vault
    // backing and side reserve both move (the second LP deposit lands between
    // them). The carry debit would take a longer derivation than the
    // allowance arithmetic this test exists to pin. It is also not what the
    // payout under test is made of.
    config.borrow_rate = 0;
    config.increased_borrow_rate = 0;
    config.funding_increase = 0;
    config.funding_decrease = 0;
    config.threshold_stable_funding = 0;
    config.funding_min = 0;
    config.funding_max = 0;
    let mut fixture = TestFixture::create_with(config.clone());

    const PRICE_500K: i128 = 500_000 * PRICE_SCALAR;
    const PRICE_450K: i128 = 450_000 * PRICE_SCALAR;

    let lp1 = fixture.new_user(1_001_000 * DOLLAR);
    let lp2 = fixture.new_user(2_001_000 * DOLLAR);
    let whale = fixture.new_user(202_000 * DOLLAR);
    let loser = fixture.new_user(202_000 * DOLLAR);

    let seed = fixture.vault_deposit(&fixture.keeper, &lp1, 1_000_000 * DOLLAR, 0, PRICE_100K);
    assert!(seed.filled, "the $1M seed deposit fills");

    // The whale: $200k notional on $200k margin at $100k (2 BTC).
    let open_price = fixture.price(PRICE_100K);
    fixture.open(
        &whale,
        true,
        200_000 * DOLLAR,
        200_000 * DOLLAR,
        &open_price,
    );

    // At $500k the whale's ask-marked reserve alone ($1M) would exceed the
    // open cap on the original vault. The second deposit adds the headroom
    // before the move, while the gate still admits it.
    fixture.jump(5);
    let grow = fixture.vault_deposit(&fixture.keeper, &lp2, 2_000_000 * DOLLAR, 0, PRICE_100K);
    assert!(grow.filled, "the $2M growth deposit fills");

    // The loser buys the top: $300k notional on $190k margin at $500k.
    let top_price = fixture.price(PRICE_500K);
    fixture.open(&loser, true, 300_000 * DOLLAR, 190_000 * DOLLAR, &top_price);

    // At $450k: whale +$700k, loser -$30k, side ~$670k. The side nets UNDER
    // the ~$677k allowance read just before the close.
    fixture.jump(35);
    let allowance = fixture.vault.total_assets() * 225 / 1000;
    let uncapped = 2 * (450_000 - 100_000) * DOLLAR;
    assert!(uncapped > allowance, "the raw profit exceeds the allowance");

    let wallet_before = fixture.balance(&whale);
    let close_price = fixture.price(PRICE_450K);
    fixture.close(&whale, true, 200_000 * DOLLAR, 0, &close_price);
    let returned = fixture.balance(&whale) - wallet_before;

    // Whale fees, mirrored from config: open worsened the whole size
    // (0.5% base + 2% impact on $200k), the close improved it (0.3% base +
    // 2% impact). The self-keeper fill refunds the keeper cut of the close
    // fee. paid_pnl backs the realized profit out of the wallet delta.
    let margin_net = 200_000 * DOLLAR
        - factor_ceil(200_000 * DOLLAR, config.fee_dom)
        - impact_fee(200_000 * DOLLAR, config.impact_scalar);
    let close_fees = factor_ceil(200_000 * DOLLAR, config.fee_non_dom)
        + impact_fee(200_000 * DOLLAR, config.impact_scalar);
    let keeper_refund = close_fees / 10;
    let paid_pnl = returned - keeper_refund - margin_net + close_fees;

    // The netted side sits under the allowance, so nothing engages: the
    // whale's raw margin-backed profit passes through in full even though
    // its own claim alone exceeds the allowance (the accepted state).
    let side_net = uncapped - 30_000 * DOLLAR;
    assert!(
        side_net < allowance,
        "the side nets under the allowance (constructed state)"
    );
    assert_eq!(
        paid_pnl, uncapped,
        "the raw profit passes through untouched"
    );
    fixture.assert_borrowing_books(&fixture.users);
}
