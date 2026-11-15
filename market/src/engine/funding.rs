//! The funding velocity model, its accrual [`MarketData::accrue_funding`], and
//! the pool's claim entry [`claim_credit`].

use crate::engine::config::Config;
use crate::engine::market_data::MarketData;
use crate::engine::math;
use crate::engine::status::Status;
use crate::errors::MarketError;
use crate::events::ClaimCredit;
use crate::storage;
use soroban_fixed_point_math::SorobanFixedPoint;
use soroban_sdk::{panic_with_error, token, Address, Env};

/// The evolution a window applies to the saved funding rate.
enum RateChange {
    /// Ramps toward the dominant side.
    Increase,
    /// Decays flat toward zero.
    Decrease,
    /// Stands still.
    Hold,
}

/// Returns the next funding rate after `elapsed` seconds under the velocity
/// model (`SCALAR_18`, per second). Positive means longs pay shorts.
///
/// The token skew `|long - short| / (long + short)` steers the saved rate:
/// it ramps toward the dominant side, decays, or holds by the config bands.
///
/// # Arguments
/// - `saved`: the stored funding rate to evolve (`SCALAR_18`, per second).
/// - `long_tokens`: the long side's base size (base-dec).
/// - `short_tokens`: the short side's base size (base-dec).
/// - `elapsed`: seconds since the last accrual. Non-positive returns `saved`.
/// - `funding_increase`: the acceleration at full skew (`SCALAR_18`, per
///   second per second).
/// - `funding_decrease`: the flat decay speed (`SCALAR_18`, per second per
///   second).
/// - `threshold_stable`: the skew above which a same-direction rate keeps
///   accelerating (`SCALAR_18`).
/// - `threshold_decrease`: the skew below which a same-direction rate decays
///   (`SCALAR_18`).
/// - `funding_max`: the cap on the rate magnitude (`SCALAR_18`, per second).
#[allow(clippy::too_many_arguments)]
fn funding_rate(
    e: &Env,
    saved: i128,
    long_tokens: i128,
    short_tokens: i128,
    elapsed: i128,
    funding_increase: i128,
    funding_decrease: i128,
    threshold_stable: i128,
    threshold_decrease: i128,
    funding_max: i128,
) -> i128 {
    if elapsed <= 0 {
        return saved;
    }
    let total = long_tokens + short_tokens;
    // An empty market has no skew to steer by, so the rate resets to zero.
    if total == 0 {
        return 0;
    }

    // The signed token imbalance (base-dec). Positive means longs are
    // dominant.
    let imbalance = long_tokens - short_tokens;
    // skew = |imbalance| / total (SCALAR_18). It sits in [0, 1] because
    // `|imbalance| <= total`. A one-sided book (skew = 1) ramps at full
    // acceleration.
    let skew = math::to_ratio_floor(e, imbalance.abs(), total);
    // The dominant, paying side: +1 longs, -1 shorts, 0 balanced.
    let direction = imbalance.signum();
    // A zero saved rate counts as not same-direction, so it ramps from zero.
    // A flip steps the rate toward the new side. It can cross zero over
    // later accruals.
    let same_direction = (saved > 0 && direction > 0) || (saved < 0 && direction < 0);

    let change = if direction == 0 {
        // A balanced book has no side to steer toward, so the rate holds.
        RateChange::Hold
    } else if !same_direction {
        // A fresh or flipped rate ramps toward the dominant side regardless
        // of the bands.
        RateChange::Increase
    } else if skew > threshold_stable {
        // Wide same-direction skew keeps accelerating.
        RateChange::Increase
    } else if skew < threshold_decrease {
        // Narrow same-direction skew decays.
        RateChange::Decrease
    } else {
        // Inside the hold band the rate stands still.
        RateChange::Hold
    };

    let next = match change {
        RateChange::Increase => {
            // The acceleration scaled by skew (SCALAR_18, per second per
            // second), floored.
            let velocity = math::apply_factor_floor(e, funding_increase, skew);
            // The rate moves toward the dominant side by velocity * elapsed.
            saved + direction * velocity * elapsed
        }
        RateChange::Decrease => {
            // A flat magnitude shrink toward zero: `funding_decrease` per
            // elapsed second.
            let decay = funding_decrease * elapsed;
            let magnitude = saved.abs();
            // An overshooting decay saturates at the ±1 floor, the smallest
            // signed step. The sign survives for the next window's
            // same-direction check.
            if magnitude <= decay {
                saved.signum()
            } else {
                (magnitude - decay) * saved.signum()
            }
        }
        RateChange::Hold => saved,
    };

    next.clamp(-funding_max, funding_max)
}

impl MarketData {
    /// Advances the funding accrual by `elapsed` seconds under the velocity
    /// model. The caller owns the clock and stamps `accrued_at`.
    ///
    /// The whole window charges at the evolved `funding_rate`, with the
    /// charged magnitude floored at `funding_min`. The stored rate is not
    /// floored, so it can decay through the floor, reach zero, and flip sign.
    /// With an empty receiving side the paid funding is never redistributed
    /// and accumulates as pool surplus.
    pub fn accrue_funding(&mut self, e: &Env, elapsed: i128, config: &Config) {
        if elapsed == 0 {
            return;
        }

        self.funding_rate = funding_rate(
            e,
            self.funding_rate,
            self.tokens.long,
            self.tokens.short,
            elapsed,
            config.funding_increase,
            config.funding_decrease,
            config.threshold_stable_funding,
            config.threshold_decrease_funding,
            config.funding_max,
        );
        if self.funding_rate == 0 {
            return;
        }

        let longs_pay = self.funding_rate > 0;
        // The stored rate stays unfloored for velocity correctness.
        let rate_magnitude = self.funding_rate.abs().max(config.funding_min);
        // A per-second SCALAR_18 rate times a plain seconds count stays
        // SCALAR_18.
        let pay_delta = rate_magnitude * elapsed;
        self.funding_idx.add(longs_pay, pay_delta);

        // The receiver credit conserves the paid total:
        // recv = pay * payer notional / receiver notional, floored so the
        // pool never over-distributes.
        let payer_notional = self.notional.get(longs_pay);
        let receiver_notional = self.notional.get(!longs_pay);
        if receiver_notional > 0 {
            let recv_delta = pay_delta.fixed_mul_floor(e, &payer_notional, &receiver_notional);
            self.funding_idx.add(!longs_pay, -recv_delta);
        }
    }
}

/// Pays out `user`'s claimable credit from the pool and returns the
/// amount paid (token-dec). Refer to
/// [`Market::claim_credit`](crate::Market::claim_credit).
pub fn claim_credit(e: &Env, user: &Address) -> i128 {
    user.require_auth();
    if storage::get_status(e) == Status::Frozen {
        panic_with_error!(e, MarketError::MarketFrozen);
    }

    let mut data = storage::get_market_data(e);
    // The payout caps at what the pool holds right now. The remainder stays
    // claimable for a later claim.
    let amount = storage::get_claimable_credit(e, user).min(data.credit_pool);
    if amount <= 0 {
        panic_with_error!(e, MarketError::NothingToClaim);
    }
    storage::add_claimable_credit(e, user, -amount);

    data.credit_pool -= amount;
    data.credit_owed -= amount;
    storage::set_market_data(e, &data);

    token::Client::new(e, &storage::get_token(e)).transfer(
        &e.current_contract_address(),
        user,
        &amount,
    );
    ClaimCredit {
        user: user.clone(),
        amount,
    }
    .publish(e);
    amount
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::constants::{BASE_UNIT, SCALAR_18};
    use crate::engine::market_data::SidePair;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{contract, Env};

    // Funding fixture: acceleration 1000/s^2, flat decay 500/s^2, hold band
    // between 2% and 5% skew, rate cap 1e6/s.
    const FUNDING_INCREASE: i128 = 1000;
    const FUNDING_DECREASE: i128 = 500;
    const THRESHOLD_STABLE: i128 = 5 * SCALAR_18 / 100;
    const THRESHOLD_DECREASE: i128 = 2 * SCALAR_18 / 100;
    const FUNDING_MAX: i128 = 1_000_000;

    fn next_funding_rate(
        e: &Env,
        saved: i128,
        long_tokens: i128,
        short_tokens: i128,
        elapsed: i128,
    ) -> i128 {
        funding_rate(
            e,
            saved,
            long_tokens,
            short_tokens,
            elapsed,
            FUNDING_INCREASE,
            FUNDING_DECREASE,
            THRESHOLD_STABLE,
            THRESHOLD_DECREASE,
            FUNDING_MAX,
        )
    }

    // An empty market resets any saved rate to zero.
    #[test]
    fn funding_rate_empty_market_resets_to_zero() {
        let e = Env::default();
        assert_eq!(next_funding_rate(&e, 12_345, 0, 0, 10), 0);
    }

    // Fresh rate, longs dominant: skew = 2/4 = 0.5, step = 1000 * 0.5 * 10 s =
    // 5000 toward the longs-pay direction.
    #[test]
    fn funding_rate_ramps_from_zero_toward_dominant_side() {
        let e = Env::default();
        assert_eq!(next_funding_rate(&e, 0, 3 * BASE_UNIT, BASE_UNIT, 10), 5000);
    }

    // A rate against the skew (shorts were paying, longs now dominant) is
    // pulled up through zero: -3000 + 5000 = 2000.
    #[test]
    fn funding_rate_reversal_ramps_through_zero() {
        let e = Env::default();
        assert_eq!(
            next_funding_rate(&e, -3000, 3 * BASE_UNIT, BASE_UNIT, 10),
            2000
        );
    }

    // Same direction above the stable threshold keeps accelerating:
    // 5000 + 5000 = 10000.
    #[test]
    fn funding_rate_wide_skew_keeps_increasing() {
        let e = Env::default();
        assert_eq!(
            next_funding_rate(&e, 5000, 3 * BASE_UNIT, BASE_UNIT, 10),
            10_000
        );
    }

    // Skew 6/200 = 3% sits between the 2% and 5% thresholds: the rate holds.
    #[test]
    fn funding_rate_inside_hold_band_is_unchanged() {
        let e = Env::default();
        assert_eq!(
            next_funding_rate(&e, 5000, 103 * BASE_UNIT / 100, 97 * BASE_UNIT / 100, 10),
            5000
        );
    }

    // A fresh or flipped rate ramps toward the dominant side whatever band
    // the skew sits in. A fresh rate at skew 6/200 = 3%, inside the hold
    // band, steps 1000 * 3% * 10 s = 300 toward either side. A flipped 5000
    // at skew 2/200 = 1%, inside the decay band, steps 1000 * 1% * 10 s = 100
    // toward the new side instead of decaying.
    #[test]
    fn funding_rate_fresh_or_flipped_rate_ramps_inside_the_bands() {
        let e = Env::default();
        assert_eq!(
            next_funding_rate(&e, 0, 103 * BASE_UNIT / 100, 97 * BASE_UNIT / 100, 10),
            300
        );
        assert_eq!(
            next_funding_rate(&e, 0, 97 * BASE_UNIT / 100, 103 * BASE_UNIT / 100, 10),
            -300
        );
        assert_eq!(
            next_funding_rate(&e, 5000, 99 * BASE_UNIT / 100, 101 * BASE_UNIT / 100, 10),
            4900
        );
        assert_eq!(
            next_funding_rate(&e, -5000, 101 * BASE_UNIT / 100, 99 * BASE_UNIT / 100, 10),
            -4900
        );
    }

    // The band edges belong to the hold band. At skew 10/200 = 5%, exactly
    // the stable threshold, a same-direction rate stops accelerating. At
    // skew 4/200 = 2%, exactly the decay threshold, it does not decay yet.
    #[test]
    fn funding_rate_holds_at_the_exact_band_edges() {
        let e = Env::default();
        assert_eq!(
            next_funding_rate(&e, 5000, 105 * BASE_UNIT / 100, 95 * BASE_UNIT / 100, 10),
            5000
        );
        assert_eq!(
            next_funding_rate(&e, 5000, 102 * BASE_UNIT / 100, 98 * BASE_UNIT / 100, 10),
            5000
        );
    }

    // Skew 2/200 = 1% is inside the decay band: 5000 - 500 * 4 s = 3000.
    #[test]
    fn funding_rate_narrow_skew_decays_flat() {
        let e = Env::default();
        assert_eq!(
            next_funding_rate(&e, 5000, 101 * BASE_UNIT / 100, 99 * BASE_UNIT / 100, 4),
            3000
        );
    }

    // A decay past zero parks at the smallest signed step, so the sign
    // survives: 5000 - 500 * 10 s would hit zero, so the rate parks at +1.
    #[test]
    fn funding_rate_full_decay_parks_at_signed_unit() {
        let e = Env::default();
        assert_eq!(
            next_funding_rate(&e, 5000, 101 * BASE_UNIT / 100, 99 * BASE_UNIT / 100, 10),
            1
        );
        assert_eq!(
            next_funding_rate(&e, -5000, 99 * BASE_UNIT / 100, 101 * BASE_UNIT / 100, 10),
            -1
        );
    }

    // The hard cap clamps the saved rate: 999_000 + 5000 -> 1_000_000.
    #[test]
    fn funding_rate_clamps_at_funding_max() {
        let e = Env::default();
        assert_eq!(
            next_funding_rate(&e, 999_000, 3 * BASE_UNIT, BASE_UNIT, 10),
            FUNDING_MAX
        );
    }

    // A one-sided book has skew 1 and ramps at the full acceleration:
    // 1000 * 1 * 10 s = 10_000.
    #[test]
    fn funding_rate_one_sided_book_ramps_at_full_skew() {
        let e = Env::default();
        assert_eq!(next_funding_rate(&e, 0, BASE_UNIT, 0, 10), 10_000);
    }

    // A token-balanced book has no side to steer toward: the saved rate holds
    // even though the skew sits inside the decay band.
    #[test]
    fn funding_rate_balanced_book_holds() {
        let e = Env::default();
        assert_eq!(next_funding_rate(&e, 5000, BASE_UNIT, BASE_UNIT, 10), 5000);
    }

    // Zero elapsed returns the saved rate untouched.
    #[test]
    fn funding_rate_zero_elapsed_is_noop() {
        let e = Env::default();
        assert_eq!(
            next_funding_rate(&e, 5000, 3 * BASE_UNIT, BASE_UNIT, 0),
            5000
        );
    }

    // Accrual fixture: 2 long base units (2e7 base-dec at the 18-dec feed
    // scale) against 5.0 short notional, with the funding curve above and no
    // charge floor.
    fn funding_market() -> MarketData {
        MarketData {
            notional: SidePair {
                long: 20_0000000,
                short: 5_0000000,
            },
            tokens: SidePair {
                long: 2 * BASE_UNIT,
                short: 0,
            },
            ..Default::default()
        }
    }

    fn funding_config() -> Config {
        Config {
            funding_increase: FUNDING_INCREASE,
            funding_decrease: FUNDING_DECREASE,
            threshold_stable_funding: THRESHOLD_STABLE,
            threshold_decrease_funding: THRESHOLD_DECREASE,
            funding_min: 0,
            funding_max: FUNDING_MAX,
            ..crate::engine::config::test_config()
        }
    }

    // All-long tokens: skew 1, rate ramps to 1000 * 1 * 10 s = 10_000. Longs
    // pay 10_000 * 10 s = 100_000 on the index. The short credit spreads the
    // paid total over the short notional: floor(100_000 * 20/5) = 400_000.
    #[test]
    fn accrue_funding_charges_payer_and_credits_receiver() {
        let e = Env::default();
        let mut data = funding_market();
        data.accrue_funding(&e, 10, &funding_config());
        assert_eq!(data.funding_rate, 10_000);
        assert_eq!(data.funding_idx.long, 100_000);
        assert_eq!(data.funding_idx.short, -400_000);
    }

    // With no short notional the paid funding is never redistributed: the
    // short index stands still and the payments accumulate as pool surplus.
    #[test]
    fn accrue_funding_one_sided_book_credits_nothing() {
        let e = Env::default();
        let mut data = funding_market();
        data.notional.short = 0;
        data.accrue_funding(&e, 10, &funding_config());
        assert_eq!(data.funding_idx.long, 100_000);
        assert_eq!(data.funding_idx.short, 0);
    }

    // The stored rate evolves to 10_000, below the 50_000 floor: the charge
    // uses the floored magnitude (500_000 paid, 2_000_000 credited) while the
    // stored rate keeps its unfloored value.
    #[test]
    fn accrue_funding_floors_charge_but_not_stored_rate() {
        let e = Env::default();
        let mut data = funding_market();
        let config = Config {
            funding_min: 50_000,
            ..funding_config()
        };
        data.accrue_funding(&e, 10, &config);
        assert_eq!(data.funding_rate, 10_000);
        assert_eq!(data.funding_idx.long, 500_000);
        assert_eq!(data.funding_idx.short, -2_000_000);
    }

    // A token-balanced book keeps the rate at zero. No index moves. The
    // caller advances the timestamp.
    #[test]
    fn accrue_funding_zero_rate_moves_no_index() {
        let e = Env::default();
        let mut data = funding_market();
        data.tokens.short = data.tokens.long;
        data.accrue_funding(&e, 10, &funding_config());
        assert_eq!(data.funding_rate, 0);
        assert_eq!(data.funding_idx.long, 0);
        assert_eq!(data.funding_idx.short, 0);
    }

    // All-short tokens ramp the rate negative: shorts pay 100_000 on their
    // index. The long credit is floor(100_000 * 5/20) = 25_000.
    #[test]
    fn accrue_funding_negative_rate_charges_shorts() {
        let e = Env::default();
        let mut data = funding_market();
        data.tokens = SidePair {
            long: 0,
            short: 2 * BASE_UNIT,
        };
        data.accrue_funding(&e, 10, &funding_config());
        assert_eq!(data.funding_rate, -10_000);
        assert_eq!(data.funding_idx.short, 100_000);
        assert_eq!(data.funding_idx.long, -25_000);
    }

    #[contract]
    struct Harness;

    // A claim pays the full balance from the contract's token holdings, deletes
    // the entry, and shrinks both pool totals by the paid amount.
    #[test]
    fn claim_credit_pays_and_clears_balance() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let user = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &1_000);

        e.as_contract(&contract_id, || {
            storage::set_token(&e, &token.address());
            storage::set_status(&e, &Status::Active);
            storage::set_market_data(
                &e,
                &MarketData {
                    credit_pool: 500,
                    credit_owed: 300,
                    ..Default::default()
                },
            );
            storage::add_claimable_credit(&e, &user, 300);
            assert_eq!(claim_credit(&e, &user), 300);

            assert_eq!(storage::get_claimable_credit(&e, &user), 0);
            let data = storage::get_market_data(&e);
            assert_eq!(data.credit_pool, 200);
            assert_eq!(data.credit_owed, 0);
        });
        assert_eq!(token::Client::new(&e, &token.address()).balance(&user), 300);
    }

    // A claim with no balance aborts before any state or transfer.
    #[test]
    #[should_panic(expected = "#760")]
    fn claim_credit_panics_with_nothing_to_claim() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let user = Address::generate(&e);
        e.as_contract(&contract_id, || {
            storage::set_status(&e, &Status::Active);
            storage::set_market_data(&e, &MarketData::default());
            claim_credit(&e, &user);
        });
    }
}
