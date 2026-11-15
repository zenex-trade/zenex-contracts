//! Vault order creation and cancellation against the full deployed stack.
//!
//! Scope is the price-free create and cancel entry points only: escrow of assets
//! or shares, the stored order row, and the creation rejection matrix. Keeper
//! fills and PnL gates belong to `test_vault_order_fills`.

use market::{MarketError, VaultOrderKind};
use test_suites::constants::{BTC_PRICE, EXEC_FEE};
use test_suites::setup::create_fixture_with_data;
use test_suites::test_fixture::TestFixture;
use test_suites::SCALAR_7;

/// A comfortable deposit that clears every dust floor: 100 tokens (7-dec).
const DEPOSIT_AMOUNT: i128 = 100 * SCALAR_7;

// --- deposit creation ---

/// A deposit order moves the full assets plus the flat exec_fee into the
/// market contract and stores a readable row.
#[test]
fn deposit_creation_escrows_assets_and_records_order() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(200 * SCALAR_7);
    let now = fixture.env.ledger().timestamp();

    let id = fixture.market.create_vault_order(
        &user,
        &(VaultOrderKind::Deposit as u32),
        &DEPOSIT_AMOUNT,
        &0,
    );

    // First order for a fresh user is id 1 (the counter starts at 1).
    assert_eq!(id, 1, "first vault order id");

    // The full amount plus the flat exec_fee leaves the user and rests on the
    // market contract. The vault fill fee is only taken at fill.
    assert_eq!(
        fixture.balance(&user),
        200 * SCALAR_7 - DEPOSIT_AMOUNT - EXEC_FEE,
        "user balance drops by the escrowed amount plus exec_fee",
    );
    assert_eq!(
        fixture.balance(&fixture.market.address),
        DEPOSIT_AMOUNT + EXEC_FEE,
        "market contract holds the escrow plus exec_fee",
    );
    let order = fixture.market.get_vault_order(&user, &id);
    assert_eq!(order.kind, VaultOrderKind::Deposit as u32, "order kind");
    assert_eq!(order.amount, DEPOSIT_AMOUNT, "order amount");
    assert_eq!(order.min_out, 0, "unbounded fill bound stored");
    assert_eq!(order.exec_fee, EXEC_FEE, "exec_fee stamped from config");
    assert_eq!(order.created_at, now, "created_at is the creation ledger");
}

// --- redeem creation ---

/// A redeem order escrows the user's vault shares, plus the flat exec_fee in
/// the settlement token, into the market.
#[test]
fn redeem_creation_escrows_shares_and_exec_fee() {
    let mut fixture = TestFixture::create();
    let user = fixture.new_user(200 * SCALAR_7);

    // Mint shares via a two-step deposit (create, then a keeper fill a ledger
    // later, satisfying the commit-then-execute price-time gate).
    let deposit = fixture.vault_deposit(&fixture.keeper, &user, DEPOSIT_AMOUNT, 0, BTC_PRICE);
    assert!(deposit.filled, "seed deposit fills a ledger after creation");
    let shares = fixture.vault.balance(&user);
    assert!(shares > 0, "deposit fill minted shares to the user");

    let now = fixture.env.ledger().timestamp();
    let user_before = fixture.balance(&user);
    // The deposit consumed id 1, so the redeem takes id 2 for this user.
    let redeem_id =
        fixture
            .market
            .create_vault_order(&user, &(VaultOrderKind::Redeem as u32), &shares, &0);
    assert_eq!(redeem_id, 2, "second vault order id for the user");

    // The shares move out of the user and into the market contract, and the
    // flat exec_fee leaves the user's settlement-token wallet alongside them.
    assert_eq!(fixture.vault.balance(&user), 0, "user shares escrowed");
    assert_eq!(
        fixture.vault.balance(&fixture.market.address),
        shares,
        "market contract holds the escrowed shares",
    );
    assert_eq!(
        user_before - fixture.balance(&user),
        EXEC_FEE,
        "the redeem order's exec_fee left the user's wallet",
    );

    let order = fixture.market.get_vault_order(&user, &redeem_id);
    assert_eq!(order.kind, VaultOrderKind::Redeem as u32, "order kind");
    assert_eq!(order.amount, shares, "order amount is the escrowed shares");
    assert_eq!(order.exec_fee, EXEC_FEE, "exec_fee stamped from config");
    assert_eq!(order.created_at, now, "created_at is the creation ledger");
}

// --- cancellation ---

/// Cancelling a pending deposit refunds the full escrow and removes the row.
#[test]
fn cancel_deposit_refunds_escrow_and_removes_row() {
    let mut fixture = create_fixture_with_data();
    let starting_balance = 200 * SCALAR_7;
    let user = fixture.new_user(starting_balance);

    let id = fixture.market.create_vault_order(
        &user,
        &(VaultOrderKind::Deposit as u32),
        &DEPOSIT_AMOUNT,
        &0,
    );
    let refunded = fixture.market.cancel_vault_order(&user, &id);
    assert_eq!(refunded, DEPOSIT_AMOUNT, "cancel returns the full escrow");
    assert_eq!(
        fixture.balance(&user),
        starting_balance,
        "user balance made whole",
    );
    assert_eq!(
        fixture.balance(&fixture.market.address),
        0,
        "market contract no longer holds the escrow",
    );
    assert!(
        fixture.market.try_get_vault_order(&user, &id).is_err(),
        "row removed",
    );
}

/// Cancelling a pending redeem returns the escrowed shares to the user and
/// removes the row.
#[test]
fn cancel_redeem_returns_shares_and_removes_row() {
    let mut fixture = TestFixture::create();
    let user = fixture.new_user(200 * SCALAR_7);

    let deposit = fixture.vault_deposit(&fixture.keeper, &user, DEPOSIT_AMOUNT, 0, BTC_PRICE);
    assert!(deposit.filled, "seed deposit fills a ledger after creation");
    let shares = fixture.vault.balance(&user);
    assert!(shares > 0, "deposit fill minted shares to the user");

    let redeem_id =
        fixture
            .market
            .create_vault_order(&user, &(VaultOrderKind::Redeem as u32), &shares, &0);
    assert_eq!(fixture.vault.balance(&user), 0, "shares escrowed on create");

    let refunded = fixture.market.cancel_vault_order(&user, &redeem_id);
    assert_eq!(refunded, shares, "cancel returns the escrowed shares");
    assert_eq!(fixture.vault.balance(&user), shares, "user shares restored",);
    assert_eq!(
        fixture.vault.balance(&fixture.market.address),
        0,
        "market contract released the shares",
    );
    assert!(
        fixture
            .market
            .try_get_vault_order(&user, &redeem_id)
            .is_err(),
        "row removed",
    );
}

/// Cancelling an id that was never issued rejects `VaultOrderNotFound`.
#[test]
fn cancel_missing_vault_order_rejects_not_found() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(0);

    assert_eq!(
        fixture.market.try_cancel_vault_order(&user, &999),
        Err(Ok(MarketError::VaultOrderNotFound.into())),
        "cancel of a missing id rejects",
    );
}

// --- creation rejections ---

/// A negative `min_out` slippage bound is rejected before any escrow moves.
#[test]
fn create_rejects_negative_min_out() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(200 * SCALAR_7);

    assert_eq!(
        fixture.market.try_create_vault_order(
            &user,
            &(VaultOrderKind::Deposit as u32),
            &DEPOSIT_AMOUNT,
            &-1,
        ),
        Err(Ok(MarketError::NegativeValueNotAllowed.into())),
        "negative min_out rejected",
    );
}

/// A redeem of zero shares fails the positive-amount guard.
#[test]
fn create_rejects_non_positive_redeem_amount() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(0);

    assert_eq!(
        fixture
            .market
            .try_create_vault_order(&user, &(VaultOrderKind::Redeem as u32), &0, &0),
        Err(Ok(MarketError::InvalidOrder.into())),
        "zero-share redeem rejected",
    );
}

/// A deposit under the gross `min_deposit` floor is rejected at creation.
#[test]
fn create_rejects_deposit_below_min_deposit() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(200 * SCALAR_7);

    // The floor is gross: one unit under the 1_000_000 min_deposit rejects.
    let amount = 999_999;
    assert_eq!(
        fixture.market.try_create_vault_order(
            &user,
            &(VaultOrderKind::Deposit as u32),
            &amount,
            &0
        ),
        Err(Ok(MarketError::InvalidOrder.into())),
        "deposit below min_deposit rejected",
    );
}

// --- id allocation ---

/// Ids increment per user and run on an independent counter for each user.
#[test]
fn vault_order_ids_increment_per_user_and_are_independent() {
    let mut fixture = create_fixture_with_data();
    let user_a = fixture.new_user(500 * SCALAR_7);
    let user_b = fixture.new_user(500 * SCALAR_7);

    let a_first = fixture.market.create_vault_order(
        &user_a,
        &(VaultOrderKind::Deposit as u32),
        &DEPOSIT_AMOUNT,
        &0,
    );
    let a_second = fixture.market.create_vault_order(
        &user_a,
        &(VaultOrderKind::Deposit as u32),
        &DEPOSIT_AMOUNT,
        &0,
    );
    let b_first = fixture.market.create_vault_order(
        &user_b,
        &(VaultOrderKind::Deposit as u32),
        &DEPOSIT_AMOUNT,
        &0,
    );

    assert_eq!(a_first, 1, "user A first id");
    assert_eq!(a_second, 2, "user A second id increments");
    assert_eq!(b_first, 1, "user B counter is independent");
}
