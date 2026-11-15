//! Fee-escape regression suite: a deeply-eroded position must not dodge the
//! saturated liquidation fee by closing itself, fully or in fractions.
//!
//! # The rule under test
//!
//! Every liquidation charges `min(equity, liq_fee * notional)`. A position
//! whose settled equity has fallen below `liq_fee * notional` is in the
//! saturated band: `Position::liquidate` charges its entire remaining equity
//! as the fee and pays the trader nothing. Every decrease (partial or full,
//! manual or ADL) opens by settling the position to close-grade numbers. It
//! refuses with `PositionLiquidatable` (723) when the settled equity is below
//! the maintenance margin on the **pre-decrease** notional. `liquidate`
//! accepts on the exact inverse of the same predicate, so for a liquidatable
//! position the keeper liquidation is the only exit.
//!
//! The escape this file guards against: if the gate measured the survivor
//! against the **reduced** notional, closing ~80% (fixture config) of a
//! deeply-eroded position would shrink the maintenance bar under its
//! near-invariant equity. That close would convert the ~98_000_000 saturated
//! fee into a payout of the same size.
//!
//! # Derivation anchors (default config, 7-dec token, BTC feed at 18 decimals)
//!
//! - Position: `$1,000` notional on `$200` margin (5x), opened at `$100k`.
//!   Opening a lone long is fully worsening: base `fee_dom` 0.5% = `5e7`, plus
//!   the size-quadratic impact `(1e10)^2 / 1e14` = `1e6`. Margin banks
//!   `2e9 - 5.1e7 = 1_949_000_000`, tokens `1e5`.
//! - Every close is fully improving (it shrinks the only side), so base is
//!   `fee_non_dom` 0.3% of the closed notional plus the same quadratic impact.
//! - Mark `$81,800`: whole-position `pnl = 1e5 * 8.18e22 / 1e18 - 1e10
//!   = -1_820_000_000`.
//! - Ratios on the full `1e10` notional: maintenance 5% = `5e8`, liquidation
//!   fee cap `liq_fee` 1% = `1e8`, initial margin 10% = `1e9`.
//!
//! # Confounders satisfied
//!
//! - **`notional_lock` (30s).** Every hard-tier test jumps 35 seconds past the
//!   open, so the lock is never the reason a call rejects.
//!   `notional_lock_still_gates_healthy_decreases` pins the lock's own error
//!   on a healthy position.
//! - **Accruals.** The default config runs the deployed carry curves, so the
//!   35 second jump does move equity. [`hold_carry`] derives the borrowing and
//!   funding it charges, and every exact settled figure below subtracts it.
//!   The charge is ~113 stroops against a ~98_000_000 equity, so it moves no
//!   position across a tier boundary.

use soroban_sdk::{Address, Bytes};

use market::{MarketError, OrderKind};
use test_suites::carry::{lone_side_carry, Carry};
use test_suites::constants::{BTC_PRICE, EXEC_FEE, PRICE_SCALAR, SCALAR_18, SCALAR_7};
use test_suites::default_config;
use test_suites::setup::{create_fixture_with_data, DEFAULT_VAULT_LIQUIDITY};
use test_suites::test_fixture::{FillAttempt, TestFixture};

/// Notional every position under test opens: `$1,000` (token-dec).
const NOTIONAL: i128 = 1_000 * SCALAR_7;
/// Margin every position under test posts: `$200` (token-dec), i.e. 5x.
const MARGIN: i128 = 200 * SCALAR_7;

/// Margin banked by the open, net of its own fees (token-dec).
const MARGIN_AFTER_OPEN: i128 = 1_949_000_000;
/// Base size bought at the `$100k` ask (base-dec). One base unit is worth a
/// cent at `$100k` on the 18-dec feed, so `$1,000` buys `1e5` of them.
const TOKENS_AFTER_OPEN: i128 = 100_000;

/// The mark that drives the position into the hard tier (whole dollars).
const HARD_TIER_PRICE: i128 = 81_800;
/// The open mark, at which the position is comfortably healthy (whole dollars).
const HEALTHY_PRICE: i128 = 100_000;
/// Whole-position PnL at [`HARD_TIER_PRICE`] (token-dec).
const PNL_AT_HARD_TIER: i128 = -1_820_000_000;

/// Fees a full close settles at this size: base `0.3% * 1e10` plus impact `1e6`.
const FULL_CLOSE_FEES: i128 = 31_000_000;
/// Trade fee an open of [`NOTIONAL`] pays: base `0.5% * 1e10` plus impact `1e6`.
const OPEN_FEES: i128 = 51_000_000;

/// The liquidation-fee cap on the full notional: `liq_fee` 1% of `1e10`.
/// Equity below this saturates the fee.
const LIQ_MARGIN: i128 = 100_000_000;
/// Maintenance margin on the full notional: 5% of `1e10`.
const MAINTENANCE_FULL: i128 = 500_000_000;

/// Seconds jumped after an open to clear the 30-second `notional_lock`.
const PAST_NOTIONAL_LOCK: u64 = 35;

/// The carry one standard position owes over [`PAST_NOTIONAL_LOCK`] seconds at
/// a `price_usd` mark, with `positions` identical longs sharing the side
/// (token-dec).
///
/// The side reserves its ask-marked value, `positions * 1e5 * price_usd` at
/// the 18-dec feed, against the seeded liquidity plus each open's vault leg
/// (the `5.1e7` trade fee less the keeper's 10% and the treasury's 5%). The
/// book is one-sided, so the funding charge has no receiver and runs at the
/// `funding_min` floor over this window.
fn hold_carry(price_usd: i128, positions: i128) -> Carry {
    let open_vault_leg = OPEN_FEES - OPEN_FEES / 10 - OPEN_FEES / 20;
    lone_side_carry(
        &default_config(),
        NOTIONAL,
        positions * TOKENS_AFTER_OPEN * price_usd,
        DEFAULT_VAULT_LIQUIDITY + positions * open_vault_leg,
        PAST_NOTIONAL_LOCK as i128,
    )
}

/// Settled equity of a full close at [`HARD_TIER_PRICE`] with `positions`
/// longs on the side: what the saturated liquidation fee charges in full
/// (token-dec).
fn hard_tier_equity(positions: i128) -> i128 {
    MARGIN_AFTER_OPEN + PNL_AT_HARD_TIER
        - FULL_CLOSE_FEES
        - hold_carry(HARD_TIER_PRICE, positions).total()
}

/// The vault's leg of a saturated liquidation at [`HARD_TIER_PRICE`] with
/// `positions` longs on the side (token-dec): the realized loss, the
/// liquidation fee and the trade fee net of the keeper's 10% and the
/// treasury's 5%, and the borrowing fee net of the treasury's 5%.
///
/// The keeper takes no cut of borrowing, and the funding leg banks into the
/// market's credit pool, so the vault never sees it.
fn hard_tier_vault_leg(positions: i128) -> i128 {
    let liq_fee = hard_tier_equity(positions);
    let borrowing = hold_carry(HARD_TIER_PRICE, positions).borrowing;
    -PNL_AT_HARD_TIER
        + (liq_fee - liq_fee / 10 - liq_fee / 20)
        + (FULL_CLOSE_FEES - FULL_CLOSE_FEES / 10 - FULL_CLOSE_FEES / 20)
        + (borrowing - borrowing / 20)
}

/// Open the standard 5x long at `$100k` and jump clear of the notional lock,
/// returning its user.
fn open_five_x_past_lock(fixture: &mut TestFixture) -> Address {
    let user = fixture.new_user(1_000 * SCALAR_7);
    let open_price = fixture.price(BTC_PRICE);
    fixture.open(&user, true, NOTIONAL, MARGIN, &open_price);
    fixture.jump(PAST_NOTIONAL_LOCK);
    user
}

/// Submit and immediately attempt a `MarketDecrease` of `notional` with **no**
/// withdrawal, keepered by the fixture keeper so the trader's own wallet does
/// not absorb the keeper cut. The order rests if the fill leg rejects.
fn try_decrease(
    fixture: &TestFixture,
    user: &Address,
    notional: i128,
    price: &Bytes,
) -> FillAttempt {
    fixture.create_and_try_fill(
        &fixture.keeper,
        user,
        true,
        OrderKind::MarketDecrease as u32,
        notional,
        0,
        0,
        0,
        fixture.far_expiration(),
        price,
    )
}

/// Pre-fee equity of `(user, true)` marked at the `bid` implied by
/// `price_usd`: `margin + pnl` (token-dec). Mirrors `math::pnl` for a long.
fn equity_at(fixture: &TestFixture, user: &Address, price_usd: i128) -> i128 {
    let position = fixture.position(user, true);
    let bid = price_usd * PRICE_SCALAR;
    let value = position.tokens * bid / SCALAR_18;
    position.margin + value - position.notional
}

/// Assert `(user, true)` still holds the untouched standard position.
fn assert_position_intact(fixture: &TestFixture, user: &Address) {
    let position = fixture.position(user, true);
    assert_eq!(position.notional, NOTIONAL, "notional intact");
    assert_eq!(position.margin, MARGIN_AFTER_OPEN, "margin intact");
    assert_eq!(position.tokens, TOKENS_AFTER_OPEN, "tokens intact");
}

// ---------------------------------------------------------------------------
// 1. Baseline: the hard tier is real, and liquidation still works
// ---------------------------------------------------------------------------

/// The `$81,800` mark puts the position squarely in the saturated band: its
/// settled equity (~`98_000_000`) is below the `1e8` fee cap, so the
/// liquidation fee charges the whole remainder and pays the trader nothing.
///
/// This is the control the rest of the file measures against. It also pins the
/// exact settlement legs a liquidation moves.
#[test]
fn saturated_liquidation_charges_the_whole_remainder() {
    let mut fixture = create_fixture_with_data();
    let user = open_five_x_past_lock(&mut fixture);

    // The tier is decided on equity net of the close's own fees and the carry
    // the hold accrued, which is what `liquidate` measures on the settled
    // numbers.
    let carry = hold_carry(HARD_TIER_PRICE, 1);
    let settled_equity =
        equity_at(&fixture, &user, HARD_TIER_PRICE) - FULL_CLOSE_FEES - carry.total();
    assert_eq!(
        settled_equity,
        hard_tier_equity(1),
        "settled equity at the mark"
    );
    assert!(
        settled_equity < LIQ_MARGIN,
        "below the fee cap: the fee saturates"
    );
    assert!(
        settled_equity < MAINTENANCE_FULL,
        "below maintenance: liquidatable"
    );

    let trader_before = fixture.balance(&user);
    let vault_before = fixture.vault.total_assets();
    let treasury_before = fixture.balance(&fixture.treasury.address);

    let price = fixture.price_usd(HARD_TIER_PRICE);
    let reward = fixture
        .market
        .execute_liquidation(&fixture.keeper, &user, &true, &price);

    // keeper_rate 10% of the 3.1e7 close fee plus 10% of the saturated
    // liquidation fee, which is the settled equity itself.
    assert_eq!(
        reward,
        FULL_CLOSE_FEES / 10 + settled_equity / 10,
        "keeper reward"
    );
    assert_eq!(
        fixture.balance(&user),
        trader_before,
        "saturation pays the trader nothing"
    );
    // Treasury takes 5% of the trade fee (1_550_000), 5% of the saturated
    // liquidation fee, and 5% of the borrowing fee.
    assert_eq!(
        fixture.balance(&fixture.treasury.address) - treasury_before,
        FULL_CLOSE_FEES / 20 + settled_equity / 20 + carry.borrowing / 20,
        "treasury cut of the trade, liquidation, and borrowing fees",
    );
    // Vault banks the 1_820_000_000 realized loss, the liquidation fee and
    // the trade fee net of the keeper and treasury cuts, and the borrowing
    // fee net of the treasury cut.
    assert_eq!(
        fixture.vault.total_assets() - vault_before,
        hard_tier_vault_leg(1),
        "vault banks loss + liq fee + fee remainder + borrowing",
    );
    assert_eq!(
        fixture.position(&user, true).notional,
        0,
        "position cleared"
    );
}

// ---------------------------------------------------------------------------
// 2. The gate: no decrease of a liquidatable position lands
// ---------------------------------------------------------------------------

/// A voluntary close of the whole deeply-eroded position rejects with
/// `PositionLiquidatable` (723): the entry gate settles the position and
/// refuses before the close branch is reached.
#[test]
fn hard_tier_full_close_is_rejected_by_the_liquidatable_gate() {
    let mut fixture = create_fixture_with_data();
    let user = open_five_x_past_lock(&mut fixture);

    let price = fixture.price_usd(HARD_TIER_PRICE);
    let attempt = try_decrease(&fixture, &user, i128::MAX, &price);

    assert!(!attempt.filled, "the full close must not land");
    assert_eq!(
        attempt.error,
        MarketError::PositionLiquidatable as u32,
        "the liquidatable gate rejects with 723",
    );
    assert_position_intact(&fixture, &user);
}

/// The 80% close that would shrink the maintenance bar under the surviving
/// equity rejects at the same gate, before notional moves. The keeper
/// liquidation (and its saturated fee) remains the only exit.
#[test]
fn hard_tier_partial_close_is_rejected_by_the_liquidatable_gate() {
    let mut fixture = create_fixture_with_data();
    let user = open_five_x_past_lock(&mut fixture);

    // Pre-decrease: the position is liquidatable and in the saturated band.
    let settled_equity = equity_at(&fixture, &user, HARD_TIER_PRICE) - FULL_CLOSE_FEES;
    assert!(
        settled_equity < LIQ_MARGIN,
        "saturated band before the attempt"
    );

    let price = fixture.price_usd(HARD_TIER_PRICE);
    let attempt = try_decrease(&fixture, &user, 800 * SCALAR_7, &price);

    assert!(!attempt.filled, "the 80% close must not land");
    assert_eq!(
        attempt.error,
        MarketError::PositionLiquidatable as u32,
        "the liquidatable gate rejects with 723",
    );
    assert_position_intact(&fixture, &user);

    // The saturated fee still lands on the untouched position.
    let trader_before = fixture.balance(&user);
    fixture
        .market
        .execute_liquidation(&fixture.keeper, &user, &true, &price);
    // The liquidation clears the resting decrease order and refunds its
    // escrowed exec_fee. The saturated fee itself pays the trader nothing.
    assert_eq!(
        fixture.balance(&user) - trader_before,
        EXEC_FEE,
        "only the resting order's escrow comes back",
    );
    assert_eq!(
        fixture.position(&user, true).notional,
        0,
        "position cleared"
    );
}

/// No fraction escapes: every close size from 25% to the full position rejects
/// with 723 at the entry gate. The gate runs before notional shrinks, so the
/// fraction cannot matter.
#[test]
fn no_fraction_of_a_liquidatable_position_escapes() {
    let cases = [
        (250 * SCALAR_7, "25%"),
        (450 * SCALAR_7, "45%"),
        (600 * SCALAR_7, "60%"),
        (790 * SCALAR_7, "79%"),
        (800 * SCALAR_7, "80%"),
        (900 * SCALAR_7, "90%"),
        (i128::MAX, "full"),
    ];

    for (closed, label) in cases {
        let mut fixture = create_fixture_with_data();
        let user = open_five_x_past_lock(&mut fixture);
        let price = fixture.price_usd(HARD_TIER_PRICE);

        let attempt = try_decrease(&fixture, &user, closed, &price);

        assert!(!attempt.filled, "{label} close must not land");
        assert_eq!(
            attempt.error,
            MarketError::PositionLiquidatable as u32,
            "{label} rejects on the liquidatable gate (723)",
        );
        assert_position_intact(&fixture, &user);
    }
}

/// Head to head: two identical hard-tier positions, one of which attempts the
/// 80% escape first. Both are liquidated at the same mark, both collect
/// nothing, and both move identical legs to the vault. The attempted escape
/// changes no outcome.
///
/// The two open in the same ledger, so they snapshot the same accrual indices
/// and owe the same carry at the liquidation. A twin opened later would owe
/// less carry and settle a different leg for a reason that has nothing to do
/// with the escape attempt.
#[test]
fn escaper_and_twin_end_in_the_same_place() {
    let mut fixture = create_fixture_with_data();
    let escaper = fixture.new_user(1_000 * SCALAR_7);
    let twin = fixture.new_user(1_000 * SCALAR_7);
    let open_price = fixture.price(BTC_PRICE);
    fixture.open(&escaper, true, NOTIONAL, MARGIN, &open_price);
    fixture.open(&twin, true, NOTIONAL, MARGIN, &open_price);
    // One jump clears the notional lock for both.
    fixture.jump(PAST_NOTIONAL_LOCK);

    let escaper_wallet_before = fixture.balance(&escaper);
    let twin_wallet_before = fixture.balance(&twin);
    let price = fixture.price_usd(HARD_TIER_PRICE);

    // The escape attempt rejects. The order rests with its escrowed exec_fee.
    let attempt = try_decrease(&fixture, &escaper, 800 * SCALAR_7, &price);
    assert!(!attempt.filled, "the escape must not land");
    assert_position_intact(&fixture, &escaper);

    // Both positions are liquidated at the same mark. The closes shrink the
    // only populated side, so both are fully improving and their legs are
    // directly comparable.
    let escaper_vault_before = fixture.vault.total_assets();
    fixture
        .market
        .execute_liquidation(&fixture.keeper, &escaper, &true, &price);
    let escaper_vault_leg = fixture.vault.total_assets() - escaper_vault_before;

    let twin_vault_before = fixture.vault.total_assets();
    fixture
        .market
        .execute_liquidation(&fixture.keeper, &twin, &true, &price);
    let twin_vault_leg = fixture.vault.total_assets() - twin_vault_before;

    assert_eq!(escaper_vault_leg, twin_vault_leg, "identical vault legs");
    // Two longs share the side, so the side reserve (and with it the
    // borrowing rate) is twice one position's.
    assert_eq!(
        escaper_vault_leg,
        hard_tier_vault_leg(2),
        "loss + liq fee + fee remainder + borrowing"
    );
    // The escaper's only wallet motion is the escrowed exec_fee going out at
    // order creation and coming back when the liquidation clears the resting
    // order: both traders end exactly where they started, minus everything.
    assert_eq!(
        fixture.balance(&escaper) - escaper_wallet_before,
        0,
        "the attempted escape changes nothing for the escaper",
    );
    assert_eq!(
        fixture.balance(&twin) - twin_wallet_before,
        0,
        "the twin collects nothing"
    );
}

// ---------------------------------------------------------------------------
// 3. The gate does not over-block
// ---------------------------------------------------------------------------

/// A healthy position's partial close still lands: at the open mark the settled
/// equity clears maintenance by a wide margin, the gate passes, and the 60%
/// decrease fills.
#[test]
fn healthy_partial_close_still_lands() {
    let mut fixture = create_fixture_with_data();
    let user = open_five_x_past_lock(&mut fixture);

    let price = fixture.price_usd(HEALTHY_PRICE);
    let attempt = try_decrease(&fixture, &user, 600 * SCALAR_7, &price);

    assert!(
        attempt.filled,
        "a healthy decrease lands: error {}",
        attempt.error
    );
    assert_eq!(
        fixture.position(&user, true).notional,
        400 * SCALAR_7,
        "the survivor keeps the unclosed fraction",
    );
}

/// `notional_lock` still gates early decreases on a healthy position: while the
/// lock is live the close rejects with `NotionalLocked` (721) after passing the
/// liquidatable gate. The identical request lands once the lock expires.
#[test]
fn notional_lock_still_gates_healthy_decreases() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(1_000 * SCALAR_7);
    let open_price = fixture.price(BTC_PRICE);
    fixture.open(&user, true, NOTIONAL, MARGIN, &open_price);

    // Ten seconds in, the whole notional is still locked.
    fixture.jump(10);
    let price = fixture.price_usd(HEALTHY_PRICE);
    let locked = try_decrease(&fixture, &user, 600 * SCALAR_7, &price);
    assert!(!locked.filled, "a locked decrease cannot land");
    assert_eq!(
        locked.error,
        MarketError::NotionalLocked as u32,
        "the lock rejects with 721",
    );

    // Past the 30-second lock, the identical request lands.
    fixture.jump(PAST_NOTIONAL_LOCK);
    let price = fixture.price_usd(HEALTHY_PRICE);
    let unlocked = try_decrease(&fixture, &user, 600 * SCALAR_7, &price);
    assert!(
        unlocked.filled,
        "the same close lands once the lock expires"
    );
    assert_eq!(fixture.position(&user, true).notional, 400 * SCALAR_7);
}
