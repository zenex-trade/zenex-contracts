//! Integration coverage for the `Frozen`-status borrowing-config waiver:
//! `set_config` may change borrowing params on a frozen market without the
//! same-ledger accrual (which `Frozen` makes impossible, as `Market::load` is
//! blocked). The first accrual at unfreeze therefore prices the entire frozen
//! window at the new rates. That is the retroactive fee waiver: during an
//! emergency freeze the owner can zero or soften the carry for an interval
//! nobody could exit. Every other status keeps the accrue-before-rate-change
//! guard. Without a waiver the window bills at the active rate, and the
//! first fill after unfreeze prices it into the share price.

use market::{Config, MarketError, Status, VaultOrderKind};
use test_suites::carry::borrowing_index;
use test_suites::constants::{BTC_PRICE, EXEC_FEE};
use test_suites::test_fixture::{TestFixture, TREASURY_RATE};
use test_suites::{default_config, SCALAR_18, SCALAR_7};

/// Borrowing kink curve turned on, funding left at zero (mirrors
/// `test_funding::borrowing_config`): below-kink slope 4e7 per second,
/// full-utilization rate 4e8, kink at the default 80% utilization.
fn borrowing_config() -> Config {
    Config {
        borrow_rate: 40_000_000,
        increased_borrow_rate: 400_000_000,
        // Funding off, so the borrowing index carries the only charge the
        // waiver tests measure.
        funding_increase: 0,
        funding_decrease: 0,
        threshold_stable_funding: 0,
        funding_min: 0,
        funding_max: 0,
        ..default_config()
    }
}

/// The waiver target: the same curve with both rates zeroed.
fn zeroed_borrowing_config() -> Config {
    Config {
        borrow_rate: 0,
        increased_borrow_rate: 0,
        ..borrowing_config()
    }
}

/// A funded fixture with the borrowing curve on and a lone $1,000 long open,
/// so the long side is dominant and accrues whenever time passes.
fn borrowing_fixture<'a>() -> TestFixture<'a> {
    let mut fixture = TestFixture::create_with(borrowing_config());
    fixture.seed_vault(5_000 * SCALAR_7);
    let user = fixture.new_user(1_000 * SCALAR_7);
    let price = fixture.price(BTC_PRICE);
    fixture.open(&user, true, 1_000 * SCALAR_7, 200 * SCALAR_7, &price);
    fixture
}

/// The full-waiver runbook: freeze, zero the borrowing rates while frozen (no
/// same-ledger accrual possible or required), unfreeze, and the first accrual
/// bills the whole frozen window at 0. The borrowing index stands exactly
/// still. The original rates then restore under the just-run accrual.
#[test]
fn frozen_waiver_bills_frozen_window_at_zero() {
    let fixture = borrowing_fixture();

    // Sanity: the curve bites while active. An hour of a dominant long moves
    // its borrowing index, so a standing-still index below is meaningful.
    fixture.jump(3_600);
    let price = fixture.price(BTC_PRICE);
    let data = fixture.market.accrue(&price);
    assert!(
        data.borrowing_idx.long > 0,
        "borrowing accrues while active"
    );
    let idx_at_freeze = data.borrowing_idx.long;

    // Freeze, then zero the borrowing rates mid-freeze: the accrual guard is
    // waived under `Frozen`.
    fixture.market.set_status(&(Status::Frozen as u32));
    fixture.market.set_config(&zeroed_borrowing_config());

    // A day passes frozen. Nothing can accrue (`Market::load` is blocked).
    fixture.jump(86_400);

    // Unfreeze and run the first accrual: the entire frozen window prices at
    // the new zero rate.
    fixture.market.set_status(&(Status::Active as u32));
    let price = fixture.price(BTC_PRICE);
    let data = fixture.market.accrue(&price);
    assert_eq!(
        data.borrowing_idx.long, idx_at_freeze,
        "frozen window billed at 0"
    );
    assert_eq!(
        data.accrued_at,
        fixture.env.ledger().timestamp(),
        "the accrual advanced through the frozen window",
    );

    // Restore the original rates under the same-ledger accrual just run: the
    // waived window stays at 0 and the guard holds again for active traffic.
    fixture.market.set_config(&borrowing_config());
    assert_eq!(
        fixture.market.get_config().borrow_rate,
        40_000_000,
        "rates restored"
    );
}

/// On an `Active` market the guard is unchanged: a borrowing-param change
/// without a same-ledger accrual still traps with `MarketNotAccrued`. The
/// same change lands once an accrual runs in the same ledger.
#[test]
fn active_borrowing_change_still_requires_same_ledger_accrual() {
    let fixture = borrowing_fixture();
    fixture.jump(3_600);

    let result = fixture.market.try_set_config(&zeroed_borrowing_config());
    assert_eq!(
        result,
        Err(Ok(MarketError::MarketNotAccrued.into())),
        "active market still requires the same-ledger accrual",
    );

    // The same change lands after a same-ledger accrue.
    let price = fixture.price(BTC_PRICE);
    fixture.market.accrue(&price);
    fixture.market.set_config(&zeroed_borrowing_config());
    assert_eq!(
        fixture.market.get_config().borrow_rate,
        0,
        "change lands post-accrual"
    );
}

/// `Retired` is terminal, not an emergency freeze: the waiver does not apply
/// and a borrowing-param change over a stale accrual still traps.
#[test]
fn retired_borrowing_change_still_rejects() {
    // An empty book retires directly from Active. Time passes first so the
    // deploy-time accrual is stale.
    let fixture = TestFixture::create_with(borrowing_config());
    fixture.jump(3_600);
    fixture.market.set_status(&(Status::Retired as u32));

    let result = fixture.market.try_set_config(&zeroed_borrowing_config());
    assert_eq!(
        result,
        Err(Ok(MarketError::MarketNotAccrued.into())),
        "retired market keeps the guard",
    );
}

/// Without a waiver the frozen window bills at the active rate, all at the
/// first fill after unfreeze, and that fill prices it. A deposit placed
/// before the freeze cannot fill during it, so no depositor buys in before
/// the window's borrowing counts in the share price.
#[test]
fn the_first_deposit_after_unfreeze_prices_the_whole_frozen_window() {
    let mut fixture = borrowing_fixture();
    let lp = fixture.new_user(100 * SCALAR_7 + EXEC_FEE);
    let id = fixture.market.create_vault_order(
        &lp,
        &(VaultOrderKind::Deposit as u32),
        &(100 * SCALAR_7),
        &0,
    );

    fixture.market.set_status(&(Status::Frozen as u32));
    fixture.jump(86_400);
    let price = fixture.price(BTC_PRICE);
    assert_eq!(
        fixture
            .market
            .try_execute_vault_order(&fixture.keeper, &lp, &id, &price),
        Err(Ok(MarketError::MarketFrozen.into())),
        "no deposit fills while frozen",
    );

    fixture.market.set_status(&(Status::Active as u32));
    let vault_balance = fixture.vault.total_assets();
    fixture
        .market
        .execute_vault_order(&fixture.keeper, &lp, &id, &price);

    // The $1,000 long opened at index 0 and marks zero PnL at $100k. Its
    // whole frozen day accrues at this fill, against the backing the freeze
    // left unchanged. The deposit marks the vault's share rounded up.
    let index = borrowing_index(&borrowing_config(), 1_000 * SCALAR_7, vault_balance, 86_400);
    let pending = (1_000 * SCALAR_7 * index + SCALAR_18 - 1) / SCALAR_18;
    let income = (pending * (SCALAR_18 - TREASURY_RATE) + SCALAR_18 - 1) / SCALAR_18;
    assert!(income > 0);
    assert_eq!(fixture.last_vault_fill_net_pnl(), -income);
    assert_eq!(fixture.market.get_market_data().borrowing_idx.long, index);
}
