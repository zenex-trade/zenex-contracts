//! Local TTL maintenance regressions. Ledger aging observes host expiry and
//! refresh coverage. It does not prove RPC archival/restoration. In particular,
//! expired temporary caches are deleted and have no restoration transaction.

use market::{storage::DataKey, OrderKind, VaultOrderKind};
use soroban_sdk::{
    contracttype,
    testutils::{storage::Persistent, Deployer, Ledger},
    Address, Symbol, Vec,
};
use test_suites::test_fixture::TestFixture;
use test_suites::SCALAR_7;

const XLM_PRICE: i128 = 180_000_000_000_000_000;

const DAY: u32 = 17_280;

fn ttl(f: &TestFixture<'_>, key: &DataKey) -> u32 {
    f.env.as_contract(&f.market.address, || {
        f.env.storage().persistent().get_ttl(key)
    })
}

fn instance_and_code(f: &TestFixture<'_>, address: &Address) -> (u32, u32) {
    (
        f.env.deployer().get_contract_instance_ttl(address),
        f.env.deployer().get_contract_code_ttl(address),
    )
}

#[test]
fn market_reads_refresh_position_and_order_but_not_claim_or_counter() {
    let mut f = TestFixture::create_testnet_from_wasm();
    f.seed_vault(100_000 * SCALAR_7);
    let user = f.new_user(1_000 * SCALAR_7);
    f.open(
        &user,
        true,
        100 * SCALAR_7,
        20 * SCALAR_7,
        &f.price(XLM_PRICE),
    );
    let id = f.submit_order(
        &user,
        true,
        OrderKind::LimitDecrease,
        10 * SCALAR_7,
        0,
        2 * XLM_PRICE,
        0,
        f.env.ledger().sequence() + 2_000_000,
    );
    let vault_id = f.market.create_vault_order(
        &user,
        &(VaultOrderKind::Deposit as u32),
        &(10 * SCALAR_7),
        &0,
    );
    let claimant = f.new_user(1_000 * SCALAR_7);
    f.open(
        &claimant,
        true,
        100 * SCALAR_7,
        20 * SCALAR_7,
        &f.price(XLM_PRICE),
    );
    let close_id = f.submit_order(
        &claimant,
        true,
        OrderKind::MarketDecrease,
        100 * SCALAR_7,
        0,
        0,
        0,
        f.far_expiration(),
    );
    f.jump(35);
    f.token.set_authorized(&claimant, &false);
    f.execute_order(&f.keeper, &claimant, close_id, &f.price(XLM_PRICE));
    let credit = f.market.get_claimable_credit(&claimant);
    assert!(credit > 0, "real rejected payout creates the claim record");
    f.token.set_authorized(&claimant, &true);

    let position = DataKey::Position(user.clone(), true);
    let order = DataKey::Order(user.clone(), id);
    let vault_order = DataKey::VaultOrder(user.clone(), vault_id);
    let counter = DataKey::OrderCounter(user.clone());
    let claim = DataKey::ClaimableCredit(claimant.clone());
    let keys = [&position, &order, &vault_order, &counter, &claim];
    let before: std::vec::Vec<u32> = keys.iter().map(|k| ttl(&f, k)).collect();
    f.env.ledger().with_mut(|l| l.sequence_number += 21 * DAY);
    for (i, key) in keys.iter().enumerate() {
        assert_eq!(ttl(&f, key), before[i] - 21 * DAY);
    }
    // Timestamp is deliberately fixed here: isolate lease coverage from fees,
    // order validity, and price policy. Real time/protocol expiry is ops-owned.
    assert_eq!(f.position(&user, true).notional, 100 * SCALAR_7);
    f.market.get_order(&user, &id);
    f.market.get_vault_order(&user, &vault_id);
    assert_eq!(f.market.get_claimable_credit(&claimant), credit);
    let next_id = f.market.get_order_counter(&user);
    for key in [&position, &order, &vault_order] {
        assert_eq!(ttl(&f, key), 120 * DAY);
    }
    assert_eq!(
        ttl(&f, &counter),
        before[3] - 21 * DAY,
        "counter view is not maintenance"
    );
    assert_eq!(
        ttl(&f, &claim),
        before[4] - 21 * DAY,
        "claim view is not maintenance"
    );
    let allocated = f.submit_order(
        &user,
        true,
        OrderKind::LimitIncrease,
        10 * SCALAR_7,
        5 * SCALAR_7,
        XLM_PRICE / 2,
        0,
        f.far_expiration(),
    );
    assert_eq!(allocated, next_id);
    assert_eq!(
        ttl(&f, &counter),
        120 * DAY,
        "allocation refreshes the counter"
    );
    assert_eq!(f.market.claim_credit(&claimant), credit);
    assert_eq!(f.market.get_claimable_credit(&claimant), 0);
    assert_eq!(
        ttl(&f, &claim),
        120 * DAY,
        "claim mutation refreshes its zero balance"
    );
}

#[test]
fn active_market_does_not_refresh_quiet_vault_or_verifier_on_memo_hits() {
    let f = TestFixture::create_testnet_from_wasm();
    f.seed_vault(100_000 * SCALAR_7);
    let report = f.price(XLM_PRICE);
    f.market.accrue(&report);
    let market_before = instance_and_code(&f, &f.market.address);
    let vault_before = instance_and_code(&f, &f.vault.address);
    let verifier_before = instance_and_code(&f, &f.chainlink_verifier);
    assert_eq!(market_before, (31 * DAY, 31 * DAY));
    assert_eq!(vault_before, (31 * DAY, 31 * DAY));
    // The real verifier has a 90-day maintenance
    // target. Cache hits never call it and therefore never extend its lease.
    assert_eq!(verifier_before, (90 * DAY, 90 * DAY));
    f.env.ledger().with_mut(|l| l.sequence_number += 2 * DAY);
    let fresh = f.price(XLM_PRICE); // same timestamp, but expired memo => miss
    f.market.accrue(&fresh);
    assert_eq!(
        f.verified_count(),
        1,
        "expired memo must execute the verifier"
    );
    assert_eq!(
        instance_and_code(&f, &f.market.address),
        (31 * DAY, 31 * DAY)
    );
    assert_eq!(
        instance_and_code(&f, &f.vault.address),
        (29 * DAY, 29 * DAY),
        "vault views do not maintain code/instance"
    );
    assert_eq!(
        instance_and_code(&f, &f.chainlink_verifier),
        (88 * DAY, 88 * DAY),
        "above-threshold verifier has no bump yet"
    );
    f.env.ledger().with_mut(|l| l.sequence_number += 1);
    f.market.accrue(&fresh);
    assert_eq!(f.verified_count(), 0);
    assert_eq!(
        instance_and_code(&f, &f.chainlink_verifier),
        (88 * DAY - 1, 88 * DAY - 1)
    );
    assert_eq!(
        instance_and_code(&f, &f.vault.address),
        (29 * DAY - 1, 29 * DAY - 1)
    );
}

/// Private storage ABI keys are reproduced only for lease inspection.
#[contracttype]
enum QueueKey {
    Queued(u32),
}
#[contracttype]
enum FactoryKey {
    Pools(Address),
}

#[test]
fn queue_and_registry_reads_have_distinct_instance_maintenance_coverage() {
    let f = TestFixture::create_testnet_from_wasm();
    let env = &f.env;
    let address = env.register(
        test_suites::dependencies::governance::GOVERNANCE_WASM,
        (&f.owner, 60u64),
    );
    let gov = governance::GovernanceClient::new(env, &address);
    let nonce = gov.queue(
        &f.oracle.address,
        &Symbol::new(env, "trade_staleness"),
        &Vec::new(env),
    );
    let queued = gov.get_queued(&nonce);
    env.ledger().with_mut(|l| l.sequence_number += 21 * DAY);
    assert_eq!(gov.get_delay(), 60);
    assert_eq!(gov.get_queued(&nonce), queued);
    assert_eq!(
        instance_and_code(&f, &address),
        (40 * DAY, 40 * DAY),
        "queue views do not extend instance or code"
    );
    assert_eq!(
        env.as_contract(&address, || env
            .storage()
            .persistent()
            .get_ttl(&QueueKey::Queued(nonce))),
        120 * DAY
    );
    assert!(f.factory.is_deployed(&f.market.address));
    assert_eq!(
        env.as_contract(&f.factory.address, || env
            .storage()
            .persistent()
            .get_ttl(&FactoryKey::Pools(f.market.address.clone()))),
        120 * DAY
    );
    assert_eq!(
        env.deployer().get_contract_instance_ttl(&f.factory.address),
        31 * DAY,
        "factory registry lookup extends its own instance too"
    );
    gov.queue(
        &f.oracle.address,
        &Symbol::new(env, "close_staleness"),
        &Vec::new(env),
    );
    assert_eq!(
        instance_and_code(&f, &address),
        (61 * DAY, 61 * DAY),
        "queue mutation maintains governance"
    );
}
