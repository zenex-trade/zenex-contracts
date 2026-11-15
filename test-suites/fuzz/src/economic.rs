//! Input-derived acceptance model for bounded complete economic lifecycles.
//!
//! This is intentionally separate from the rejection-heavy stress grammar.
//! Every input opens real positions and MUST settle them, fill an LP redeem
//! and a deposit (or its gated refusal) against the live book, refund all
//! escrows, collect funded claims, retire and redeem every LP. Amounts are
//! predicted from inputs and local integer arithmetic, never initialized from
//! receipts.
use crate::fixture::{Fixture, SCALAR_18 as S, SCALAR_7 as U, USER_BALANCE, VAULT_SEED};
use crate::invariants::{position, Ledger, Snapshot};
use crate::observer::Values;
use crate::price::TESTNET_START_PRICE;
use crate::stats::record;
use market::Config;
use soroban_sdk::testutils::Events;
use soroban_sdk::{contractclient, Address, Bytes, Env, Map, Symbol, TryFromVal, Val};
use test_suites::chainlink_helper::{build_report, ReportTimes};
use test_suites::testnet_profile::TestnetProfile;
use treasury::TreasuryClient;

#[allow(dead_code)]
#[contractclient(name = "OwnershipClient")]
pub trait Ownership {
    fn get_owner(e: Env) -> Option<Address>;
    fn transfer_ownership(e: Env, new_owner: Address, live_until_ledger: u32);
    fn accept_ownership(e: Env);
}

const KEEPER: usize = 3;
const TREASURY: usize = 4;
const VAULT: usize = 5;
const SEED: usize = 6;
const OWNER: usize = 7;
const MARKET: usize = 8;
fn ceil(n: i128, d: i128) -> i128 {
    assert!(n >= 0 && d > 0);
    n / d + i128::from(n % d != 0)
}
fn floor_factor(n: i128, r: i128) -> i128 {
    n * r / S
}
fn charge(n: i128, r: i128) -> i128 {
    ceil(n * r, S)
}
fn signed_charge(n: i128, r: i128) -> i128 {
    if r >= 0 {
        charge(n, r)
    } else {
        -floor_factor(n, -r)
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Pos {
    notional: i128,
    tokens: i128,
    margin: i128,
    funding: i128,
    borrowing: i128,
}
#[derive(Clone)]
struct Model {
    config: Config,
    treasury_rate: i128,
    balances: [i128; 9],
    positions: [Pos; 2],
    funding: [i128; 2],
    borrowing: [i128; 2],
    rate: i128,
    pool: i128,
    claims: [i128; 3],
    shares: i128,
    /// Vault shares user 2 holds after the mid-book deposit.
    lp2_shares: i128,
}
impl Model {
    fn new(config: Config, treasury_rate: i128, share_scale: i128) -> Self {
        let mut m = Self {
            config,
            treasury_rate,
            balances: [0; 9],
            positions: [Pos::default(); 2],
            funding: [0; 2],
            borrowing: [0; 2],
            rate: 0,
            pool: 0,
            claims: [0; 3],
            shares: 0,
            lp2_shares: 0,
        };
        for b in &mut m.balances[..3] {
            *b = USER_BALANCE;
        }
        let fee = floor_factor(VAULT_SEED, m.config.deposit_fee);
        m.balances[SEED] = U - m.config.exec_fee;
        m.shares = (VAULT_SEED - fee) * share_scale;
        m.balances[VAULT] = VAULT_SEED - fee;
        m.balances[MARKET] = fee + m.config.exec_fee;
        m.split(fee, 0, 0, m.config.exec_fee, 0, SEED);
        m
    }
    /// `MarketData::side_pnl` from the model's position: trader-best
    /// (`maximize`) marks a long at the ask rounded up and a short at the bid
    /// rounded down, trader-worst the reverse. A loss clips at the margin.
    fn side_pnl(&self, side: usize, bid: i128, ask: i128, maximize: bool) -> i128 {
        let p = self.positions[side];
        let pnl = match (side == 0, maximize) {
            (true, true) => ceil(p.tokens * ask, S) - p.notional,
            (true, false) => p.tokens * bid / S - p.notional,
            (false, true) => p.notional - p.tokens * bid / S,
            (false, false) => p.notional - ceil(p.tokens * ask, S),
        };
        pnl.max(-p.margin)
    }
    /// `Market::lp_net_pnl`: each side's PnL capped at `max_pnl_trader` of
    /// half the vault, less the vault's cut of the pending borrowing (after
    /// the treasury's). `maximize` (redeem) floors the borrowing and the cut,
    /// trader-worst (deposit) ceils both.
    fn lp_net_pnl(&self, bid: i128, ask: i128, maximize: bool) -> i128 {
        let cap = floor_factor(self.balances[VAULT] / 2, self.config.max_pnl_trader);
        let pnl = self.side_pnl(0, bid, ask, maximize).min(cap)
            + self.side_pnl(1, bid, ask, maximize).min(cap);
        let exact = self.pending_exact();
        let vault_rate = S - self.treasury_rate;
        let income = if maximize {
            floor_factor(exact / S, vault_rate)
        } else {
            ceil(ceil(exact, S) * vault_rate, S)
        };
        pnl - income
    }
    /// The borrowing both sides owe but have not settled, before the one
    /// rounding (token-dec times `S`).
    fn pending_exact(&self) -> i128 {
        (0..2)
            .map(|side| {
                let p = self.positions[side];
                p.notional * (self.borrowing[side] - p.borrowing)
            })
            .sum()
    }
    /// Shares outstanding, escrowed ones included.
    fn supply(&self) -> i128 {
        self.shares + self.lp2_shares
    }
    fn transfer(&mut self, from: usize, to: usize, n: i128) {
        assert!(n >= 0);
        self.balances[from] -= n;
        self.balances[to] += n;
    }
    fn split(
        &mut self,
        fee: i128,
        borrow: i128,
        pnl_and_debt: i128,
        exec: i128,
        payout: i128,
        user: usize,
    ) -> i128 {
        let keeper = floor_factor(fee, self.config.keeper_rate);
        let treasury =
            floor_factor(fee, self.treasury_rate) + floor_factor(borrow, self.treasury_rate);
        let vault = fee + borrow - keeper - treasury - pnl_and_debt;
        if vault >= 0 {
            self.transfer(MARKET, VAULT, vault);
        } else {
            self.transfer(VAULT, MARKET, -vault);
        }
        self.transfer(MARKET, KEEPER, keeper + exec);
        self.transfer(MARKET, TREASURY, treasury);
        self.transfer(MARKET, user, payout);
        keeper + exec
    }
    fn fees(&self, side: usize, n: i128, t: i128) -> (i128, i128) {
        let old = self.positions[0].tokens - self.positions[1].tokens;
        let new = old + if side == 0 { t } else { -t };
        let worsening = if old != 0 && new != 0 && old.signum() != new.signum() {
            new.abs()
        } else if new.abs() > old.abs() {
            t.abs()
        } else {
            0
        };
        let worsened = if t == 0 {
            0
        } else {
            ceil(n.abs() * worsening, t.abs())
        };
        (
            charge(worsened, self.config.fee_dom)
                + charge(n.abs() - worsened, self.config.fee_non_dom),
            ceil(n * n, self.config.impact_scalar).min(ceil(n.abs(), 10)),
        )
    }
    fn open(&mut self, side: usize, n: i128, margin: i128, price: i128) -> i128 {
        let tokens = if side == 0 {
            n * S / price
        } else {
            ceil(n * S, price)
        };
        let (base, impact) = self.fees(side, n, tokens);
        self.positions[side] = Pos {
            notional: n,
            tokens,
            margin: margin - base - impact,
            funding: self.funding[side],
            borrowing: self.borrowing[side],
        };
        self.split(base + impact, 0, 0, self.config.exec_fee, 0, side)
    }
    fn accrue(&mut self, seconds: u64, ask: i128) {
        let dt = i128::from(seconds);
        if dt == 0 {
            return;
        }
        let [l, r] = self.positions.map(|p| p.tokens);
        let total = l + r;
        for side in 0..2 {
            if self.positions[side].tokens < self.positions[1 - side].tokens {
                continue;
            }
            let reserve = if side == 0 {
                ceil(l * ask, S)
            } else {
                self.positions[1].notional
            };
            let capacity = floor_factor(self.balances[VAULT] / 2, self.config.max_util_open);
            let util = if reserve == 0 {
                0
            } else if capacity == 0 {
                S
            } else {
                ceil(reserve * S, capacity).min(S)
            };
            let rate = charge(self.config.borrow_rate, util)
                + if util > self.config.target_util {
                    ceil(
                        (self.config.increased_borrow_rate - self.config.borrow_rate)
                            * (util - self.config.target_util),
                        S - self.config.target_util,
                    )
                } else {
                    0
                };
            self.borrowing[side] += rate * dt;
        }
        if total == 0 {
            self.rate = 0;
            return;
        }
        let direction = (l - r).signum();
        let skew = (l - r).abs() * S / total;
        let same = self.rate.signum() == direction && self.rate != 0;
        if direction != 0 && (!same || skew > self.config.threshold_stable_funding) {
            self.rate += direction * floor_factor(self.config.funding_increase, skew) * dt;
        } else if direction != 0 && skew < self.config.threshold_decrease_funding {
            self.rate =
                self.rate.signum() * (self.rate.abs() - self.config.funding_decrease * dt).max(1);
        }
        self.rate = self
            .rate
            .clamp(-self.config.funding_max, self.config.funding_max);
        if self.rate == 0 {
            return;
        }
        let payer = usize::from(self.rate < 0);
        let receiver = 1 - payer;
        let paid = self.rate.abs().max(self.config.funding_min) * dt;
        self.funding[payer] += paid;
        if self.positions[receiver].notional > 0 {
            self.funding[receiver] -=
                paid * self.positions[payer].notional / self.positions[receiver].notional;
        }
    }
    fn close(&mut self, side: usize, bid: i128, ask: i128, liquidate: bool, exec: i128) -> Outcome {
        let p = self.positions[side];
        assert!(p.notional > 0);
        let (base, impact) = self.fees(side, -p.notional, -p.tokens);
        let funding = signed_charge(p.notional, self.funding[side] - p.funding);
        let borrowing = charge(p.notional, self.borrowing[side] - p.borrowing);
        if funding > 0 {
            self.pool += funding;
        } else {
            self.claims[side] -= funding;
        }
        let raw = if side == 0 {
            p.tokens * bid / S - p.notional
        } else {
            p.notional - ceil(p.tokens * ask, S)
        };
        let best = if side == 0 {
            ceil(p.tokens * ask, S) - p.notional
        } else {
            p.notional - p.tokens * bid / S
        };
        let cap = floor_factor(self.balances[VAULT] / 2, self.config.max_pnl_trader);
        let pnl = if raw > 0 && best > cap {
            raw * cap / best
        } else {
            raw
        };
        let equity = p.margin + pnl - base - impact - borrowing - funding.max(0);
        let eligible = equity < charge(p.notional, self.config.maintenance_margin);
        let liq_fee = if liquidate {
            charge(p.notional, self.config.liq_fee).min(equity.max(0))
        } else {
            0
        };
        let bad_debt = (-equity).max(0);
        let returned = equity.max(0) - liq_fee;
        let reward = self.split(
            base + impact + liq_fee,
            borrowing,
            pnl + bad_debt,
            exec,
            returned,
            side,
        );
        self.positions[side] = Pos::default();
        Outcome {
            price: if side == 0 { bid } else { ask },
            base,
            impact,
            funding,
            borrowing,
            pnl,
            bad_debt,
            returned,
            liq_fee,
            reward,
            eligible,
        }
    }
    fn partial_adl(&mut self, amount: i128, bid: i128, ask: i128) -> Outcome {
        let p = self.positions[0];
        let tokens = p.tokens * amount / p.notional;
        let (base, impact) = self.fees(0, -amount, -tokens);
        let funding = signed_charge(p.notional, self.funding[0] - p.funding);
        let borrowing = charge(p.notional, self.borrowing[0] - p.borrowing);
        if funding > 0 {
            self.pool += funding;
        } else {
            self.claims[0] -= funding;
        }
        let raw = tokens * bid / S - amount;
        assert!(raw > 0);
        let best = ceil(p.tokens * ask, S) - p.notional;
        let cap = floor_factor(self.balances[VAULT] / 2, self.config.max_pnl_trader);
        let pnl = if best > cap { raw * cap / best } else { raw };
        let cost = base + impact + borrowing + funding.max(0);
        let returned = (pnl - cost).max(0);
        let margin = p.margin - (cost - pnl).max(0);
        assert!(margin > 0);
        let reward = self.split(base + impact, borrowing, pnl, 0, returned, 0);
        self.positions[0] = Pos {
            notional: p.notional - amount,
            tokens: p.tokens - tokens,
            margin,
            funding: self.funding[0],
            borrowing: self.borrowing[0],
        };
        Outcome {
            price: bid,
            base,
            impact,
            funding,
            borrowing,
            pnl,
            bad_debt: 0,
            returned,
            liq_fee: 0,
            reward,
            eligible: false,
        }
    }
    fn check(&self, f: &Fixture, context: &str) {
        let actors = [
            f.user(0),
            f.user(1),
            f.user(2),
            f.keeper.clone(),
            f.treasury_address.clone(),
            f.vault_address.clone(),
            f.seed_lp.clone(),
            f.owner.clone(),
            f.market_address.clone(),
        ];
        let actual = actors.map(|a| f.token.balance(&a));
        assert_eq!(
            actual, self.balances,
            "independent actor balances: {context}"
        );
        assert!(
            self.balances.iter().all(|b| *b >= 0),
            "model insolvency: {context}"
        );
        assert_eq!(
            self.balances.iter().sum::<i128>(),
            3 * USER_BALANCE + VAULT_SEED + U,
            "total issued tokens"
        );
        let s = Snapshot::take(f);
        for side in 0..2 {
            let p = position(f, &f.user(side), side == 0);
            let expected = self.positions[side];
            assert_eq!(
                (p.notional, p.tokens, p.margin),
                (expected.notional, expected.tokens, expected.margin),
                "modeled position: {context}"
            );
            if expected.notional > 0 {
                assert_eq!(
                    p.borrowing_idx, expected.borrowing,
                    "modeled borrowing checkpoint: {context}"
                );
            }
        }
        assert_eq!(
            (s.market.funding_idx.long, s.market.funding_idx.short),
            (self.funding[0], self.funding[1]),
            "funding indices: {context}"
        );
        assert_eq!(
            (s.market.borrowing_idx.long, s.market.borrowing_idx.short),
            (self.borrowing[0], self.borrowing[1]),
            "borrowing indices: {context}"
        );
        // The unsettled borrowing from the model's own indices alone: the
        // figure `lp_net_pnl` prices LP shares with, at both roundings.
        let exact = self.pending_exact();
        assert_eq!(
            (
                s.market.pending_borrowing(&f.env, true),
                s.market.pending_borrowing(&f.env, false)
            ),
            (exact / S, ceil(exact, S)),
            "modeled pending borrowing: {context}"
        );
        assert_eq!(s.market.funding_rate, self.rate, "funding velocity");
        assert_eq!(s.market.credit_pool, self.pool, "independent funding pool");
        assert_eq!(
            s.market.credit_owed,
            self.claims.iter().sum::<i128>(),
            "independent liability total"
        );
        for (actual, expected) in s.users.iter().zip(self.claims) {
            assert_eq!(actual.claimable, expected, "independent user claims");
        }
    }
}
#[derive(Debug)]
struct Outcome {
    price: i128,
    base: i128,
    impact: i128,
    funding: i128,
    borrowing: i128,
    pnl: i128,
    bad_debt: i128,
    returned: i128,
    liq_fee: i128,
    reward: i128,
    eligible: bool,
}
fn report(f: &Fixture, bid: i128, ask: i128, age: u64) -> Bytes {
    build_report(
        &f.env,
        &f.feed,
        ReportTimes::at(f.env.ledger().timestamp() - age),
        (bid + ask) / 2,
        bid,
        ask,
    )
}
fn narrowed(bid: i128, ask: i128) -> (i128, i128) {
    let cut = (ask - bid) / 2 / 2;
    (bid + cut, ask - cut)
}
/// The data map of the market's latest `name` event.
fn latest_event(f: &Fixture, name: &str) -> Map<Symbol, Val> {
    let events = f.env.events().all().filter_by_contract(&f.market_address);
    let body = events
        .events()
        .iter()
        .rev()
        .find_map(|e| {
            let soroban_sdk::xdr::ContractEventBody::V0(b) = &e.body;
            let first = b.topics.first()?;
            let symbol =
                Symbol::try_from_val(&f.env, &Val::try_from_val(&f.env, first).ok()?).ok()?;
            (symbol == Symbol::new(&f.env, name)).then_some(b)
        })
        .expect("required execution receipt");
    Map::<Symbol, Val>::try_from_val(&f.env, &Val::try_from_val(&f.env, &body.data).unwrap())
        .unwrap()
}
/// Checks the latest `name` event's integer `fields` against the model.
fn receipt_fields(f: &Fixture, name: &str, fields: &[(&str, i128)]) {
    let map = latest_event(f, name);
    for (key, value) in fields {
        let actual = i128::try_from_val(
            &f.env,
            &map.get(Symbol::new(&f.env, key)).expect("receipt field"),
        )
        .unwrap();
        assert_eq!(actual, *value, "independent execution receipt {name}.{key}");
    }
}
fn receipt(f: &Fixture, name: &str, expected: &Outcome) {
    let mut fields = std::vec![
        ("price", expected.price),
        ("base_fee", expected.base),
        ("impact_fee", expected.impact),
        ("funding", expected.funding),
        ("borrowing", expected.borrowing),
        ("pnl", expected.pnl),
        ("returned", expected.returned),
    ];
    if name != "decrease_fill" {
        fields.push(("bad_debt", expected.bad_debt));
    }
    if name == "liquidation" {
        fields.push(("liq_fee", expected.liq_fee));
    }
    receipt_fields(f, name, &fields);
}
fn reject<T: std::fmt::Debug, E: std::fmt::Debug>(
    f: &Fixture,
    before: Values,
    result: Result<Result<T, E>, Result<soroban_sdk::Error, soroban_sdk::InvokeError>>,
    code: u32,
) {
    match result {
        Err(Ok(e)) => assert_eq!(
            e,
            soroban_sdk::Error::from_contract_error(code),
            "expected rejection"
        ),
        other => panic!("expected rejection {code}: {other:?}"),
    }
    before.assert_unchanged(&f.env);
    record("expected_rejection", 1);
    record("rollback_checked", 1);
}

/// Fixed-size bytes give every libFuzzer input a meaningful complete trace.
///
/// - mode 0: voluntary.
/// - mode 1: bad-debt liquidation.
/// - mode 2: cached ADL winner.
/// - mode 3: explicitly blocked frozen accrual, then recovery.
/// - mode 4: terminal override.
///
/// All modes keep the recorded economics. Admin changes are restored before
/// measured trading. Treasury withdrawals remain independently reconciled.
pub fn run(input: &[u8]) {
    run_inner(input, false, false, false);
}
fn run_inner(input: &[u8], deny_valid_exit: bool, misroute_fee: bool, wrong_price: bool) {
    let byte = |i: usize| input.get(i).copied().unwrap_or(0);
    let mode = byte(0) % 5;
    let f = Fixture::create_testnet();
    let profile = TestnetProfile::load();
    let cfg = profile.test_config();
    let mut m = Model::new(cfg.clone(), profile.dials().treasury_rate, f.share_scale);
    m.check(&f, "bootstrap");
    record("economic_attempt", 1);
    let treasury = TreasuryClient::new(&f.env, &f.treasury_address);
    admin_churn(&f, &m, &byte);
    // Use varied 18-dec XLM prices, including base-token rounding dust.
    let start = TESTNET_START_PRICE + i128::from(byte(1)) * 100_003;
    let large = (20_000 + i128::from(byte(2)) * 100) * U + i128::from(byte(7));
    let small = (500 + i128::from(byte(3)) * 10) * U + i128::from(byte(6));
    let ns = if mode == 2 {
        [70_000 * U, small]
    } else if byte(4) & 1 == 0 {
        [large, small]
    } else {
        [small, large]
    };
    open_book(&f, &mut m, ns, mode, start, misroute_fee);
    // Preserve live trade and vault escrow across settlements, then refund it.
    let pending_margin = 2 * U;
    let pending = f.market.create_order(
        &f.user(2),
        &true,
        &0,
        &U,
        &pending_margin,
        &0,
        &0,
        &f.far_expiration(),
    );
    m.transfer(2, MARKET, pending_margin + cfg.exec_fee);
    let deposit = f.market.create_vault_order(&f.user(2), &0, &U, &0);
    m.transfer(2, MARKET, U + cfg.exec_fee);
    m.check(&f, "pending escrows");
    // The seed LP must be permitted to submit a complete exit. While leveraged
    // traders remain it is correctly blocked, with rollback. A small redeem
    // placed alongside it fills mid-book first, against pending borrowing;
    // the exit then covers every remaining share. A sized exit the vault can
    // pay follows, blocked by the reserve gate.
    let part = m.shares / 1000;
    let part_order = f.market.create_vault_order(&f.seed_lp, &1, &part, &0);
    m.transfer(SEED, MARKET, cfg.exec_fee);
    let lp_order = f
        .market
        .create_vault_order(&f.seed_lp, &1, &(m.shares - part), &0);
    m.transfer(SEED, MARKET, cfg.exec_fee);
    let hold = cfg.redeem_lock + 60 + u64::from(byte(5)) * 120;
    f.jump(hold);
    let mut repricing = cfg.clone();
    repricing.borrow_rate += 1;
    repricing.increased_borrow_rate += 1;
    let before = Values::take(&f.env);
    reject(&f, before, f.market.try_set_config(&repricing), 703);
    m.accrue(hold, start);
    f.market.accrue(&report(&f, start, start, 0));
    m.check(&f, "held book");
    record("carry_checked", 1);
    let early_fees = m.balances[TREASURY] * i128::from(byte(6)) / 255;
    treasury.withdraw(&f.token_address, &f.owner, &early_fees);
    m.transfer(TREASURY, OWNER, early_fees);
    let before = Values::take(&f.env);
    reject(
        &f,
        before,
        treasury.try_withdraw(&f.token_address, &f.owner, &(m.balances[TREASURY] + 1)),
        10,
    );
    m.check(&f, "mid-book treasury withdrawal");
    record("treasury_withdrawal_checked", 1);
    // The mid-book redeem prices the vault's cut of the pending borrowing,
    // floored, into the backing the burned shares pay out of.
    let net_pnl = m.lp_net_pnl(start, start, true);
    let redeemed = part * (m.balances[VAULT] - net_pnl + 1) / (m.supply() + f.share_scale);
    let fee = floor_factor(redeemed, cfg.redeem_fee);
    let reward = f.market.execute_vault_order(
        &f.keeper,
        &f.seed_lp,
        &part_order,
        &report(&f, start, start, 0),
    );
    m.transfer(VAULT, MARKET, redeemed);
    let expected = m.split(fee, 0, 0, cfg.exec_fee, redeemed - fee, SEED);
    m.shares -= part;
    assert_eq!(reward, expected, "independent mid-book redeem keeper leg");
    receipt_fields(
        &f,
        "redeem_fill",
        &[
            ("shares", part),
            ("assets", redeemed),
            ("fee", fee),
            ("net_pnl", net_pnl),
        ],
    );
    m.check(&f, "mid-book redeem");
    record("lp_mid_redeem", 1);
    // The full exit prices the vault's share of pending borrowing into the
    // backing, which the vault does not hold until the traders settle. After
    // the hold that share is at least one unit, so the payout exceeds the
    // vault's tokens and the vault traps it with `PayoutExceedsAssets` (803)
    // before its token transfer. `test_share_pricing.rs` pins the same
    // refusal.
    assert!(
        m.lp_net_pnl(start, start, true) <= -1,
        "the held book must owe the vault at least one unit of borrowing"
    );
    let before = Values::take(&f.env);
    reject(
        &f,
        before,
        f.market.try_execute_vault_order(
            &f.keeper,
            &f.seed_lp,
            &lp_order,
            &report(&f, start, start, 0),
        ),
        803,
    );
    f.market.cancel_vault_order(&f.seed_lp, &lp_order);
    m.transfer(MARKET, SEED, cfg.exec_fee);
    // A 90% exit is payable but leaves the larger side's reserve (at least
    // 20k) past `max_util_withdraw` of half the remaining 10%: the reserve
    // gate (714) blocks it after the burn, and everything rolls back.
    let sized = f
        .market
        .create_vault_order(&f.seed_lp, &1, &(m.shares * 9 / 10), &0);
    m.transfer(SEED, MARKET, cfg.exec_fee);
    f.jump(cfg.redeem_lock);
    m.accrue(cfg.redeem_lock, start);
    f.market.accrue(&report(&f, start, start, 0));
    let before = Values::take(&f.env);
    reject(
        &f,
        before,
        f.market.try_execute_vault_order(
            &f.keeper,
            &f.seed_lp,
            &sized,
            &report(&f, start, start, 0),
        ),
        714,
    );
    f.market.cancel_vault_order(&f.seed_lp, &sized);
    m.transfer(MARKET, SEED, cfg.exec_fee);
    m.check(&f, "blocked LP exits");
    if mode == 3 {
        f.market.set_status(&2);
        assert_eq!(f.market.get_status(), 2);
        let before = Values::take(&f.env);
        reject(
            &f,
            before,
            f.market.try_accrue(&report(&f, start, start, 0)),
            704,
        );
        assert!(
            Snapshot::take(&f).market.notional.total() > 0,
            "frozen classification requires live positions"
        );
        record("blocked_frozen", 1);
        f.market.set_status(&0);
    }
    let mark = match mode {
        1 => start * (if byte(4) & 2 == 0 { 50 } else { 180 }) / 100,
        2 => start * 4,
        _ => start * (990 + i128::from(byte(7) % 21)) / 1000,
    };
    if mode == 4 {
        f.market.set_status(&3);
        f.jump(86_400);
        m.accrue(86_400, start);
        f.market.accrue(&report(&f, start, start, 0));
        f.market.set_terminal_price(&mark);
        record("terminal_price_success", 1);
    }
    // Cache a distinct fresh spread, then use an older start-price payload for
    // protective calls. Expected execution uses our input-selected cache.
    let spread = if mode == 4 {
        0
    } else {
        i128::from(byte(6) % 20) * mark / 10_000
    };
    let (bid, ask) = narrowed(mark - spread, mark + spread);
    if mode != 4 {
        f.jump(1);
        m.accrue(1, ask);
    }
    f.market
        .accrue(&report(&f, mark - spread, mark + spread, 0));
    let state = f
        .market
        .update_adl_state(&report(&f, mark - spread, mark + spread, 0));
    if mode == 2 {
        assert!(state.long, "generated winner must reach ADL");
        record("adl_armed", 1);
    }
    if mode == 2 {
        let amount = m.positions[0].notional / 4;
        let expected = m.partial_adl(amount, bid, ask);
        let reward = f.market.execute_adl(
            &f.keeper,
            &f.user(0),
            &true,
            &amount,
            &report(&f, start, start, 1),
        );
        assert_eq!(
            reward, expected.reward,
            "independent partial ADL keeper leg"
        );
        receipt(&f, "decrease_fill", &expected);
        m.check(&f, "partial ADL");
        record("adl_success", 1);
    }
    // User 2's deposit fills at the moved mark against capped PnL and the
    // pending borrowing, ceiled, or the deposit gate refuses it while either
    // side's trader-best profit tops `max_pnl_trader` of half the vault.
    let mut deposit_filled = false;
    {
        let price = if mode == 4 {
            Bytes::new(&f.env)
        } else {
            report(&f, mark - spread, mark + spread, 0)
        };
        let gate = floor_factor(m.balances[VAULT] / 2, cfg.max_pnl_trader);
        if (0..2).any(|side| m.side_pnl(side, bid, ask, true) > gate) {
            let before = Values::take(&f.env);
            reject(
                &f,
                before,
                f.market
                    .try_execute_vault_order(&f.keeper, &f.user(2), &deposit, &price),
                754,
            );
            record("lp_mid_deposit_refused", 1);
        } else {
            let fee = floor_factor(U, cfg.deposit_fee);
            let net_pnl = m.lp_net_pnl(bid, ask, false);
            let minted =
                (U - fee) * (m.supply() + f.share_scale) / (m.balances[VAULT] - net_pnl + 1);
            let reward = f
                .market
                .execute_vault_order(&f.keeper, &f.user(2), &deposit, &price);
            m.transfer(MARKET, VAULT, U - fee);
            let expected = m.split(fee, 0, 0, cfg.exec_fee, 0, 2);
            m.lp2_shares = minted;
            assert_eq!(reward, expected, "independent mid-book deposit keeper leg");
            receipt_fields(
                &f,
                "deposit_fill",
                &[
                    ("assets", U),
                    ("shares", minted),
                    ("fee", fee),
                    ("net_pnl", net_pnl),
                ],
            );
            assert_eq!(f.vault.balance(&f.user(2)), minted, "minted shares");
            m.check(&f, "mid-book deposit");
            record("lp_mid_deposit", 1);
            deposit_filled = true;
        }
    }
    let sides = if byte(4) & 4 == 0 {
        [1usize, 0usize]
    } else {
        [0, 1]
    };
    for side in sides {
        let mut preview = m.clone();
        let outcome = preview.close(side, bid, ask, false, 0);
        let liquidate = outcome.eligible;
        let exec = if liquidate { 0 } else { cfg.exec_fee };
        let id = if exec > 0 {
            let id = f.market.create_order(
                &f.user(side),
                &(side == 0),
                &3,
                &i128::MAX,
                &0,
                &0,
                &0,
                &f.far_expiration(),
            );
            m.transfer(side, MARKET, exec);
            Some(id)
        } else {
            None
        };
        let mut expected = m.close(side, bid, ask, liquidate, exec);
        if wrong_price {
            expected.price += 1;
        }
        record("exit_attempt", 1);
        let price = if mode == 4 {
            Bytes::new(&f.env)
        } else if liquidate {
            report(&f, start, start, 1)
        } else {
            report(&f, mark - spread, mark + spread, 0)
        };
        let reward = if liquidate {
            f.market
                .execute_liquidation(&f.keeper, &f.user(side), &(side == 0), &price)
        } else {
            let actual = if deny_valid_exit {
                Err(Ok(soroban_sdk::Error::from_contract_error(723)))
            } else {
                f.market
                    .try_execute_order(&f.keeper, &f.user(side), &id.unwrap(), &price)
            };
            mandatory_fill(actual)
        };
        assert_eq!(reward, expected.reward, "independent exit keeper leg");
        receipt(
            &f,
            if liquidate {
                "liquidation"
            } else {
                "close_fill"
            },
            &expected,
        );
        m.check(&f, "settlement");
        record("exit_success", 1);
        if liquidate {
            record("liquidation_success", 1);
        }
        if expected.bad_debt > 0 {
            record("bad_debt_checked", 1);
        }
        if m.claims[side] > 0 {
            let paid = m.claims[side].min(m.pool);
            if paid > 0 {
                assert_eq!(f.market.claim_credit(&f.user(side)), paid);
                m.claims[side] -= paid;
                m.pool -= paid;
                m.transfer(MARKET, side, paid);
                record("claim_success", 1);
            } else {
                let before = Values::take(&f.env);
                reject(&f, before, f.market.try_claim_credit(&f.user(side)), 760);
                record("blocked_unfunded_claim", 1);
            }
            m.check(&f, "receiver-first claim");
        }
    }
    for side in 0..2 {
        if m.claims[side] > 0 {
            let paid = m.claims[side];
            assert!(m.pool >= paid, "closed payers must fund every receiver");
            assert_eq!(f.market.claim_credit(&f.user(side)), paid);
            m.claims[side] = 0;
            m.pool -= paid;
            m.transfer(MARKET, side, paid);
            record("claim_success", 1);
        }
    }
    assert_eq!(
        f.market.cancel_order(&f.user(2), &pending),
        pending_margin + cfg.exec_fee
    );
    m.transfer(MARKET, 2, pending_margin + cfg.exec_fee);
    if !deposit_filled {
        f.market.cancel_vault_order(&f.user(2), &deposit);
        m.transfer(MARKET, 2, U + cfg.exec_fee);
    }
    m.check(&f, "all escrows refunded");
    retire_and_redeem(&f, &mut m);
}

fn admin_churn(f: &Fixture, m: &Model, byte: &impl Fn(usize) -> u8) {
    let treasury = TreasuryClient::new(&f.env, &f.treasury_address);
    let cfg = m.config.clone();
    // Generated valid and invalid admin/config changes have exact poststates.
    let before = Values::take(&f.env);
    reject(&f, before, treasury.try_set_rate(&(S / 2 + 1)), 900);
    let rate = i128::from(byte(6)) * S / 510;
    treasury.set_rate(&rate);
    assert_eq!(treasury.get_rate(), rate);
    treasury.set_rate(&m.treasury_rate);
    let mut changed = cfg.clone();
    changed.max_position_notional -= i128::from(byte(7)) * U;
    f.market.set_config(&changed);
    assert_eq!(
        f.market.get_config().max_position_notional,
        changed.max_position_notional
    );
    f.market.set_config(&cfg);
    let ownership = OwnershipClient::new(&f.env, &f.market_address);
    ownership.transfer_ownership(&f.user(2), &f.far_expiration());
    ownership.accept_ownership();
    assert_eq!(ownership.get_owner(), Some(f.user(2)));
    ownership.transfer_ownership(&f.owner, &f.far_expiration());
    ownership.accept_ownership();
    assert_eq!(ownership.get_owner(), Some(f.owner.clone()));
    record("admin_success", 8);
}

fn open_book(f: &Fixture, m: &mut Model, ns: [i128; 2], mode: u8, start: i128, misroute_fee: bool) {
    let cfg = m.config.clone();
    for (side, n) in ns.into_iter().enumerate() {
        let margin = if mode == 1 { n / 10 } else { n / 2 };
        let user = f.user(side);
        let id = f.market.create_order(
            &user,
            &(side == 0),
            &0,
            &n,
            &margin,
            &0,
            &0,
            &f.far_expiration(),
        );
        m.transfer(side, MARKET, margin + cfg.exec_fee);
        m.check(&f, "open escrow");
        let expected = m.open(side, n, margin, start);
        let reward = f
            .market
            .execute_order(&f.keeper, &user, &id, &report(&f, start, start, 0));
        assert_eq!(reward, expected, "independent increase keeper fee");
        if misroute_fee {
            m.balances[KEEPER] += 1;
            m.balances[TREASURY] -= 1;
        }
        m.check(&f, "open fill");
        record("open_success", 1);
    }
}

fn retire_and_redeem(f: &Fixture, m: &mut Model) {
    let treasury = TreasuryClient::new(&f.env, &f.treasury_address);
    require_flat(&f);
    Ledger::new(f.users.len()).check(&f, "mandatory terminal book");
    f.market.set_status(&4);
    m.transfer(MARKET, VAULT, m.pool);
    m.pool = 0;
    m.check(&f, "retirement surplus");
    // Retired direct redemption has no fee and includes virtual asset/share
    // rounding. The dust is explicitly accounted, not labeled unexplained.
    // The mid-book depositor exits first, then the seed takes the rest.
    if m.lp2_shares > 0 {
        let assets = m.lp2_shares * (m.balances[VAULT] + 1) / (m.supply() + f.share_scale);
        assert_eq!(
            f.market
                .create_vault_order(&f.user(2), &1, &m.lp2_shares, &assets),
            0
        );
        m.transfer(VAULT, 2, assets);
        m.lp2_shares = 0;
        m.check(&f, "mid-book depositor exit");
    }
    let assets = m.shares * (m.balances[VAULT] + 1) / (m.supply() + f.share_scale);
    assert_eq!(
        f.market
            .create_vault_order(&f.seed_lp, &1, &m.shares, &assets),
        0
    );
    m.transfer(VAULT, SEED, assets);
    m.shares = 0;
    assert_eq!(
        f.vault.total_supply(),
        0,
        "seed LP exhausted the share supply"
    );
    assert_eq!(
        f.vault.balance(&f.market_address),
        0,
        "no share escrow stranded"
    );
    m.check(&f, "final LP exit");
    record("lp_terminal_success", 1);
    let fees = m.balances[TREASURY];
    treasury.withdraw(&f.token_address, &f.owner, &fees);
    m.transfer(TREASURY, OWNER, fees);
    m.check(&f, "treasury withdrawal");
    assert_eq!(f.contract_balance(), 0, "terminal contract custody");
    record("economic_terminal", 1);
    crate::stats::flush();
}

fn mandatory_fill<E: std::fmt::Debug>(
    result: Result<Result<i128, E>, Result<soroban_sdk::Error, soroban_sdk::InvokeError>>,
) -> i128 {
    match result {
        Ok(Ok(reward)) => reward,
        other => panic!("mandatory valid exit refused: {other:?}"),
    }
}

/// A terminal check is mandatory here: non-flat state is a failure, never an
/// early return. Stress traces use a separately named nonterminal classification.
pub fn require_flat(f: &Fixture) {
    let s = Snapshot::take(f);
    assert_eq!(
        (
            s.market.notional.total(),
            s.market.tokens.total(),
            s.market.margin.total()
        ),
        (0, 0, 0),
        "mandatory terminal flatness"
    );
    for u in s.users {
        assert!(
            u.orders.is_empty() && u.vault_orders.is_empty(),
            "terminal escrow remains"
        );
        assert_eq!(u.claimable, 0, "terminal user liability");
    }
    assert_eq!(s.market.credit_owed, 0, "terminal liability total");
    record("flat_assertion_reached", 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn complete_modes_reach_required_economic_properties() {
        for mode in 0..5 {
            for order in [0, 5, 6, 7] {
                run(&[mode, 19, 55, 27, order, 20, 7, 21]);
            }
        }
    }
    #[test]
    #[should_panic(expected = "mandatory valid exit refused")]
    fn refused_valid_close_is_detected() {
        run_inner(&[0], true, false, false);
    }
    #[test]
    #[should_panic(expected = "independent actor balances")]
    fn swapped_fee_recipients_are_detected() {
        run_inner(&[0], false, true, false);
    }
    #[test]
    #[should_panic(expected = "independent execution receipt")]
    fn wrong_execution_price_is_detected() {
        run_inner(&[1], false, false, true);
    }
    #[test]
    #[should_panic(expected = "mandatory terminal flatness")]
    fn nonflat_cannot_skip_terminal_property() {
        let f = Fixture::create_testnet();
        let n = 100 * U;
        let price = 180_000_000_000_000_000;
        let id = f
            .market
            .create_order(&f.user(0), &true, &0, &n, &n, &0, &0, &f.far_expiration());
        f.market
            .execute_order(&f.keeper, &f.user(0), &id, &report(&f, price, price, 0));
        require_flat(&f);
    }
}
