//! An increase consumes capacity only on its own side. Price drift or a cap
//! change on the opposite side must not prevent rebalancing. Redemptions
//! still need sufficient backing for both sides.

use market::{MarketError, OrderKind, VaultOrderKind};
use soroban_sdk::{xdr::ToXdr, Address};
use test_suites::{test_fixture::TestFixture, SCALAR_18, SCALAR_7};

const ENTRY: i128 = 180_000_000_000_000_000;
const RALLY: i128 = 184_210_000_000_000_000;
const LARGE: i128 = 20_000 * SCALAR_7;
const SMALL: i128 = 202_600_000; // $20.26

/// Build a constrained book with recorded testnet economics: $50k LP
/// backing, $20k on one side and $20.26 on the other. A rally lifts the long
/// reserve past its cap. A cap reduction exercises the symmetric short case.
fn constrained_book(over_long: bool) -> (TestFixture<'static>, Address, Address, Address, i128) {
    let mut f = TestFixture::create_testnet();
    let lp = f.new_user(60_000 * SCALAR_7);
    assert!(
        f.vault_deposit(&f.keeper, &lp, 50_000 * SCALAR_7, 0, ENTRY)
            .filled
    );
    let large = f.new_user(20_000 * SCALAR_7);
    let small = f.new_user(5_000 * SCALAR_7);
    f.open(&large, over_long, LARGE, 10_000 * SCALAR_7, &f.price(ENTRY));
    f.open(&small, !over_long, SMALL, 10 * SCALAR_7, &f.price(ENTRY));
    let mark = if over_long {
        RALLY
    } else {
        // Shorts reserve entry notional, so price alone cannot enlarge their
        // reserve. An authorized cap reduction creates the same constraint.
        let mut config = f.market.get_config();
        config.max_util_open = 7 * SCALAR_18 / 10;
        f.market.set_config(&config);
        ENTRY
    };
    assert_side_over_cap(&f, over_long, mark);
    (f, lp, large, small, mark)
}

fn reserve(f: &TestFixture, is_long: bool, ask: i128) -> i128 {
    let data = f.market.get_market_data();
    if is_long {
        (data.tokens.long * ask + SCALAR_18 - 1) / SCALAR_18
    } else {
        data.notional.short
    }
}

fn assert_side_over_cap(f: &TestFixture, is_long: bool, mark: i128) {
    let config = f.market.get_config();
    let cap = (f.vault.total_assets() / 2) * config.max_util_open / SCALAR_18;
    assert!(
        reserve(f, is_long, mark) > cap,
        "the opposite side really exceeds capacity"
    );
    assert!(
        reserve(f, !is_long, mark) < cap,
        "the increasing side has spare capacity"
    );
}

fn submit(f: &TestFixture, user: &Address, is_long: bool, notional: i128) -> u32 {
    f.submit_order(
        user,
        is_long,
        OrderKind::MarketIncrease,
        notional,
        500 * SCALAR_7,
        0,
        0,
        f.far_expiration(),
    )
}

fn opposite_side_can_increase(over_long: bool, existing: bool) {
    let (mut f, _, large, small, mark) = constrained_book(over_long);
    let trader = if existing {
        small
    } else {
        f.new_user(5_000 * SCALAR_7)
    };
    let before = f.position(&trader, !over_long).notional;
    let large_before = f.position(&large, over_long).to_xdr(&f.env);
    let id = submit(&f, &trader, !over_long, 1_000 * SCALAR_7);
    let result = f
        .market
        .try_execute_order(&f.keeper, &trader, &id, &f.price(mark));
    assert!(
        result.is_ok(),
        "an under-cap side must fill despite the opposite reserve: {result:?}"
    );
    assert_eq!(
        f.position(&trader, !over_long).notional,
        before + 1_000 * SCALAR_7
    );
    assert_eq!(f.position(&large, over_long).to_xdr(&f.env), large_before);
    assert_side_over_cap(&f, over_long, mark);

    // Adding size to the over-cap side must still trap, with all order,
    // position, fee and token effects rolled back after the escrow is posted.
    let blocked = submit(&f, &large, over_long, 1_000 * SCALAR_7);
    let price = f.price(mark);
    let ledger_before = f.env.to_ledger_snapshot();
    assert_eq!(
        f.market
            .try_execute_order(&f.keeper, &large, &blocked, &price),
        Err(Ok(MarketError::UtilizationExceeded.into()))
    );
    assert!(
        ledger_before == f.env.to_ledger_snapshot(),
        "a rejected increase must revert its settlement"
    );
    let wallet = f.balance(&large);
    f.market.cancel_order(&large, &blocked);
    assert_eq!(
        f.balance(&large) - wallet,
        500 * SCALAR_7 + f.market.get_config().exec_fee
    );
}

#[test]
fn a_long_rally_does_not_block_a_new_short() {
    opposite_side_can_increase(true, false);
}

#[test]
fn a_long_rally_does_not_block_an_existing_short_increase() {
    opposite_side_can_increase(true, true);
}

#[test]
fn an_over_cap_short_side_does_not_block_a_new_long() {
    opposite_side_can_increase(false, false);
}

#[test]
fn an_over_cap_short_side_does_not_block_an_existing_long_increase() {
    opposite_side_can_increase(false, true);
}

#[test]
fn the_rebalancing_side_still_cannot_exceed_its_own_capacity() {
    for over_long in [true, false] {
        let (f, _, _, small, mark) = constrained_book(over_long);
        let id = f.submit_order(
            &small,
            !over_long,
            OrderKind::MarketIncrease,
            21_000 * SCALAR_7,
            3_000 * SCALAR_7,
            0,
            0,
            f.far_expiration(),
        );
        let price = f.price(mark);
        let before = f.env.to_ledger_snapshot();
        assert_eq!(
            f.market.try_execute_order(&f.keeper, &small, &id, &price),
            Err(Ok(MarketError::UtilizationExceeded.into()))
        );
        assert!(before == f.env.to_ledger_snapshot());
    }
}

#[test]
fn redemptions_still_check_both_sides_and_roll_back() {
    for over_long in [true, false] {
        let (f, lp, _, _, mark) = constrained_book(over_long);
        let shares = f.share_balance(&lp) / 2;
        let id = f
            .market
            .create_vault_order(&lp, &(VaultOrderKind::Redeem as u32), &shares, &0);
        f.jump(f.market.get_config().redeem_lock + 5);
        let price = f.price(mark);
        let ledger_before = f.env.to_ledger_snapshot();
        assert_eq!(
            f.market
                .try_execute_vault_order(&f.keeper, &lp, &id, &price),
            Err(Ok(MarketError::UtilizationExceeded.into()))
        );
        let after = f.env.to_ledger_snapshot();
        // Host TTL extensions can survive a trapped call. Every ledger key
        // and stored value must still match, including escrow and balances.
        assert!(
            ledger_before
                .ledger_entries
                .iter()
                .map(|(key, (value, _))| (key, value))
                .eq(after
                    .ledger_entries
                    .iter()
                    .map(|(key, (value, _))| (key, value))),
            "a rejected redeem must preserve all ledger values"
        );
    }
}

#[test]
fn over_cap_positions_can_add_margin_and_reduce_size() {
    for over_long in [true, false] {
        let (f, _, large, _, mark) = constrained_book(over_long);
        let margin_before = f.position(&large, over_long).margin;
        let id = submit(&f, &large, over_long, 0);
        f.execute_order(&f.keeper, &large, id, &f.price(mark));
        assert_eq!(f.position(&large, over_long).notional, LARGE);
        assert_eq!(
            f.position(&large, over_long).margin,
            margin_before + 500 * SCALAR_7
        );
        f.jump(f.market.get_config().notional_lock + 1);
        f.close(&large, over_long, 1_000 * SCALAR_7, 0, &f.price(mark));
        assert_eq!(
            f.position(&large, over_long).notional,
            LARGE - 1_000 * SCALAR_7
        );
    }
}
