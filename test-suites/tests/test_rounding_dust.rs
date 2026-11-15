//! Dust rounding: every rounding step in LP share pricing and borrowing
//! settlement goes against whoever transacts, so no depositor, redeemer or
//! trader can collect dust. Contracts run as optimized WASM.
//!
//! - A deposit followed at once by a redeem never returns more than it put
//!   in, whatever the book marks.
//! - A donation into a near-empty vault costs the donor about `10^6` times
//!   what the next depositor loses, the virtual-share offset's guarantee.
//! - The vault collects at least the pending borrowing a share price marks.
//!   Over `n` positions it collects at most `2n - 2` units more than the
//!   deposit mark and `2n` more than the redeem mark.
//! - A trader cannot shave borrowing by settling often: each settlement
//!   rounds up.

use market::engine::constants::MAX_BORROW_RATE;
use market::{Config, VaultOrderKind};
use soroban_sdk::{contractclient, token::TokenClient, Address, Env};
use test_suites::constants::{BTC_PRICE, EXEC_FEE, MIN_ORDER_MARGIN, PRICE_SCALAR};
use test_suites::default_config;
use test_suites::test_fixture::TestFixture;
use test_suites::SCALAR_7;

#[contractclient(name = "SupplyClient")]
pub trait Supply {
    fn total_supply(e: Env) -> i128;
}

/// No vault fees, no vault cap and no funding, so each round trip measures
/// rounding alone. The recorded borrowing curve stays on.
fn dust_config() -> Config {
    Config {
        deposit_fee: 0,
        redeem_fee: 0,
        max_vault_balance: i128::MAX,
        funding_increase: 0,
        funding_decrease: 0,
        threshold_stable_funding: 0,
        funding_min: 0,
        funding_max: 0,
        ..default_config()
    }
}

/// The same config with the borrowing curve off. Set after an accrual in the
/// same ledger, it freezes the pending borrowing for later fills.
fn frozen(config: &Config) -> Config {
    Config {
        borrow_rate: 0,
        increased_borrow_rate: 0,
        ..config.clone()
    }
}

/// Fills `user`'s vault order of `kind` for `amount` a ledger after its
/// creation, against a report with the given mid, bid and ask.
fn fill_vault_order(
    f: &TestFixture,
    user: &Address,
    kind: VaultOrderKind,
    amount: i128,
    (mid, bid, ask): (i128, i128, i128),
) {
    let id = f
        .market
        .create_vault_order(user, &(kind as u32), &amount, &0);
    f.jump(5);
    let report = f.price_bid_ask(mid, bid, ask);
    f.market.execute_vault_order(&f.keeper, user, &id, &report);
}

/// Deposits `amount` for `lp`, redeems every share the deposit minted, and
/// returns the assets the redeem paid (token-dec). Both fills price at
/// `quote`.
fn round_trip(f: &TestFixture, lp: &Address, amount: i128, quote: (i128, i128, i128)) -> i128 {
    let shares = f.share_balance(lp);
    fill_vault_order(f, lp, VaultOrderKind::Deposit, amount, quote);
    let minted = f.share_balance(lp) - shares;
    // The redeem escrows its exec_fee from the wallet at creation.
    let wallet = f.balance(lp) - EXEC_FEE;
    fill_vault_order(f, lp, VaultOrderKind::Redeem, minted, quote);
    f.balance(lp) - wallet
}

fn flat(price: i128) -> (i128, i128, i128) {
    (price, price, price)
}

/// A round trip never pays: the deposit marks the book adverse to the
/// depositor and the redeem adverse to the redeemer. Checked at par, then
/// against a $10,000 long carrying pending borrowing: flat, at a trader
/// loss, at a trader profit, and across a bid-ask spread. At par both
/// conversions are exact, so a round trip returns what it put in.
#[test]
fn vault_round_trips_never_return_more_than_they_put_in() {
    let config = dust_config();
    let mut f = TestFixture::create_with(config.clone());
    let seed = f.new_user(100_000 * SCALAR_7 + EXEC_FEE);
    fill_vault_order(
        &f,
        &seed,
        VaultOrderKind::Deposit,
        100_000 * SCALAR_7,
        flat(BTC_PRICE),
    );
    let lp = f.new_user(1_000_000 * SCALAR_7);
    let sizes = [1_000_000, 1_234_567, 98_765_432_101];
    let (mut paid_in, mut paid_out) = (0, 0);

    for amount in sizes {
        let out = round_trip(&f, &lp, amount, flat(BTC_PRICE));
        assert_eq!(out, amount, "par: {amount} in, {out} out");
        paid_in += amount;
        paid_out += out;
    }

    // A long accrues an hour of borrowing, then the curve turns off so the
    // pending borrowing holds still through the round trips.
    let trader = f.new_user(2_000 * SCALAR_7);
    f.open(
        &trader,
        true,
        10_000 * SCALAR_7,
        2_000 * SCALAR_7 - EXEC_FEE,
        &f.price(BTC_PRICE),
    );
    f.jump(3_600);
    f.market.accrue(&f.price(BTC_PRICE));
    f.market.set_config(&frozen(&config));
    assert!(f.market.get_market_data().borrowing_idx.long > 0);

    let quotes = [
        flat(BTC_PRICE),
        flat(95_000 * PRICE_SCALAR),
        flat(105_000 * PRICE_SCALAR),
        (BTC_PRICE, 99_990 * PRICE_SCALAR, 100_010 * PRICE_SCALAR),
    ];
    for quote in quotes {
        for amount in sizes {
            let out = round_trip(&f, &lp, amount, quote);
            assert!(out <= amount, "{quote:?}: {amount} in, {out} out");
            paid_in += amount;
            paid_out += out;
        }
    }
    assert!(paid_out <= paid_in);
    f.assert_borrowing_books([&trader]);
}

/// The classic donation attack against the fixed `10^6` virtual shares, with
/// no vault fees. The attacker makes the minimum deposit into the empty
/// vault, then donates 1,000,000 tokens to inflate the share price. A
/// 1,000-token victim deposit still mints 1,000,000,899 shares. The victim
/// redeems 10 units short. The attacker redeems 9_999_981 units short,
/// about 10^6 times the victim's loss. The virtual shares keep both losses.
#[test]
fn a_donation_attack_costs_the_attacker_a_million_times_the_victims_loss() {
    let mut f = TestFixture::create_with(dust_config());
    let donation = 1_000_000 * SCALAR_7;
    let attacker = f.new_user(donation + 1_000_000 + 2 * EXEC_FEE);
    let victim = f.new_user(1_000 * SCALAR_7 + 2 * EXEC_FEE);

    fill_vault_order(
        &f,
        &attacker,
        VaultOrderKind::Deposit,
        1_000_000,
        flat(BTC_PRICE),
    );
    assert_eq!(f.share_balance(&attacker), 1_000_000_000_000);
    TokenClient::new(&f.env, &f.token_address).transfer(&attacker, &f.vault.address, &donation);

    fill_vault_order(
        &f,
        &victim,
        VaultOrderKind::Deposit,
        1_000 * SCALAR_7,
        flat(BTC_PRICE),
    );
    assert_eq!(f.share_balance(&victim), 1_000_000_899);

    let wallet = f.balance(&victim) - EXEC_FEE;
    fill_vault_order(
        &f,
        &victim,
        VaultOrderKind::Redeem,
        1_000_000_899,
        flat(BTC_PRICE),
    );
    assert_eq!(f.balance(&victim) - wallet, 1_000 * SCALAR_7 - 10);

    let wallet = f.balance(&attacker) - EXEC_FEE;
    fill_vault_order(
        &f,
        &attacker,
        VaultOrderKind::Redeem,
        1_000_000_000_000,
        flat(BTC_PRICE),
    );
    assert_eq!(
        f.balance(&attacker) - wallet,
        donation + 1_000_000 - 9_999_981
    );
    assert_eq!(f.vault.total_assets(), 9_999_981 + 10);
    assert_eq!(
        SupplyClient::new(&f.env, &f.vault.address).total_supply(),
        0
    );
}

/// No trade fees, a one-unit impact fee, no vault fees and no funding, so
/// the vault's balance moves only by borrowing and that unit. Prices sit at
/// $1, where base size equals notional and every side marks zero PnL.
fn borrowing_gap_config() -> Config {
    Config {
        fee_dom: 0,
        fee_non_dom: 0,
        impact_scalar: i128::MAX,
        borrow_rate: MAX_BORROW_RATE,
        increased_borrow_rate: MAX_BORROW_RATE,
        ..dust_config()
    }
}

/// Five longs with odd notionals open at different indices and accrue an
/// hour. With the curve then off, a deposit and a redeem mark the pending
/// borrowing, and all five close at the same index. The vault collects at
/// least both marks, at most `2n - 2 = 8` units more than the deposit mark
/// and `2n = 10` more than the redeem mark.
#[test]
fn borrowing_income_lands_within_the_rounding_bound_of_both_marks() {
    let config = borrowing_gap_config();
    let mut f = TestFixture::create_with(config.clone());
    let one = PRICE_SCALAR;
    let lp = f.new_user(1_000_000 * SCALAR_7 + 2 * EXEC_FEE);
    fill_vault_order(
        &f,
        &lp,
        VaultOrderKind::Deposit,
        1_000_000 * SCALAR_7,
        flat(one),
    );

    let notionals = [
        1_234_567_891,
        2_345_678_901,
        3_456_789_013,
        987_654_321,
        4_567_890_123,
    ];
    let mut traders = std::vec::Vec::new();
    for (i, notional) in notionals.into_iter().enumerate() {
        let trader = f.new_user(notional / 5 + EXEC_FEE);
        f.open(&trader, true, notional, notional / 5, &f.price(one));
        traders.push(trader);
        f.jump(17 + 13 * i as u64);
    }
    f.jump(3_600);
    f.market.accrue(&f.price(one));
    f.market.set_config(&frozen(&config));
    f.assert_borrowing_books(&traders);

    let depositor = f.new_user(100 * SCALAR_7 + EXEC_FEE);
    fill_vault_order(
        &f,
        &depositor,
        VaultOrderKind::Deposit,
        100 * SCALAR_7,
        flat(one),
    );
    let deposit_mark = -f.last_vault_fill_net_pnl();
    fill_vault_order(
        &f,
        &lp,
        VaultOrderKind::Redeem,
        f.share_balance(&lp) / 1_000,
        flat(one),
    );
    let redeem_mark = -f.last_vault_fill_net_pnl();

    let before = f.vault.total_assets();
    for (trader, notional) in traders.iter().zip(notionals) {
        f.close(trader, true, notional, 0, &f.price(one));
    }
    f.assert_borrowing_books(&traders);
    // Each close also donates its one-unit impact fee.
    let collected = f.vault.total_assets() - before - 5;

    assert!(redeem_mark <= deposit_mark && deposit_mark <= collected);
    assert!(
        collected - deposit_mark <= 8,
        "deposit gap {}",
        collected - deposit_mark
    );
    assert!(
        collected - redeem_mark <= 10,
        "redeem gap {}",
        collected - redeem_mark
    );
}

/// Settling often never pays less borrowing. Two equal longs open together.
/// One adds the minimum margin every ledger for ten ledgers, settling its
/// borrowing each time. The other only closes. Each settlement rounds up,
/// so the frequent settler pays at least as much, and at most one unit more
/// per extra settlement.
#[test]
fn settling_every_ledger_never_pays_less_borrowing_than_settling_once() {
    let mut f = TestFixture::create_with(borrowing_gap_config());
    let one = PRICE_SCALAR;
    let lp = f.new_user(1_000_000 * SCALAR_7 + EXEC_FEE);
    fill_vault_order(
        &f,
        &lp,
        VaultOrderKind::Deposit,
        1_000_000 * SCALAR_7,
        flat(one),
    );

    let notional = 3_456_789_013;
    let margin = notional / 5;
    let frequent = f.new_user(2 * margin + 10 * (MIN_ORDER_MARGIN + EXEC_FEE));
    let once = f.new_user(margin + EXEC_FEE);
    f.open(&frequent, true, notional, margin, &f.price(one));
    f.open(&once, true, notional, margin, &f.price(one));

    let mut frequent_paid = 0;
    for _ in 0..10 {
        f.jump(5);
        let before = f.position(&frequent, true).margin;
        f.open(&frequent, true, 0, MIN_ORDER_MARGIN, &f.price(one));
        frequent_paid += MIN_ORDER_MARGIN - (f.position(&frequent, true).margin - before);
    }
    // Each margin add moved only the frequent settler's checkpoint.
    f.assert_borrowing_books([&frequent, &once]);
    assert!(f.position(&frequent, true).borrowing_idx > f.position(&once, true).borrowing_idx);

    // Both close in the same ledger. A close returns the margin less the
    // one-unit impact fee and the borrowing, at zero PnL.
    let close_borrowing = |trader: &Address| {
        let margin = f.position(trader, true).margin;
        let wallet = f.balance(trader);
        f.close(trader, true, notional, 0, &f.price(one));
        margin - 1 - (f.balance(trader) - wallet)
    };
    frequent_paid += close_borrowing(&frequent);
    let once_paid = close_borrowing(&once);

    assert!(once_paid > 0);
    assert!(frequent_paid >= once_paid, "{frequent_paid} < {once_paid}");
    assert!(frequent_paid - once_paid <= 10);
    f.assert_borrowing_books([&frequent, &once]);
}
