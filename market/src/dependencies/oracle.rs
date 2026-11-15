//! Oracle client and the verified `PriceData` type.

use soroban_sdk::{contractclient, contracttype, Bytes, BytesN, Env};

/// A verified price for one feed. It mirrors the oracle's `PriceData`, with
/// the same XDR encoding.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceData {
    /// Best bid after spread reduction, in the feed's native precision.
    pub bid: i128,
    /// Best ask after spread reduction, in the same precision as `bid`.
    pub ask: i128,
    /// The report's observation time as a Unix timestamp (seconds).
    pub publish_time: u64,
}

impl PriceData {
    /// Returns the price a position enters at. A long takes the ask, a short
    /// the bid.
    pub fn entry(&self, is_long: bool) -> i128 {
        if is_long {
            self.ask
        } else {
            self.bid
        }
    }

    /// Returns the price a position closes or liquidates at. A long takes the
    /// bid, a short the ask.
    pub fn exit(&self, is_long: bool) -> i128 {
        if is_long {
            self.bid
        } else {
            self.ask
        }
    }
}

/// The oracle contract interface.
#[allow(dead_code)] // consumed by the generated `OracleClient`
#[contractclient(name = "OracleClient")]
pub trait Oracle {
    /// Verifies `report` for `feed_id` and returns the gated price, under
    /// the wider close staleness window with `protective`, else the strict
    /// trade window. Traps on every oracle rejection.
    fn verify_price(env: Env, report: Bytes, feed_id: BytesN<32>, protective: bool) -> PriceData;
}
