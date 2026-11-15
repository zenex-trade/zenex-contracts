//! Governed changed-code upgrade of a live market. The replacement compiles
//! current market modules plus one observable revision export. It changes no
//! schema, so no migration is appropriate. This does not promise compatibility
//! for arbitrary future storage changes or malicious owner-selected code.

use governance::{GovernanceClient, GovernanceError};
use market::{MarketError, OrderKind, VaultOrderKind};
use soroban_sdk::{
    contractclient,
    testutils::{MockAuth, MockAuthInvoke},
    xdr::{self, ToXdr},
    Address, Env, Error, Executable, IntoVal, Symbol, Val, Vec,
};
use test_suites::carry::borrowing_index;
use test_suites::test_fixture::TestFixture;
use test_suites::testnet_profile::TestnetProfile;
use test_suites::{SCALAR_18 as S, SCALAR_7 as U};

const ENTRY: i128 = S / 4;
const DELAY: u64 = 60;
const GOVERNANCE_WASM: &[u8] = include_bytes!("../../target/wasm32v1-none/release/governance.wasm");
const UPGRADE_WASM: &[u8] = include_bytes!("../../target/upgrade-fixture/upgrade_market.wasm");

#[test]
fn upgrade_fixture_adds_only_the_revision_entry_to_the_market_interface() {
    let mut market = soroban_spec::read::from_wasm(include_bytes!(
        "../../target/wasm32v1-none/release/market.wasm"
    ))
    .unwrap();
    let mut upgrade = soroban_spec::read::from_wasm(UPGRADE_WASM).unwrap();
    assert_eq!(upgrade.len(), market.len() + 1);
    upgrade.retain(|entry| {
        !matches!(entry, xdr::ScSpecEntry::FunctionV0(function)
            if function.name.to_utf8_string_lossy() == "audit_revision")
    });
    market.sort();
    upgrade.sort();
    assert_eq!(upgrade, market);
}

#[contractclient(name = "OwnableClient")]
pub trait Ownable {
    fn transfer_ownership(e: &Env, new_owner: Address, live_until_ledger: u32);
    fn get_owner(e: &Env) -> Option<Address>;
}

#[contractclient(name = "RevisionClient")]
pub trait Revision {
    fn audit_revision(e: &Env) -> u32;
}

fn auth(f: &TestFixture, who: &Address, target: &Address, method: &'static str, args: Vec<Val>) {
    f.env.mock_auths(&[MockAuth {
        address: who,
        invoke: &MockAuthInvoke {
            contract: target,
            fn_name: method,
            args,
            sub_invokes: &[],
        },
    }]);
}

fn queue(f: &TestFixture, gov: &GovernanceClient, name: &str, args: Vec<Val>) -> u32 {
    let method = Symbol::new(&f.env, name);
    auth(
        f,
        &f.owner,
        &gov.address,
        "queue",
        (&f.market.address, &method, &args).into_val(&f.env),
    );
    gov.queue(&f.market.address, &method, &args)
}

fn govern(f: &TestFixture) -> GovernanceClient<'static> {
    let id = f.env.register(GOVERNANCE_WASM, (&f.owner, DELAY));
    let gov = GovernanceClient::new(&f.env, &id);
    let ownable = OwnableClient::new(&f.env, &f.market.address);
    let until = f.far_expiration();
    auth(
        f,
        &f.owner,
        &f.market.address,
        "transfer_ownership",
        (&id, until).into_val(&f.env),
    );
    ownable.transfer_ownership(&id, &until);
    let nonce = queue(f, &gov, "accept_ownership", Vec::new(&f.env));
    f.jump(DELAY);
    f.env.mock_auths(&[]);
    gov.execute(&nonce);
    assert_eq!(ownable.get_owner(), Some(id));
    gov
}

fn ceil(n: i128, d: i128) -> i128 {
    (n + d - 1) / d
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

/// Every market-owned ledger key and value, including instance storage,
/// temporary price cache, persistent positions, orders, counters and claims.
/// Only the executable hash and host-maintained TTL are normalized.
fn market_storage(f: &TestFixture) -> std::vec::Vec<(xdr::LedgerKey, xdr::LedgerEntry)> {
    let address: xdr::ScAddress = (&f.market.address).into();
    f.env
        .to_ledger_snapshot()
        .ledger_entries
        .into_iter()
        .filter_map(|(key, (mut value, _))| {
            let xdr::LedgerEntryData::ContractData(data) = &mut value.data else {
                return None;
            };
            if data.contract != address {
                return None;
            }
            if let xdr::ScVal::ContractInstance(instance) = &mut data.val {
                instance.executable = xdr::ContractExecutable::Wasm(xdr::Hash([0; 32]));
            }
            Some((*key, *value))
        })
        .collect()
}

#[test]
fn timelocked_changed_code_upgrade_preserves_complete_live_state_and_resumes_orders_and_claims() {
    let mut f = TestFixture::create_testnet();
    let c = TestnetProfile::load().test_config();
    let gov = govern(&f);
    f.env.mock_all_auths_allowing_non_root_auth();
    let lp = f.new_user(60_000 * U);
    assert!(f.vault_deposit(&f.keeper, &lp, 50_000 * U, 0, ENTRY).filled);
    let long = f.new_user(2_000 * U);
    let short = f.new_user(2_000 * U);
    f.open(&long, true, 2_000 * U, 500 * U, &f.price(ENTRY));
    f.open(&short, false, 1_000 * U, 500 * U, &f.price(ENTRY));
    let backing = f.vault.total_assets();
    let borrow = borrowing_index(&c, 2_000 * U, backing, 600);
    let skew = S / 3;
    let saved_rate = c.funding_increase * skew / S * 600;
    let funding = saved_rate.max(c.funding_min) * 600;
    f.jump(600);
    f.market.accrue(&f.price(ENTRY));
    let data = f.market.get_market_data();
    assert_eq!(
        (data.borrowing_idx.long, data.borrowing_idx.short),
        (borrow, 0)
    );
    assert_eq!(
        (data.funding_idx.long, data.funding_idx.short),
        (funding, -2 * funding)
    );
    assert_eq!(data.funding_rate, saved_rate);
    // Receiver-first crystallization retains a real claim across upgrade.
    // The payer then backs it, including the independent rounding surplus.
    f.open(&short, false, 0, U, &f.price(ENTRY));
    let earned = 1_000 * U * (2 * funding) / S;
    assert!(earned > 0);
    assert_eq!(f.market.get_claimable_credit(&short), earned);
    assert_eq!(f.market.get_market_data().credit_pool, 0);
    f.open(&long, true, 0, U, &f.price(ENTRY));
    let paid = ceil(2_000 * U * funding, S);
    assert_eq!(f.market.get_market_data().credit_pool, paid);
    assert_eq!(f.market.get_market_data().credit_owed, earned);

    let increase = f.submit_order(
        &long,
        true,
        OrderKind::LimitIncrease,
        100 * U,
        50 * U,
        ENTRY,
        0,
        f.far_expiration(),
    );
    let decrease = f.submit_order(
        &long,
        true,
        OrderKind::LimitDecrease,
        100 * U,
        0,
        ENTRY * 2,
        0,
        f.far_expiration(),
    );
    let short_decrease = f.submit_order(
        &short,
        false,
        OrderKind::LimitDecrease,
        100 * U,
        0,
        ENTRY / 2,
        0,
        f.far_expiration(),
    );
    let deposit =
        f.market
            .create_vault_order(&lp, &(VaultOrderKind::Deposit as u32), &(123 * U + 7), &0);
    let shares = f.share_balance(&lp) / 4;
    let redeem = f
        .market
        .create_vault_order(&lp, &(VaultOrderKind::Redeem as u32), &shares, &0);
    assert_eq!(f.share_balance(&f.market.address), shares);
    let old_hash = f.market.address.executable().unwrap();
    let revision = RevisionClient::new(&f.env, &f.market.address);
    assert!(
        revision.try_audit_revision().is_err(),
        "old code has no revision export"
    );
    let replacement = f.env.deployer().upload_contract_wasm(UPGRADE_WASM);
    assert_ne!(old_hash, Executable::Wasm(replacement.clone()));
    // Neither an old-owner signature nor the owner's name as operator can
    // bypass the governance-owned upgrade entry.
    let args: Vec<Val> = (&replacement, &f.owner).into_val(&f.env);
    auth(&f, &f.owner, &f.market.address, "upgrade", args.clone());
    unchanged(&f, || {
        assert!(f
            .env
            .try_invoke_contract::<(), Error>(
                &f.market.address,
                &Symbol::new(&f.env, "upgrade"),
                args
            )
            .is_err())
    });
    let nonce = queue(
        &f,
        &gov,
        "upgrade",
        (&replacement, &gov.address).into_val(&f.env),
    );
    f.env.mock_auths(&[]);
    assert_eq!(
        gov.try_execute(&nonce),
        Err(Ok(GovernanceError::NotUnlocked.into()))
    );
    assert_eq!(f.market.address.executable(), Some(old_hash));
    f.jump(DELAY);
    let stored = market_storage(&f);
    let cash: std::vec::Vec<i128> = [
        &lp,
        &long,
        &short,
        &f.keeper,
        &f.market.address,
        &f.vault.address,
        &f.treasury.address,
    ]
    .iter()
    .map(|a| f.balance(a))
    .collect();
    let lp_shares = f.share_balance(&lp);
    // Permissionless execution invokes the real governance -> market call.
    // There is no fabricated signature authorizing the governance address.
    gov.execute(&nonce);
    assert_eq!(
        f.market.address.executable(),
        Some(Executable::Wasm(replacement))
    );
    assert_eq!(revision.audit_revision(), 2);
    assert_eq!(
        market_storage(&f),
        stored,
        "all market storage survives changed code"
    );
    assert_eq!(
        f.market.get_config().to_xdr(&f.env),
        c.clone().to_xdr(&f.env)
    );
    assert_eq!(f.market.get_claimable_credit(&short), earned);
    assert_eq!(f.share_balance(&lp), lp_shares);
    assert_eq!(f.share_balance(&f.market.address), shares);
    let after_cash: std::vec::Vec<i128> = [
        &lp,
        &long,
        &short,
        &f.keeper,
        &f.market.address,
        &f.vault.address,
        &f.treasury.address,
    ]
    .iter()
    .map(|a| f.balance(a))
    .collect();
    assert_eq!(after_cash, cash);
    assert_eq!(
        gov.try_execute(&nonce),
        Err(Ok(GovernanceError::NotQueued.into()))
    );

    // Existing claim and every escrow remain usable after the code swap.
    auth(
        &f,
        &short,
        &f.market.address,
        "claim_credit",
        (&short,).into_val(&f.env),
    );
    let wallet = f.balance(&short);
    assert_eq!(f.market.claim_credit(&short), earned);
    assert_eq!(f.balance(&short), wallet + earned);
    assert_eq!(f.market.get_market_data().credit_pool, paid - earned);
    f.env.mock_all_auths_allowing_non_root_auth();
    for (user, id) in [(&long, decrease), (&short, short_decrease)] {
        let wallet = f.balance(user);
        assert_eq!(f.market.cancel_order(user, &id), c.exec_fee);
        assert_eq!(f.balance(user), wallet + c.exec_fee);
    }
    let wallet = f.balance(&lp);
    assert_eq!(f.market.cancel_vault_order(&lp, &deposit), 123 * U + 7);
    assert_eq!(f.market.cancel_vault_order(&lp, &redeem), shares);
    assert_eq!(f.balance(&lp), wallet + 123 * U + 7 + 2 * c.exec_fee);
    assert_eq!(f.share_balance(&lp), lp_shares + shares);
    assert_eq!(f.share_balance(&f.market.address), 0);
    let long_before = f.position(&long, true);
    let backing = f.vault.total_assets();
    let expected_borrow = ceil(
        2_000 * U * borrowing_index(&c, 2_000 * U, backing, DELAY as i128),
        S,
    );
    let evolved_rate = saved_rate + c.funding_increase * skew / S * DELAY as i128;
    let expected_funding = ceil(
        2_000 * U * evolved_rate.max(c.funding_min) * DELAY as i128,
        S,
    );
    let trade_fee = ceil(100 * U * c.fee_dom, S)
        + ceil((100 * U).pow(2), c.impact_scalar).min(ceil(100 * U, 10));
    let keeper_cut = trade_fee * c.keeper_rate / S;
    let treasury_cut = trade_fee * TestnetProfile::load().treasury.rate / S
        + expected_borrow * TestnetProfile::load().treasury.rate / S;
    let keeper_before = f.balance(&f.keeper);
    let treasury_before = f.balance(&f.treasury.address);
    f.execute_order(&f.keeper, &long, increase, &f.price(ENTRY));
    assert_eq!(f.position(&long, true).notional, 2_100 * U);
    assert_eq!(
        f.position(&long, true).margin,
        long_before.margin + 50 * U - trade_fee - expected_borrow - expected_funding
    );
    assert_eq!(
        f.balance(&f.keeper) - keeper_before,
        keeper_cut + c.exec_fee
    );
    assert_eq!(
        f.balance(&f.treasury.address) - treasury_before,
        treasury_cut
    );
    assert_eq!(
        f.vault.total_assets() - backing,
        trade_fee + expected_borrow - keeper_cut - treasury_cut
    );
    assert_eq!(f.position(&short, false).notional, 1_000 * U);
    assert!(f.market.try_get_order(&long, &increase).is_err());
    assert_eq!(revision.audit_revision(), 2);
}

#[test]
fn governed_rate_update_retries_same_nonce_after_required_old_rate_accrual() {
    let mut f = TestFixture::create_testnet();
    let c = TestnetProfile::load().test_config();
    let gov = govern(&f);
    f.env.mock_all_auths_allowing_non_root_auth();
    let lp = f.new_user(60_000 * U);
    assert!(f.vault_deposit(&f.keeper, &lp, 50_000 * U, 0, ENTRY).filled);
    let user = f.new_user(1_000 * U);
    f.open(&user, true, 1_000 * U, 200 * U, &f.price(ENTRY));
    let backing = f.vault.total_assets();
    // This admin-boundary probe deliberately changes rates by one atom. It
    // does not substitute a different economic profile for the other tests.
    let mut updated = c.clone();
    updated.borrow_rate += 1;
    updated.increased_borrow_rate += 1;
    updated.funding_increase += 1;
    let nonce = queue(&f, &gov, "set_config", (&updated,).into_val(&f.env));
    f.jump(DELAY);
    f.env.mock_auths(&[]);
    unchanged(&f, || {
        assert_eq!(
            gov.try_execute(&nonce),
            Err(Ok(MarketError::MarketNotAccrued.into()))
        )
    });
    assert_eq!(
        f.market.get_config().to_xdr(&f.env),
        c.clone().to_xdr(&f.env)
    );
    let queued = gov.get_queued(&nonce).to_xdr(&f.env);
    f.market.accrue(&f.price(ENTRY));
    assert_eq!(
        f.market.get_market_data().borrowing_idx.long,
        borrowing_index(&c, 1_000 * U, backing, DELAY as i128)
    );
    assert_eq!(gov.get_queued(&nonce).to_xdr(&f.env), queued);
    gov.execute(&nonce);
    assert_eq!(f.market.get_config().to_xdr(&f.env), updated.to_xdr(&f.env));
    assert_eq!(
        gov.try_execute(&nonce),
        Err(Ok(GovernanceError::NotQueued.into()))
    );
}
