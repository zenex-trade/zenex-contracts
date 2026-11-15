#![no_main]

//! Fuzz target for the `oracle` contract's report decode and gates.
//!
//! Signature verification is delegated to Chainlink's verifier contract, so
//! this target stands in a `MockChainlinkVerifier` (which echoes its input)
//! and drives bytes straight into the V3 decode, the freshness gates, and
//! the spread reduction.
//!
//! Two modes feed `verify_price`:
//!
//! - **raw** — arbitrary bytes (almost always rejected at the length check).
//! - **built-then-mutated** — a structurally valid report body assembled by
//!   [`chainlink_helper::build_report`] from fuzzer-chosen fields, with one
//!   optional byte flipped. This reaches the per-word decode, the timestamp
//!   gates, and the price-sanity paths that pure-random bytes never hit.
//!
//! The contract MUST either return price data or panic with a documented
//! `OracleError`. Any other panic (host error, OOB read, decode crash,
//! integer overflow) is a bug.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use soroban_sdk::BytesN;
use test_suites::chainlink_helper::{build_report, ReportTimes};

/// Mirrors the ledger timestamp `zenex_fuzz::oracle_model::setup` starts at.
const LEDGER_TIMESTAMP: u64 = 1_700_000_000;

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    /// Raw bytes to submit directly (random, almost always rejected on length).
    raw: Vec<u8>,
    /// Fields for the built-report mode.
    report: FuzzReport,
    /// Offset within the built report at which to flip a byte (0 = no mutation).
    mutation_offset: u16,
    /// XOR mask applied at `mutation_offset`.
    mutation_mask: u8,
    /// Feed anchor for the scoped read, driving the `FeedMismatch` path.
    query_feed: [u8; 32],
    /// Spread reduction the oracle is configured with (masked into range).
    spread_reduction: u64,
    /// Staleness class selector passed to `verify_price` (false = trade
    /// window, true = close window).
    protective: bool,
}

#[derive(Arbitrary, Debug)]
struct FuzzReport {
    feed: [u8; 32],
    price: i128,
    bid: i128,
    ask: i128,
    /// Offset (seconds, may be negative) applied to the observation timestamp
    /// to exercise the staleness and ahead paths.
    time_skew_secs: i32,
    /// Seconds from observation to `expiresAt` (0 = expires at the observation).
    validity_secs: u32,
}

fuzz_target!(|input: FuzzInput| {
    // Any factor in [0, SCALAR_18] is a valid configuration.
    let spread_reduction = (input.spread_reduction as i128) % (1_000_000_000_000_000_000 + 1);
    let (env, address) = zenex_fuzz::oracle_model::setup(spread_reduction);

    // ── Mode A: raw arbitrary bytes ─────────────────────────────────────────
    {
        zenex_fuzz::oracle_model::check(
            &env,
            &address,
            &input.raw,
            &input.query_feed,
            input.protective,
            spread_reduction,
        );
    }

    // ── Mode B: built report with optional single-byte mutation ────────────
    {
        let observed = (LEDGER_TIMESTAMP as i64 + input.report.time_skew_secs as i64).max(0) as u64;
        let times = ReportTimes {
            valid_from: observed,
            observations: observed,
            expires_at: observed.saturating_add(input.report.validity_secs as u64),
        };
        // Force the V3 schema prefix so built reports reach the timestamp
        // and price gates instead of dying on the prefix check. The raw mode
        // and the mutation still cover non-V3 ids.
        let mut feed_bytes = input.report.feed;
        feed_bytes[0] = 0x00;
        feed_bytes[1] = 0x03;
        let feed = BytesN::from_array(&env, &feed_bytes);
        let built = build_report(
            &env,
            &feed,
            times,
            input.report.price,
            input.report.bid,
            input.report.ask,
        );

        let mut buf: Vec<u8> = built.iter().collect();
        if !buf.is_empty() && input.mutation_offset != 0 {
            let off = (input.mutation_offset as usize) % buf.len();
            buf[off] ^= input.mutation_mask;
        }

        // Scoped read against the report's own feed: the happy path when the
        // body survives mutation.
        zenex_fuzz::oracle_model::check(
            &env,
            &address,
            &buf,
            &feed_bytes,
            input.protective,
            spread_reduction,
        );

        // Scoped read against a fuzzer-chosen anchor.
        zenex_fuzz::oracle_model::check(
            &env,
            &address,
            &buf,
            &input.query_feed,
            input.protective,
            spread_reduction,
        );
        // Every decoded input includes a valid-biased report, guaranteeing
        // successful price assertions even when unconstrained fields reject.
        let mid = input.report.price.unsigned_abs() % 100_000_000_000_000_000_000_000 + 1000;
        let spread = u128::from(input.mutation_mask);
        let valid = build_report(
            &env,
            &feed,
            ReportTimes::at(LEDGER_TIMESTAMP),
            mid as i128,
            (mid - spread) as i128,
            (mid + spread) as i128,
        );
        let raw: Vec<u8> = valid.iter().collect();
        zenex_fuzz::oracle_model::check(
            &env,
            &address,
            &raw,
            &feed_bytes,
            input.protective,
            spread_reduction,
        );
    }
    zenex_fuzz::stats::flush();
});
