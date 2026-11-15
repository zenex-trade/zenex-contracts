//! Independent ABI and price-policy reference. No production decoder/math is
//! called. The verifier is an echo mock: signatures are outside this scope.
use oracle::{Oracle, OracleClient, PriceData};
use soroban_sdk::testutils::{Address as _, Ledger};
use soroban_sdk::{Address, Bytes, BytesN, Env};
use test_suites::mock_chainlink::MockChainlinkVerifier;

pub const NOW: u64 = 1_700_000_000;
const S: i128 = 1_000_000_000_000_000_000;

/// Exact expected contract error or price, including structural error order.
pub fn predict(
    raw: &[u8],
    feed: &[u8; 32],
    now: u64,
    protective: bool,
    factor: i128,
) -> Result<(i128, i128, u64), u32> {
    if raw.len() != 288 {
        return Err(780);
    }
    let word = |n: usize| &raw[n * 32..(n + 1) * 32];
    let unsigned = |n: usize| -> Result<u64, u32> {
        let w = word(n);
        if w[..28].iter().any(|&v| v != 0) {
            return Err(780);
        }
        Ok(u32::from_be_bytes(w[28..].try_into().unwrap()) as u64)
    };
    let valid = unsigned(1)?;
    let published = unsigned(2)?;
    for n in [3, 4] {
        if word(n)[..8].iter().any(|&v| v != 0) {
            return Err(780);
        }
    }
    let expires = unsigned(5)?;
    let mut prices = [0; 3];
    for (n, p) in prices.iter_mut().enumerate() {
        let w = word(n + 6);
        let low: [u8; 16] = w[16..].try_into().unwrap();
        *p = i128::from_be_bytes(low);
        let sign = if *p < 0 { 255 } else { 0 };
        if w[..16].iter().any(|&v| v != sign) {
            return Err(781);
        }
    }
    if word(0) != feed {
        return Err(790);
    }
    if raw[..2] != [0, 3] {
        return Err(780);
    }
    if valid > now + 15 {
        return Err(793);
    }
    if expires < now {
        return Err(784);
    }
    let age = if protective { 60 } else { 15 };
    if published.saturating_add(age) < now {
        return Err(782);
    }
    if published > now + 15 {
        return Err(793);
    }
    let [benchmark, bid, ask] = prices;
    if benchmark <= 0 || bid <= 0 || ask < bid {
        return Err(781);
    }
    // Quotient/remainder decomposition avoids multiplying an arbitrary i128
    // price by a 1e18 rate. Both partial products fit, unlike naive x*rate.
    let half = (ask - bid) / 2;
    if factor == S {
        return Ok((bid + half, bid + half, published));
    }
    let cut = (half / S) * factor + ((half % S) * factor) / S;
    Ok((bid + cut, ask - cut, published))
}

pub fn assert_output(expected: Result<(i128, i128, u64), u32>, actual: Result<PriceData, u32>) {
    let actual = actual.map(|p| (p.bid, p.ask, p.publish_time));
    assert_eq!(actual, expected, "independent oracle acceptance/output");
}

pub fn setup(factor: i128) -> (Env, Address) {
    let env = Env::default();
    env.ledger().with_mut(|l| {
        l.timestamp = NOW;
        l.protocol_version = 26;
    });
    test_suites::resources::enforce(&env);
    let owner = Address::generate(&env);
    let verifier = env.register(MockChainlinkVerifier, ());
    let address = env.register(Oracle, (&owner, &verifier, 15u64, 60u64, factor));
    (env, address)
}

pub fn check(
    env: &Env,
    address: &Address,
    raw: &[u8],
    feed: &[u8; 32],
    protective: bool,
    factor: i128,
) {
    let expected = predict(raw, feed, env.ledger().timestamp(), protective, factor);
    let result = OracleClient::new(env, address).try_verify_price(
        &Bytes::from_slice(env, raw),
        &BytesN::from_array(env, feed),
        &protective,
    );
    let actual = match result {
        Ok(Ok(price)) => Ok(price),
        Err(Ok(e)) if e.is_type(soroban_sdk::xdr::ScErrorType::Contract) => Err(e.get_code()),
        other => panic!("oracle host/conversion failure: {other:?}"),
    };
    assert_output(expected, actual);
    crate::stats::record("oracle_attempt", 1);
    crate::stats::record(
        if expected.is_ok() {
            "oracle_success"
        } else {
            "oracle_rejection"
        },
        1,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_suites::chainlink_helper::{build_report, ReportTimes};
    fn report(env: &Env) -> ([u8; 32], Vec<u8>) {
        let mut feed = [7; 32];
        feed[..2].copy_from_slice(&[0, 3]);
        let bytes = build_report(
            env,
            &BytesN::from_array(env, &feed),
            ReportTimes::at(NOW),
            102,
            100,
            105,
        );
        (feed, bytes.iter().collect())
    }
    #[test]
    fn oracle_success_rejections_cache_classes_and_rounding_are_reached() {
        for factor in [0, 1, S / 2, S - 1, S] {
            let (env, address) = setup(factor);
            let (feed, raw) = report(&env);
            assert!(predict(&raw, &feed, NOW, false, factor).is_ok());
            check(&env, &address, &raw, &feed, false, factor);
            env.ledger().with_mut(|l| l.timestamp = NOW + 16);
            assert_eq!(predict(&raw, &feed, NOW + 16, false, factor), Err(782));
            check(&env, &address, &raw, &feed, false, factor);
            check(&env, &address, &raw, &feed, true, factor);
            let mut foreign = feed;
            foreign[31] ^= 1;
            check(&env, &address, &raw, &foreign, true, factor);
            for (offset, code) in [(32, 780), (3 * 32, 780), (6 * 32, 781)] {
                let mut mutated = raw.clone();
                mutated[offset] = 1;
                assert_eq!(predict(&mutated, &feed, NOW, true, factor), Err(code));
                check(&env, &address, &mutated, &feed, true, factor);
            }
        }
    }
    #[test]
    fn exact_time_price_schema_and_wide_arithmetic_boundaries() {
        let (env, address) = setup(S / 3);
        let (feed, _) = report(&env);
        for (valid, published, expires, protective, code) in [
            (NOW + 16, NOW, NOW + 100, false, 793),
            (NOW, NOW + 16, NOW + 100, true, 793),
            (NOW, NOW, NOW - 1, false, 784),
            (NOW, NOW - 16, NOW + 100, false, 782),
            (NOW, NOW - 61, NOW + 100, true, 782),
        ] {
            let body = build_report(
                &env,
                &BytesN::from_array(&env, &feed),
                ReportTimes {
                    valid_from: valid,
                    observations: published,
                    expires_at: expires,
                },
                102,
                100,
                105,
            );
            let raw: Vec<u8> = body.iter().collect();
            assert_eq!(predict(&raw, &feed, NOW, protective, S / 3), Err(code));
            check(&env, &address, &raw, &feed, protective, S / 3);
        }
        for (benchmark, bid, ask) in [
            (0, 100, 105),
            (1, 0, 105),
            (1, 105, 100),
            (-1, 100, 105),
            (1, 1, i128::MAX),
        ] {
            let body = build_report(
                &env,
                &BytesN::from_array(&env, &feed),
                ReportTimes::at(NOW),
                benchmark,
                bid,
                ask,
            );
            let raw: Vec<u8> = body.iter().collect();
            assert_eq!(
                predict(&raw, &feed, NOW, false, S / 3).is_ok(),
                ask == i128::MAX
            );
            check(&env, &address, &raw, &feed, false, S / 3);
        }
        let (mut feed, mut raw) = report(&env);
        feed[1] = 4;
        raw[1] = 4;
        assert_eq!(predict(&raw, &feed, NOW, false, S / 3), Err(780));
        check(&env, &address, &raw, &feed, false, S / 3);
    }
    #[test]
    #[should_panic(expected = "independent oracle acceptance/output")]
    fn wrong_success_value_is_detected() {
        assert_output(
            Ok((100, 105, NOW)),
            Ok(PriceData {
                bid: 101,
                ask: 105,
                publish_time: NOW,
            }),
        );
    }
    #[test]
    #[should_panic(expected = "independent oracle acceptance/output")]
    fn malformed_success_is_detected() {
        assert_output(
            Err(780),
            Ok(PriceData {
                bid: 1,
                ask: 1,
                publish_time: NOW,
            }),
        );
    }
}
