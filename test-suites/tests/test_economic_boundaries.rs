//! Deterministic economic boundaries with the recorded testnet configuration.
//! Only the position, open-interest and vault caps are raised 1000x. Reports
//! are synthetic, at 18 decimals, and the market/vault execute as optimized WASM.

use market::{Config, MarketError, OrderKind, VaultOrderKind};
use soroban_sdk::{token::TokenClient, xdr::ToXdr, Address};
use test_suites::carry::{borrowing_index, funding_index};
use test_suites::test_fixture::TestFixture;
use test_suites::testnet_profile::TestnetProfile;
use test_suites::{SCALAR_18 as S, SCALAR_7 as U};

const ENTRY: i128 = S / 4;

fn ceil(n: i128, d: i128) -> i128 {
    (n + d - 1) / d
}

fn fee(c: &Config, size: i128, worsening: bool) -> i128 {
    ceil(size * if worsening { c.fee_dom } else { c.fee_non_dom }, S)
        + ceil(size * size, c.impact_scalar).min(ceil(size, 10))
}

fn seed(f: &mut TestFixture, amount: i128) -> Address {
    let lp = f.new_user(amount + U);
    let result = f.vault_deposit(&f.keeper, &lp, amount, 0, ENTRY);
    assert!(result.filled, "LP deposit error {}", result.error);
    lp
}

fn unchanged(f: &TestFixture, call: impl FnOnce()) {
    let before = f.env.to_ledger_snapshot();
    call();
    let after = f.env.to_ledger_snapshot();
    assert!(before
        .ledger_entries
        .iter()
        .map(|(k, (v, _))| (k, v))
        .eq(after.ledger_entries.iter().map(|(k, (v, _))| (k, v))));
}

fn oi_boundary(is_long: bool) {
    let mut f = TestFixture::create_testnet();
    let c = TestnetProfile::load().test_config();
    seed(&mut f, 90_000_000 * U);
    // Normal capped deposits provide at most $40M of opening capacity per
    // side, below the $50M OI cap. A real token donation makes that independent
    // OI guard reachable without changing any configuration field.
    let donor = f.new_user(50_000_000 * U);
    TokenClient::new(&f.env, &f.token_address).transfer(
        &donor,
        &f.vault.address,
        &(50_000_000 * U),
    );
    assert!(f.vault.total_assets() > c.max_vault_balance);
    let first = f.new_user(10_000_000 * U);
    let second = f.new_user(10_000_000 * U);
    let third = f.new_user(100 * U);
    f.open(
        &first,
        is_long,
        c.max_position_notional,
        7_000_000 * U,
        &f.price(ENTRY),
    );
    f.open(
        &second,
        is_long,
        c.max_position_notional - U,
        7_000_000 * U,
        &f.price(ENTRY),
    );
    assert_eq!(
        f.market.get_market_data().notional.get(is_long),
        c.max_open_interest - U
    );
    f.open(&third, is_long, U, 10 * U, &f.price(ENTRY));
    assert_eq!(
        f.market.get_market_data().notional.get(is_long),
        c.max_open_interest
    );
    let id = f.submit_order(
        &third,
        is_long,
        OrderKind::MarketIncrease,
        U,
        U,
        0,
        0,
        f.far_expiration(),
    );
    let escrowed = f.balance(&third);
    unchanged(&f, || {
        assert_eq!(
            f.market
                .try_execute_order(&f.keeper, &third, &id, &f.price(ENTRY)),
            Err(Ok(MarketError::OpenInterestExceeded.into()))
        )
    });
    assert_eq!(f.market.cancel_order(&third, &id), U + c.exec_fee);
    assert_eq!(f.balance(&third), escrowed + U + c.exec_fee);
    // An opposite-side position and margin top-ups remain valid at the cap.
    f.open(&third, !is_long, U, 10 * U, &f.price(ENTRY));
    f.open(&third, is_long, 0, U, &f.price(ENTRY));
    assert_eq!(
        f.market.get_market_data().notional.get(is_long),
        c.max_open_interest
    );
    assert_eq!(f.market.get_config().to_xdr(&f.env), c.to_xdr(&f.env));
}

#[test]
fn long_open_interest_accepts_exact_cap_and_rejects_growth_with_refundable_escrow() {
    oi_boundary(true);
}

#[test]
fn short_open_interest_accepts_exact_cap_and_rejects_growth_with_refundable_escrow() {
    oi_boundary(false);
}

fn liquidation_boundary(is_long: bool) {
    for atom in [-1_i128, 0, 1] {
        let mut f = TestFixture::create_testnet();
        let c = TestnetProfile::load().test_config();
        seed(&mut f, 50_000 * U);
        let user = f.new_user(1_000 * U);
        let size = 1_000 * U;
        let margin = 200 * U + atom;
        let opened_fee = fee(&c, size, true);
        let closing_fee = fee(&c, size, false);
        let required = ceil(size * c.maintenance_margin, S);
        let loss = 200 * U - opened_fee - closing_fee - required;
        // ENTRY buys exactly 4 * size tokens on either side. Choosing a
        // quarter-dollar entry makes this loss representable without rounding.
        let movement = loss * S / (4 * size);
        assert_eq!(4 * size * movement / S, loss);
        let mark = if is_long {
            ENTRY - movement
        } else {
            ENTRY + movement
        };
        f.open(&user, is_long, size, margin, &f.price(ENTRY));
        assert_eq!(f.position(&user, is_long).tokens, 4 * size);
        assert_eq!(f.position(&user, is_long).margin, margin - opened_fee);
        // No elapsed time: configured carry remains enabled but accrues zero.
        assert_eq!(margin - opened_fee - closing_fee - loss, required + atom);
        let report = f.price(mark);
        if atom >= 0 {
            unchanged(&f, || {
                assert_eq!(
                    f.market
                        .try_execute_liquidation(&f.keeper, &user, &is_long, &report),
                    Err(Ok(MarketError::NotLiquidatable.into()))
                )
            });
        } else {
            let liq_fee = ceil(size * c.liq_fee, S);
            let keeper_cut = (closing_fee + liq_fee) * c.keeper_rate / S;
            let treasury_cut = (closing_fee + liq_fee) * TestnetProfile::load().treasury.rate / S;
            let before = [
                f.balance(&user),
                f.balance(&f.keeper),
                f.balance(&f.treasury.address),
                f.vault.total_assets(),
                f.balance(&f.market.address),
            ];
            let reward = f
                .market
                .execute_liquidation(&f.keeper, &user, &is_long, &report);
            assert_eq!(reward, keeper_cut);
            assert_eq!(f.balance(&user) - before[0], required - 1 - liq_fee);
            assert_eq!(f.balance(&f.keeper) - before[1], keeper_cut);
            assert_eq!(f.balance(&f.treasury.address) - before[2], treasury_cut);
            assert_eq!(
                f.vault.total_assets() - before[3],
                loss + closing_fee + liq_fee - keeper_cut - treasury_cut
            );
            assert_eq!(
                f.balance(&f.market.address) - before[4],
                -(margin - opened_fee)
            );
            let position = f.position(&user, is_long);
            assert_eq!(
                (position.notional, position.tokens, position.margin),
                (0, 0, 0)
            );
        }
    }
}

#[test]
fn long_liquidation_is_strictly_below_health_equality_at_one_token_atom() {
    liquidation_boundary(true);
}

#[test]
fn short_liquidation_is_strictly_below_health_equality_at_one_token_atom() {
    liquidation_boundary(false);
}

#[test]
fn borrowing_switches_charged_side_after_token_dominance_reverses() {
    let mut f = TestFixture::create_testnet();
    let c = TestnetProfile::load().test_config();
    seed(&mut f, 50_000 * U);
    let long = f.new_user(2_000 * U);
    let short = f.new_user(2_000 * U);
    f.open(&long, true, 2_000 * U, 500 * U, &f.price(ENTRY));
    f.open(&short, false, 1_000 * U, 500 * U, &f.price(ENTRY));
    let old_backing = f.vault.total_assets();
    let long_idx = borrowing_index(&c, 2_000 * U, old_backing, 600);
    f.jump(600);
    f.market.accrue(&f.price(ENTRY));
    let first = f.market.get_market_data();
    assert_eq!(
        (first.borrowing_idx.long, first.borrowing_idx.short),
        (long_idx, 0)
    );
    assert!(long_idx > 0);
    f.open(&short, false, 2_000 * U, 500 * U, &f.price(ENTRY));
    assert!(f.market.get_market_data().tokens.short > f.market.get_market_data().tokens.long);
    let short_idx = borrowing_index(&c, 3_000 * U, f.vault.total_assets(), 600);
    f.jump(600);
    f.market.accrue(&f.price(ENTRY));
    let second = f.market.get_market_data();
    assert_eq!(
        (second.borrowing_idx.long, second.borrowing_idx.short),
        (long_idx, short_idx)
    );
    assert!(short_idx > 0);
    // Settle both windows without another size change. Borrowing pays the
    // treasury and vault, never a keeper. Funding stays in the market pool.
    for (user, side, size, idx) in [
        (&long, true, 2_000 * U, long_idx),
        (&short, false, 3_000 * U, short_idx),
    ] {
        let expected = ceil(size * idx, S);
        let cut = expected * TestnetProfile::load().treasury.rate / S;
        let treasury_before = f.balance(&f.treasury.address);
        let vault_before = f.vault.total_assets();
        f.open(user, side, 0, U, &f.price(ENTRY));
        assert_eq!(f.balance(&f.treasury.address) - treasury_before, cut);
        assert_eq!(f.vault.total_assets() - vault_before, expected - cut);
        assert_eq!(f.position(user, side).borrowing_idx, idx);
    }
    assert_eq!(f.market.get_config().to_xdr(&f.env), c.to_xdr(&f.env));
}

#[test]
fn deposit_accrues_existing_exposure_before_increasing_backing() {
    let mut f = TestFixture::create_testnet();
    let c = TestnetProfile::load().test_config();
    let lp = seed(&mut f, 50_000 * U);
    let long = f.new_user(10_000 * U);
    f.open(&long, true, 15_000 * U, 5_000 * U, &f.price(ENTRY));
    let depositor = f.new_user(50_001 * U);
    let amount = 50_000 * U;
    let id =
        f.market
            .create_vault_order(&depositor, &(VaultOrderKind::Deposit as u32), &amount, &0);
    let backing = f.vault.total_assets();
    let supply = f.share_balance(&lp);
    let borrowing = borrowing_index(&c, 15_000 * U, backing, 600);
    let wrong_post_deposit = borrowing_index(&c, 15_000 * U, backing + amount, 600);
    assert!(
        borrowing > wrong_post_deposit,
        "scenario distinguishes deposit ordering"
    );
    f.jump(600);
    let fee = amount * c.deposit_fee / S;
    let keeper_cut = fee * c.keeper_rate / S;
    let treasury_cut = fee * TestnetProfile::load().treasury.rate / S;
    // The share price counts the vault's share of the long's unpaid borrowing.
    let owed = ceil(15_000 * U * borrowing, S);
    let income = ceil(owed * (S - TestnetProfile::load().treasury.rate), S);
    let minted = (amount - fee) * (supply + 1_000_000) / (backing + income + 1);
    let keeper_before = f.balance(&f.keeper);
    let treasury_before = f.balance(&f.treasury.address);
    let reward = f
        .market
        .execute_vault_order(&f.keeper, &depositor, &id, &f.price(ENTRY));
    assert_eq!(reward, keeper_cut + c.exec_fee);
    assert_eq!(f.balance(&f.keeper) - keeper_before, reward);
    assert_eq!(
        f.balance(&f.treasury.address) - treasury_before,
        treasury_cut
    );
    assert_eq!(f.share_balance(&depositor), minted);
    assert_eq!(
        f.vault.total_assets(),
        backing + amount - keeper_cut - treasury_cut
    );
    let data = f.market.get_market_data();
    assert_eq!(
        (data.borrowing_idx.long, data.borrowing_idx.short),
        (borrowing, 0)
    );
    assert_eq!(
        (data.funding_idx.long, data.funding_idx.short),
        (funding_index(&c, 600), 0)
    );
    assert_eq!(data.accrued_at, f.env.ledger().timestamp());
    // The next window actually uses the increased backing.
    let next = borrowing_index(&c, 15_000 * U, f.vault.total_assets(), 600);
    f.jump(600);
    f.market.accrue(&f.price(ENTRY));
    assert_eq!(
        f.market.get_market_data().borrowing_idx.long,
        borrowing + next
    );
}

#[test]
fn unavailable_vault_recipient_rolls_back_fills_and_recovers_without_double_payment() {
    let mut f = TestFixture::create_testnet();
    let c = TestnetProfile::load().test_config();
    seed(&mut f, 50_000 * U);
    let user = f.new_user(1_000 * U);
    let id = f.submit_order(
        &user,
        true,
        OrderKind::MarketIncrease,
        1_000 * U,
        200 * U,
        0,
        0,
        f.far_expiration(),
    );
    f.token.set_authorized(&f.vault.address, &false);
    unchanged(&f, || {
        assert!(f
            .market
            .try_execute_order(&f.keeper, &user, &id, &f.price(ENTRY))
            .is_err())
    });
    f.token.set_authorized(&f.vault.address, &true);
    let before = f.vault.total_assets();
    f.execute_order(&f.keeper, &user, id, &f.price(ENTRY));
    let trade_fee = fee(&c, 1_000 * U, true);
    assert_eq!(
        f.vault.total_assets() - before,
        trade_fee
            - trade_fee * c.keeper_rate / S
            - trade_fee * TestnetProfile::load().treasury.rate / S
    );
    assert!(f.market.try_get_order(&user, &id).is_err());
    let id = f
        .market
        .create_vault_order(&user, &(VaultOrderKind::Deposit as u32), &(100 * U), &0);
    f.jump(1); // Keeper fills must postdate a resting vault order.
    f.token.set_authorized(&f.vault.address, &false);
    unchanged(&f, || {
        assert!(f
            .market
            .try_execute_vault_order(&f.keeper, &user, &id, &f.price(ENTRY))
            .is_err())
    });
    f.token.set_authorized(&f.vault.address, &true);
    f.market
        .execute_vault_order(&f.keeper, &user, &id, &f.price(ENTRY));
    assert!(f.market.try_get_vault_order(&user, &id).is_err());
    assert!(f.share_balance(&user) > 0);
}

#[test]
fn trade_and_redeem_cancellation_restore_all_escrows_after_recipient_reauthorization() {
    let mut f = TestFixture::create_testnet();
    let c = TestnetProfile::load().test_config();
    let lp = seed(&mut f, 50_000 * U);
    let user = f.new_user(1_000 * U);
    f.open(&user, true, 1_000 * U, 200 * U, &f.price(ENTRY));
    for (kind, margin) in [
        (OrderKind::LimitIncrease, 100 * U),
        (OrderKind::LimitDecrease, 0),
    ] {
        let id = f.submit_order(
            &user,
            true,
            kind,
            100 * U,
            margin,
            ENTRY,
            0,
            f.far_expiration(),
        );
        let before = f.balance(&user);
        f.token.set_authorized(&user, &false);
        unchanged(&f, || {
            assert!(f.market.try_cancel_order(&user, &id).is_err())
        });
        f.token.set_authorized(&user, &true);
        assert_eq!(f.market.cancel_order(&user, &id), margin + c.exec_fee);
        assert_eq!(f.balance(&user) - before, margin + c.exec_fee);
        unchanged(&f, || {
            assert!(f.market.try_cancel_order(&user, &id).is_err())
        });
    }
    let shares = f.share_balance(&lp) / 2;
    let before_shares = f.share_balance(&lp);
    let id = f
        .market
        .create_vault_order(&lp, &(VaultOrderKind::Redeem as u32), &shares, &0);
    let before_cash = f.balance(&lp);
    f.token.set_authorized(&lp, &false);
    unchanged(&f, || {
        assert!(f.market.try_cancel_vault_order(&lp, &id).is_err())
    });
    assert_eq!(f.share_balance(&lp), before_shares - shares);
    assert_eq!(f.share_balance(&f.market.address), shares);
    f.token.set_authorized(&lp, &true);
    assert_eq!(f.market.cancel_vault_order(&lp, &id), shares);
    assert_eq!(f.share_balance(&lp), before_shares);
    assert_eq!(f.share_balance(&f.market.address), 0);
    assert_eq!(f.balance(&lp), before_cash + c.exec_fee);
}

#[test]
fn active_redemption_parks_unavailable_recipient_payout_and_claim_recovers_once() {
    let mut f = TestFixture::create_testnet();
    let c = TestnetProfile::load().test_config();
    let lp = seed(&mut f, 50_000 * U);
    let supply = f.share_balance(&lp);
    let shares = supply / 4;
    let backing = f.vault.total_assets();
    let gross = shares * (backing + 1) / (supply + 1_000_000);
    let fee = gross * c.redeem_fee / S;
    let expected = gross - fee;
    let keeper_cut = fee * c.keeper_rate / S;
    let treasury_cut = fee * TestnetProfile::load().treasury.rate / S;
    let id = f
        .market
        .create_vault_order(&lp, &(VaultOrderKind::Redeem as u32), &shares, &expected);
    f.jump(c.redeem_lock);
    f.token.set_authorized(&lp, &false);
    let wallet = f.balance(&lp);
    let keeper = f.balance(&f.keeper);
    let treasury = f.balance(&f.treasury.address);
    assert_eq!(
        f.market
            .execute_vault_order(&f.keeper, &lp, &id, &f.price(ENTRY)),
        keeper_cut + c.exec_fee
    );
    assert_eq!(f.balance(&lp), wallet);
    assert_eq!(f.balance(&f.keeper) - keeper, keeper_cut + c.exec_fee);
    assert_eq!(f.balance(&f.treasury.address) - treasury, treasury_cut);
    assert_eq!(
        f.vault.total_assets(),
        backing - gross + fee - keeper_cut - treasury_cut
    );
    assert_eq!(f.share_balance(&lp), supply - shares);
    assert_eq!(f.share_balance(&f.market.address), 0);
    assert_eq!(f.market.get_claimable_credit(&lp), expected);
    assert_eq!(f.market.get_market_data().credit_pool, expected);
    assert_eq!(f.market.get_market_data().credit_owed, expected);
    assert_eq!(f.balance(&f.market.address), expected);
    assert!(f.market.try_get_vault_order(&lp, &id).is_err());
    unchanged(&f, || assert!(f.market.try_claim_credit(&lp).is_err()));
    f.token.set_authorized(&lp, &true);
    assert_eq!(f.market.claim_credit(&lp), expected);
    assert_eq!(f.balance(&lp), wallet + expected);
    unchanged(&f, || {
        assert_eq!(
            f.market.try_claim_credit(&lp),
            Err(Ok(MarketError::NothingToClaim.into()))
        )
    });
    assert_eq!(f.balance(&f.market.address), 0);
    assert_eq!(f.market.get_market_data().credit_pool, 0);
    assert_eq!(f.market.get_market_data().credit_owed, 0);
}
