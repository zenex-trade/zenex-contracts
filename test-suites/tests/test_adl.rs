//! Integration coverage for auto-deleveraging: the per-side pending-PnL
//! trigger and flags (`update_adl_state`/`get_adl`), the per-side open-stop,
//! the keeper-sized forced close (`execute_adl`) with its act-then-check
//! gates, and a sweep of sequential closes that records each failure.
//!
//! Each test drives a real position through the market, moves the verified
//! price to a chosen pending-PnL ratio, and asserts the flags, eligibility, and
//! the deleveraging settlement. Every expected amount is derived from the
//! default config by hand. The derivations live in the per-test comments.
//!
//! Shared anchors (default config, 7-dec token, BTC feed at 18 decimals):
//! - Both ADL rungs measure a side's pending PnL against half the vault
//!   balance: `adl_max_pnl` 50% arms, `adl_clear_target` 40% clears,
//!   `max_pnl_trader` 90% starts the haircut. `keeper_rate` is 10%. The
//!   fixture treasury takes 5%, and the vault banks the remaining 85% of
//!   every trade fee.
//! - The base long winner: `$1,000` notional on `$200` margin opened at
//!   `$100k` (tokens `1e5`), against a `$1,300` seeded vault. The open pays
//!   a `$5.10` fee (base 0.5% = `$5` plus the size-quadratic impact
//!   `notional^2 / impact_scalar` = `$0.10`), whose `$4.335` vault share
//!   lifts the backing to 13_043_350_000. The armed trigger is
//!   floor(50% * 13_043_350_000 / 2) = 3_260_837_500 and the clear allowance
//!   floor(40% * 13_043_350_000 / 2) = 2_608_670_000.
//! - The reserve marks longs at the ask, so a long winner deep enough to
//!   trigger ADL also over-reserves the book. The open-stop and eligibility
//!   tests therefore use short winners (a short's reserve is its fixed entry
//!   notional), the mirror image of a long-flagged book.

use soroban_sdk::Address;

use market::{MarketError, OrderKind};
use test_suites::carry::lone_side_carry;
use test_suites::constants::SCALAR_7;
use test_suites::test_fixture::TestFixture;
use test_suites::{default_config, SCALAR_18};

/// Vault backing seeded for every scenario: `$1,300` (token-dec).
const VAULT: i128 = 1_300 * SCALAR_7;

/// A fixture with `max_util_open`/`max_util_withdraw` widened to 500%.
///
/// The reserve model's utilization cap floors each side's own half the
/// vault (`side capacity = max_util_open * vault / 2`). The default 80% cap
/// cannot clear it at this file's deliberately thin `$1,300` vault against a
/// `$1,000` winner marked well past its entry. Widening only the utilization
/// caps keeps every other default (and every dollar figure derived from the
/// `$1,300` vault throughout this file) unchanged. ADL's own triggers are
/// unaffected by `max_util_open`/`max_util_withdraw`.
fn adl_config() -> market::Config {
    market::Config {
        max_util_open: 5 * SCALAR_18,
        max_util_withdraw: 5 * SCALAR_18,
        ..default_config()
    }
}

fn adl_fixture<'a>() -> TestFixture<'a> {
    TestFixture::create_with(adl_config())
}

/// The base winner's notional: `$1,000` (token-dec).
const NOTIONAL: i128 = 1_000 * SCALAR_7;
/// The base winner's margin: `$200` (token-dec), i.e. 5x.
const MARGIN: i128 = 200 * SCALAR_7;

/// Seed the vault, fund a user, open the base `$1,000`/`$200` long at
/// `$100k`, and return the user. tokens = floor(1e10 * 1e18 / 1e23) = 1e5.
fn open_base_long(fixture: &mut TestFixture) -> Address {
    fixture.seed_vault(VAULT);
    let user = fixture.new_user(2_000 * SCALAR_7);
    let open_price = fixture.price_usd(100_000);
    fixture.open(&user, true, NOTIONAL, MARGIN, &open_price);
    user
}

/// Below the trigger the flags stay unset: a long up `$50` at `$105k`
/// (side PnL = 1.05e10 - 1e10 = 5e8) sits far under the armed trigger
/// (3_260_837_500), so neither side flags.
#[test]
fn update_adl_state_below_trigger_leaves_flags_unset() {
    let mut fixture = adl_fixture();
    open_base_long(&mut fixture);
    fixture.jump(60);

    let price = fixture.price_usd(105_000);
    let state = fixture.market.update_adl_state(&price);

    assert!(!state.long, "long unset below trigger");
    assert!(!state.short, "short unset (empty side)");
    assert!(!fixture.market.get_adl().long, "stored long flag");
    fixture.assert_borrowing_books(&fixture.users);
}

/// Above the trigger only the winning side flags: a long up `$700` at `$170k`
/// (side PnL = 1.7e10 - 1e10 = 7e9) exceeds the 3_260_837_500 trigger, so the
/// long flags while the empty short side stays unset.
#[test]
fn update_adl_state_above_trigger_flags_winning_side_only() {
    let mut fixture = adl_fixture();
    open_base_long(&mut fixture);
    fixture.jump(60);

    let price = fixture.price_usd(170_000);
    let state = fixture.market.update_adl_state(&price);

    assert!(state.long, "winning long flags above trigger");
    assert!(!state.short, "empty short never flags");
    fixture.assert_borrowing_books(&fixture.users);
}

/// Hysteresis: once set at `$170k` (side PnL 7e9 > the 3_260_837_500 trigger),
/// the long flag holds while the ratio recovers into the band, `$130k` giving
/// 3e9 which sits above the 2_608_670_000 clear allowance but below the
/// trigger. It then clears once the side PnL falls to or below the allowance,
/// `$126k` giving 2.6e9.
#[test]
fn update_adl_state_holds_flag_through_hysteresis_band() {
    let mut fixture = adl_fixture();
    open_base_long(&mut fixture);
    fixture.jump(60);

    let set = fixture.price_usd(170_000);
    assert!(
        fixture.market.update_adl_state(&set).long,
        "flag set at trigger"
    );

    // Side PnL 3e9 is inside the band (2_608_670_000, 3_260_837_500): holds.
    let hold = fixture.price_usd(130_000);
    assert!(
        fixture.market.update_adl_state(&hold).long,
        "flag holds in band"
    );

    // Side PnL 2.6e9 <= the 2_608_670_000 clear allowance: the flag clears.
    let clear = fixture.price_usd(126_000);
    assert!(
        !fixture.market.update_adl_state(&clear).long,
        "flag clears at target"
    );
    fixture.assert_borrowing_books(&fixture.users);
}

/// The per-side open-stop blocks the flagged side and lets the other through.
///
/// A short winner keeps the book openable (its reserve is the fixed entry
/// notional, not a mark that balloons with the winning price). A `$1,000` short
/// at `$100k` dropped to `$30k` is up `$700` (side PnL 1e10 - 3e9 = 7e9 > the
/// 3_260_837_500 trigger), so the short side flags. A fresh short increase is
/// halted with `IncreaseHalted` (705) while a fresh long increase, on the
/// unflagged side, fills.
#[test]
fn adl_open_stop_blocks_flagged_side_and_allows_other() {
    let mut fixture = adl_fixture();
    fixture.seed_vault(VAULT);
    let user = fixture.new_user(2_000 * SCALAR_7);

    let open_price = fixture.price_usd(100_000);
    fixture.open(&user, false, NOTIONAL, MARGIN, &open_price);
    fixture.jump(60);

    // Drop to $30k: the short is up $700 and its side flags.
    let price = fixture.price_usd(30_000);
    let state = fixture.market.update_adl_state(&price);
    assert!(state.short, "short winner flags");
    assert!(!state.long, "long side unflagged");

    let opener = fixture.new_user(2_000 * SCALAR_7);

    // A fresh short increase (flagged side) is halted before it reaches the fill.
    let blocked = fixture.create_and_try_fill(
        &opener,
        &opener,
        false,
        OrderKind::MarketIncrease as u32,
        20 * SCALAR_7,
        20 * SCALAR_7,
        0,
        0,
        fixture.far_expiration(),
        &price,
    );
    assert!(!blocked.filled, "short increase halted");
    assert_eq!(
        blocked.error,
        MarketError::IncreaseHalted as u32,
        "705 open-stop"
    );

    // A fresh long increase (unflagged side) fills. Only the long side's
    // reserve is checked: ceil(6_666 long tokens * $30k) = 199_980_000, far
    // under the 500% cap on the long side's half of the vault.
    let allowed = fixture.create_and_try_fill(
        &opener,
        &opener,
        true,
        OrderKind::MarketIncrease as u32,
        20 * SCALAR_7,
        20 * SCALAR_7,
        0,
        0,
        fixture.far_expiration(),
        &price,
    );
    assert!(allowed.filled, "long increase fills on the unflagged side");
    assert_eq!(allowed.error, 0, "no error on the allowed side");
    fixture.assert_borrowing_books(&fixture.users);
}

/// Below the trigger `execute_adl` rejects with `AdlNotTriggered` (770): the
/// stored per-side flag was never armed at `$105k` (side PnL $50 against the
/// 3_260_837_500 trigger), so the flag gate fires before any position loads.
#[test]
fn execute_adl_below_trigger_rejects_not_triggered() {
    let mut fixture = adl_fixture();
    let user = open_base_long(&mut fixture);
    fixture.jump(60);

    let price = fixture.price_usd(105_000);
    fixture.market.update_adl_state(&price);
    let result = fixture
        .market
        .try_execute_adl(&fixture.keeper, &user, &true, &NOTIONAL, &price);

    assert_eq!(result, Err(Ok(MarketError::AdlNotTriggered.into())));
    // The rejected call traps: the position is untouched.
    assert_eq!(fixture.position(&user, true).notional, NOTIONAL);
    fixture.assert_borrowing_books(&fixture.users);
}

/// ADL only accepts closes that reduce the side's pending PnL: a losing
/// position inside the armed side is rejected by the act-then-check with
/// `AdlNotEligible` (772) and the close rolls back.
///
/// The armed side is a short book: the base `$1,000` winner opened at `$100k`
/// plus a `$20` loser opened by a second user at `$25k`. At `$30k` the side is
/// pending 1.02e10 - (1e5 + 8e3) * 3e22 / 1e18 = 6_960_000_000, past the
/// trigger, and arms. Closing the loser (pnl 2e8 - 2.4e8 = -4e7) RAISES the
/// side PnL to 7e9, so the check fires and the loser survives.
#[test]
fn execute_adl_on_losing_position_rejects_not_eligible() {
    let mut fixture = adl_fixture();
    fixture.seed_vault(VAULT);
    let winner = fixture.new_user(2_000 * SCALAR_7);
    let loser = fixture.new_user(2_000 * SCALAR_7);

    let open_price = fixture.price_usd(100_000);
    fixture.open(&winner, false, NOTIONAL, MARGIN, &open_price);
    fixture.jump(60);

    // The loser shorts $20 at $25k (tokens 8e3). The book is not armed yet,
    // and the short reserve (1.02e10 entry notional) stays under the cap.
    let low_price = fixture.price_usd(25_000);
    fixture.open(&loser, false, 20 * SCALAR_7, MARGIN, &low_price);
    fixture.jump(60);

    // Back at $30k the short side arms on the winner's gains.
    let price = fixture.price_usd(30_000);
    let state = fixture.market.update_adl_state(&price);
    assert!(state.short, "short side armed");

    let result =
        fixture
            .market
            .try_execute_adl(&fixture.keeper, &loser, &false, &(20 * SCALAR_7), &price);

    assert_eq!(result, Err(Ok(MarketError::AdlNotEligible.into())));
    assert_eq!(
        fixture.position(&loser, false).notional,
        20 * SCALAR_7,
        "loser untouched"
    );
    fixture.assert_borrowing_books(&fixture.users);
}

/// A keeper-sized forced close deleverages the winner and settles through the
/// regular decrease path. Inside the haircut regime the realized profit scales
/// by the trader allowance.
///
/// At `$170k` the side PnL is 7e9 against the 5_869_507_500 haircut allowance
/// (90% of half the 13_043_350_000 vault), so the $600 close's raw pro-rata
/// pnl of 4_200_000_000 scales to floor(4.2e9 * 5_869_507_500 / 7e9) =
/// 3_521_704_500. The close fee is the improving ceil(0.3% * 6e9) =
/// 18_000_000 plus the (6e9)^2 / 1e14 = 360_000 impact, and the 60 second
/// hold's carry settles alongside it. Both come out of the realized profit:
/// the trader receives 3_503_344_500 less the carry, the keeper the 10% fee
/// cut (1_836_000), and the vault funds 3_521_704_500 - 15_606_000 (its fee
/// share) less its share of the borrowing fee. The remaining side PnL (2.8e9)
/// stays far above the clear allowance re-measured on the settled balance, so
/// the close lands.
#[test]
fn execute_adl_partial_close_deleverages_within_the_band() {
    let mut fixture = adl_fixture();
    let user = open_base_long(&mut fixture);
    let held: i128 = 60;
    fixture.jump(held as u64);

    let price = fixture.price_usd(170_000);
    fixture.market.update_adl_state(&price);

    let trader_before = fixture.balance(&user);
    let keeper_before = fixture.balance(&fixture.keeper);
    let vault_before = fixture.vault.total_assets();

    let close = 600 * SCALAR_7; // $600 of the $1,000 winner
    let reward = fixture
        .market
        .execute_adl(&fixture.keeper, &user, &true, &close, &price);

    // Surviving position row: notional and tokens reduced pro-rata, the
    // margin untouched (the fee settled out of the realized profit).
    let position = fixture.position(&user, true);
    assert_eq!(position.notional, 4_000_000_000, "surviving notional");
    assert_eq!(position.tokens, 40_000, "surviving tokens (base-dec)");
    assert_eq!(position.margin, 1_949_000_000, "margin untouched");
    assert_eq!(
        fixture.market.get_market_data().notional.long,
        4_000_000_000,
        "long OI reduced"
    );

    // Keeper payout = the 10% cut of the close fee (base + impact).
    assert_eq!(reward, 1_836_000, "keeper payout (fee cut)");
    assert_eq!(
        fixture.balance(&fixture.keeper) - keeper_before,
        1_836_000,
        "keeper credited the fee cut"
    );

    // The hold accrues carry against the `$1,300` vault plus the open's
    // `$4.335` fee share: the lone long reserves its ask-marked value at the
    // `$170k` mark (1e5 tokens * $170k). The one-sided book charges funding
    // at the `funding_min` floor over this window.
    let carry = lone_side_carry(
        &adl_config(),
        NOTIONAL,
        100_000 * 170_000,
        13_043_350_000,
        held,
    );

    // Victim proceeds = the haircut PnL less the close fees and the carry it
    // covered.
    assert_eq!(
        fixture.balance(&user) - trader_before,
        3_503_344_500 - carry.total(),
        "victim paid the haircut PnL net of the fees and the carry"
    );

    // The vault funds the haircut PnL, offset by its 85% share of the fee and
    // by the borrowing fee net of the treasury's 5% (the keeper takes no cut
    // of borrowing). The funding leg banks into the market's credit pool.
    assert_eq!(
        vault_before - fixture.vault.total_assets(),
        3_506_098_500 - (carry.borrowing - carry.borrowing / 20),
        "vault funded the realized PnL net of its fee share and the borrowing"
    );
    fixture.assert_borrowing_books(&fixture.users);
}

/// A keeper sweep of four sequential `execute_adl` calls records each failed
/// close and runs to the end.
///
/// Four targets at `$170k`: the first $600 close lands (as in the partial-close
/// test). The second $600 exceeds the surviving $400, so it runs as a full
/// close, lands the side PnL at zero under the clear allowance, and is
/// recorded as `AdlOvershoot` (771). The third targets the unarmed short
/// side and reports `AdlNotTriggered` (770). The fourth repeats the
/// overshoot. Every failure rolls back only its own close.
#[test]
fn adl_sweep_records_each_failed_close() {
    let mut fixture = adl_fixture();
    let user = open_base_long(&mut fixture);
    fixture.jump(60);

    let price = fixture.price_usd(170_000);
    fixture.market.update_adl_state(&price);

    let close = 600 * SCALAR_7;
    let adl_close = |is_long: bool| {
        fixture
            .market
            .try_execute_adl(&fixture.keeper, &user, &is_long, &close, &price)
    };

    assert_eq!(
        adl_close(true),
        Ok(Ok(1_836_000)),
        "first close keeper payout"
    );
    assert_eq!(
        adl_close(true),
        Err(Ok(MarketError::AdlOvershoot.into())),
        "771 recorded"
    );
    assert_eq!(
        adl_close(false),
        Err(Ok(MarketError::AdlNotTriggered.into())),
        "770 recorded"
    );
    assert_eq!(
        adl_close(true),
        Err(Ok(MarketError::AdlOvershoot.into())),
        "771 recorded"
    );

    // The overshoots trapped: the survivor still holds the post-first-close size.
    assert_eq!(fixture.position(&user, true).notional, 4_000_000_000);
    fixture.assert_borrowing_books(&fixture.users);
}

/// The stored flag survives a legal sweep (which must leave the side at or
/// above the clear allowance) and clears on the next update once the price
/// recovers. After the $600 close the side still pends 2.8e9 at `$170k`. At
/// `$120k` it pends 8e8, under the 1_907_451_242 clear allowance of the settled
/// 9_537_256_212 vault, so a fresh `update_adl_state` disarms the side.
#[test]
fn update_adl_state_clears_after_price_recovers() {
    let mut fixture = adl_fixture();
    let user = open_base_long(&mut fixture);
    fixture.jump(60);

    let price = fixture.price_usd(170_000);
    fixture.market.update_adl_state(&price);
    fixture
        .market
        .execute_adl(&fixture.keeper, &user, &true, &(600 * SCALAR_7), &price);

    // The close cannot legally clear the flag: the side stays armed.
    assert!(fixture.market.get_adl().long, "flag holds after the close");

    // The price eases to $120k: side PnL 4e4 * 1.2e23 / 1e18 - 4e9 = 8e8,
    // under the clear allowance, and the flag disarms.
    let recovered = fixture.price_usd(120_000);
    let state = fixture.market.update_adl_state(&recovered);
    assert!(!state.long, "flag clears once the ratio recovers");
    assert!(!state.short, "empty short unset");
    fixture.assert_borrowing_books(&fixture.users);
}

/// The haircut regime implies ADL is armed. `check_valid` orders the bands
/// (`adl_max_pnl <= max_pnl_trader`) on the same half-vault measure, so a side
/// crosses the ADL trigger (3_260_837_500) before the haircut allowance
/// (5_869_507_500). At `$140k` the long side pends 4e9: armed, still under the
/// haircut. At `$170k` it pends 7e9, past it: still armed, and a sized forced
/// close executes, so wherever a voluntary close would be haircut, keepers can
/// deleverage instead.
#[test]
fn adl_is_armed_inside_the_haircut_regime() {
    let mut fixture = adl_fixture();
    let user = open_base_long(&mut fixture);
    fixture.jump(60);

    // The validated band ordering is what generalizes the two spot checks.
    let config = fixture.market.get_config();
    assert!(
        config.adl_max_pnl <= config.max_pnl_trader,
        "trigger at or below the haircut threshold"
    );

    // Side PnL 4e9: above the trigger, below the haircut allowance. ADL leads
    // the haircut by the whole band.
    let inside_band = fixture.price_usd(140_000);
    assert!(
        fixture.market.update_adl_state(&inside_band).long,
        "armed before the haircut engages"
    );

    // Side PnL 7e9: past the haircut allowance, the flag holds and a sized
    // deleveraging close executes.
    let past_cap = fixture.price_usd(170_000);
    assert!(
        fixture.market.update_adl_state(&past_cap).long,
        "armed inside the haircut regime"
    );
    let notional_before = fixture.position(&user, true).notional;
    let result =
        fixture
            .market
            .try_execute_adl(&fixture.keeper, &user, &true, &(600 * SCALAR_7), &past_cap);
    assert!(
        result.is_ok(),
        "forced close executes inside the haircut regime"
    );
    assert_eq!(
        fixture.position(&user, true).notional,
        notional_before - 600 * SCALAR_7,
        "the deleveraging close reduced the position by the requested notional",
    );
    fixture.assert_borrowing_books(&fixture.users);
}
