//! Full runtime WASM resource gates, including six actual secp256k1 recoveries
//! in the pinned verifier. SDK 26.1.1/host26.1.4 model protocol 26 execution.
//! Limits are a protocol 27 network snapshot from 2026-09-08, NOT current RPC
//! measurements. Runtime protocol 27 measurements and archives belong to ops.
//! Testnet economics/dials come from TestnetProfile. Only the position,
//! open-interest and vault caps are enlarged.

use market::{engine::constants::MAX_ORDERS_PER_SIDE, OrderKind};
use soroban_sdk::{testutils::Ledger, Address, Bytes, Vec};
use test_suites::{chainlink_helper::hex, dependencies::verifier};
use test_suites::{test_fixture::TestFixture, SCALAR_7};

const XLM_PRICE: i128 = 180_000_000_000_000_000;

const NOTIONAL: i128 = 1_000 * SCALAR_7;
const MARGIN: i128 = 200 * SCALAR_7;
const LIQUIDATION_MARK: i128 = 140_000_000_000_000_000;
const WINNING_MARK: i128 = 198_000_000_000_000_000;
const MARGIN_PERCENT: u64 = 80;

fn fixture(fault_tolerance: u32) -> TestFixture<'static> {
    let mut f = TestFixture::create_testnet_from_wasm();
    f.set_verifier_fault_tolerance(fault_tolerance);
    f
}

/// Capture modelled resources before getters replace the last invocation.
fn assert_within_budget(
    f: &TestFixture<'_>,
    label: &str,
    verification_calls: u32,
    batch: u32,
    report: &Bytes,
) {
    let env = &f.env;
    let r = env.cost_estimate().resources();
    let limits = test_suites::resources::limits();
    assert_eq!(
        f.verified_count(),
        verification_calls,
        "{label}: real verifier calls"
    );
    let footprint = u64::from(r.disk_read_entries + r.memory_read_entries + r.write_entries);
    for (name, used, limit) in [
        (
            "instructions",
            r.instructions as u64,
            limits.instructions as u64,
        ),
        ("memory_bytes", r.mem_bytes as u64, limits.mem_bytes as u64),
        ("footprint_entries", footprint, limits.ledger_entries as u64),
        (
            "write_entries",
            u64::from(r.write_entries),
            u64::from(limits.write_entries),
        ),
        (
            "disk_read_entries",
            u64::from(r.disk_read_entries),
            u64::from(limits.disk_read_entries),
        ),
        (
            "disk_read_bytes",
            u64::from(r.disk_read_bytes),
            limits.disk_read_bytes as u64,
        ),
        (
            "write_bytes",
            u64::from(r.write_bytes),
            limits.write_bytes as u64,
        ),
        (
            "event_bytes",
            u64::from(r.contract_events_size_bytes),
            u64::from(limits.contract_events_size_bytes),
        ),
    ] {
        assert!(
            used < limit * MARGIN_PERCENT / 100,
            "{label}: {name} {used} must be below {MARGIN_PERCENT}% of {limit}"
        );
    }
    use test_suites::dependencies::{
        factory::FACTORY_WASM, market::MARKET_WASM, oracle::ORACLE_WASM, treasury::TREASURY_WASM,
        vault::VAULT_WASM,
    };
    let wasm_hashes: serde_json::Map<String, serde_json::Value> = [
        ("factory", FACTORY_WASM),
        ("market", MARKET_WASM),
        ("oracle", ORACLE_WASM),
        ("treasury", TREASURY_WASM),
        ("vault", VAULT_WASM),
    ]
    .into_iter()
    .map(|(name, bytes)| {
        (
            name.to_string(),
            hex(&env
                .crypto()
                .sha256(&Bytes::from_slice(env, bytes))
                .to_array())
            .into(),
        )
    })
    .collect();
    println!(
        "{}",
        serde_json::json!({
            "schemaVersion": 1, "scenario": label, "profile": "testnet-record-2026-09-05-capacity-x1000",
            "stage": "host_metered", "source": "soroban-sdk cost_estimate().resources()",
            "hostProtocol": env.ledger().get().protocol_version, "limitProtocol": 27,
            "limitSnapshotDate": "2026-09-08", "sdk": "26.1.1", "host": "26.1.4",
        "verifierSha256": verifier::SHA256, "fixtureFaultTolerance": f.verifier_fault_tolerance(),
            "reportSha256": hex(&env.crypto().sha256(report).to_array()),
            "profileSha256": hex(&env.crypto().sha256(&Bytes::from_slice(env, test_suites::testnet_profile::TESTNET_PROFILE_JSON.as_bytes())).to_array()),
            "wasmSha256": wasm_hashes,
            "verificationCalls": verification_calls, "batchSize": batch,
            "instructions": r.instructions, "memoryBytes": r.mem_bytes,
            "footprintEntries": footprint, "memoryReadEntries": r.memory_read_entries,
            "diskReadEntries": r.disk_read_entries, "diskReadBytes": r.disk_read_bytes,
            "writeEntries": r.write_entries, "writeBytes": r.write_bytes,
            "contractEventBytes": r.contract_events_size_bytes,
            "returnValueBytes": null, "transactionBytes": null, "instructionReserve": 0,
            "headroomPercent": 100 - MARGIN_PERCENT,
        })
    );
}

fn park(f: &TestFixture<'_>, user: &Address, count: u32) {
    for _ in 0..count {
        f.submit_order(
            user,
            true,
            OrderKind::LimitDecrease,
            10 * SCALAR_7,
            0,
            10 * XLM_PRICE,
            0,
            f.far_expiration(),
        );
    }
    assert_eq!(f.position(user, true).decrease_orders.len(), count);
}

fn assert_closed(f: &TestFixture<'_>, user: &Address, parked: &Vec<u32>) {
    let position = f.position(user, true);
    assert_eq!(position.notional, 0);
    assert!(position.decrease_orders.is_empty());
    for id in parked.iter() {
        assert!(
            f.market.try_get_order(user, &id).is_err(),
            "swept escrow order must be removed"
        );
    }
}

#[test]
fn liquidation_with_max_parked_orders_fits_tx_limits() {
    let mut f = fixture(5);
    f.seed_vault(100_000 * SCALAR_7);
    let user = f.new_user(1_000 * SCALAR_7);
    f.open(&user, true, NOTIONAL, MARGIN, &f.price(XLM_PRICE));
    park(&f, &user, MAX_ORDERS_PER_SIDE);
    let parked = f.position(&user, true).decrease_orders;
    let keeper_before = f.balance(&f.keeper);
    let price = f.price(LIQUIDATION_MARK);
    f.market
        .execute_liquidation(&f.keeper, &user, &true, &price);
    assert_within_budget(&f, "resource-max-parked-liquidation", 1, 1, &price);
    assert_closed(&f, &user, &parked);
    assert!(f.balance(&f.keeper) > keeper_before);
    assert_eq!(f.market.get_market_data().notional.long, 0);
}

#[test]
fn winning_close_with_vault_draw_and_max_sweep_fits_tx_limits() {
    let mut f = fixture(5);
    f.seed_vault(100_000 * SCALAR_7);
    let user = f.new_user(1_000 * SCALAR_7);
    f.open(&user, true, NOTIONAL, MARGIN, &f.price(XLM_PRICE));
    park(&f, &user, MAX_ORDERS_PER_SIDE - 1);
    let parked = f.position(&user, true).decrease_orders;
    f.jump(35);
    let wallet = f.balance(&user);
    let vault = f.balance(&f.vault.address);
    let price = f.price(WINNING_MARK);
    f.close(&user, true, NOTIONAL, 0, &price);
    assert_within_budget(&f, "resource-max-parked-close", 1, 1, &price);
    assert_closed(&f, &user, &parked);
    assert!(
        f.balance(&user) - wallet > MARGIN,
        "winning payout draws vault assets"
    );
    assert!(f.balance(&f.vault.address) < vault);
}

#[test]
fn adl_with_max_parked_orders_fits_tx_limits() {
    let mut f = fixture(5);
    f.seed_vault(6_000 * SCALAR_7);
    let other = f.new_user(2_000 * SCALAR_7);
    let user = f.new_user(2_000 * SCALAR_7);
    let opening = f.price(XLM_PRICE);
    f.open(&other, true, NOTIONAL, MARGIN, &opening);
    f.open(&user, true, NOTIONAL, MARGIN, &opening);
    park(&f, &user, MAX_ORDERS_PER_SIDE);
    let parked = f.position(&user, true).decrease_orders;
    f.jump(60);
    f.market.update_adl_state(&f.price(4 * XLM_PRICE));
    let wallet = f.balance(&user);
    // A distinct valid report measures cold ADL after update_adl_state's memo.
    f.env.ledger().with_mut(|l| l.timestamp += 1);
    let price = f.price(4 * XLM_PRICE);
    f.market
        .execute_adl(&f.keeper, &user, &true, &NOTIONAL, &price);
    assert_within_budget(&f, "resource-max-parked-adl", 1, 1, &price);
    assert_closed(&f, &user, &parked);
    assert!(f.balance(&user) > wallet);
    assert_eq!(f.position(&other, true).notional, NOTIONAL);
}

/// Four liquidations priced by one report, each its own call, measured per
/// call. Cold: the first call verifies the report and the later three hit the
/// oracle's verified-report cache. Cached: a prior `verify_price` warms the
/// cache, so no call verifies. Every cached call costs fewer instructions than
/// the cold first call.
#[test]
fn liquidations_measure_cold_and_cached_verification() {
    // Explicit keeper operating bound, not a protocol-enforced cap.
    const COUNT: u32 = 4;
    let mut cold_first = None;
    for cached in [false, true] {
        let mut f = fixture(5);
        f.seed_vault(100_000 * SCALAR_7);
        let mut positions = std::vec::Vec::new();
        let opening = f.price(XLM_PRICE);
        for _ in 0..COUNT {
            let user = f.new_user(1_000 * SCALAR_7);
            f.open(&user, true, NOTIONAL, MARGIN, &opening);
            park(&f, &user, MAX_ORDERS_PER_SIDE);
            let parked = f.position(&user, true).decrease_orders;
            positions.push((user, parked));
        }
        let price = f.price(LIQUIDATION_MARK);
        if cached {
            f.oracle.verify_price(&price, &f.feed_id, &true);
        }
        let mut instructions = std::vec::Vec::new();
        for (call, (user, _)) in (1..=COUNT).zip(positions.iter()) {
            let payout = f.market.execute_liquidation(&f.keeper, user, &true, &price);
            instructions.push(f.env.cost_estimate().resources().instructions);
            let verifies = !cached && call == 1;
            let label = format!(
                "resource-liquidation-{}-call-{call}",
                if verifies { "fresh" } else { "cached" }
            );
            assert_within_budget(&f, &label, u32::from(verifies), 1, &price);
            assert!(
                payout > 0,
                "each liquidation must succeed with keeper payout"
            );
        }
        // The cold run's first call is the one verifying call. Every other
        // call, in either run, hits the cache and must cost less.
        let cold = *cold_first.get_or_insert(instructions[0]);
        for (call, used) in instructions.iter().enumerate() {
            if cached || call > 0 {
                assert!(
                    *used < cold,
                    "cached call {} costs fewer instructions than the cold first call",
                    call + 1
                );
            }
        }
        for (user, parked) in positions {
            assert_closed(&f, &user, &parked);
        }
        assert_eq!(f.market.get_market_data().notional.long, 0);
    }
}

#[test]
fn maximum_verifier_signer_threshold_fits_the_resource_gate() {
    // The pinned verifier admits at most 31 signers, requires n > 3f, and checks
    // exactly f+1 signatures: f=10 and 11 recoveries are its supported maximum.
    let f = fixture(10);
    let report = f.price(XLM_PRICE);
    let price = f.oracle.verify_price(&report, &f.feed_id, &false);
    assert_within_budget(&f, "resource-verifier-maximum-signatures", 1, 1, &report);
    assert_eq!((price.bid, price.ask), (XLM_PRICE, XLM_PRICE));
}
