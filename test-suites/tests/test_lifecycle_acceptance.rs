//! Combined lifecycle acceptance at the recorded testnet economics of
//! `fixtures/testnet-2026-09-05.json`. Only the position, open-interest and
//! vault caps differ from that deployment, multiplied for test capacity.
//! The default stack executes WASM with synthetic 18-decimal XLM reports
//! signed by the shared helper and checked by the real verifier.

use governance::{GovernanceClient, GovernanceError};
use market::{Config, MarketError, OrderKind, Status, VaultOrderKind};
use soroban_sdk::testutils::{Events, MockAuth, MockAuthInvoke};
use soroban_sdk::{
    contractclient, Address, Bytes, Env, IntoVal, Map, Symbol, TryFromVal, Val, Vec,
};
use test_suites::carry::{borrowing_index, funding_index};
use test_suites::dependencies::governance::GOVERNANCE_WASM;
use test_suites::test_fixture::TestFixture;
use test_suites::testnet_profile::TestnetProfile;
use test_suites::{SCALAR_18 as S, SCALAR_7 as U};

const ENTRY: i128 = S / 5;
const LP_DEPOSIT: i128 = 100_000 * U;
const PAYER_SIZE: i128 = 2_001 * U;
const RECEIVER_SIZE: i128 = 1_000 * U;
const DAY: u64 = 86_400;
const DEADLINE: u64 = 7 * DAY;
const DELAY: u64 = 3_600;

fn ceil(n: i128, d: i128) -> i128 {
    assert!(n >= 0 && d > 0);
    (n + d - 1) / d
}
fn cut(n: i128, rate: i128) -> i128 {
    n * rate / S
}
fn fee(c: &Config, size: i128, worsening: bool) -> i128 {
    ceil(size * if worsening { c.fee_dom } else { c.fee_non_dom }, S)
        + ceil(size * size, c.impact_scalar).min(ceil(size, 10))
}

/// Token movements predicted from inputs, never initialized from settlement
/// output. Every actor is checked after every economic step. The market is
/// tracked as a real token balance, not the contract's `margin + credit_pool`
/// bookkeeping.
struct Cash {
    balances: std::vec::Vec<(Address, i128)>,
    minted: i128,
}
impl Cash {
    fn new(f: &TestFixture) -> Self {
        Self {
            balances: [
                f.keeper.clone(),
                f.treasury.address.clone(),
                f.vault.address.clone(),
                f.market.address.clone(),
                f.owner.clone(),
            ]
            .into_iter()
            .map(|a| (a, 0))
            .collect(),
            minted: 0,
        }
    }
    fn user(&mut self, f: &mut TestFixture, amount: i128) -> Address {
        let a = f.new_user(amount);
        self.balances.push((a.clone(), amount));
        self.minted += amount;
        a
    }
    fn balance(&self, a: &Address) -> i128 {
        self.balances.iter().find(|(b, _)| a == b).unwrap().1
    }
    fn change(&mut self, a: &Address, delta: i128) {
        self.balances.iter_mut().find(|(b, _)| a == b).unwrap().1 += delta;
    }
    fn transfer(&mut self, from: &Address, to: &Address, amount: i128) {
        self.change(from, -amount);
        self.change(to, amount);
    }
    fn assert(&self, f: &TestFixture, step: &str) {
        let mut total = 0;
        for (a, expected) in &self.balances {
            let actual = f.balance(a);
            assert_eq!(actual, *expected, "{step}: wallet {a:?}");
            assert!(*expected >= 0, "negative model balance at {step}");
            total += actual;
        }
        assert_eq!(total, self.minted, "{step}: token conservation");
    }
    fn fees(&mut self, f: &TestFixture, split_fee: i128, borrow: i128, exec: i128) -> i128 {
        let c = TestnetProfile::load().test_config();
        let treasury = TestnetProfile::load().treasury.rate;
        let keeper = cut(split_fee, c.keeper_rate) + exec;
        let protocol = cut(split_fee, treasury) + cut(borrow, treasury);
        self.transfer(&f.market.address, &f.keeper, keeper);
        self.transfer(&f.market.address, &f.treasury.address, protocol);
        self.transfer(
            &f.market.address,
            &f.vault.address,
            split_fee + borrow + exec - keeper - protocol,
        );
        keeper
    }
}

fn lp(f: &mut TestFixture, cash: &mut Cash) -> (Address, i128) {
    let c = TestnetProfile::load().test_config();
    let a = cash.user(f, LP_DEPOSIT + 10 * U);
    let deposit_fee = cut(LP_DEPOSIT, c.deposit_fee);
    let shares = (LP_DEPOSIT - deposit_fee) * 1_000_000;
    cash.transfer(&a, &f.market.address, LP_DEPOSIT + c.exec_fee);
    cash.transfer(
        &f.market.address,
        &f.vault.address,
        LP_DEPOSIT - deposit_fee,
    );
    let reward = cash.fees(f, deposit_fee, 0, c.exec_fee);
    let result = f.vault_deposit(&f.keeper, &a, LP_DEPOSIT, shares, ENTRY);
    assert!(result.filled, "LP deposit failed: {}", result.error);
    assert_eq!(result.payout, reward);
    assert_eq!(f.share_balance(&a), shares);
    cash.assert(f, "initial real LP deposit");
    (a, shares)
}

fn open(
    f: &TestFixture,
    cash: &mut Cash,
    a: &Address,
    side: bool,
    size: i128,
    margin: i128,
    worsening: bool,
) {
    let c = TestnetProfile::load().test_config();
    let charge = fee(&c, size, worsening);
    let id = f.submit_order(
        a,
        side,
        OrderKind::MarketIncrease,
        size,
        margin,
        0,
        0,
        f.far_expiration(),
    );
    cash.transfer(a, &f.market.address, margin + c.exec_fee);
    cash.assert(f, "increase escrow");
    let expected_reward = cash.fees(f, charge, 0, c.exec_fee);
    assert_eq!(
        f.execute_order(&f.keeper, a, id, &f.price(ENTRY)),
        expected_reward
    );
    let p = f.position(a, side);
    assert_eq!(
        (p.notional, p.tokens, p.margin),
        (size, size * 5, margin - charge)
    );
    cash.assert(f, "increase settlement");
    f.assert_borrowing_books(&f.users);
}

/// The tests deliberately keep a known 2001:1000 token ratio, then a lone side.
/// Funding is derived from that input ratio and independent signed-integer
/// rounding. Borrowing uses the maintained arithmetic-only helper, with the
/// expected cash model supplying backing instead of a contract balance read.
#[derive(Clone)]
struct Indices {
    at: u64,
    borrow: [i128; 2],
    funding: [i128; 2],
    rate: i128,
}
fn side(s: bool) -> usize {
    if s {
        0
    } else {
        1
    }
}
impl Indices {
    fn new(at: u64) -> Self {
        Self {
            at,
            borrow: [0; 2],
            funding: [0; 2],
            rate: 0,
        }
    }
    fn pair_window(
        &mut self,
        c: &Config,
        payer_long: bool,
        seconds: u64,
        reserve: i128,
        backing: i128,
    ) {
        let p = side(payer_long);
        let r = 1 - p;
        let skew = (PAYER_SIZE - RECEIVER_SIZE) * S / (PAYER_SIZE + RECEIVER_SIZE);
        let velocity = c.funding_increase * skew / S;
        let magnitude = (self.rate.abs() + velocity * seconds as i128).min(c.funding_max);
        self.rate = if payer_long { magnitude } else { -magnitude };
        let pay = magnitude.max(c.funding_min) * seconds as i128;
        self.funding[p] += pay;
        self.funding[r] -= pay * PAYER_SIZE / RECEIVER_SIZE;
        self.borrow[p] += borrowing_index(c, reserve, backing, seconds as i128);
        self.at += seconds;
    }
    fn assert(&self, f: &TestFixture) {
        let d = f.market.get_market_data();
        assert_eq!(d.accrued_at, self.at, "single accrual clock");
        assert_eq!(d.funding_rate, self.rate, "saved funding velocity");
        assert_eq!([d.funding_idx.long, d.funding_idx.short], self.funding);
        assert_eq!([d.borrowing_idx.long, d.borrowing_idx.short], self.borrow);
    }
}

fn flat(f: &TestFixture, users: &[(&Address, bool)]) {
    let d = f.market.get_market_data();
    for pair in [&d.notional, &d.tokens, &d.margin, &d.borrowing_weight] {
        assert_eq!((pair.long, pair.short), (0, 0));
    }
    for (a, s) in users {
        let p = f.position(a, *s);
        assert_eq!(
            (
                p.notional,
                p.tokens,
                p.margin,
                p.locked_notional,
                p.unlocks_at
            ),
            (0, 0, 0, 0, 0)
        );
        assert!(p.decrease_orders.is_empty());
        assert_eq!(
            (p.funding_idx, p.borrowing_idx),
            (0, 0),
            "closed row is reset"
        );
    }
}

fn close(
    f: &TestFixture,
    cash: &mut Cash,
    a: &Address,
    side: bool,
    size: i128,
    gross_margin: i128,
    opening_fee: i128,
    closing_fee: i128,
    borrow: i128,
    funding_paid: i128,
) {
    let c = TestnetProfile::load().test_config();
    let id = f.submit_order(
        a,
        side,
        OrderKind::MarketDecrease,
        size,
        0,
        0,
        0,
        f.far_expiration(),
    );
    cash.transfer(a, &f.market.address, c.exec_fee);
    let reward = cash.fees(f, closing_fee, borrow, c.exec_fee);
    cash.transfer(
        &f.market.address,
        a,
        gross_margin - opening_fee - closing_fee - borrow - funding_paid,
    );
    assert_eq!(f.execute_order(&f.keeper, a, id, &f.price(ENTRY)), reward);
    cash.assert(f, "voluntary full close");
    f.assert_borrowing_books(&f.users);
}

fn idle_reopen(payer_long: bool, explicit_idle_accrue: bool) {
    let c = TestnetProfile::load().test_config();
    let mut f = TestFixture::create_testnet();
    let mut cash = Cash::new(&f);
    let (lp_user, lp_shares) = lp(&mut f, &mut cash);
    let payer = cash.user(&mut f, 3_000 * U);
    let receiver = cash.user(&mut f, 2_000 * U);
    let newcomer = cash.user(&mut f, 1_000 * U);
    let payer_open = fee(&c, PAYER_SIZE, true);
    let receiver_open = fee(&c, RECEIVER_SIZE, false);
    open(
        &f,
        &mut cash,
        &payer,
        payer_long,
        PAYER_SIZE,
        1_000 * U,
        true,
    );
    open(
        &f,
        &mut cash,
        &receiver,
        !payer_long,
        RECEIVER_SIZE,
        500 * U,
        false,
    );
    let mut expected = Indices::new(f.env.ledger().timestamp());
    expected.pair_window(
        &c,
        payer_long,
        DAY,
        PAYER_SIZE,
        cash.balance(&f.vault.address),
    );
    f.jump(DAY);
    f.market.accrue(&f.price(ENTRY));
    expected.assert(&f);
    let p = side(payer_long);
    let r = 1 - p;
    let paid = ceil(PAYER_SIZE * expected.funding[p], S);
    let owed = RECEIVER_SIZE * -expected.funding[r] / S;
    let borrow = ceil(PAYER_SIZE * expected.borrow[p], S);
    assert!(
        paid > U && owed > U && borrow > U / 10,
        "meaningful actual-rate carry"
    );
    close(
        &f,
        &mut cash,
        &receiver,
        !payer_long,
        RECEIVER_SIZE,
        500 * U,
        receiver_open,
        fee(&c, RECEIVER_SIZE, true),
        0,
        0,
    );
    assert_eq!(f.market.get_claimable_credit(&receiver), owed);
    assert_eq!(
        f.market.get_market_data().credit_pool,
        0,
        "receiver settled before payer"
    );
    assert_eq!(
        f.market.try_claim_credit(&receiver),
        Err(Ok(MarketError::NothingToClaim.into()))
    );
    cash.assert(&f, "unfunded claim reverts");
    close(
        &f,
        &mut cash,
        &payer,
        payer_long,
        PAYER_SIZE,
        1_000 * U,
        payer_open,
        fee(&c, PAYER_SIZE, false),
        borrow,
        paid,
    );
    flat(&f, &[(&payer, payer_long), (&receiver, !payer_long)]);
    assert_eq!(f.market.get_market_data().credit_pool, paid);
    assert_eq!(f.market.get_market_data().credit_owed, owed);
    assert_eq!(
        paid - owed,
        1,
        "payer ceil minus receiver floor leaves one token atom"
    );
    // Both closes and this poke share a timestamp. An empty book at zero
    // elapsed preserves the saved nonzero rate and all indices.
    f.market.accrue(&f.price(ENTRY));
    expected.assert(&f);
    assert_ne!(expected.rate, 0);
    cash.assert(&f, "same-timestamp empty accrual");

    f.jump(30 * DAY);
    expected.at += 30 * DAY;
    expected.rate = 0;
    if explicit_idle_accrue {
        f.market.accrue(&f.price(ENTRY));
        expected.assert(&f);
        assert_eq!(f.market.get_claimable_credit(&receiver), owed);
        cash.assert(&f, "30-day explicit empty accrual");
        f.jump(DAY);
        expected.at += DAY;
    }
    // The new entrant takes the opposite side to the old payer, so a stale
    // nonzero saved rate would also have the wrong direction.
    let new_side = !payer_long;
    let size = 777 * U + 3;
    let margin = 200 * U + 1;
    let entry_fee = fee(&c, size, true);
    open(&f, &mut cash, &newcomer, new_side, size, margin, true);
    expected.assert(&f);
    let pos = f.position(&newcomer, new_side);
    assert_eq!(
        pos.funding_idx, expected.funding[r],
        "newcomer snapshots historical index"
    );
    assert_eq!(pos.borrowing_idx, expected.borrow[r]);
    assert_eq!(pos.margin, margin - entry_fee, "no historical idle debt");
    assert_eq!(
        f.market.get_claimable_credit(&receiver),
        owed,
        "old claim survives reopening"
    );
    assert_eq!(
        (
            f.market.get_market_data().credit_pool,
            f.market.get_market_data().credit_owed
        ),
        (paid, owed)
    );

    let funding_delta = funding_index(&c, DELAY as i128);
    let borrow_delta = borrowing_index(&c, size, cash.balance(&f.vault.address), DELAY as i128);
    expected.funding[r] += funding_delta;
    expected.borrow[r] += borrow_delta;
    expected.rate =
        (c.funding_increase * DELAY as i128).min(c.funding_max) * if new_side { 1 } else { -1 };
    expected.at += DELAY;
    let new_paid = ceil(size * funding_delta, S);
    let new_borrow = ceil(size * borrow_delta, S);
    f.jump(DELAY);
    close(
        &f,
        &mut cash,
        &newcomer,
        new_side,
        size,
        margin,
        entry_fee,
        fee(&c, size, false),
        new_borrow,
        new_paid,
    );
    expected.assert(&f);
    flat(
        &f,
        &[
            (&payer, payer_long),
            (&receiver, !payer_long),
            (&newcomer, new_side),
        ],
    );
    assert_eq!(f.market.get_claimable_credit(&newcomer), 0);
    assert_eq!(f.market.claim_credit(&receiver), owed);
    cash.transfer(&f.market.address, &receiver, owed);
    cash.assert(&f, "old receiver claims after new entrant exits");
    assert_eq!(
        (
            f.market.get_market_data().credit_pool,
            f.market.get_market_data().credit_owed
        ),
        (paid - owed + new_paid, 0)
    );
    assert_eq!(
        cash.balance(&f.market.address),
        paid - owed + new_paid,
        "only documented funding surplus remains"
    );
    assert_no_orders(&f, &payer, 3);
    assert_no_orders(&f, &receiver, 3);
    assert_no_orders(&f, &newcomer, 3);
    assert_no_orders(&f, &lp_user, 2);
    assert_eq!(
        f.share_balance(&lp_user),
        lp_shares,
        "idle scenario retains its real LP ownership"
    );
    for a in [&payer, &receiver, &newcomer] {
        assert_eq!(f.market.get_claimable_credit(a), 0);
    }
    println!("idle payer_long={payer_long} explicit={explicit_idle_accrue}: original funding paid={paid}, earned={owed}, borrowing={borrow}; newcomer funding={new_paid}, borrowing={new_borrow}; retained pool={}, LP shares={lp_shares}", paid - owed + new_paid);
}

#[test]
fn long_payer_flat_idle_accrue_then_short_reopens() {
    idle_reopen(true, true);
}
#[test]
fn short_payer_flat_idle_accrue_then_long_reopens() {
    idle_reopen(false, true);
}
#[test]
fn long_payer_flat_idle_then_short_reopens_directly() {
    idle_reopen(true, false);
}
#[test]
fn short_payer_flat_idle_then_long_reopens_directly() {
    idle_reopen(false, false);
}

/// TTL extensions are host maintenance. All ledger keys and stored values,
/// including token balances, claims, order counters and queue entries, roll back.
fn unchanged(f: &TestFixture, rejected: impl FnOnce()) {
    let before = f.env.to_ledger_snapshot();
    rejected();
    let after = f.env.to_ledger_snapshot();
    assert!(
        before
            .ledger_entries
            .iter()
            .map(|(key, (value, _))| (key, value))
            .eq(after
                .ledger_entries
                .iter()
                .map(|(key, (value, _))| (key, value))),
        "rejected call changed ledger values"
    );
}

fn assert_no_orders(f: &TestFixture, a: &Address, next: u32) {
    assert_eq!(
        f.env
            .as_contract(&f.market.address, || market::storage::get_order_counter(
                &f.env, a
            )),
        next
    );
    for id in 1..next {
        assert!(f.market.try_get_order(a, &id).is_err());
        assert!(f.market.try_get_vault_order(a, &id).is_err());
    }
}

#[contractclient(name = "OwnableClient")]
pub trait Ownable {
    fn transfer_ownership(e: &Env, new_owner: Address, live_until_ledger: u32);
    fn get_owner(e: &Env) -> Option<Address>;
}
#[contractclient(name = "SupplyClient")]
pub trait Supply {
    fn total_supply(e: &Env) -> i128;
}

fn auth(f: &TestFixture, who: &Address, contract: &Address, name: &'static str, args: Vec<Val>) {
    f.env.mock_auths(&[MockAuth {
        address: who,
        invoke: &MockAuthInvoke {
            contract,
            fn_name: name,
            args,
            sub_invokes: &[],
        },
    }]);
}
fn gov_status(f: &TestFixture, gov: &GovernanceClient, status: Status) {
    auth(
        f,
        &f.owner,
        &gov.address,
        "set_status",
        (f.market.address.clone(), status as u32).into_val(&f.env),
    );
    gov.set_status(&f.market.address, &(status as u32));
}
fn queue(f: &TestFixture, gov: &GovernanceClient, name: &str, args: Vec<Val>) -> u32 {
    let function = Symbol::new(&f.env, name);
    auth(
        f,
        &f.owner,
        &gov.address,
        "queue",
        (f.market.address.clone(), function.clone(), args.clone()).into_val(&f.env),
    );
    gov.queue(&f.market.address, &function, &args)
}
fn token_authorized(f: &TestFixture, a: &Address, enabled: bool) {
    auth(
        f,
        &f.owner,
        &f.token_address,
        "set_authorized",
        (a.clone(), enabled).into_val(&f.env),
    );
    f.token.set_authorized(a, &enabled);
}
fn claim(f: &TestFixture, cash: &mut Cash, a: &Address, amount: i128) {
    auth(
        f,
        a,
        &f.market.address,
        "claim_credit",
        (a.clone(),).into_val(&f.env),
    );
    assert_eq!(f.market.claim_credit(a), amount);
    cash.transfer(&f.market.address, a, amount);
    cash.assert(f, "authorized claim");
}

fn authorize_retired_redeem(f: &TestFixture, lp_user: &Address, shares: i128, redeemed: i128) {
    f.env.mock_auths(&[MockAuth {
        address: lp_user,
        invoke: &MockAuthInvoke {
            contract: &f.market.address,
            fn_name: "create_vault_order",
            args: (
                lp_user.clone(),
                VaultOrderKind::Redeem as u32,
                shares,
                redeemed,
            )
                .into_val(&f.env),
            sub_invokes: &[MockAuthInvoke {
                contract: &f.vault.address,
                fn_name: "transfer",
                args: (lp_user.clone(), f.market.address.clone(), shares).into_val(&f.env),
                sub_invokes: &[],
            }],
        },
    }]);
}

/// Check the actual liquidation receipt as well as independent token movements.
fn liquidation_receipt(f: &TestFixture, a: &Address, is_long: bool, values: &[(&str, i128)]) {
    let topic: Vec<Val> = (Symbol::new(&f.env, "liquidation"), a.clone(), is_long).into_val(&f.env);
    let expected_topics: soroban_sdk::xdr::VecM<soroban_sdk::xdr::ScVal> = topic.into();
    let events = f.env.events().all().filter_by_contract(&f.market.address);
    let body = events
        .events()
        .iter()
        .rev()
        .find_map(|event| {
            let soroban_sdk::xdr::ContractEventBody::V0(body) = &event.body;
            (body.topics == expected_topics).then_some(body)
        })
        .expect("liquidation receipt");
    let data = Val::try_from_val(&f.env, &body.data).unwrap();
    let map = Map::<Symbol, Val>::try_from_val(&f.env, &data).unwrap();
    for (key, expected) in values {
        assert_eq!(
            i128::try_from_val(&f.env, &map.get(Symbol::new(&f.env, key)).unwrap()).unwrap(),
            *expected,
            "receipt {key}"
        );
    }
}

#[test]
fn governed_receiver_first_default_retirement_and_last_lp_redemption() {
    let c = TestnetProfile::load().test_config();
    let mut f = TestFixture::create_testnet();
    let gov_id = f.env.register(GOVERNANCE_WASM, (&f.owner, DELAY));
    let gov = GovernanceClient::new(&f.env, &gov_id);
    let ownable = OwnableClient::new(&f.env, &f.market.address);
    let until = f.far_expiration();
    auth(
        &f,
        &f.owner,
        &f.market.address,
        "transfer_ownership",
        (gov_id.clone(), until).into_val(&f.env),
    );
    ownable.transfer_ownership(&gov_id, &until);
    // Governance accepts through its real queued invocation. Its contract
    // authority is automatic. There is no fake signature for the governance
    // address.
    let accept = queue(&f, &gov, "accept_ownership", Vec::new(&f.env));
    f.jump(DELAY);
    f.env.mock_auths(&[]);
    gov.execute(&accept);
    assert_eq!(ownable.get_owner(), Some(gov_id));
    // Economic setup uses fixture transaction auth. All governed actions,
    // liquidations, refunds, claims and final redemption below narrow it.
    f.env.mock_all_auths_allowing_non_root_auth();
    let mut cash = Cash::new(&f);
    let (lp_user, shares) = lp(&mut f, &mut cash);
    let payer = cash.user(&mut f, 1_000 * U);
    let receiver = cash.user(&mut f, 1_000 * U);
    let depositor = cash.user(&mut f, 500 * U);
    open(&f, &mut cash, &payer, true, PAYER_SIZE, 200 * U, true);
    open(
        &f,
        &mut cash,
        &receiver,
        false,
        RECEIVER_SIZE,
        400 * U,
        false,
    );
    let payer_margin = 200 * U - fee(&c, PAYER_SIZE, true);
    let receiver_margin = 400 * U - fee(&c, RECEIVER_SIZE, false);
    let payer_increase = f.submit_order(
        &payer,
        true,
        OrderKind::LimitIncrease,
        100 * U,
        50 * U,
        ENTRY / 2,
        0,
        f.far_expiration(),
    );
    cash.transfer(&payer, &f.market.address, 50 * U + c.exec_fee);
    let payer_decrease = f.submit_order(
        &payer,
        true,
        OrderKind::LimitDecrease,
        100 * U,
        0,
        ENTRY * 2,
        0,
        f.far_expiration(),
    );
    cash.transfer(&payer, &f.market.address, c.exec_fee);
    let receiver_decrease = f.submit_order(
        &receiver,
        false,
        OrderKind::LimitDecrease,
        100 * U,
        0,
        ENTRY / 2,
        0,
        f.far_expiration(),
    );
    cash.transfer(&receiver, &f.market.address, c.exec_fee);
    let deposit = 123 * U + 7;
    let deposit_id =
        f.market
            .create_vault_order(&depositor, &(VaultOrderKind::Deposit as u32), &deposit, &0);
    cash.transfer(&depositor, &f.market.address, deposit + c.exec_fee);
    let redeem_shares = shares / 4;
    let redeem_id = f.market.create_vault_order(
        &lp_user,
        &(VaultOrderKind::Redeem as u32),
        &redeem_shares,
        &0,
    );
    cash.transfer(&lp_user, &f.market.address, c.exec_fee);
    assert_eq!(f.share_balance(&f.market.address), redeem_shares);
    cash.assert(&f, "all live trade and vault escrows");

    let delisted = f.env.ledger().timestamp();
    let mut indices = Indices::new(delisted);
    gov_status(&f, &gov, Status::Delisted);
    assert_eq!(f.market.get_retirement(), Some((0, delisted)));
    let terminal = ENTRY / 2;
    let terminal_nonce = queue(&f, &gov, "set_terminal_price", (terminal,).into_val(&f.env));
    f.env.mock_auths(&[]);
    assert_eq!(
        gov.try_execute(&terminal_nonce),
        Err(Ok(GovernanceError::NotUnlocked.into()))
    );
    f.jump(DAY - 1);
    assert!(
        gov.try_execute(&terminal_nonce).is_err(),
        "mature queue still obeys market grace"
    );
    assert_eq!(
        gov.get_queued(&terminal_nonce).unlock_time,
        delisted + DELAY,
        "failed target preserves queued action"
    );
    assert_eq!(f.market.get_retirement(), Some((0, delisted)));
    f.jump(1);
    auth(
        &f,
        &f.owner,
        &gov.address,
        "set_status",
        (f.market.address.clone(), Status::Active as u32).into_val(&f.env),
    );
    assert!(
        gov.try_set_status(&f.market.address, &(Status::Active as u32))
            .is_err(),
        "exact grace boundary forbids reactivation"
    );
    f.env.mock_auths(&[]);
    gov.execute(&terminal_nonce);
    assert_eq!(f.market.get_retirement(), Some((terminal, delisted)));
    auth(
        &f,
        &f.owner,
        &gov.address,
        "set_status",
        (f.market.address.clone(), Status::Retired as u32).into_val(&f.env),
    );
    unchanged(&f, || {
        assert!(
            gov.try_set_status(&f.market.address, &(Status::Retired as u32))
                .is_err(),
            "governance cannot retire an open book"
        )
    });
    f.env.mock_auths(&[]);
    // The terminal path accepts an empty payload, proving this scenario no
    // longer depends on live oracle freshness after governance sets the mark.
    let ignored = Bytes::new(&f.env);
    indices.pair_window(
        &c,
        true,
        DAY,
        PAYER_SIZE / 2,
        cash.balance(&f.vault.address),
    );
    f.market.accrue(&ignored);
    indices.assert(&f);
    cash.assert(&f, "governed grace and terminal mark");
    f.jump(DEADLINE - DAY - 1);
    assert_eq!(
        f.market
            .try_execute_liquidation(&f.keeper, &receiver, &false, &ignored),
        Err(Ok(MarketError::NotLiquidatable.into())),
        "healthy receiver is not force-closeable one second before deadline"
    );
    indices.assert(&f);
    cash.assert(&f, "pre-deadline rejection rolls back accrual");
    f.jump(1);
    indices.pair_window(
        &c,
        true,
        DEADLINE - DAY,
        PAYER_SIZE / 2,
        cash.balance(&f.vault.address),
    );
    let payer_funding = ceil(PAYER_SIZE * indices.funding[0], S);
    let earned = RECEIVER_SIZE * -indices.funding[1] / S;
    let borrowing = ceil(PAYER_SIZE * indices.borrow[0], S);
    let receiver_close_fee = fee(&c, RECEIVER_SIZE, true);
    let receiver_pnl = RECEIVER_SIZE / 2;
    let liq_fee = ceil(RECEIVER_SIZE * c.liq_fee, S);
    let receiver_returned = receiver_margin + receiver_pnl - receiver_close_fee - liq_fee;
    let parked = receiver_returned + c.exec_fee; // swept decrease-order fee
    assert!(payer_funding > 7 * U && earned > 7 * U && borrowing > U / 10);

    // Keeper/treasury transfer failure traps the transaction. Only the
    // trader leg parks. Both failures must preserve positions and escrows.
    for recipient in [&f.keeper, &f.treasury.address] {
        token_authorized(&f, recipient, false);
        f.env.mock_auths(&[]);
        unchanged(&f, || {
            assert!(f
                .market
                .try_execute_liquidation(&f.keeper, &receiver, &false, &ignored)
                .is_err())
        });
        assert_eq!(f.position(&receiver, false).notional, RECEIVER_SIZE);
        assert!(f
            .market
            .try_get_order(&receiver, &receiver_decrease)
            .is_ok());
        assert_eq!(f.market.get_claimable_credit(&receiver), 0);
        cash.assert(&f, "failed fee recipient rolls back liquidation");
        token_authorized(&f, recipient, true);
    }
    token_authorized(&f, &receiver, false);
    f.env.mock_auths(&[]);
    let reward = cash.fees(&f, receiver_close_fee + liq_fee, 0, 0);
    cash.transfer(&f.vault.address, &f.market.address, receiver_pnl);
    assert_eq!(
        f.market
            .execute_liquidation(&f.keeper, &receiver, &false, &ignored),
        reward
    );
    liquidation_receipt(
        &f,
        &receiver,
        false,
        &[
            ("price", terminal),
            ("notional", RECEIVER_SIZE),
            ("tokens", RECEIVER_SIZE * 5),
            ("margin", receiver_margin),
            ("pnl", receiver_pnl),
            ("base_fee", ceil(RECEIVER_SIZE * c.fee_dom, S)),
            (
                "impact_fee",
                ceil(RECEIVER_SIZE * RECEIVER_SIZE, c.impact_scalar),
            ),
            ("funding", -earned),
            ("borrowing", 0),
            ("bad_debt", 0),
            ("returned", receiver_returned),
            ("liq_fee", liq_fee),
        ],
    );
    indices.assert(&f);
    cash.assert(&f, "receiver settled first and payout parked");
    assert_eq!(f.market.get_claimable_credit(&receiver), earned + parked);
    assert_eq!(
        (
            f.market.get_market_data().credit_pool,
            f.market.get_market_data().credit_owed
        ),
        (parked, earned + parked)
    );
    assert!(f
        .market
        .try_get_order(&receiver, &receiver_decrease)
        .is_err());
    // An ordinary claim cannot itself park a failing transfer.
    auth(
        &f,
        &receiver,
        &f.market.address,
        "claim_credit",
        (receiver.clone(),).into_val(&f.env),
    );
    unchanged(&f, || {
        assert!(f.market.try_claim_credit(&receiver).is_err())
    });
    assert_eq!(f.market.get_claimable_credit(&receiver), earned + parked);
    cash.assert(&f, "deauthorized claim rolls back");
    token_authorized(&f, &receiver, true);
    claim(&f, &mut cash, &receiver, parked);
    assert_eq!(
        f.market.get_claimable_credit(&receiver),
        earned,
        "partially funded claim retains real funding debt"
    );
    assert_eq!(
        (
            f.market.get_market_data().credit_pool,
            f.market.get_market_data().credit_owed
        ),
        (0, earned)
    );

    let payer_close_fee = fee(&c, PAYER_SIZE, false);
    let payer_pnl = -PAYER_SIZE / 2;
    let bad_debt = -(payer_margin + payer_pnl - payer_close_fee - borrowing - payer_funding);
    assert!(
        bad_debt > 800 * U,
        "payer defaults despite later settlement"
    );
    let reward = cash.fees(&f, payer_close_fee, borrowing, 0);
    // The vault recognizes the loss only up to net collectible collateral:
    // full marked loss + bad debt cancel the uncollectible economic leg.
    cash.transfer(&f.vault.address, &f.market.address, payer_pnl + bad_debt);
    cash.transfer(&f.market.address, &payer, c.exec_fee); // swept decrease
    f.env.mock_auths(&[]);
    assert_eq!(
        f.market
            .execute_liquidation(&f.keeper, &payer, &true, &ignored),
        reward
    );
    liquidation_receipt(
        &f,
        &payer,
        true,
        &[
            ("price", terminal),
            ("notional", PAYER_SIZE),
            ("tokens", PAYER_SIZE * 5),
            ("margin", payer_margin),
            ("pnl", payer_pnl),
            ("base_fee", ceil(PAYER_SIZE * c.fee_non_dom, S)),
            ("impact_fee", ceil(PAYER_SIZE * PAYER_SIZE, c.impact_scalar)),
            ("funding", payer_funding),
            ("borrowing", borrowing),
            ("bad_debt", bad_debt),
            ("returned", 0),
            ("liq_fee", 0),
        ],
    );
    indices.assert(&f);
    cash.assert(&f, "later payer default funds previously earned claim");
    flat(&f, &[(&payer, true), (&receiver, false)]);
    assert!(f.market.try_get_order(&payer, &payer_decrease).is_err());
    assert!(
        f.market.try_get_order(&payer, &payer_increase).is_ok(),
        "increases are not position-close sweep entries"
    );
    assert_eq!(
        (
            f.market.get_market_data().credit_pool,
            f.market.get_market_data().credit_owed
        ),
        (payer_funding, earned)
    );
    let dust = payer_funding - earned;
    assert_eq!(
        dust, 1,
        "payer ceil minus receiver floor leaves one token atom"
    );
    // Retirement sweeps only rounding surplus, retaining both real claims
    // and every surviving asset/share escrow.
    gov_status(&f, &gov, Status::Retired);
    cash.transfer(&f.market.address, &f.vault.address, dust);
    cash.assert(&f, "retirement carve-outs");
    assert_eq!(
        (
            f.market.get_market_data().credit_pool,
            f.market.get_market_data().credit_owed
        ),
        (earned, earned)
    );
    assert_eq!(
        cash.balance(&f.market.address),
        earned + 50 * U + deposit + 3 * c.exec_fee
    );
    assert_eq!(f.share_balance(&f.market.address), redeem_shares);
    claim(&f, &mut cash, &receiver, earned);
    assert_eq!(f.market.get_claimable_credit(&receiver), 0);

    // Retired cancellations are ordinary transfers. An unavailable recipient
    // must reauthorize before its escrow can be refunded, with no double use.
    token_authorized(&f, &depositor, false);
    auth(
        &f,
        &depositor,
        &f.market.address,
        "cancel_vault_order",
        (depositor.clone(), deposit_id).into_val(&f.env),
    );
    unchanged(&f, || {
        assert!(f
            .market
            .try_cancel_vault_order(&depositor, &deposit_id)
            .is_err())
    });
    assert!(f
        .market
        .try_get_vault_order(&depositor, &deposit_id)
        .is_ok());
    cash.assert(&f, "failed escrow refund leaves order intact");
    token_authorized(&f, &depositor, true);
    auth(
        &f,
        &depositor,
        &f.market.address,
        "cancel_vault_order",
        (depositor.clone(), deposit_id).into_val(&f.env),
    );
    assert_eq!(
        f.market.cancel_vault_order(&depositor, &deposit_id),
        deposit
    );
    cash.transfer(&f.market.address, &depositor, deposit + c.exec_fee);
    auth(
        &f,
        &payer,
        &f.market.address,
        "cancel_order",
        (payer.clone(), payer_increase).into_val(&f.env),
    );
    f.market.cancel_order(&payer, &payer_increase);
    cash.transfer(&f.market.address, &payer, 50 * U + c.exec_fee);
    auth(
        &f,
        &lp_user,
        &f.market.address,
        "cancel_vault_order",
        (lp_user.clone(), redeem_id).into_val(&f.env),
    );
    assert_eq!(
        f.market.cancel_vault_order(&lp_user, &redeem_id),
        redeem_shares
    );
    cash.transfer(&f.market.address, &lp_user, c.exec_fee);
    assert_eq!(f.share_balance(&lp_user), shares);
    assert_eq!(f.share_balance(&f.market.address), 0);
    cash.assert(&f, "all retired escrows recovered");
    assert_eq!(cash.balance(&f.market.address), 0);
    assert_eq!(
        (
            f.market.get_market_data().credit_pool,
            f.market.get_market_data().credit_owed
        ),
        (0, 0)
    );

    let assets = cash.balance(&f.vault.address);
    let redeemed = shares * (assets + 1) / (shares + 1_000_000);
    // Retired direct redemption requires the LP's nested share transfer auth.
    // The vault's strategy authorization comes from the real market call.
    token_authorized(&f, &lp_user, false);
    authorize_retired_redeem(&f, &lp_user, shares, redeemed);
    unchanged(&f, || {
        assert!(
            f.market
                .try_create_vault_order(
                    &lp_user,
                    &(VaultOrderKind::Redeem as u32),
                    &shares,
                    &redeemed
                )
                .is_err(),
            "retired direct redeem rolls back when recipient cannot receive"
        )
    });
    cash.assert(&f, "failed direct redemption keeps both shares and assets");
    token_authorized(&f, &lp_user, true);
    authorize_retired_redeem(&f, &lp_user, shares, redeemed);
    assert_eq!(
        f.market.create_vault_order(
            &lp_user,
            &(VaultOrderKind::Redeem as u32),
            &shares,
            &redeemed
        ),
        0
    );
    cash.transfer(&f.vault.address, &lp_user, redeemed);
    cash.assert(
        &f,
        "final LP redemption without fee or keeper execution charge",
    );
    assert_eq!(f.share_balance(&lp_user), 0);
    assert_eq!(f.share_balance(&f.market.address), 0);
    assert_eq!(
        SupplyClient::new(&f.env, &f.vault.address).total_supply(),
        0
    );
    assert_eq!(
        assets - redeemed,
        0,
        "virtual shares round to no residual asset for this loss-bearing vault"
    );
    flat(&f, &[(&payer, true), (&receiver, false)]);
    assert_eq!(f.market.get_status(), Status::Retired as u32);
    assert_no_orders(&f, &payer, 4);
    assert_no_orders(&f, &receiver, 3);
    assert_no_orders(&f, &depositor, 2);
    assert_no_orders(&f, &lp_user, 3);
    for a in [&payer, &receiver, &depositor, &lp_user] {
        assert_eq!(f.market.get_claimable_credit(a), 0);
    }
    println!("wind-down: funding paid={payer_funding}, earned={earned}, borrowing={borrowing}, parked={parked}, bad debt={bad_debt}, funding dust={dust}; final LP redeemed={redeemed}, virtual-share remainder={}; keeper={}, treasury={}, payer={}, receiver={}", assets - redeemed, cash.balance(&f.keeper), cash.balance(&f.treasury.address), cash.balance(&payer), cash.balance(&receiver));
}
