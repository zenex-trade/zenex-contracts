//! The oracle's own price policy: every gate a verified report must clear,
//! and the spread reduction, between the decoded [`ReportDataV3`] and the
//! [`PriceData`] the market consumes. The Chainlink verifier checks
//! signatures and config activity only, so every check here is load-bearing.

use soroban_fixed_point_math::SorobanFixedPoint;
use soroban_sdk::{panic_with_error, BytesN, Env};

use crate::constants::SCALAR_18;
use crate::error::OracleError;
use crate::report::ReportDataV3;
use crate::PriceData;

/// Runs every protocol gate and shapes the verified [`PriceData`].
/// `publish_time` is the observation time, never `validFromTimestamp`, which
/// only bounds the report's applicability window.
///
/// The window is asymmetric: `max_staleness` bounds an observation's age per
/// class, and `max_ahead` bounds how far it may sit ahead of the ledger
/// clock. Forward skew is only ledger-clock lag, identical for every route,
/// so `max_ahead` never widens with the class. A wide forward allowance
/// would let a protective call cache a timestamp no strict call could
/// produce.
///
/// # Errors
/// - Refer to [`Oracle::verify_price`](crate::Oracle::verify_price) errors.
pub fn to_price_data(
    env: &Env,
    report: ReportDataV3,
    feed_id: &BytesN<32>,
    max_staleness: u64,
    max_ahead: u64,
    spread_reduction_factor: i128,
) -> PriceData {
    if report.feed_id != *feed_id {
        panic_with_error!(env, OracleError::FeedMismatch);
    }
    // The stream's schema version is the id's first two bytes. Only V3
    // carries bid/ask in the nine-word shape decoded here.
    if report.feed_id.get(0) != Some(0x00) || report.feed_id.get(1) != Some(0x03) {
        panic_with_error!(env, OracleError::InvalidData);
    }

    let now_s = env.ledger().timestamp();
    // The validity window opens at `validFrom`. The ledger clock trails real
    // time, so a fresh report sits slightly ahead of it. The forward
    // allowance bounds that skew.
    if report.valid_from_timestamp.saturating_sub(now_s) > max_ahead {
        panic_with_error!(env, OracleError::PriceAhead);
    }
    if now_s > report.expires_at {
        panic_with_error!(env, OracleError::ReportExpired);
    }

    // A two-sided bound on the observation time: the class window behind,
    // the fixed forward allowance ahead.
    let publish_time = report.observations_timestamp;
    if now_s.saturating_sub(publish_time) > max_staleness {
        panic_with_error!(env, OracleError::PriceStale);
    }
    if publish_time.saturating_sub(now_s) > max_ahead {
        panic_with_error!(env, OracleError::PriceAhead);
    }

    // Every consumer divides or scales by a price side, so each must be
    // strictly positive. A crossed market (bid > ask) is malformed. The
    // benchmark is held to the same line as basic body sanity.
    if report.benchmark_price <= 0 || report.bid <= 0 || report.ask <= 0 || report.bid > report.ask
    {
        panic_with_error!(env, OracleError::InvalidPrice);
    }

    let (bid, ask) = reduce_spread(env, report.bid, report.ask, spread_reduction_factor);

    PriceData {
        bid,
        ask,
        publish_time,
    }
}

/// Narrows `bid` and `ask` symmetrically toward their midpoint by
/// `factor / SCALAR_18`, rounded down. `0` is off. `SCALAR_18` collapses
/// both onto `bid + (ask - bid) / 2`.
fn reduce_spread(env: &Env, bid: i128, ask: i128, factor: i128) -> (i128, i128) {
    if factor == 0 {
        return (bid, ask);
    }
    if factor == SCALAR_18 {
        let mid = bid + (ask - bid) / 2;
        return (mid, mid);
    }
    let half_spread = (ask - bid) / 2;
    let cut = half_spread.fixed_mul_floor(env, &factor, &SCALAR_18);
    (bid + cut, ask - cut)
}

#[cfg(test)]
mod tests {
    use soroban_sdk::Env;

    use super::*;

    #[test]
    fn reduce_spread_floors_the_half_spread_and_the_cut() {
        let env = Env::default();
        // Odd spread: half_spread (2) floors, and the cut (1) floors again.
        // Both sides move by the same floored amount.
        let (bid, ask) = reduce_spread(&env, 100, 105, SCALAR_18 / 2);
        assert_eq!((bid, ask), (101, 104));
        // A full collapse lands on bid + (ask - bid) / 2.
        let (bid, ask) = reduce_spread(&env, 100, 105, SCALAR_18);
        assert_eq!((bid, ask), (102, 102));
        // Off is bit-for-bit identity.
        assert_eq!(reduce_spread(&env, 100, 105, 0), (100, 105));
    }

    #[test]
    fn reduce_spread_never_inverts_the_sides() {
        let env = Env::default();
        for factor in [1, SCALAR_18 / 3, SCALAR_18 - 1, SCALAR_18] {
            let (bid, ask) = reduce_spread(&env, 999_999, 1_000_001, factor);
            assert!(bid <= ask, "factor {factor}: {bid} > {ask}");
        }
    }
}
