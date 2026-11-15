//! Signed-report integration against a pinned real verifier WASM with public,
//! test-owned signers. This establishes no provider identity or RPC receipt.

use oracle::OracleError;
use soroban_sdk::{
    contracttype,
    testutils::{storage::Temporary, Address as _, Ledger},
    Address, Bytes, BytesN, Error, IntoVal, Symbol,
};
use test_suites::chainlink_helper::{build_report, ReportTimes};
use test_suites::{
    assertions::unchanged,
    chainlink_helper::{envelope, hex, signer_addresses, CONFIG},
    dependencies::verifier,
    test_fixture::TestFixture,
};

const XLM_PRICE: i128 = 180_000_000_000_000_000;

fn body(f: &TestFixture<'_>, price: i128) -> Bytes {
    build_report(
        &f.env,
        &f.feed_id,
        ReportTimes::at(f.env.ledger().timestamp()),
        price,
        price,
        price,
    )
}

/// Oracle storage ABI, used only to inspect temporary memo expiry.
#[contracttype]
enum MemoKey {
    VerifiedReport(BytesN<32>),
}

fn memo_key(f: &TestFixture<'_>, report: &Bytes) -> MemoKey {
    MemoKey::VerifiedReport(f.env.crypto().sha256(report).into())
}

fn memo_exists(f: &TestFixture<'_>, report: &Bytes) -> bool {
    f.env.as_contract(&f.oracle.address, || {
        f.env.storage().temporary().has(&memo_key(f, report))
    })
}

fn rejects(f: &TestFixture<'_>, report: &Bytes, protective: bool, code: u32) {
    let s = &f;
    unchanged(&s.env, || {
        assert_eq!(
            s.env.try_invoke_contract::<oracle::PriceData, Error>(
                &s.oracle.address,
                &Symbol::new(&s.env, "verify_price"),
                (report, &s.feed_id, protective).into_val(&s.env),
            ),
            Err(Ok(Error::from_contract_error(code)))
        );
    });
}

#[test]
fn pinned_artifact_accepts_signed_price_with_oracle_sender_auth() {
    let f = TestFixture::create_testnet();
    let s = &f;
    assert_eq!(
        hex(&s
            .env
            .crypto()
            .sha256(&Bytes::from_slice(&s.env, verifier::WASM))
            .to_array()),
        verifier::SHA256
    );
    s.env.set_auths(&[]);
    let report = f.price(XLM_PRICE);
    let price = s.oracle.verify_price(&report, &s.feed_id, &false);
    assert_eq!(
        (price.bid, price.ask, price.publish_time),
        (XLM_PRICE, XLM_PRICE, s.env.ledger().timestamp())
    );
    assert_eq!(
        f.verified_count(),
        1,
        "cold verification must execute the real verifier"
    );
    let cached = s.oracle.verify_price(&report, &s.feed_id, &true);
    assert_eq!(cached, price);
    assert_eq!(f.verified_count(), 0, "memo hit must skip signature work");

    let spoofed_sender = Address::generate(&s.env);
    assert!(
        f.verifier().try_verify(&report, &spoofed_sender).is_err(),
        "an arbitrary sender cannot be impersonated without auth"
    );
    // This artifact authenticates the sender. It has no subscription/allowlist.
    s.env.mock_all_auths();
    assert_eq!(f.verifier().verify(&report, &spoofed_sender).len(), 288);
}

#[test]
fn signed_rejection_matrix_leaves_no_oracle_memo_or_contract_mutations() {
    let f = TestFixture::create_testnet();
    let s = &f;
    let body = body(s, XLM_PRICE);
    let wrong_config = envelope(&s.env, &body, [0x99; 32], 1, 2);
    rejects(&f, &wrong_config, false, 9); // verifier DigestNotSet
    let rogue = envelope(&s.env, &body, CONFIG, 30, 2);
    rejects(&f, &rogue, false, 12); // verifier BadVerification
    let wrong_count = envelope(&s.env, &body, CONFIG, 1, 1);
    rejects(&f, &wrong_count, false, 14);
    let ahead = f.report_at(
        ReportTimes::at(s.env.ledger().timestamp() + 11),
        XLM_PRICE,
        XLM_PRICE,
        XLM_PRICE,
    );
    for protective in [false, true] {
        rejects(&f, &ahead, protective, OracleError::PriceAhead as u32);
    }
    let signed = f.price(XLM_PRICE);
    let mut bytes = signed.to_alloc_vec();
    bytes[256 + 223] ^= 1; // authenticated benchmark word
    rejects(&f, &Bytes::from_slice(&s.env, &bytes), false, 12);
    let wrong_feed = BytesN::from_array(&s.env, &[3; 32]);
    let body = build_report(
        &s.env,
        &wrong_feed,
        ReportTimes::at(s.env.ledger().timestamp()),
        XLM_PRICE,
        XLM_PRICE,
        XLM_PRICE,
    );
    let wrong_feed_report = envelope(&s.env, &body, CONFIG, 1, 2);
    rejects(
        &f,
        &wrong_feed_report,
        false,
        OracleError::FeedMismatch as u32,
    );
    assert!(
        !memo_exists(&f, &wrong_feed_report),
        "verification succeeded but policy rejection must undo the memo"
    );
    assert!(!memo_exists(&f, &signed));
    assert_eq!(
        s.oracle.verify_price(&signed, &s.feed_id, &false).bid,
        XLM_PRICE
    );
    assert_eq!(
        f.verified_count(),
        1,
        "valid retry still performs verification"
    );
}

#[test]
fn memo_reapplies_current_feed_time_and_spread_policy() {
    let f = TestFixture::create_testnet();
    let s = &f;
    let now = s.env.ledger().timestamp();
    let body = build_report(
        &s.env,
        &s.feed_id,
        ReportTimes::at(now),
        XLM_PRICE,
        XLM_PRICE - 20,
        XLM_PRICE + 20,
    );
    let report = envelope(&s.env, &body, CONFIG, 1, 2);
    assert_eq!(
        s.oracle.verify_price(&report, &s.feed_id, &false).bid,
        XLM_PRICE - 10
    );
    s.oracle
        .update_spread_reduction_factor(&1_000_000_000_000_000_000);
    assert_eq!(
        s.oracle.verify_price(&report, &s.feed_id, &false).bid,
        XLM_PRICE
    );
    assert_eq!(f.verified_count(), 0);
    let wrong_anchor = BytesN::from_array(&s.env, &[9; 32]);
    unchanged(&s.env, || {
        assert_eq!(
            s.oracle.try_verify_price(&report, &wrong_anchor, &true),
            Err(Ok(Error::from_contract_error(
                OracleError::FeedMismatch as u32
            )))
        );
    });
    s.env.ledger().with_mut(|l| l.timestamp = now + 11);
    rejects(&f, &report, false, OracleError::PriceStale as u32);
    assert_eq!(
        s.oracle
            .verify_price(&report, &s.feed_id, &true)
            .publish_time,
        now
    );
    s.oracle.update_staleness(&3, &10);
    rejects(&f, &report, true, OracleError::PriceStale as u32);
    assert!(
        memo_exists(&f, &report),
        "policy rejection must preserve an existing memo"
    );
}

#[test]
fn revocation_horizon_includes_forward_allowance_but_never_bypasses_expiry() {
    let f = TestFixture::create_testnet();
    let s = &f;
    let start = s.env.ledger().timestamp();
    let report = f.report_at(ReportTimes::at(start + 10), XLM_PRICE, XLM_PRICE, XLM_PRICE);
    s.oracle.verify_price(&report, &s.feed_id, &false);
    f.verifier()
        .deactivate_config(&BytesN::from_array(&s.env, &CONFIG));
    s.env.ledger().with_mut(|l| l.timestamp = start + 70);
    assert_eq!(
        s.oracle.verify_price(&report, &s.feed_id, &true).bid,
        XLM_PRICE
    );
    assert_eq!(f.verified_count(), 0);
    s.env.ledger().with_mut(|l| l.timestamp = start + 71);
    rejects(&f, &report, true, OracleError::PriceStale as u32);
    // Deliberately hold ledger sequence fixed to isolate policy's time horizon.
    // The separate test below exercises actual temporary-entry expiry.
    f.verifier()
        .activate_config(&BytesN::from_array(&s.env, &CONFIG));
    let expiring = f.report_at(
        ReportTimes {
            valid_from: start + 71,
            observations: start + 71,
            expires_at: start + 72,
        },
        XLM_PRICE,
        XLM_PRICE,
        XLM_PRICE,
    );
    s.oracle.verify_price(&expiring, &s.feed_id, &false);
    s.env.ledger().with_mut(|l| l.timestamp = start + 73);
    rejects(&f, &expiring, true, OracleError::ReportExpired as u32);
}

#[test]
fn expired_memo_rechecks_deactivation_and_same_digest_signer_replacement() {
    let f = TestFixture::create_testnet();
    let s = &f;
    let report = f.price(XLM_PRICE);
    s.oracle.verify_price(&report, &s.feed_id, &false);
    let ttl = s.env.as_contract(&s.oracle.address, || {
        s.env.storage().temporary().get_ttl(&memo_key(&f, &report))
    });
    f.verifier()
        .deactivate_config(&BytesN::from_array(&s.env, &CONFIG));
    assert_eq!(
        s.oracle.verify_price(&report, &s.feed_id, &true).bid,
        XLM_PRICE
    );
    s.env.ledger().with_mut(|l| l.sequence_number += ttl + 1);
    assert!(
        !memo_exists(&f, &report),
        "expired temporary entry is discarded, not archived"
    );
    rejects(&f, &report, true, 10);
    f.verifier()
        .activate_config(&BytesN::from_array(&s.env, &CONFIG));
    s.oracle.verify_price(&report, &s.feed_id, &false);
    f.verifier().update_config(
        &BytesN::from_array(&s.env, &CONFIG),
        &signer_addresses(&s.env, 1, 4),
        &signer_addresses(&s.env, 5, 4),
        &1,
    );
    assert_eq!(
        s.oracle.verify_price(&report, &s.feed_id, &true).bid,
        XLM_PRICE,
        "same-digest signer replacement cannot invalidate an existing memo"
    );
    assert_eq!(f.verified_count(), 0);
    s.env.ledger().with_mut(|l| l.sequence_number += ttl + 1);
    rejects(&f, &report, true, 12);
    let replacement = envelope(&s.env, &body(s, XLM_PRICE), CONFIG, 5, 2);
    assert_eq!(
        s.oracle.verify_price(&replacement, &s.feed_id, &false).bid,
        XLM_PRICE
    );
    assert_eq!(f.verified_count(), 1);
}

#[test]
fn signed_rejected_market_execution_preserves_escrow_and_retries() {
    let mut f = TestFixture::create_testnet();
    f.seed_vault(100_000 * test_suites::SCALAR_7);
    let user = f.new_user(1_000 * test_suites::SCALAR_7);
    let id = f.submit_order(
        &user,
        true,
        market::OrderKind::MarketIncrease,
        100 * test_suites::SCALAR_7,
        20 * test_suites::SCALAR_7,
        0,
        0,
        f.far_expiration(),
    );
    let bad = envelope(&f.env, &body(&f, XLM_PRICE), [0x99; 32], 1, 2);
    unchanged(&f.env, || {
        assert_eq!(
            f.env.try_invoke_contract::<i128, Error>(
                &f.market.address,
                &Symbol::new(&f.env, "execute_order"),
                (&f.keeper, &user, id, &bad).into_val(&f.env)
            ),
            Err(Ok(Error::from_contract_error(9)))
        );
    });
    let signed = f.price(XLM_PRICE);
    f.execute_order(&f.keeper, &user, id, &signed);
    assert_eq!(f.verified_count(), 1);
    assert_eq!(
        f.position(&user, true).notional,
        100 * test_suites::SCALAR_7
    );
    assert!(f.market.try_get_order(&user, &id).is_err());
}

/// One fixed report anchors the shared signer and the real verifier/decoder.
/// This report was independently matched by the ops ds-sim encoder.
#[test]
fn reference_report_matches_signer_and_all_decoded_fields() {
    let f = TestFixture::create_testnet();
    let reference: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/report.json")).unwrap();
    let fields = &reference["decoded"];
    assert_eq!(
        hex(&f.price(XLM_PRICE).to_alloc_vec()),
        reference["fullReport"]
    );
    assert_eq!(hex(&CONFIG), reference["configDigest"]);
    assert_eq!(
        f.verifier_fault_tolerance(),
        reference["faultTolerance"].as_u64().unwrap() as u32
    );
    assert_eq!(
        serde_json::json!(signer_addresses(&f.env, 1, 4)
            .iter()
            .map(|a| hex(&a.to_array()))
            .collect::<std::vec::Vec<_>>()),
        reference["signers"],
    );
    let encoded = reference["fullReport"].as_str().unwrap();
    let bytes: std::vec::Vec<u8> = (0..encoded.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&encoded[i..i + 2], 16).unwrap())
        .collect();
    let verified = f
        .verifier()
        .verify(&Bytes::from_slice(&f.env, &bytes), &f.owner);
    let decoded = oracle::decode_report(&f.env, &verified);
    assert_eq!(
        serde_json::json!({
            "feedId": hex(&decoded.feed_id.to_array()),
            "validFrom": decoded.valid_from_timestamp,
            "observations": decoded.observations_timestamp,
            "nativeFee": "0", "linkFee": "0",
            "expiresAt": decoded.expires_at,
            "price": decoded.benchmark_price.to_string(),
            "bid": decoded.bid.to_string(), "ask": decoded.ask.to_string(),
        }),
        *fields
    );
    assert_eq!(decoded.native_fee, [0; 24]);
    assert_eq!(decoded.link_fee, [0; 24]);
}
