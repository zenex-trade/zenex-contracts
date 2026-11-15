//! The [`Market`] working set: the per-call view the keeper loads to operate
//! on the stored market state.

use crate::dependencies::{OracleClient, PriceData, TreasuryClient, VaultClient};
use crate::engine::config::Config;
use crate::engine::constants::SCALAR_18;
use crate::engine::market_data::MarketData;
use crate::engine::math;
use crate::engine::status::Status;
use crate::errors::MarketError;
use crate::storage;
use soroban_fixed_point_math::SorobanFixedPoint;
use soroban_sdk::{panic_with_error, Address, Bytes, Env};

/// The keeper's per-call working set over the market. Not the crate-root
/// [`Market`](crate::Market) trait, the contract's external interface.
pub struct Market {
    /// The market config at load.
    pub config: Config,
    /// The stored market record. It is mutated in place and written back by
    /// [`Market::store`].
    pub data: MarketData,
    /// The market status at load.
    pub status: Status,
    /// The effective execution price for the call.
    pub price: PriceData,
    /// The settlement token, the collateral asset.
    pub token: Address,
    /// The strategy-vault address.
    pub vault: Address,
    /// The vault's `total_assets`, loaded once and tracked through each
    /// settlement (token-dec).
    pub vault_balance: i128,
    /// The treasury contract, the payee of the treasury cuts.
    pub treasury: Address,
    /// The treasury's fee rate at load (`SCALAR_18`). The treasury contract
    /// bounds it to `[0, SCALAR_18 / 2]`.
    pub treasury_rate: i128,
    /// The ledger timestamp at load (seconds).
    pub now: u64,
}

impl Market {
    /// Loads the keeper's working set at the effective price, with both
    /// accrual indices advanced to now at that price.
    ///
    /// A verified payload newer than the cache advances the cache. Under a
    /// stored terminal price the market prices flat at it, and `price_data`
    /// is ignored and never verified.
    ///
    /// # Arguments
    /// - `price_data`: the oracle report to verify against the market's feed.
    /// - `newest_price`: executes at the cached price when it is newer than
    ///   the verified payload.
    /// - `protective`: verifies under the oracle's wider close staleness
    ///   window instead of the strict trade window.
    ///
    /// # Errors
    /// - [`MarketError::MarketFrozen`] if the market status is
    ///   [`Status::Frozen`] or [`Status::Retired`].
    pub fn load(e: &Env, price_data: &Bytes, newest_price: bool, protective: bool) -> Self {
        let status = storage::get_status(e);
        if status == Status::Frozen || status == Status::Retired {
            panic_with_error!(e, MarketError::MarketFrozen);
        }
        let now = e.ledger().timestamp();

        let price = if storage::has_terminal_price(e) {
            // Flat settlement: the cache is neither read nor maintained.
            let terminal = storage::get_terminal_price(e);
            PriceData {
                bid: terminal,
                ask: terminal,
                publish_time: now,
            }
        } else {
            let submitted = OracleClient::new(e, &storage::get_oracle(e)).verify_price(
                price_data,
                &storage::get_feed_id(e),
                &protective,
            );
            // The cache moves forward only. A panic later in the call reverts
            // the write.
            match storage::get_price_cache(e) {
                Some(cached) => {
                    if submitted.publish_time > cached.publish_time {
                        storage::set_price_cache(e, &submitted);
                        submitted
                    } else if newest_price && cached.publish_time > submitted.publish_time {
                        // Substitution: the cache holds a strictly newer mark.
                        cached
                    } else {
                        submitted
                    }
                }
                None => {
                    storage::set_price_cache(e, &submitted);
                    submitted
                }
            }
        };

        let config = storage::get_config(e);
        let mut data = storage::get_market_data(e);
        let token = storage::get_token(e);
        let vault = storage::get_vault(e);
        let vault_balance = VaultClient::new(e, &vault).total_assets();
        let treasury = storage::get_treasury(e);
        let treasury_rate = TreasuryClient::new(e, &treasury).get_rate();

        // The indices accrue before any position is touched. Both indices
        // share one clock: the elapsed window is computed once and stamped
        // once.
        let elapsed = (now - data.accrued_at) as i128;
        data.accrue_borrowing(e, elapsed, &price, &config, vault_balance);
        data.accrue_funding(e, elapsed, &config);
        data.accrued_at = now;

        Market {
            config,
            data,
            status,
            price,
            token,
            vault,
            vault_balance,
            treasury,
            treasury_rate,
            now,
        }
    }

    /// Writes the mutated market data back to storage.
    pub fn store(&self, e: &Env) {
        storage::set_market_data(e, &self.data);
    }

    /// Traps unless the selected side's reserved value fits within `cap` of
    /// half the tracked `vault_balance`.
    ///
    /// # Arguments
    /// - `cap`: the utilization cap applied to the selected side's half of the
    ///   tracked `vault_balance` (`SCALAR_18`).
    /// - `is_long`: the side whose reserve is checked. Increases check their
    ///   own side. Vault redemptions must check both sides.
    ///
    /// # Errors
    /// - [`MarketError::UtilizationExceeded`] if the selected side's
    ///   [`MarketData::side_reserved`] exceeds `cap * vault_balance / 2`.
    pub fn require_utilization(&self, e: &Env, cap: i128, is_long: bool) {
        // The capacity floors and the long reserve rounds up. Both round
        // toward rejection.
        let max_reserved = math::half_factor(e, self.vault_balance, cap);
        if self.data.side_reserved(e, &self.price, is_long) > max_reserved {
            panic_with_error!(e, MarketError::UtilizationExceeded);
        }
    }

    /// Returns the trade fee split `(base_fee, impact_fee)` (token-dec) for
    /// a signed change of `signed_notional` (token-dec) and `signed_tokens`
    /// (base-dec) on `is_long`'s side. Positive is an increase. All rounding
    /// moves toward a higher fee.
    ///
    /// The skew effect is judged on the token change through
    /// [`MarketData::skew_split`] and mapped onto the fill's notional
    /// pro-rata. The worsening leg pays `fee_dom`, the improving leg
    /// `fee_non_dom`. The impact fee is size-quadratic on the full notional,
    /// regardless of skew direction.
    pub fn trade_fees(
        &self,
        e: &Env,
        is_long: bool,
        signed_notional: i128,
        signed_tokens: i128,
    ) -> (i128, i128) {
        let split = self.data.skew_split(is_long, signed_tokens);
        // The worsening notional rounds up, so the dominant fee errs high.
        // The improving leg takes the exact remainder, so the legs sum to
        // the fill.
        let notional = signed_notional.abs();
        let delta_tokens = signed_tokens.abs();
        let worsening = if delta_tokens == 0 {
            0
        } else {
            notional.fixed_mul_ceil(e, &split.worsening, &delta_tokens)
        };
        let improving = notional - worsening;
        let base_fee = math::apply_factor_ceil(e, worsening, self.config.fee_dom)
            + math::apply_factor_ceil(e, improving, self.config.fee_non_dom);
        let impact_fee = math::impact_fee(e, notional, self.config.impact_scalar);
        (base_fee, impact_fee)
    }

    /// Applies the realized-profit haircut and returns the payable PnL
    /// (token-dec). A loss passes through unchanged. The allowance and the
    /// scaled payout round down, against the trader.
    ///
    /// The haircut applies while `is_long`'s side carries pending PnL beyond
    /// `max_pnl_trader` of half the vault balance. A positive `pnl` then
    /// scales by the ratio of that allowance to the side's current pending
    /// PnL. It measures the pre-action book, so call it before the closing
    /// mutation lands.
    ///
    /// # Security Warning
    /// Slicing a close across many fills partially escapes the haircut.
    /// Every fill re-reads a relieved ratio, so the summed payouts exceed
    /// the single-close payout. The same PnL overhang arms ADL through
    /// [`update_adl_state`](crate::engine::adl::update_adl_state). Any
    /// keeper can then deleverage the winning side to `adl_clear_target`,
    /// which bounds what slicing can extract.
    pub fn haircut_pnl(&self, e: &Env, is_long: bool, pnl: i128) -> i128 {
        if pnl <= 0 {
            return pnl;
        }
        let side_pnl = self.data.side_pnl(e, &self.price, is_long, true);
        let cap = math::half_factor(e, self.vault_balance, self.config.max_pnl_trader);
        if side_pnl <= cap {
            return pnl;
        }
        math::prorate(e, pnl, cap, side_pnl)
    }

    /// Returns the signed NAV adjustment for share pricing (token-dec).
    ///
    /// It is the net of both sides' [`MarketData::side_pnl`] at `maximize`,
    /// less the vault's share of [`MarketData::pending_borrowing`]. Each
    /// side's profit caps at the [`Market::haircut_pnl`] allowance,
    /// `max_pnl_trader` of half the vault balance.
    pub fn lp_net_pnl(&self, e: &Env, maximize: bool) -> i128 {
        // The per-side cap keeps the recognized liability within
        // `max_pnl_trader` of the vault balance.
        let cap = math::half_factor(e, self.vault_balance, self.config.max_pnl_trader);
        let pnl = self.data.side_pnl(e, &self.price, true, maximize).min(cap)
            + self.data.side_pnl(e, &self.price, false, maximize).min(cap);
        // The vault's share rounds with the same mark: trader-best is the
        // smallest income.
        let pending = self.data.pending_borrowing(e, maximize);
        let vault_rate = SCALAR_18 - self.treasury_rate;
        let income = if maximize {
            math::apply_factor_floor(e, pending, vault_rate)
        } else {
            math::apply_factor_ceil(e, pending, vault_rate)
        };
        // The pending borrowing is a claim on margin, not a price mark, so
        // the PnL cap does not clip it.
        pnl - income
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::constants::{BASE_UNIT, PRICE_SCALAR};
    use crate::engine::market_data::SidePair;
    use soroban_sdk::{contract, contractimpl, symbol_short, Bytes, Env};

    // Token dominance decides the fee split even when the entry-frozen
    // notional says the opposite: longs hold 200.0 notional in 10 base tokens
    // (entered high), shorts 100.0 in 12 (entered low), so the book is
    // net short 2 tokens. A 50.0 long increase at $10 buys 5 tokens and
    // crosses balance: 2 improve, 3 worsen. Mapped onto the 50.0 notional
    // that is 30.0 worsening / 20.0 improving, so base_fee =
    // ceil(0.5% * 30) + ceil(0.3% * 20) = 0.15 + 0.06 = 0.21. The impact fee
    // reads the full 50.0: 50.0^2 / 100_000 (the test impact_scalar) = 0.025.
    #[test]
    fn trade_fees_follow_token_imbalance_not_notional() {
        use soroban_sdk::testutils::Address as _;
        let e = Env::default();
        let market = Market {
            config: crate::engine::config::test_config(),
            data: MarketData {
                notional: SidePair {
                    long: 200_0000000,
                    short: 100_0000000,
                },
                tokens: SidePair {
                    long: 10 * BASE_UNIT,
                    short: 12 * BASE_UNIT,
                },
                ..Default::default()
            },
            status: Status::Active,
            price: PriceData {
                bid: 10 * PRICE_SCALAR,
                ask: 10 * PRICE_SCALAR,
                publish_time: 0,
            },
            token: Address::generate(&e),
            vault: Address::generate(&e),
            vault_balance: 0,
            treasury: Address::generate(&e),
            treasury_rate: 0,
            now: 0,
        };
        let (base_fee, impact_fee) = market.trade_fees(&e, true, 50_0000000, 5 * BASE_UNIT);
        assert_eq!(base_fee, 2_100_000);
        assert_eq!(impact_fee, 250_000);
    }

    // Haircut fixture: a long side entered at $8 (80.0 notional, 10 base
    // tokens) marked at $10 carries 20.0 pending PnL. The gate factor is 90%.
    fn haircut_market(e: &Env, vault_balance: i128) -> Market {
        use soroban_sdk::testutils::Address as _;
        Market {
            config: crate::engine::config::test_config(),
            data: MarketData {
                notional: SidePair {
                    long: 80_0000000,
                    short: 0,
                },
                margin: SidePair {
                    long: 10_0000000,
                    short: 0,
                },
                tokens: SidePair {
                    long: 10 * BASE_UNIT,
                    short: 0,
                },
                ..Default::default()
            },
            status: Status::Active,
            price: PriceData {
                bid: 10 * PRICE_SCALAR,
                ask: 10 * PRICE_SCALAR,
                publish_time: 0,
            },
            token: Address::generate(e),
            vault: Address::generate(e),
            vault_balance,
            treasury: Address::generate(e),
            treasury_rate: 0,
            now: 0,
        }
    }

    // cap 90% * 50.0 = 45.0 clears the 20.0 side PnL: the profit passes.
    #[test]
    fn haircut_pnl_passes_profit_under_cap() {
        let e = Env::default();
        assert_eq!(
            haircut_market(&e, 100_0000000).haircut_pnl(&e, true, 5_0000000),
            5_0000000
        );
    }

    // cap 90% * 5.0 = 4.5 sits under the 20.0 side PnL: the 5.0 profit
    // scales by 4.5/20 to 1.125.
    #[test]
    fn haircut_pnl_scales_profit_above_cap() {
        let e = Env::default();
        assert_eq!(
            haircut_market(&e, 10_0000000).haircut_pnl(&e, true, 5_0000000),
            1_1250000
        );
    }

    // Unwinding the whole 20.0-PnL side in one fill pays 20.0 * 4.5/20 = 4.5,
    // exactly the cap.
    #[test]
    fn haircut_pnl_single_full_unwind_pays_the_cap() {
        let e = Env::default();
        assert_eq!(
            haircut_market(&e, 10_0000000).haircut_pnl(&e, true, 20_0000000),
            4_5000000
        );
    }

    // The accepted pro-rata slicing leak: the same unwind in four quarter
    // slices, with the book and the vault marked down after each slice,
    // re-reads a relieved ratio every fill.
    // Per slice, pay = 5.0 * cap / side with cap = floor(90% * vault / 2):
    //   side 20.0, vault 10.0, cap 4.5           -> 1.125
    //   side 15.0, vault 8.875, cap 3.99375      -> 1.33125
    //   side 10.0, vault 7.5437500, cap 3.3946875 -> 1.6973437
    //   side  5.0, vault 5.8464063, cap 2.6308827 -> 2.6308827
    // Total 6.7844764 exceeds the 4.5 single-close payout.
    #[test]
    fn haircut_pnl_split_pays_more_than_single_close() {
        let e = Env::default();
        let single = haircut_market(&e, 10_0000000).haircut_pnl(&e, true, 20_0000000);

        let mut market = haircut_market(&e, 10_0000000);
        let mut total = 0;
        for slice_payout in [1_1250000, 1_3312500, 1_6973437, 2_6308827] {
            let paid = market.haircut_pnl(&e, true, 5_0000000);
            assert_eq!(paid, slice_payout);
            total += paid;
            // Land the slice: a quarter of the book closes and the payout
            // leaves the vault.
            market.data.tokens.long -= 25 * BASE_UNIT / 10;
            market.data.notional.long -= 20_0000000;
            market.vault_balance -= paid;
        }
        assert_eq!(total, 6_7844764);
        assert!(total > single);
    }

    // A loss is never haircut.
    #[test]
    fn haircut_pnl_passes_losses() {
        let e = Env::default();
        assert_eq!(
            haircut_market(&e, 10_0000000).haircut_pnl(&e, true, -5_0000000),
            -5_0000000
        );
    }

    // A losing side aggregate never trips the gate for an individually
    // profitable position: side PnL floors at -10.0 (the margin), under
    // the 9.0 cap.
    #[test]
    fn haircut_pnl_skips_losing_side() {
        let e = Env::default();
        let mut market = haircut_market(&e, 10_0000000);
        market.data.notional.long = 120_0000000;
        assert_eq!(market.haircut_pnl(&e, true, 5_0000000), 5_0000000);
    }

    // Reserve fixture. The long reserve marks 0.9 base tokens at the 10.0
    // ask to exactly 9.0 token-dec, the tighter side against a 20.0 vault.
    // The short's 5.0 notional reserve sits under that cap.
    fn reserve_market(e: &Env) -> Market {
        use soroban_sdk::testutils::Address as _;
        Market {
            config: crate::engine::config::test_config(),
            data: MarketData {
                notional: SidePair {
                    long: 8_0000000,
                    short: 5_0000000,
                },
                tokens: SidePair {
                    long: 9 * BASE_UNIT / 10,
                    short: 0,
                },
                ..Default::default()
            },
            status: Status::Active,
            price: PriceData {
                bid: 9 * PRICE_SCALAR,
                ask: 10 * PRICE_SCALAR,
                publish_time: 0,
            },
            token: Address::generate(e),
            vault: Address::generate(e),
            vault_balance: 20_0000000,
            treasury: Address::generate(e),
            treasury_rate: 0,
            now: 0,
        }
    }

    // Long reserved = 9.0 lands exactly on 90% of half a 20.0 vault.
    // Equality passes.
    #[test]
    fn require_utilization_passes_at_exact_cap() {
        let e = Env::default();
        let cap = 9 * crate::engine::constants::SCALAR_18 / 10;
        reserve_market(&e).require_utilization(&e, cap, true);
    }

    #[test]
    fn require_utilization_ignores_the_opposite_side_at_exact_cap() {
        let e = Env::default();
        let cap = 9 * crate::engine::constants::SCALAR_18 / 10;
        for is_long in [true, false] {
            let mut market = reserve_market(&e);
            if is_long {
                market.data.notional.short = 10_0000000;
            } else {
                market.data.tokens.long = BASE_UNIT;
                market.data.notional.short = 9_0000000;
            }
            market.require_utilization(&e, cap, is_long);
        }
    }

    #[test]
    #[should_panic(expected = "#714")]
    fn require_utilization_rejects_short_reserve_one_atom_above_cap() {
        let e = Env::default();
        let cap = 9 * crate::engine::constants::SCALAR_18 / 10;
        let mut market = reserve_market(&e);
        market.data.notional.short = 9_0000001;
        market.require_utilization(&e, cap, false);
    }

    // One token unit less vault and the floor-rounded cap drops below the
    // reserve.
    #[test]
    #[should_panic(expected = "#714")]
    fn require_utilization_panics_above_cap() {
        let e = Env::default();
        let cap = 9 * crate::engine::constants::SCALAR_18 / 10;
        let mut market = reserve_market(&e);
        market.vault_balance -= 1;
        market.require_utilization(&e, cap, true);
    }

    // A redeem that empties the vault passes only when nothing is reserved.
    // A zero cap backs a zero reserve.
    #[test]
    fn require_utilization_on_zero_balance_needs_zero_reserve() {
        let e = Env::default();
        let mut market = reserve_market(&e);
        market.data = MarketData::default();
        market.vault_balance = 0;
        let cap = 9 * crate::engine::constants::SCALAR_18 / 10;
        market.require_utilization(&e, cap, true);
    }

    #[test]
    #[should_panic(expected = "#714")]
    fn require_utilization_on_zero_balance_panics_with_reserve() {
        let e = Env::default();
        let cap = 9 * crate::engine::constants::SCALAR_18 / 10;
        let mut market = reserve_market(&e);
        market.vault_balance = 0;
        market.require_utilization(&e, cap, true);
    }

    // A market over `data` at the flat `price`, with the given vault balance
    // (token-dec) and treasury rate (`SCALAR_18`).
    fn nav_market(
        e: &Env,
        data: MarketData,
        price: i128,
        vault_balance: i128,
        treasury_rate: i128,
    ) -> Market {
        use soroban_sdk::testutils::Address as _;
        Market {
            config: crate::engine::config::test_config(),
            data,
            status: Status::Active,
            price: PriceData {
                bid: price,
                ask: price,
                publish_time: 0,
            },
            token: Address::generate(e),
            vault: Address::generate(e),
            vault_balance,
            treasury: Address::generate(e),
            treasury_rate,
            now: 0,
        }
    }

    // The long side marks zero PnL at a flat $1 and owes 2.1243757712... of
    // borrowing: 1,000.0 checkpointed at index 0.001 and 333.3333333 at
    // 0.001500000000000333, marked at 0.002718281828459045. The NAV
    // adjustment is minus the vault's share after the treasury cut. The
    // redeem mark floors the total and the share. The deposit mark ceils
    // both.
    #[test]
    fn lp_net_pnl_subtracts_the_vault_share_of_pending_borrowing() {
        let e = Env::default();
        let data = MarketData {
            notional: SidePair {
                long: 1333_3333333,
                short: 0,
            },
            tokens: SidePair {
                long: 1333_3333333,
                short: 0,
            },
            borrowing_idx: SidePair {
                long: 2_718_281_828_459_045,
                short: 0,
            },
            borrowing_weight: SidePair {
                long: 1000_0000000 * 1_000_000_000_000_000 + 333_3333333 * 1_500_000_000_000_333,
                short: 0,
            },
            ..Default::default()
        };
        // (treasury rate, redeem income, deposit income)
        for (treasury_rate, redeem, deposit) in [
            (0, 2_1243757, 2_1243758),
            (SCALAR_18 / 10, 1_9119381, 1_9119383),
            (SCALAR_18 / 2, 1_0621878, 1_0621879),
        ] {
            let market = nav_market(&e, data.clone(), PRICE_SCALAR, 1000_0000000, treasury_rate);
            assert_eq!(market.lp_net_pnl(&e, true), -redeem);
            assert_eq!(market.lp_net_pnl(&e, false), -deposit);
        }
    }

    // The PnL cap clips each side's profit apart, but not the borrowing. A
    // 100.0 vault caps each side at 45.0. At $10 the long (entered 50.0 for 10
    // base tokens) and the short (entered 150.0 for 10 base tokens) each mark
    // 50.0 of profit and count as 45.0. The long's 10.0 of borrowing (index
    // 0.2 since a zero checkpoint) subtracts in full: 45.0 + 45.0 - 10.0.
    #[test]
    fn lp_net_pnl_caps_each_sides_profit_but_not_borrowing() {
        let e = Env::default();
        let data = MarketData {
            notional: SidePair {
                long: 50_0000000,
                short: 150_0000000,
            },
            margin: SidePair {
                long: 10_0000000,
                short: 10_0000000,
            },
            tokens: SidePair {
                long: 10 * BASE_UNIT,
                short: 10 * BASE_UNIT,
            },
            borrowing_idx: SidePair {
                long: SCALAR_18 / 5,
                short: 0,
            },
            ..Default::default()
        };
        let market = nav_market(&e, data, 10 * PRICE_SCALAR, 100_0000000, 0);
        assert_eq!(market.lp_net_pnl(&e, true), 80_0000000);
        assert_eq!(market.lp_net_pnl(&e, false), 80_0000000);
    }

    // A side's loss counts only down to its posted margin, but its borrowing
    // counts in full even when the margin cannot pay it. The long side
    // entered 150.0 for 10 base tokens on 20.0 margin, so at $10 its 50.0
    // loss counts as 20.0. It also owes 10.0 of borrowing: 100.0
    // checkpointed at index 0.05 and 50.0 at 0, marked at 0.1. The NAV
    // adjustment is -20.0 - 10.0 = -30.0.
    #[test]
    fn lp_net_pnl_clips_loss_at_margin_but_counts_all_borrowing() {
        let e = Env::default();
        let data = MarketData {
            notional: SidePair {
                long: 150_0000000,
                short: 0,
            },
            margin: SidePair {
                long: 20_0000000,
                short: 0,
            },
            tokens: SidePair {
                long: 10 * BASE_UNIT,
                short: 0,
            },
            borrowing_idx: SidePair {
                long: SCALAR_18 / 10,
                short: 0,
            },
            borrowing_weight: SidePair {
                long: 100_0000000 * (SCALAR_18 / 20),
                short: 0,
            },
            ..Default::default()
        };
        let market = nav_market(&e, data, 10 * PRICE_SCALAR, 100_0000000, 0);
        assert_eq!(market.lp_net_pnl(&e, true), -30_0000000);
        assert_eq!(market.lp_net_pnl(&e, false), -30_0000000);
    }

    #[contract]
    struct Harness;

    // `load` aborts on a frozen market before it verifies the price, so
    // seeding the status alone, with no mock verifier or vault, reaches the
    // gate.
    #[test]
    #[should_panic(expected = "#704")]
    fn load_panics_when_market_frozen() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            storage::set_status(&e, &Status::Frozen);
            Market::load(&e, &Bytes::new(&e), true, true);
        });
    }

    // `Retired` ends the price-bearing paths through the same gate.
    #[test]
    #[should_panic(expected = "#704")]
    fn load_panics_when_market_retired() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            storage::set_status(&e, &Status::Retired);
            Market::load(&e, &Bytes::new(&e), true, true);
        });
    }

    // Returns the price seeded via `set_price`, ignoring the payload bytes.
    #[contract]
    struct MockVerifier;

    #[contractimpl]
    impl MockVerifier {
        pub fn set_price(e: Env, price: PriceData) {
            e.storage().instance().set(&symbol_short!("price"), &price);
        }

        pub fn verify_price(
            e: Env,
            _report: Bytes,
            _feed_id: soroban_sdk::BytesN<32>,
            _protective: bool,
        ) -> PriceData {
            e.storage().instance().get(&symbol_short!("price")).unwrap()
        }
    }

    // Reports a zero backing balance. At elapsed 0 the accruals never read it.
    #[contract]
    struct MockVault;

    #[contractimpl]
    impl MockVault {
        pub fn total_assets(_e: Env) -> i128 {
            0
        }
    }

    // Charges no treasury cut.
    #[contract]
    struct MockTreasury;

    #[contractimpl]
    impl MockTreasury {
        pub fn get_rate(_e: Env) -> i128 {
            0
        }
    }

    // An Active market wired to the mock verifier, vault and treasury. The verifier
    // serves whatever `MockVerifier::set_price` seeded.
    fn cache_fixture(e: &Env) -> (Address, MockVerifierClient<'_>) {
        use soroban_sdk::testutils::Address as _;
        let contract_id = e.register(Harness, ());
        let verifier = e.register(MockVerifier, ());
        let vault = e.register(MockVault, ());
        let treasury = e.register(MockTreasury, ());
        e.as_contract(&contract_id, || {
            storage::set_status(e, &Status::Active);
            storage::set_feed_id(e, &soroban_sdk::BytesN::from_array(e, &[1u8; 32]));
            storage::set_oracle(e, &verifier);
            storage::set_token(e, &Address::generate(e));
            storage::set_vault(e, &vault);
            storage::set_treasury(e, &treasury);
            storage::set_config(e, &crate::engine::config::test_config());
            storage::set_market_data(e, &MarketData::default());
        });
        (contract_id, MockVerifierClient::new(e, &verifier))
    }

    fn price_at(publish_time: u64, price: i128) -> PriceData {
        PriceData {
            bid: price,
            ask: price,
            publish_time,
        }
    }

    // The first verified payload seeds the cache under either policy.
    #[test]
    fn load_writes_first_price_to_cache() {
        let e = Env::default();
        let (contract_id, verifier) = cache_fixture(&e);
        verifier.set_price(&price_at(100, 10 * PRICE_SCALAR));
        e.as_contract(&contract_id, || {
            let market = Market::load(&e, &Bytes::new(&e), false, false);
            assert_eq!(market.price.publish_time, 100);
            assert_eq!(storage::get_price_cache(&e).unwrap().publish_time, 100);
        });
    }

    // The write outlives the network-minimum temporary lifetime, so a quiet
    // stretch longer than the close window cannot lapse the cache.
    #[test]
    fn load_extends_the_cache_past_the_network_minimum() {
        use soroban_sdk::testutils::storage::Temporary as _;
        let e = Env::default();
        let (contract_id, verifier) = cache_fixture(&e);
        verifier.set_price(&price_at(100, 10 * PRICE_SCALAR));
        e.as_contract(&contract_id, || {
            Market::load(&e, &Bytes::new(&e), false, false);
            assert_eq!(
                e.storage()
                    .temporary()
                    .get_ttl(&storage::DataKey::PriceCache),
                storage::LEDGER_BUMP_PRICE_CACHE
            );
        });
    }

    // A newer payload executes and advances the cache.
    #[test]
    fn load_advances_cache_on_newer_payload() {
        let e = Env::default();
        let (contract_id, verifier) = cache_fixture(&e);
        verifier.set_price(&price_at(200, 11 * PRICE_SCALAR));
        e.as_contract(&contract_id, || {
            storage::set_price_cache(&e, &price_at(100, 10 * PRICE_SCALAR));
            let market = Market::load(&e, &Bytes::new(&e), true, true);
            assert_eq!(market.price.publish_time, 200);
            assert_eq!(market.price.bid, 11 * PRICE_SCALAR);
            assert_eq!(storage::get_price_cache(&e).unwrap().publish_time, 200);
        });
    }

    // As-submitted: an older payload executes at its own price and leaves the
    // newer cache untouched.
    #[test]
    fn load_as_submitted_keeps_older_payload_and_cache() {
        let e = Env::default();
        let (contract_id, verifier) = cache_fixture(&e);
        verifier.set_price(&price_at(100, 10 * PRICE_SCALAR));
        e.as_contract(&contract_id, || {
            storage::set_price_cache(&e, &price_at(200, 11 * PRICE_SCALAR));
            let market = Market::load(&e, &Bytes::new(&e), false, false);
            assert_eq!(market.price.publish_time, 100);
            assert_eq!(market.price.bid, 10 * PRICE_SCALAR);
            assert_eq!(storage::get_price_cache(&e).unwrap().publish_time, 200);
        });
    }

    // Newest-price: an older payload is upgraded to the cached price.
    #[test]
    fn load_newest_price_substitutes_newer_cache() {
        let e = Env::default();
        let (contract_id, verifier) = cache_fixture(&e);
        verifier.set_price(&price_at(100, 10 * PRICE_SCALAR));
        e.as_contract(&contract_id, || {
            storage::set_price_cache(&e, &price_at(200, 11 * PRICE_SCALAR));
            let market = Market::load(&e, &Bytes::new(&e), true, true);
            assert_eq!(market.price.publish_time, 200);
            assert_eq!(market.price.bid, 11 * PRICE_SCALAR);
        });
    }

    // An equal publish time is not "newer": the submitted payload executes
    // and the cache is not rewritten.
    #[test]
    fn load_equal_publish_time_keeps_submitted() {
        let e = Env::default();
        let (contract_id, verifier) = cache_fixture(&e);
        verifier.set_price(&price_at(100, 10 * PRICE_SCALAR));
        e.as_contract(&contract_id, || {
            storage::set_price_cache(&e, &price_at(100, 11 * PRICE_SCALAR));
            let market = Market::load(&e, &Bytes::new(&e), true, true);
            assert_eq!(market.price.bid, 10 * PRICE_SCALAR);
            assert_eq!(storage::get_price_cache(&e).unwrap().bid, 11 * PRICE_SCALAR);
        });
    }

    // Under a terminal price the market prices flat and the cache is neither
    // read nor written.
    #[test]
    fn load_terminal_price_ignores_cache() {
        let e = Env::default();
        let (contract_id, verifier) = cache_fixture(&e);
        verifier.set_price(&price_at(100, 10 * PRICE_SCALAR));
        e.as_contract(&contract_id, || {
            storage::set_terminal_price(&e, 9 * PRICE_SCALAR);
            storage::set_price_cache(&e, &price_at(200, 11 * PRICE_SCALAR));
            let market = Market::load(&e, &Bytes::new(&e), true, true);
            assert_eq!(market.price.bid, 9 * PRICE_SCALAR);
            assert_eq!(storage::get_price_cache(&e).unwrap().publish_time, 200);
        });
    }
}
