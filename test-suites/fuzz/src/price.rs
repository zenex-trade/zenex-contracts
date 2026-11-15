//! The fuzzer's oracle: a bounded random walk over the mark, with a
//! fuzzer-chosen spread and publish-time skew.
//!
//! Every price-bearing entry takes serialized report bytes, built here by
//! [`test_suites::chainlink_helper::build_report`] and echoed by the
//! fixture's `MockChainlinkVerifier` into the real oracle. The walk is
//! bounded on purpose: an unbounded mark spends the whole run rejected by the oracle
//! (non-positive price) or the sizing gates. A walk that can move 50% a step
//! still crosses every liquidation, ADL, and haircut boundary within a
//! handful of commands.

use arbitrary::Arbitrary;
use soroban_sdk::{Bytes, Env};
use test_suites::chainlink_helper::{build_report, ReportTimes};

use crate::fixture::feed_id;

/// Production price precision: 18 decimal places per dollar.
pub const PRICE_SCALAR: i128 = 1_000_000_000_000_000_000;
/// Starting mark: BTC at $100k.
pub const START_PRICE: i128 = 100_000 * PRICE_SCALAR;
/// XLM mark used to bootstrap and vary the testnet economic acceptance traces.
pub const TESTNET_START_PRICE: i128 = 180_000_000_000_000_000;
/// Walk bounds, $1k to $10M.
pub const MIN_PRICE: i128 = 1_000 * PRICE_SCALAR;
pub const MAX_PRICE: i128 = 10_000_000 * PRICE_SCALAR;

/// The fuzzer's per-step move of the oracle.
#[derive(Arbitrary, Debug, Clone, Copy)]
pub struct PriceMove {
    /// Mark change in basis points, clamped to +/-50% before it is applied.
    pub change_bps: i16,
    /// Half-spread in basis points around the mark, capped at 100 bps (the
    /// oracle rejects a crossed book, and a wide spread starves the fill
    /// paths).
    pub spread_bps: u8,
    /// Fifteenths of a second the report's observation trails the ledger
    /// clock: [`Oracle::apply`] divides it by 15 into whole seconds. Lags past
    /// the oracle's 15s trade window are the `PriceStale` path on the fill
    /// routes. The close-class routes (liquidation, ADL, accrue) still
    /// accept them under their wider 60s window.
    pub stale_secs: u8,
}

/// The oracle the target carries across commands.
#[derive(Debug, Clone, Copy)]
pub struct Oracle {
    /// Current mark in feed-native units.
    pub mid: i128,
    /// Current half-spread in basis points.
    pub spread_bps: i128,
    /// Seconds the next payload trails the ledger clock.
    pub stale_secs: u64,
}

impl Default for Oracle {
    fn default() -> Self {
        Oracle {
            mid: START_PRICE,
            spread_bps: 0,
            stale_secs: 0,
        }
    }
}

impl Oracle {
    /// Apply `mv` to the walk. The mark stays inside `[MIN_PRICE, MAX_PRICE]`
    /// so `bid`/`ask` are always positive and the fill maths stays in range.
    pub fn apply(&mut self, mv: PriceMove) {
        let change = (mv.change_bps as i128).clamp(-5_000, 5_000);
        let next = (self.mid * (10_000 + change)) / 10_000;
        self.mid = next.clamp(MIN_PRICE, MAX_PRICE);
        self.spread_bps = (mv.spread_bps as i128).min(100);
        // Mostly fresh: only the top of the byte range reaches past the
        // oracle's 15s trade window, so stale reports stay a probed
        // edge rather than the common case.
        self.stale_secs = (mv.stale_secs as u64) / 15;
    }

    /// Best bid, rounded down.
    pub fn bid(&self) -> i128 {
        let bid = (self.mid * (10_000 - self.spread_bps)) / 10_000;
        bid.max(1)
    }

    /// Best ask, rounded up so it never crosses the bid.
    pub fn ask(&self) -> i128 {
        let ask = (self.mid * (10_000 + self.spread_bps) + 9_999) / 10_000;
        ask.max(self.bid())
    }

    /// Returns the observation time (seconds) [`Oracle::bytes`] stamps on the
    /// next payload: the ledger clock less `stale_secs`.
    pub fn publish_time(&self, env: &Env) -> u64 {
        env.ledger().timestamp().saturating_sub(self.stale_secs)
    }

    /// Serialize the current book at [`Oracle::publish_time`].
    pub fn bytes(&self, env: &Env) -> Bytes {
        build_report(
            env,
            &feed_id(env),
            ReportTimes::at(self.publish_time(env)),
            self.mid,
            self.bid(),
            self.ask(),
        )
    }

    /// Serialize a fresh (never stale) report at the current book. Teardown
    /// and the setup legs use this so a run's terminal assertions are not
    /// defeated by a fuzzer-chosen stale mark.
    pub fn fresh_bytes(&self, env: &Env) -> Bytes {
        let mut fresh = *self;
        fresh.stale_secs = 0;
        fresh.bytes(env)
    }
}
