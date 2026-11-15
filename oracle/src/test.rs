#![cfg(test)]

//! Oracle unit tests.
//!
//! The Chainlink verifier is mocked to hand back the report body verbatim —
//! signature verification is Chainlink's contract's job (covered E2E against
//! the deployed testnet verifier), so these tests feed decoded bodies and
//! exercise everything that is ours: decoding, the gates, and the spread
//! reduction.
//!
//! The primary vector is a live XLM/USD V3 report body fetched from the
//! production Data Streams API on 2026-08-11 (feed
//! `0x000358cb…5e9a`, observed 1786461747): price 0.160118639640561900,
//! bid 0.160075837793266000, ask 0.160157805133230400, `expiresAt`
//! +30 days.

use soroban_sdk::testutils::{Address as _, Events as _, Ledger as _};
use soroban_sdk::{
    contract, contractimpl, map, vec, Address, Bytes, BytesN, Env, IntoVal, Map, Symbol, Val,
};

use crate::constants::SCALAR_18;
use crate::{Oracle, OracleClient, OracleError, PriceData};

/// The live XLM/USD report body (nine ABI words, 288 bytes).
const XLM_BODY: [u8; 288] = hex_literal::hex!(
    "000358cb12b1f5bbeca8b5b4666025a40b15520af1f82516ee2fb9a335055e9a"
    "000000000000000000000000000000000000000000000000000000006a7b3e33"
    "000000000000000000000000000000000000000000000000000000006a7b3e33"
    "00000000000000000000000000000000000000000000000000009b3d178a78cc"
    "00000000000000000000000000000000000000000000000000847b434707acfe"
    "000000000000000000000000000000000000000000000000000000006aa2cb33"
    "0000000000000000000000000000000000000000000000000238db0dedb1c8ec"
    "0000000000000000000000000000000000000000000000000238b42058ca6950"
    "0000000000000000000000000000000000000000000000000238feacdaf8f140"
);

const XLM_FEED: [u8; 32] =
    hex_literal::hex!("000358cb12b1f5bbeca8b5b4666025a40b15520af1f82516ee2fb9a335055e9a");

const OBSERVED_AT: u64 = 1786461747; // seconds
const EXPIRES_AT: u64 = 1789053747; // seconds, +30 days
const BID: i128 = 160075837793266000;
const ASK: i128 = 160157805133230400;

/// Mock Chainlink verifier: returns the "signed report" bytes verbatim as
/// the verified body (so tests submit bodies directly), records the call's
/// arguments for the interaction tests, counts its calls for the memo
/// tests, and rejects every report while `set_reject(true)` holds, as the
/// real one does for a deactivated or unregistered DON config.
#[contract]
struct MockVerifier;

#[contractimpl]
impl MockVerifier {
    pub fn verify(env: Env, signed_report: Bytes, sender: Address) -> Bytes {
        let calls = Self::call_count(env.clone()) + 1;
        env.storage()
            .instance()
            .set(&soroban_sdk::symbol_short!("calls"), &calls);
        if env
            .storage()
            .instance()
            .get(&soroban_sdk::symbol_short!("reject"))
            .unwrap_or(false)
        {
            soroban_sdk::panic_with_error!(&env, VerifierError::DigestInactive)
        }
        env.storage()
            .instance()
            .set(&soroban_sdk::symbol_short!("report"), &signed_report);
        env.storage()
            .instance()
            .set(&soroban_sdk::symbol_short!("sender"), &sender);
        signed_report
    }

    pub fn call_count(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&soroban_sdk::symbol_short!("calls"))
            .unwrap_or(0)
    }

    pub fn set_reject(env: Env, reject: bool) {
        env.storage()
            .instance()
            .set(&soroban_sdk::symbol_short!("reject"), &reject);
    }

    pub fn last_report(env: Env) -> Bytes {
        env.storage()
            .instance()
            .get(&soroban_sdk::symbol_short!("report"))
            .unwrap()
    }

    pub fn last_sender(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&soroban_sdk::symbol_short!("sender"))
            .unwrap()
    }
}

/// The verifier's rejection surface, from its deployed error table.
#[soroban_sdk::contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
enum VerifierError {
    DigestInactive = 10,
}

struct Fixture {
    env: Env,
    oracle: OracleClient<'static>,
    oracle_address: Address,
    verifier: Address,
    owner: Address,
}

impl Fixture {
    /// A client for the fixture's mock verifier.
    fn mock(&self) -> MockVerifierClient<'static> {
        MockVerifierClient::new(&self.env, &self.verifier)
    }
}

/// The fixture's staleness windows (seconds): strict trade window at its
/// 15s ceiling, protective close window at the 60s operational target.
const TRADE_STALENESS: u64 = 15;
const CLOSE_STALENESS: u64 = 60;

/// Deploy the mock verifier and an oracle at `trade_staleness` 15 /
/// `close_staleness` 60 (seconds), spread reduction `factor`, with the
/// ledger clock at the vector's observation time.
fn fixture(factor: i128) -> Fixture {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = OBSERVED_AT);
    let owner = Address::generate(&env);
    let verifier = env.register(MockVerifier, ());
    let oracle = env.register(
        Oracle,
        (
            owner.clone(),
            verifier.clone(),
            TRADE_STALENESS,
            CLOSE_STALENESS,
            factor,
        ),
    );
    Fixture {
        oracle: OracleClient::new(&env, &oracle),
        oracle_address: oracle,
        verifier,
        env,
        owner,
    }
}

fn body(env: &Env) -> Bytes {
    Bytes::from_array(env, &XLM_BODY)
}

/// The vector body with word `index` overwritten.
fn body_with_word(env: &Env, index: usize, word: [u8; 32]) -> Bytes {
    let mut arr = XLM_BODY;
    arr[index * 32..(index + 1) * 32].copy_from_slice(&word);
    Bytes::from_array(env, &arr)
}

/// `base` with word `index` overwritten.
fn body_with_word_of(env: &Env, base: &Bytes, index: u32, word: [u8; 32]) -> Bytes {
    let mut out = base.clone();
    for (i, b) in word.iter().enumerate() {
        out.set(index * 32 + i as u32, *b);
    }
    let _ = env;
    out
}

/// A right-aligned u64 word (the uint32 fields' encoding).
fn u64_word(value: u64) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..32].copy_from_slice(&value.to_be_bytes());
    w
}

/// A sign-extended i128 word (the int192 fields' encoding).
fn i128_word(value: i128) -> [u8; 32] {
    let mut w = if value < 0 { [0xff_u8; 32] } else { [0u8; 32] };
    w[16..32].copy_from_slice(&value.to_be_bytes());
    w
}

fn feed(env: &Env) -> BytesN<32> {
    BytesN::from_array(env, &XLM_FEED)
}

// --- the live vector ---

#[test]
fn live_report_decodes_and_passes_every_gate() {
    let f = fixture(0);
    let price = f.oracle.verify_price(&body(&f.env), &feed(&f.env), &false);
    assert_eq!(
        price,
        PriceData {
            bid: BID,
            ask: ASK,
            publish_time: OBSERVED_AT,
        }
    );
}

// --- the verifier interaction ---

#[test]
fn verifier_receives_the_report_verbatim_and_our_own_address_as_sender() {
    let f = fixture(0);
    f.oracle.verify_price(&body(&f.env), &feed(&f.env), &false);
    let mock = f.mock();
    assert_eq!(
        mock.last_report(),
        body(&f.env),
        "the blob passes through untouched"
    );
    assert_eq!(
        mock.last_sender(),
        f.oracle_address,
        "the oracle authorizes itself as the verifier's sender",
    );
}

#[test]
#[should_panic(expected = "#10")]
fn rejecting_verifier_fails_the_whole_call() {
    let f = fixture(0);
    f.mock().set_reject(&true);
    f.oracle.verify_price(&body(&f.env), &feed(&f.env), &false);
}

// --- the verified report memo ---

#[test]
fn an_identical_report_verified_twice_calls_the_verifier_once() {
    let f = fixture(0);
    let first = f.oracle.verify_price(&body(&f.env), &feed(&f.env), &false);
    let second = f.oracle.verify_price(&body(&f.env), &feed(&f.env), &false);
    assert_eq!(f.mock().call_count(), 1, "the second call is a memo hit");
    assert_eq!(first, second, "both paths return the same price");
}

#[test]
fn reports_that_differ_in_one_byte_each_call_the_verifier() {
    let f = fixture(0);
    f.oracle.verify_price(&body(&f.env), &feed(&f.env), &false);
    // Word 6 is the benchmark, which no gate reads, so the second report is
    // as valid as the first and differs from it in one byte only.
    let mut other = body(&f.env);
    other.set(6 * 32 + 31, other.get(6 * 32 + 31).unwrap() ^ 1);
    f.oracle.verify_price(&other, &feed(&f.env), &false);
    assert_eq!(f.mock().call_count(), 2, "a different digest is a miss");
}

#[test]
fn the_gates_run_on_a_memo_hit() {
    let f = fixture(0);
    f.oracle.verify_price(&body(&f.env), &feed(&f.env), &false);
    f.env
        .ledger()
        .with_mut(|l| l.timestamp = OBSERVED_AT + CLOSE_STALENESS + 1);
    assert_eq!(
        f.oracle
            .try_verify_price(&body(&f.env), &feed(&f.env), &false),
        Err(Ok(OracleError::PriceStale.into())),
        "strict class rejects the memoised report once it is stale",
    );
    assert_eq!(
        f.oracle
            .try_verify_price(&body(&f.env), &feed(&f.env), &true),
        Err(Ok(OracleError::PriceStale.into())),
        "protective class rejects it too",
    );
    assert_eq!(
        f.mock().call_count(),
        1,
        "neither rejection reached the verifier"
    );
}

#[test]
fn the_class_is_not_part_of_the_memo_key() {
    let f = fixture(0);
    f.oracle.verify_price(&body(&f.env), &feed(&f.env), &false);
    f.oracle.verify_price(&body(&f.env), &feed(&f.env), &true);
    assert_eq!(
        f.mock().call_count(),
        1,
        "a strict miss serves a protective hit"
    );

    let g = fixture(0);
    g.oracle.verify_price(&body(&g.env), &feed(&g.env), &true);
    g.oracle.verify_price(&body(&g.env), &feed(&g.env), &false);
    assert_eq!(
        g.mock().call_count(),
        1,
        "a protective miss serves a strict hit"
    );
}

#[test]
fn a_rejected_report_leaves_no_memo() {
    let f = fixture(0);
    f.mock().set_reject(&true);
    assert!(
        f.oracle
            .try_verify_price(&body(&f.env), &feed(&f.env), &false)
            .is_err(),
        "the verifier's rejection fails the call",
    );
    // The rejected call reverted the mock's own counter too.
    assert_eq!(f.mock().call_count(), 0);

    f.mock().set_reject(&false);
    let price = f.oracle.verify_price(&body(&f.env), &feed(&f.env), &false);
    assert_eq!(
        f.mock().call_count(),
        1,
        "the same bytes reach the verifier"
    );
    assert_eq!(price.publish_time, OBSERVED_AT);
}

#[test]
fn a_lapsed_memo_entry_calls_the_verifier_again() {
    let f = fixture(0);
    f.env.ledger().set_min_temp_entry_ttl(16);
    f.oracle.verify_price(&body(&f.env), &feed(&f.env), &false);
    // Past the entry's minimum lifetime, but inside every persistent TTL, so
    // only the temporary tier lapses.
    f.env.ledger().set_sequence_number(1_000);
    f.oracle.verify_price(&body(&f.env), &feed(&f.env), &false);
    assert_eq!(f.mock().call_count(), 2, "the lapsed entry is a miss");
}

// --- feed identity ---

#[test]
#[should_panic(expected = "#790")]
fn wrong_feed_anchor_rejects() {
    let f = fixture(0);
    let other = BytesN::from_array(&f.env, &[7u8; 32]);
    f.oracle.verify_price(&body(&f.env), &other, &false);
}

// --- freshness ---

#[test]
fn staleness_boundary_passes_at_exactly_the_window() {
    let f = fixture(0);
    f.env
        .ledger()
        .with_mut(|l| l.timestamp = OBSERVED_AT + TRADE_STALENESS);
    f.oracle.verify_price(&body(&f.env), &feed(&f.env), &false);
}

#[test]
#[should_panic(expected = "#782")]
fn observation_older_than_the_window_rejects_stale() {
    let f = fixture(0);
    f.env
        .ledger()
        .with_mut(|l| l.timestamp = OBSERVED_AT + TRADE_STALENESS + 1);
    f.oracle.verify_price(&body(&f.env), &feed(&f.env), &false);
}

// --- the per-class split: one report, two verdicts ---

#[test]
fn between_the_windows_the_protective_class_accepts_what_the_strict_rejects() {
    // The same report, the same ledger instant: aged past the trade window
    // but inside the close window, only the class decides.
    let f = fixture(0);
    f.env
        .ledger()
        .with_mut(|l| l.timestamp = OBSERVED_AT + TRADE_STALENESS + 1);
    assert!(
        f.oracle
            .try_verify_price(&body(&f.env), &feed(&f.env), &false)
            .is_err(),
        "strict class rejects between the windows",
    );
    let price = f.oracle.verify_price(&body(&f.env), &feed(&f.env), &true);
    assert_eq!(
        price.publish_time, OBSERVED_AT,
        "protective class accepts the same report"
    );
}

#[test]
fn protective_boundary_passes_at_exactly_the_close_window() {
    let f = fixture(0);
    f.env
        .ledger()
        .with_mut(|l| l.timestamp = OBSERVED_AT + CLOSE_STALENESS);
    f.oracle.verify_price(&body(&f.env), &feed(&f.env), &true);
}

#[test]
#[should_panic(expected = "#782")]
fn observation_older_than_the_close_window_rejects_stale() {
    let f = fixture(0);
    f.env
        .ledger()
        .with_mut(|l| l.timestamp = OBSERVED_AT + CLOSE_STALENESS + 1);
    f.oracle.verify_price(&body(&f.env), &feed(&f.env), &true);
}

#[test]
fn protective_class_does_not_widen_the_forward_allowance() {
    // Only the past side widens with the class. The forward allowance is
    // the trade window for both: a wide forward window would let a
    // protective call cache a future-stamped price that no strict call
    // could produce, and cache substitution would then feed it to fills.
    let f = fixture(0);
    let b = body_with_word(&f.env, 1, u64_word(OBSERVED_AT - 100));
    f.env
        .ledger()
        .with_mut(|l| l.timestamp = OBSERVED_AT - TRADE_STALENESS - 1);
    assert!(
        f.oracle
            .try_verify_price(&b, &feed(&f.env), &false)
            .is_err(),
        "strict class rejects ahead",
    );
    assert!(
        f.oracle.try_verify_price(&b, &feed(&f.env), &true).is_err(),
        "protective class rejects the same forward skew",
    );
    // At the trade allowance exactly, both classes accept.
    f.env
        .ledger()
        .with_mut(|l| l.timestamp = OBSERVED_AT - TRADE_STALENESS);
    f.oracle.verify_price(&b, &feed(&f.env), &false);
    f.oracle.verify_price(&b, &feed(&f.env), &true);
}

#[test]
#[should_panic(expected = "#793")]
fn protective_validity_window_keeps_the_strict_forward_allowance() {
    // The `validFrom` gate is forward-only and shares the pinned allowance.
    let f = fixture(0);
    let b = body_with_word(&f.env, 1, u64_word(OBSERVED_AT + TRADE_STALENESS + 1));
    f.oracle.verify_price(&b, &feed(&f.env), &true);
}

#[test]
#[should_panic(expected = "#793")]
fn observation_past_the_forward_window_rejects_ahead() {
    let f = fixture(0);
    // validFrom would also be in the future; move it behind the clock so the
    // two-sided staleness bound is the gate under test.
    let b = body_with_word(&f.env, 1, u64_word(OBSERVED_AT - 100));
    f.env.ledger().with_mut(|l| l.timestamp = OBSERVED_AT - 16);
    f.oracle.verify_price(&b, &feed(&f.env), &false);
}

#[test]
#[should_panic(expected = "#793")]
fn validity_window_past_the_forward_allowance_rejects_ahead() {
    let f = fixture(0);
    let b = body_with_word(&f.env, 1, u64_word(OBSERVED_AT + 16));
    f.oracle.verify_price(&b, &feed(&f.env), &false);
}

#[test]
fn validity_window_inside_the_forward_allowance_passes() {
    // The ledger clock trails real time; a fresh report's window may open
    // slightly ahead of it.
    let f = fixture(0);
    let b = body_with_word(&f.env, 1, u64_word(OBSERVED_AT + 5));
    f.oracle.verify_price(&b, &feed(&f.env), &false);
}

#[test]
#[should_panic(expected = "#784")]
fn report_past_its_own_expiry_rejects_expired() {
    let f = fixture(0);
    f.env.ledger().with_mut(|l| l.timestamp = EXPIRES_AT + 1);
    f.oracle.verify_price(&body(&f.env), &feed(&f.env), &false);
}

// --- body sanity ---

#[test]
#[should_panic(expected = "#780")]
fn v2_schema_body_rejects_on_length() {
    // A V2 stream's body (no bid/ask) is seven words; the strict V3 length
    // gate refuses any other schema.
    let f = fixture(0);
    let b = body(&f.env).slice(0..224);
    f.oracle.verify_price(&b, &feed(&f.env), &false);
}

#[test]
#[should_panic(expected = "#780")]
fn non_v3_stream_id_rejects() {
    // Schema is the id's first two bytes; a non-0x0003 stream cannot be a
    // V3 report even if the caller anchors on it.
    let f = fixture(0);
    let mut raw = XLM_BODY;
    raw[1] = 0x02; // 0x0002…: V2 stream id
    let mut anchor = XLM_FEED;
    anchor[1] = 0x02;
    let b = Bytes::from_array(&f.env, &raw);
    f.oracle
        .verify_price(&b, &BytesN::from_array(&f.env, &anchor), &false);
}

#[test]
#[should_panic(expected = "#780")]
fn truncated_body_rejects() {
    let f = fixture(0);
    let b = body(&f.env).slice(0..287);
    f.oracle.verify_price(&b, &feed(&f.env), &false);
}

#[test]
#[should_panic(expected = "#780")]
fn nonzero_padding_in_a_timestamp_word_rejects() {
    let f = fixture(0);
    let mut w = u64_word(OBSERVED_AT);
    w[0] = 1;
    let b = body_with_word(&f.env, 2, w);
    f.oracle.verify_price(&b, &feed(&f.env), &false);
}

#[test]
#[should_panic(expected = "#781")]
fn crossed_market_rejects() {
    let f = fixture(0);
    // bid above ask.
    let b = body_with_word(&f.env, 7, i128_word(ASK + 1));
    f.oracle.verify_price(&b, &feed(&f.env), &false);
}

#[test]
#[should_panic(expected = "#781")]
fn non_positive_price_side_rejects() {
    let f = fixture(0);
    let b = body_with_word(&f.env, 7, i128_word(0));
    f.oracle.verify_price(&b, &feed(&f.env), &false);
}

#[test]
#[should_panic(expected = "#781")]
fn negative_benchmark_rejects() {
    let f = fixture(0);
    let b = body_with_word(&f.env, 6, i128_word(-1));
    f.oracle.verify_price(&b, &feed(&f.env), &false);
}

#[test]
#[should_panic(expected = "#781")]
fn int192_overflowing_i128_rejects() {
    let f = fixture(0);
    // A positive value with bit 130 set: valid int192, too wide for i128.
    let mut w = [0u8; 32];
    w[15] = 0x04;
    let b = body_with_word(&f.env, 8, w);
    f.oracle.verify_price(&b, &feed(&f.env), &false);
}

// --- spread reduction ---

#[test]
fn half_spread_reduction_narrows_symmetrically_toward_the_mid() {
    let f = fixture(SCALAR_18 / 2);
    let price = f.oracle.verify_price(&body(&f.env), &feed(&f.env), &false);
    let cut = (ASK - BID) / 2 / 2;
    assert_eq!(price.bid, BID + cut);
    assert_eq!(price.ask, ASK - cut);
}

#[test]
fn full_spread_reduction_collapses_onto_the_mid() {
    let f = fixture(SCALAR_18);
    let price = f.oracle.verify_price(&body(&f.env), &feed(&f.env), &false);
    let mid = BID + (ASK - BID) / 2;
    assert_eq!(price.bid, mid);
    assert_eq!(price.ask, mid);
}

#[test]
fn spread_reduction_ignores_the_benchmark() {
    // The reduction is symmetric about the bid/ask midpoint; the
    // benchmark's position cannot skew it.
    let f = fixture(SCALAR_18);
    let b = body_with_word(&f.env, 6, i128_word(ASK - 1)); // benchmark near ask
    let price = f.oracle.verify_price(&b, &feed(&f.env), &false);
    let mid = BID + (ASK - BID) / 2;
    assert_eq!(price.bid, mid);
    assert_eq!(price.ask, mid);
}

#[test]
fn spread_reduction_survives_btc_scale_prices() {
    // half_spread * factor overflows i128 at large 18-dec prices; the I256
    // path must not. $100k price, 1% spread.
    let f = fixture(SCALAR_18 / 2);
    let price_18 = 100_000 * SCALAR_18;
    let spread = price_18 / 100;
    let bid = price_18 - spread / 2;
    let ask = price_18 + spread / 2;
    let b0 = body_with_word(&f.env, 6, i128_word(price_18));
    let b1 = body_with_word_of(&f.env, &b0, 7, i128_word(bid));
    let b = body_with_word_of(&f.env, &b1, 8, i128_word(ask));
    let price = f.oracle.verify_price(&b, &feed(&f.env), &false);
    let cut = (ask - bid) / 2 / 2;
    assert_eq!(price.bid, bid + cut);
    assert_eq!(price.ask, ask - cut);
}

// --- config bounds ---

#[test]
#[should_panic(expected = "#783")]
fn trade_staleness_below_the_floor_rejects() {
    let f = fixture(0);
    f.oracle.update_staleness(&2, &60);
}

#[test]
fn staleness_pair_at_the_floor_is_accepted() {
    // Both windows may collapse onto the degenerate floor: the ordering
    // constraint is `trade <= close`, equality included.
    let f = fixture(0);
    f.oracle.update_staleness(&3, &3);
    assert_eq!(f.oracle.trade_staleness(), 3);
    assert_eq!(f.oracle.close_staleness(), 3);
}

#[test]
#[should_panic(expected = "#783")]
fn trade_staleness_above_its_cap_rejects() {
    let f = fixture(0);
    f.oracle.update_staleness(&16, &60);
}

#[test]
#[should_panic(expected = "#783")]
fn close_staleness_above_its_cap_rejects() {
    let f = fixture(0);
    f.oracle.update_staleness(&15, &121);
}

#[test]
#[should_panic(expected = "#783")]
fn close_window_narrower_than_the_trade_window_rejects() {
    let f = fixture(0);
    f.oracle.update_staleness(&10, &9);
}

#[test]
fn staleness_pair_at_both_ceilings_is_accepted() {
    let f = fixture(0);
    f.oracle.update_staleness(&15, &120);
    assert_eq!(f.oracle.trade_staleness(), 15);
    assert_eq!(f.oracle.close_staleness(), 120);
}

#[test]
#[should_panic(expected = "#783")]
fn constructor_rejects_an_invalid_pair() {
    let env = Env::default();
    let owner = Address::generate(&env);
    let verifier = env.register(MockVerifier, ());
    env.register(Oracle, (owner, verifier, 10_u64, 9_u64, 0_i128));
}

#[test]
#[should_panic(expected = "Auth, InvalidAction")]
fn non_owner_cannot_update_config() {
    let f = fixture(0);
    f.env.set_auths(&[]);
    f.oracle.update_staleness(&10, &60);
}

#[test]
#[should_panic(expected = "#785")]
fn spread_reduction_above_one_rejects() {
    let f = fixture(0);
    f.oracle.update_spread_reduction_factor(&(SCALAR_18 + 1));
}

#[test]
#[should_panic(expected = "#785")]
fn negative_spread_reduction_rejects() {
    let f = fixture(0);
    f.oracle.update_spread_reduction_factor(&-1);
}

#[test]
fn updating_the_staleness_pair_attests_the_new_windows() {
    let f = fixture(0);
    f.oracle.update_staleness(&10, &90);

    let staleness_data: Map<Symbol, Val> = map![
        &f.env,
        (
            Symbol::new(&f.env, "trade_staleness"),
            10_u64.into_val(&f.env)
        ),
        (
            Symbol::new(&f.env, "close_staleness"),
            90_u64.into_val(&f.env)
        ),
    ];
    assert_eq!(
        f.env.events().all().filter_by_contract(&f.oracle_address),
        vec![
            &f.env,
            (
                f.oracle_address.clone(),
                (Symbol::new(&f.env, "staleness_update"),).into_val(&f.env),
                staleness_data.into_val(&f.env),
            ),
        ],
    );
}

#[test]
fn updating_the_spread_reduction_factor_attests_the_new_factor() {
    let f = fixture(0);
    f.oracle.update_spread_reduction_factor(&(SCALAR_18 / 4));

    let factor_data: Map<Symbol, Val> = map![
        &f.env,
        (
            Symbol::new(&f.env, "spread_reduction_factor"),
            (SCALAR_18 / 4).into_val(&f.env)
        ),
    ];
    assert_eq!(
        f.env.events().all().filter_by_contract(&f.oracle_address),
        vec![
            &f.env,
            (
                f.oracle_address.clone(),
                (Symbol::new(&f.env, "spread_reduction_update"),).into_val(&f.env),
                factor_data.into_val(&f.env),
            ),
        ],
    );
}

/// A rejected update must leave no trace: the gates are unchanged, so there
/// is nothing to attest.
#[test]
fn a_rejected_update_publishes_nothing() {
    let f = fixture(0);
    assert!(f.oracle.try_update_staleness(&16, &60).is_err());
    assert!(f
        .oracle
        .try_update_spread_reduction_factor(&(SCALAR_18 + 1))
        .is_err());

    assert_eq!(
        f.env.events().all().filter_by_contract(&f.oracle_address),
        vec![&f.env]
    );
}

#[test]
fn owner_updates_land_and_views_read_back() {
    let f = fixture(0);
    f.oracle.update_staleness(&10, &90);
    assert_eq!(f.oracle.trade_staleness(), 10);
    assert_eq!(f.oracle.close_staleness(), 90);
    f.oracle.update_spread_reduction_factor(&(SCALAR_18 / 4));
    assert_eq!(f.oracle.spread_reduction_factor(), SCALAR_18 / 4);
    assert_eq!(
        f.oracle.verifier(),
        f.verifier,
        "the verifier is constructor-pinned"
    );
    assert_eq!(f.oracle.get_owner(), Some(f.owner.clone()));
}
