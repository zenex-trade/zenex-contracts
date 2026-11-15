//! The global [`Config`], its validation, and the owner's [`set_config`]
//! body with the accrue-before-rate-change guard.

use crate::engine::constants::{
    MAX_BORROW_RATE, MAX_FEE_RATE, MAX_FUNDING_RATE, MAX_KEEPER_RATE, MAX_LIQ_FEE, MAX_MARGIN,
    MAX_NOTIONAL_LOCK, MAX_OPEN_INTEREST, MAX_REDEEM_LOCK, MAX_UTIL, MIN_ADL_CLEAR,
    MIN_ADL_TRIGGER, MIN_CHUNK_IMPACT_CAP, MIN_DEPOSIT_DIVISOR, MIN_MARGIN, MIN_NOTIONAL_LOCK,
    SCALAR_18,
};
use crate::engine::status::Status;
use crate::errors::MarketError;
use crate::events::ConfigUpdate;
use crate::storage;
use soroban_sdk::{contracttype, panic_with_error, Env};

/// The global market parameters, an instance storage singleton. The owner
/// replaces them through `set_config`.
///
/// The factory crate mirrors this struct and its field docs. Keep the two in
/// sync.
#[derive(Clone, Debug)]
#[contracttype]
pub struct Config {
    // --- sizing and fees ---
    /// Keeper share of trade, liquidation and vault fill fees (`SCALAR_18`).
    pub keeper_rate: i128,
    /// Minimum position notional (token-dec).
    pub min_position_notional: i128,
    /// Maximum position notional (token-dec).
    pub max_position_notional: i128,
    /// Per-side open-interest ceiling (token-dec). At least
    /// `max_position_notional`, at most `MAX_OPEN_INTEREST`.
    pub max_open_interest: i128,
    /// Minimum `|notional|` per order, the dust floor (token-dec). At most
    /// `min_position_notional`.
    pub min_order_notional: i128,
    /// Minimum posted margin per order, the dust floor (token-dec).
    pub min_order_margin: i128,
    /// Flat keeper execution fee, escrowed per order at creation (token-dec).
    /// At most `min_order_margin`.
    pub exec_fee: i128,
    /// Dominant-side trade fee rate (`SCALAR_18`).
    pub fee_dom: i128,
    /// Non-dominant-side trade fee rate (`SCALAR_18`).
    pub fee_non_dom: i128,
    /// Impact fee denominator (token-dec). Every fill pays
    /// `notional * min(notional / impact_scalar, MAX_IMPACT_RATE)`.
    pub impact_scalar: i128,

    // --- utilization caps (SCALAR_18, util = side reserve / (vault / 2)) ---
    /// Utilization above which size increases on that side are blocked
    /// (`SCALAR_18`). Also each side's borrow-reserve denominator: capacity is
    /// `max_util_open * vault / 2`.
    pub max_util_open: i128,
    /// Utilization above which withdrawals are blocked (`SCALAR_18`), so the
    /// vault keeps at least `reserved / max_util_withdraw` of backing. At
    /// least `max_util_open`.
    pub max_util_withdraw: i128,

    // --- margin ---
    /// Initial margin rate (`SCALAR_18`). Max leverage is `1 / init_margin`.
    pub init_margin: i128,
    /// Hard liquidation floor (`SCALAR_18`). Below `init_margin`.
    pub maintenance_margin: i128,
    /// Liquidation fee rate (`SCALAR_18`). Every liquidation charges
    /// `min(max(equity, 0), ceil(liq_fee * notional))`.
    pub liq_fee: i128,

    // --- position lifecycle ---
    /// Decrease lock on newly added notional (seconds).
    pub notional_lock: u64,

    // --- borrowing (kink model over reserve utilization) ---
    /// Kink utilization on the normalized `[0, 1]` reserve curve
    /// (`SCALAR_18`). Below 1.
    pub target_util: i128,
    /// Borrowing-rate slope below the kink (`SCALAR_18`, per second). The
    /// rate there is `borrow_rate * utilization`.
    pub borrow_rate: i128,
    /// Borrowing rate at full utilization (`SCALAR_18`, per second). In
    /// `[borrow_rate, MAX_BORROW_RATE]`.
    pub increased_borrow_rate: i128,

    // --- funding velocity ---
    /// Velocity acceleration as skew widens (`SCALAR_18`, per second^2).
    pub funding_increase: i128,
    /// Velocity decay inside the decay band (`SCALAR_18`, per second^2).
    pub funding_decrease: i128,
    /// Skew band within which the rate holds (`SCALAR_18`).
    pub threshold_stable_funding: i128,
    /// Skew band within which the rate decays (`SCALAR_18`). At most
    /// `threshold_stable_funding`.
    pub threshold_decrease_funding: i128,
    /// Charged-rate floor (`SCALAR_18`, per second).
    pub funding_min: i128,
    /// Saved-rate hard cap (`SCALAR_18`, per second).
    pub funding_max: i128,

    // --- risk / ADL ---
    /// ADL trigger (`SCALAR_18`). A side arms when its pending PnL over half
    /// the vault exceeds this. In `[MIN_ADL_TRIGGER, max_pnl_trader]`, below 1.
    pub adl_max_pnl: i128,
    /// ADL clear target on the same measure (`SCALAR_18`). In
    /// `[MIN_ADL_CLEAR, adl_max_pnl]`.
    pub adl_clear_target: i128,
    /// Realized-profit haircut threshold (`SCALAR_18`), below 1. While a
    /// side's pending PnL exceeds this fraction of half the vault, close
    /// payouts scale by `allowance / side PnL`. Also the per-side profit cap
    /// in share-pricing PnL.
    pub max_pnl_trader: i128,
    /// Redeem gate (`SCALAR_18`). Redeems are blocked while a side's pending
    /// PnL exceeds this fraction of half the post-redeem balance. In
    /// `(0, adl_clear_target]`.
    pub max_pnl_withdraw: i128,

    // --- vault orders ---
    /// Redeem cooldown from a vault order's `created_at` (seconds). An
    /// operator liquidity-stickiness dial. `0` fills as soon as a
    /// post-creation price exists.
    pub redeem_lock: u64,
    /// Deposit fill fee rate on moved assets (`SCALAR_18`).
    pub deposit_fee: i128,
    /// Redeem fill fee rate on moved assets (`SCALAR_18`).
    pub redeem_fee: i128,
    /// Minimum assets per deposit order (token-dec).
    pub min_deposit: i128,
    /// Vault balance ceiling enforced on deposit fills (token-dec).
    pub max_vault_balance: i128,
}

/// Validates and applies a new [`Config`]. Refer to
/// [`Market::set_config`](crate::Market::set_config).
pub fn set_config(e: &Env, config: &Config) {
    config.require_valid(e);
    let current = storage::get_config(e);

    // A rate change must not reprice the un-accrued interval, so it
    // requires a same-ledger `accrue`. `Frozen` waives it: nothing can
    // accrue while frozen, and the first accrual at unfreeze prices the
    // whole frozen window at the new rates.
    if (borrowing_params_changed(&current, config) || funding_params_changed(&current, config))
        && storage::get_status(e) != Status::Frozen
        && current_market_not_accrued(e)
    {
        panic_with_error!(e, MarketError::MarketNotAccrued);
    }

    storage::set_config(e, config);
    ConfigUpdate {
        config: config.clone(),
    }
    .publish(e);
}

/// Returns `true` unless the market was accrued in the current ledger.
fn current_market_not_accrued(e: &Env) -> bool {
    storage::get_market_data(e).accrued_at != e.ledger().timestamp()
}

/// Returns `true` if any funding-velocity parameter differs between the
/// configs.
fn funding_params_changed(a: &Config, b: &Config) -> bool {
    a.funding_increase != b.funding_increase
        || a.funding_decrease != b.funding_decrease
        || a.threshold_stable_funding != b.threshold_stable_funding
        || a.threshold_decrease_funding != b.threshold_decrease_funding
        || a.funding_min != b.funding_min
        || a.funding_max != b.funding_max
}

/// Returns `true` if any parameter that feeds the borrowing-rate curve
/// differs: the kink parameters plus `max_util_open`, the borrow-reserve
/// denominator.
fn borrowing_params_changed(a: &Config, b: &Config) -> bool {
    a.target_util != b.target_util
        || a.borrow_rate != b.borrow_rate
        || a.increased_borrow_rate != b.increased_borrow_rate
        || a.max_util_open != b.max_util_open
}

impl Config {
    /// Validates the config and traps on the first violated rule.
    ///
    /// # Errors
    /// - Refer to [`Config::check_valid`] errors.
    pub(crate) fn require_valid(&self, e: &Env) {
        self.check_valid()
            .unwrap_or_else(|err| panic_with_error!(e, err));
    }

    /// Validates the config and returns the first violated rule, or `Ok(())`.
    /// It is env-free and never panics.
    ///
    /// # Errors
    /// - [`MarketError::NegativeValueNotAllowed`] if a fee, margin,
    ///   borrowing, funding, ADL, or PnL field, `min_order_margin`,
    ///   `min_deposit`, or `exec_fee` is negative. Any other negative field
    ///   fails its own bound as [`MarketError::InvalidConfig`].
    /// - [`MarketError::InvalidConfig`] if a value breaks its bound or a
    ///   cross-field rule. Each check names its rule inline.
    fn check_valid(&self) -> Result<(), MarketError> {
        if self.keeper_rate < 0
            || self.fee_dom < 0
            || self.fee_non_dom < 0
            || self.init_margin < 0
            || self.maintenance_margin < 0
            || self.liq_fee < 0
            || self.borrow_rate < 0
            || self.increased_borrow_rate < 0
            || self.target_util < 0
            || self.funding_increase < 0
            || self.funding_decrease < 0
            || self.threshold_stable_funding < 0
            || self.threshold_decrease_funding < 0
            || self.funding_min < 0
            || self.funding_max < 0
            || self.adl_max_pnl < 0
            || self.adl_clear_target < 0
            || self.deposit_fee < 0
            || self.redeem_fee < 0
            || self.min_order_margin < 0
            || self.min_deposit < 0
            || self.max_pnl_trader < 0
            || self.max_pnl_withdraw < 0
            || self.exec_fee < 0
        {
            return Err(MarketError::NegativeValueNotAllowed);
        }

        if self.keeper_rate > MAX_KEEPER_RATE
            || self.fee_dom > MAX_FEE_RATE
            || self.fee_non_dom > MAX_FEE_RATE
            || self.max_util_open > MAX_UTIL
            || self.max_util_withdraw > MAX_UTIL
            || self.init_margin > MAX_MARGIN
            || self.liq_fee > MAX_LIQ_FEE
            || self.notional_lock > MAX_NOTIONAL_LOCK
            || self.redeem_lock > MAX_REDEEM_LOCK
        {
            return Err(MarketError::InvalidConfig);
        }

        if self.init_margin < MIN_MARGIN || self.impact_scalar <= 0 {
            return Err(MarketError::InvalidConfig);
        }

        // The decrease lock must outlast the oracle's staleness window,
        // or an accepted stale price could open and close the same size.
        if self.notional_lock < MIN_NOTIONAL_LOCK {
            return Err(MarketError::InvalidConfig);
        }

        if self.min_position_notional <= 0
            || self.max_position_notional <= self.min_position_notional
        {
            return Err(MarketError::InvalidConfig);
        }

        // The per-side open-interest ceiling must admit at least one
        // maximum-size position. The upper bound keeps the borrowing weight
        // from overflowing.
        if self.max_open_interest < self.max_position_notional
            || self.max_open_interest > MAX_OPEN_INTEREST
        {
            return Err(MarketError::InvalidConfig);
        }

        // The per-order dust floors are both positive. The notional floor
        // cannot exceed min_position_notional, or an order sized exactly to a
        // minimum-size position would fall under it.
        if self.min_order_notional <= 0
            || self.min_order_margin <= 0
            || self.min_order_notional > self.min_position_notional
        {
            return Err(MarketError::InvalidConfig);
        }

        // The order-scale sanity rail, both token-dec: the keeper execution
        // fee cannot exceed the smallest order's margin, or dust orders are
        // net-negative escrow.
        if self.exec_fee > self.min_order_margin {
            return Err(MarketError::InvalidConfig);
        }

        // The vault-order fill floor keeps dust fills out.
        if self.min_deposit <= 0 {
            return Err(MarketError::InvalidConfig);
        }

        // Vault fill fee rates are bounded.
        if self.deposit_fee > MAX_FEE_RATE || self.redeem_fee > MAX_FEE_RATE {
            return Err(MarketError::InvalidConfig);
        }

        // The open cap is positive. The withdrawal cap is at least the open
        // cap. The buffer between them is withdrawal-only.
        if self.max_util_open <= 0 || self.max_util_withdraw < self.max_util_open {
            return Err(MarketError::InvalidConfig);
        }

        // The dominant side pays at least as much as the non-dominant side.
        if self.fee_dom < self.fee_non_dom {
            return Err(MarketError::InvalidConfig);
        }

        // The borrowing curve: the kink sits below full utilization, and the
        // full-utilization rate is bounded and at least the base slope.
        if self.target_util >= SCALAR_18
            || self.increased_borrow_rate < self.borrow_rate
            || self.increased_borrow_rate > MAX_BORROW_RATE
        {
            return Err(MarketError::InvalidConfig);
        }

        // Funding velocity: the decay band sits inside the stable band, and
        // the charged floor cannot exceed the cap. The accelerations share
        // the rate bound, which keeps `rate * elapsed` far from i128
        // overflow.
        if self.threshold_stable_funding > SCALAR_18
            || self.threshold_decrease_funding > self.threshold_stable_funding
            || self.funding_min > self.funding_max
            || self.funding_max > MAX_FUNDING_RATE
            || self.funding_increase > MAX_FUNDING_RATE
            || self.funding_decrease > MAX_FUNDING_RATE
        {
            return Err(MarketError::InvalidConfig);
        }

        // The solvency ladder. Every rung is a side pending PnL factor of
        // half the vault balance. The clear target at or below the trigger
        // is the hysteresis. The trigger at or below the haircut threshold
        // makes ADL de-risk before the haircut engages. The redeem gate at
        // or below the clear target means a permitted redeem leaves every
        // side at or below it. An LP exit can neither arm ADL nor hold an
        // armed flag above its clear target.
        if self.adl_max_pnl >= SCALAR_18
            || self.adl_max_pnl < MIN_ADL_TRIGGER
            || self.adl_clear_target > self.adl_max_pnl
            || self.adl_clear_target < MIN_ADL_CLEAR
            || self.max_pnl_trader >= SCALAR_18
            || self.adl_max_pnl > self.max_pnl_trader
            || self.max_pnl_withdraw <= 0
            || self.max_pnl_withdraw > self.adl_clear_target
        {
            return Err(MarketError::InvalidConfig);
        }

        if self.max_vault_balance <= 0 {
            return Err(MarketError::InvalidConfig);
        }

        // The fill floor stays a small fraction of the vault cap, so raising
        // it can never block every fill. An overflowing product is far above
        // the cap and rejected.
        match self.min_deposit.checked_mul(MIN_DEPOSIT_DIVISOR) {
            Some(scaled) if scaled <= self.max_vault_balance => {}
            _ => return Err(MarketError::InvalidConfig),
        }

        // The margin ladder: 0 <= liq_fee < maintenance_margin <
        // init_margin. liq_fee = 0 is a valid no-fee configuration. The
        // strict inequality keeps an equity band where a liquidation
        // returns a remainder.
        if self.maintenance_margin <= self.liq_fee || self.init_margin <= self.maintenance_margin {
            return Err(MarketError::InvalidConfig);
        }

        // The create liquidatable prevention: a position opened exactly at the
        // initial-margin line must clear maintenance after the worst-case
        // unavoidable close fee. That fee is the non-dominant fee plus the
        // impact of a min-size chunk. This half clears the ladder gap. The
        // next check keeps every min-chunk's impact rate at or under
        // `MIN_CHUNK_IMPACT_CAP`.
        if self.init_margin <= self.maintenance_margin + self.fee_non_dom + MIN_CHUNK_IMPACT_CAP {
            return Err(MarketError::InvalidConfig);
        }
        // rate(min chunk) <= cap is equivalent to
        // impact_scalar >= min_position_notional * (SCALAR_18 / cap). An
        // overflowing product is far above any scalar.
        match self
            .min_position_notional
            .checked_mul(SCALAR_18 / MIN_CHUNK_IMPACT_CAP)
        {
            Some(floor) if self.impact_scalar >= floor => {}
            _ => return Err(MarketError::InvalidConfig),
        }

        Ok(())
    }
}

/// Returns a valid baseline `Config` for unit tests across modules.
///
/// Ratio fields are `SCALAR_18`. The notional, order, and exec fields are
/// token-dec.
#[cfg(test)]
pub(crate) fn test_config() -> Config {
    Config {
        keeper_rate: SCALAR_18 / 10, // 10%
        min_position_notional: 10_000_000,
        max_position_notional: 1_000_000_000_000,
        max_open_interest: 10_000_000_000_000,
        min_order_notional: 1_000_000,
        min_order_margin: 1_000_000,
        exec_fee: 100_000,
        fee_dom: SCALAR_18 / 200,          // 0.5%
        fee_non_dom: 3 * SCALAR_18 / 1000, // 0.3%
        impact_scalar: 1_000_000_000_000,
        max_util_open: 8 * SCALAR_18 / 10,     // 80%
        max_util_withdraw: 9 * SCALAR_18 / 10, // 90%
        init_margin: SCALAR_18 / 10,           // 10%
        maintenance_margin: SCALAR_18 / 20,    // 5%
        liq_fee: SCALAR_18 / 100,              // 1%
        notional_lock: 30,
        target_util: SCALAR_18 / 2, // 50%
        borrow_rate: 0,
        increased_borrow_rate: 0,
        funding_increase: 0,
        funding_decrease: 0,
        threshold_stable_funding: 0,
        threshold_decrease_funding: 0,
        funding_min: 0,
        funding_max: 0,
        adl_max_pnl: SCALAR_18 / 2,             // 50%
        adl_clear_target: 4 * SCALAR_18 / 10,   // 40%
        max_pnl_trader: 9 * SCALAR_18 / 10,     // 90%
        max_pnl_withdraw: 15 * SCALAR_18 / 100, // 15%
        redeem_lock: 0,
        deposit_fee: SCALAR_18 / 1000, // 10 bps
        redeem_fee: SCALAR_18 / 1000,  // 10 bps
        min_deposit: 1_000_000,
        max_vault_balance: 10_000_000_000_000,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared baseline. Each test flips exactly one field to just over
    /// its line.
    fn base_config() -> Config {
        test_config()
    }

    #[test]
    fn baseline_passes() {
        assert_eq!(base_config().check_valid(), Ok(()));
    }

    /// A zero liquidation fee is valid (no liquidation penalty).
    #[test]
    fn liq_fee_zero_allowed() {
        assert_eq!(
            Config {
                liq_fee: 0,
                ..base_config()
            }
            .check_valid(),
            Ok(())
        );
    }

    /// Each rate/fee/margin field guarded by the negative check, flipped to -1.
    #[test]
    fn negative_fields_rejected() {
        let negative = Err(MarketError::NegativeValueNotAllowed);
        assert_eq!(
            Config {
                keeper_rate: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                fee_dom: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                fee_non_dom: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                init_margin: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                maintenance_margin: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                liq_fee: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                borrow_rate: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                increased_borrow_rate: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                target_util: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                funding_increase: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                funding_decrease: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                threshold_stable_funding: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                threshold_decrease_funding: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                funding_min: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                funding_max: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                adl_max_pnl: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                adl_clear_target: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                min_order_margin: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                min_deposit: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                max_pnl_trader: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                max_pnl_withdraw: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
        assert_eq!(
            Config {
                exec_fee: -1,
                ..base_config()
            }
            .check_valid(),
            negative
        );
    }

    /// Each value pushed one unit past its upper bound.
    #[test]
    fn out_of_bounds_rejected() {
        let invalid = Err(MarketError::InvalidConfig);
        assert_eq!(
            Config {
                keeper_rate: MAX_KEEPER_RATE + 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                fee_dom: MAX_FEE_RATE + 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                fee_non_dom: MAX_FEE_RATE + 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                max_util_open: MAX_UTIL + 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                max_util_withdraw: MAX_UTIL + 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                init_margin: MAX_MARGIN + 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                liq_fee: MAX_LIQ_FEE + 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                notional_lock: MAX_NOTIONAL_LOCK + 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                notional_lock: MIN_NOTIONAL_LOCK - 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                redeem_lock: MAX_REDEEM_LOCK + 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
    }

    /// Zero sits inside every sign check. A field that may be zero passes. A
    /// field with a positive floor of its own trips that floor as
    /// `InvalidConfig`, never as `NegativeValueNotAllowed`.
    #[test]
    fn zero_is_not_a_negative_value() {
        let invalid = Err(MarketError::InvalidConfig);
        let cases = [
            (
                "keeper_rate",
                Config {
                    keeper_rate: 0,
                    ..base_config()
                },
                Ok(()),
            ),
            (
                "both trade fees",
                Config {
                    fee_dom: 0,
                    fee_non_dom: 0,
                    ..base_config()
                },
                Ok(()),
            ),
            (
                "fee_non_dom",
                Config {
                    fee_non_dom: 0,
                    ..base_config()
                },
                Ok(()),
            ),
            (
                "target_util",
                Config {
                    target_util: 0,
                    ..base_config()
                },
                Ok(()),
            ),
            (
                "deposit_fee",
                Config {
                    deposit_fee: 0,
                    ..base_config()
                },
                Ok(()),
            ),
            (
                "redeem_fee",
                Config {
                    redeem_fee: 0,
                    ..base_config()
                },
                Ok(()),
            ),
            (
                "exec_fee",
                Config {
                    exec_fee: 0,
                    ..base_config()
                },
                Ok(()),
            ),
            (
                "init_margin",
                Config {
                    init_margin: 0,
                    ..base_config()
                },
                invalid,
            ),
            (
                "maintenance_margin",
                Config {
                    maintenance_margin: 0,
                    ..base_config()
                },
                invalid,
            ),
            (
                "adl_max_pnl",
                Config {
                    adl_max_pnl: 0,
                    ..base_config()
                },
                invalid,
            ),
            (
                "adl_clear_target",
                Config {
                    adl_clear_target: 0,
                    ..base_config()
                },
                invalid,
            ),
            (
                "max_pnl_trader",
                Config {
                    max_pnl_trader: 0,
                    ..base_config()
                },
                invalid,
            ),
            (
                "max_pnl_withdraw",
                Config {
                    max_pnl_withdraw: 0,
                    ..base_config()
                },
                invalid,
            ),
        ];
        for (label, config, expected) in cases {
            assert_eq!(config.check_valid(), expected, "{}", label);
        }
    }

    /// The documented bounds, written as literal fractions of `SCALAR_18`
    /// instead of read from their constants, each admit their exact limit.
    /// The cross-field rules admit equality.
    #[test]
    fn bounds_admit_their_exact_literal_limits() {
        // 1000% APR per second: floor(10e18 / 31_536_000 s).
        let max_rate = 317_097_919_837;
        let cases = [
            (
                "keeper_rate at 50%",
                Config {
                    keeper_rate: SCALAR_18 / 2,
                    ..base_config()
                },
            ),
            (
                "both trade fees at 1%",
                Config {
                    fee_dom: SCALAR_18 / 100,
                    fee_non_dom: SCALAR_18 / 100,
                    ..base_config()
                },
            ),
            (
                "both utilization caps at 1000%",
                Config {
                    max_util_open: 10 * SCALAR_18,
                    max_util_withdraw: 10 * SCALAR_18,
                    ..base_config()
                },
            ),
            (
                "init_margin at 50%",
                Config {
                    init_margin: SCALAR_18 / 2,
                    ..base_config()
                },
            ),
            // The margin ladder lifts to 30% maintenance and 50% initial to
            // stay ordered above the fee.
            (
                "liq_fee at 25%",
                Config {
                    liq_fee: SCALAR_18 / 4,
                    maintenance_margin: 3 * SCALAR_18 / 10,
                    init_margin: SCALAR_18 / 2,
                    ..base_config()
                },
            ),
            (
                "notional_lock at 15 s",
                Config {
                    notional_lock: 15,
                    ..base_config()
                },
            ),
            (
                "notional_lock at 1 day",
                Config {
                    notional_lock: 86_400,
                    ..base_config()
                },
            ),
            (
                "redeem_lock at 30 days",
                Config {
                    redeem_lock: 2_592_000,
                    ..base_config()
                },
            ),
            (
                "adl_max_pnl at 45%",
                Config {
                    adl_max_pnl: 45 * SCALAR_18 / 100,
                    ..base_config()
                },
            ),
            (
                "adl_clear_target at 40%",
                Config {
                    adl_clear_target: 40 * SCALAR_18 / 100,
                    ..base_config()
                },
            ),
            (
                "flat borrowing curve at the rate cap",
                Config {
                    borrow_rate: max_rate,
                    increased_borrow_rate: max_rate,
                    ..base_config()
                },
            ),
            (
                "funding at the rate cap",
                Config {
                    funding_increase: max_rate,
                    funding_decrease: max_rate,
                    funding_min: max_rate,
                    funding_max: max_rate,
                    ..base_config()
                },
            ),
            (
                "open-interest ceiling at max_position_notional",
                Config {
                    max_open_interest: 1_000_000_000_000,
                    ..base_config()
                },
            ),
            (
                "order notional floor at min_position_notional",
                Config {
                    min_order_notional: 10_000_000,
                    ..base_config()
                },
            ),
        ];
        for (label, config) in cases {
            assert_eq!(config.check_valid(), Ok(()), "{}", label);
        }
    }

    /// One unit past each literal limit rejects.
    #[test]
    fn bounds_reject_one_unit_past_their_literal_limits() {
        // 1000% APR per second: floor(10e18 / 31_536_000 s).
        let max_rate = 317_097_919_837;
        let cases = [
            (
                "keeper_rate",
                Config {
                    keeper_rate: SCALAR_18 / 2 + 1,
                    ..base_config()
                },
            ),
            (
                "fee_dom",
                Config {
                    fee_dom: SCALAR_18 / 100 + 1,
                    ..base_config()
                },
            ),
            (
                "max_util_withdraw",
                Config {
                    max_util_withdraw: 10 * SCALAR_18 + 1,
                    ..base_config()
                },
            ),
            (
                "init_margin",
                Config {
                    init_margin: SCALAR_18 / 2 + 1,
                    ..base_config()
                },
            ),
            // The same lifted ladder, so only the fee bound rejects.
            (
                "liq_fee",
                Config {
                    liq_fee: SCALAR_18 / 4 + 1,
                    maintenance_margin: 3 * SCALAR_18 / 10,
                    init_margin: SCALAR_18 / 2,
                    ..base_config()
                },
            ),
            (
                "notional_lock below the floor",
                Config {
                    notional_lock: 14,
                    ..base_config()
                },
            ),
            (
                "notional_lock above the cap",
                Config {
                    notional_lock: 86_401,
                    ..base_config()
                },
            ),
            (
                "redeem_lock",
                Config {
                    redeem_lock: 2_592_001,
                    ..base_config()
                },
            ),
            (
                "adl_max_pnl",
                Config {
                    adl_max_pnl: 45 * SCALAR_18 / 100 - 1,
                    ..base_config()
                },
            ),
            (
                "adl_clear_target",
                Config {
                    adl_clear_target: 40 * SCALAR_18 / 100 - 1,
                    ..base_config()
                },
            ),
            (
                "increased_borrow_rate",
                Config {
                    increased_borrow_rate: max_rate + 1,
                    ..base_config()
                },
            ),
            (
                "funding_max",
                Config {
                    funding_max: max_rate + 1,
                    ..base_config()
                },
            ),
            (
                "funding_increase",
                Config {
                    funding_increase: max_rate + 1,
                    ..base_config()
                },
            ),
            (
                "funding_decrease",
                Config {
                    funding_decrease: max_rate + 1,
                    ..base_config()
                },
            ),
        ];
        for (label, config) in cases {
            assert_eq!(
                config.check_valid(),
                Err(MarketError::InvalidConfig),
                "{}",
                label
            );
        }
    }

    /// Floors breached: `init_margin` one below `MIN_MARGIN`, `impact_scalar`
    /// at zero.
    #[test]
    fn below_floor_rejected() {
        let invalid = Err(MarketError::InvalidConfig);
        assert_eq!(
            Config {
                init_margin: MIN_MARGIN - 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                impact_scalar: 0,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
    }

    /// Degenerate notional, order-floor, and utilization ranges.
    #[test]
    fn degenerate_ranges_rejected() {
        let invalid = Err(MarketError::InvalidConfig);
        // notional band
        assert_eq!(
            Config {
                min_position_notional: 0,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                max_position_notional: 10_000_000,
                ..base_config()
            }
            .check_valid(),
            invalid
        ); // equals min_position_notional
           // open-interest ceiling below the maximum position size
        assert_eq!(
            Config {
                max_open_interest: 999_999_999_999,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        // per-order dust floors
        assert_eq!(
            Config {
                min_order_notional: 0,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                min_order_margin: 0,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                min_order_notional: 10_000_001,
                ..base_config()
            }
            .check_valid(),
            invalid
        ); // exceeds min_position_notional
           // vault-order fill floor must be positive
        assert_eq!(
            Config {
                min_deposit: 0,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        // utilization band
        assert_eq!(
            Config {
                max_util_open: 0,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                max_util_withdraw: 7 * SCALAR_18 / 10, // 70%, below the 80% open cap
                ..base_config()
            }
            .check_valid(),
            invalid
        ); // below max_util_open
    }

    /// Dominant-fee ordering and the `liq_fee < maintenance_margin < init_margin`
    /// ladder, each violated in isolation.
    #[test]
    fn fee_and_margin_ladder_rejected() {
        let invalid = Err(MarketError::InvalidConfig);
        // dominant fee below non-dominant
        assert_eq!(
            Config {
                fee_dom: SCALAR_18 / 500,          // 0.2%
                fee_non_dom: 3 * SCALAR_18 / 1000, // 0.3%
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        // maintenance not above liq_fee
        assert_eq!(
            Config {
                maintenance_margin: SCALAR_18 / 100, // 1%
                liq_fee: SCALAR_18 / 100,            // 1%
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        // init not above maintenance
        assert_eq!(
            Config {
                init_margin: SCALAR_18 / 20,        // 5%
                maintenance_margin: SCALAR_18 / 20, // 5%
                ..base_config()
            }
            .check_valid(),
            invalid
        );
    }

    /// Borrowing-curve rules: the kink sits below full utilization, and the
    /// full-utilization rate is bounded and at least the base slope. Each is
    /// violated in isolation, and the exact boundaries pass.
    #[test]
    fn borrowing_curve_rejected_and_boundaries_pass() {
        let invalid = Err(MarketError::InvalidConfig);
        // kink at full utilization
        assert_eq!(
            Config {
                target_util: SCALAR_18,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        // full-utilization rate below the base slope
        assert_eq!(
            Config {
                borrow_rate: 2,
                increased_borrow_rate: 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        // full-utilization rate above the bound
        assert_eq!(
            Config {
                increased_borrow_rate: MAX_BORROW_RATE + 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        // boundaries: kink just under 1, flat curve exactly at the bound
        assert_eq!(
            Config {
                target_util: SCALAR_18 - 1,
                borrow_rate: MAX_BORROW_RATE,
                increased_borrow_rate: MAX_BORROW_RATE,
                ..base_config()
            }
            .check_valid(),
            Ok(())
        );
    }

    /// Funding-velocity rules: the threshold band shape, the floor and cap
    /// ordering, and the rate-cap bound. Each is violated in isolation, and
    /// the boundaries pass.
    #[test]
    fn funding_velocity_rejected_and_boundaries_pass() {
        let invalid = Err(MarketError::InvalidConfig);
        // stable threshold beyond full skew
        assert_eq!(
            Config {
                threshold_stable_funding: SCALAR_18 + 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        // decay band wider than the stable band
        assert_eq!(
            Config {
                threshold_stable_funding: SCALAR_18 / 100,
                threshold_decrease_funding: SCALAR_18 / 50,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        // charged floor above the cap
        assert_eq!(
            Config {
                funding_min: 2,
                funding_max: 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        // cap above the bound
        assert_eq!(
            Config {
                funding_max: MAX_FUNDING_RATE + 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        // accelerations above the bound
        assert_eq!(
            Config {
                funding_increase: MAX_FUNDING_RATE + 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                funding_decrease: MAX_FUNDING_RATE + 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        // boundaries: full-skew stable band, equal bands, floor at the cap,
        // accelerations at the bound
        assert_eq!(
            Config {
                threshold_stable_funding: SCALAR_18,
                threshold_decrease_funding: SCALAR_18,
                funding_min: MAX_FUNDING_RATE,
                funding_max: MAX_FUNDING_RATE,
                funding_increase: MAX_FUNDING_RATE,
                funding_decrease: MAX_FUNDING_RATE,
                ..base_config()
            }
            .check_valid(),
            Ok(())
        );
    }

    /// ADL band rules: trigger below the insolvency line, clear target at or
    /// below the trigger.
    #[test]
    fn adl_band_rejected_and_boundaries_pass() {
        let invalid = Err(MarketError::InvalidConfig);
        // trigger at the insolvency line
        assert_eq!(
            Config {
                adl_max_pnl: SCALAR_18,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        // clear target above the trigger
        assert_eq!(
            Config {
                adl_max_pnl: SCALAR_18 / 2,
                adl_clear_target: SCALAR_18 / 2 + 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        // boundary: zero-width band just under the line, the trader haircut
        // threshold raised with it to keep the ladder ordered
        assert_eq!(
            Config {
                adl_max_pnl: SCALAR_18 - 1,
                adl_clear_target: SCALAR_18 - 1,
                max_pnl_trader: SCALAR_18 - 1,
                ..base_config()
            }
            .check_valid(),
            Ok(())
        );
    }

    /// Solvency-ladder ordering: an ADL trigger above the trader haircut
    /// threshold is rejected. The boundary sits at equality.
    #[test]
    fn profit_ladder_ordering_rejected() {
        // ADL trigger (95%) above the trader haircut threshold (90%)
        assert_eq!(
            Config {
                adl_max_pnl: 95 * SCALAR_18 / 100,
                ..base_config()
            }
            .check_valid(),
            Err(MarketError::InvalidConfig)
        );
        // boundary: trigger at the haircut threshold
        assert_eq!(
            Config {
                adl_max_pnl: 9 * SCALAR_18 / 10,
                ..base_config()
            }
            .check_valid(),
            Ok(())
        );
    }

    /// Vault fill fee rules: negative rejected, each rate bounded.
    #[test]
    fn vault_fill_fees_rejected_and_boundaries_pass() {
        let invalid = Err(MarketError::InvalidConfig);
        assert_eq!(
            Config {
                deposit_fee: -1,
                ..base_config()
            }
            .check_valid(),
            Err(MarketError::NegativeValueNotAllowed)
        );
        assert_eq!(
            Config {
                redeem_fee: -1,
                ..base_config()
            }
            .check_valid(),
            Err(MarketError::NegativeValueNotAllowed)
        );
        assert_eq!(
            Config {
                deposit_fee: MAX_FEE_RATE + 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                redeem_fee: MAX_FEE_RATE + 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                deposit_fee: MAX_FEE_RATE,
                redeem_fee: MAX_FEE_RATE,
                ..base_config()
            }
            .check_valid(),
            Ok(())
        );
    }

    /// Vault gate rules: the LP PnL gates sit below the insolvency line and
    /// the vault size cap is positive.
    #[test]
    fn vault_gates_rejected_and_boundaries_pass() {
        let invalid = Err(MarketError::InvalidConfig);
        assert_eq!(
            Config {
                max_pnl_trader: SCALAR_18,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                max_pnl_withdraw: SCALAR_18,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                max_vault_balance: 0,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        // boundary: every profit-ladder dial collapsed just under the line,
        // the vault cap at the fill floor's exact 100x bound
        assert_eq!(
            Config {
                max_pnl_withdraw: SCALAR_18 - 1,
                adl_clear_target: SCALAR_18 - 1,
                adl_max_pnl: SCALAR_18 - 1,
                max_pnl_trader: SCALAR_18 - 1,
                max_vault_balance: 100_000_000,
                ..base_config()
            }
            .check_valid(),
            Ok(())
        );
    }

    /// Born-liquidatable rail, ladder-gap side: an initial margin that does
    /// not clear maintenance plus the non-dominant close fee plus
    /// `MIN_CHUNK_IMPACT_CAP` is rejected. A position at the initial-margin
    /// line would be below maintenance at its own fill price. The boundary
    /// sits one unit above the line.
    #[test]
    fn init_margin_below_close_fee_line_rejected_and_boundary_passes() {
        // 0.2% init, 0.1% maintenance, 1% close fee: the close fee alone
        // swallows the initial margin (liq_fee zeroed to keep the ladder valid).
        assert_eq!(
            Config {
                init_margin: 2 * SCALAR_18 / 1000,    // 0.2%
                maintenance_margin: SCALAR_18 / 1000, // 0.1%
                fee_dom: MAX_FEE_RATE,                // 1%
                fee_non_dom: MAX_FEE_RATE,            // 1%
                liq_fee: 0,
                ..base_config()
            }
            .check_valid(),
            Err(MarketError::InvalidConfig)
        );
        let base = base_config();
        // A ladder gap of exactly the close-fee line is still rejected.
        assert_eq!(
            Config {
                init_margin: base.maintenance_margin + base.fee_non_dom + MIN_CHUNK_IMPACT_CAP,
                ..base_config()
            }
            .check_valid(),
            Err(MarketError::InvalidConfig)
        );
        // Boundary: one unit of headroom above the line passes.
        assert_eq!(
            Config {
                init_margin: base.maintenance_margin + base.fee_non_dom + MIN_CHUNK_IMPACT_CAP + 1,
                ..base_config()
            }
            .check_valid(),
            Ok(())
        );
    }

    /// Born-liquidatable rail, impact side: `impact_scalar` must keep a
    /// minimum-size chunk's impact rate at or under `MIN_CHUNK_IMPACT_CAP`.
    /// That is, it must sit at or above
    /// `min_position_notional * (SCALAR_18 / cap)`.
    #[test]
    fn impact_scalar_below_the_chunk_floor_rejected_and_boundary_passes() {
        let invalid = Err(MarketError::InvalidConfig);
        let floor = base_config().min_position_notional * (SCALAR_18 / MIN_CHUNK_IMPACT_CAP);
        // scalar = min_position_notional: a min-chunk would pay a 100% impact
        // rate, far past the cap.
        assert_eq!(
            Config {
                impact_scalar: 10_000_000,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        // Boundary: one unit below the floor rejects, the floor itself passes
        // (the min-chunk rate sits exactly at the cap).
        assert_eq!(
            Config {
                impact_scalar: floor - 1,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        assert_eq!(
            Config {
                impact_scalar: floor,
                ..base_config()
            }
            .check_valid(),
            Ok(())
        );
    }

    /// Born-liquidatable rail, extreme-magnitude arms: the constant-anchored
    /// comparison never wraps. An oversized minimum chunk rejects, a huge
    /// scalar passes, and a fat initial margin buys no exemption from the
    /// chunk cap.
    #[test]
    fn chunk_floor_extreme_magnitudes() {
        // Max initial margin does not exempt a degenerate scalar: the cap is
        // a hard floor on impact_scalar regardless of ladder headroom.
        assert_eq!(
            Config {
                init_margin: MAX_MARGIN,
                impact_scalar: 1,
                ..base_config()
            }
            .check_valid(),
            Err(MarketError::InvalidConfig)
        );
        // impact_scalar at i128::MAX sits far above any floor.
        assert_eq!(
            Config {
                impact_scalar: i128::MAX,
                ..base_config()
            }
            .check_valid(),
            Ok(())
        );
        // min_position_notional at i128::MAX / 2 fails the
        // `MAX_OPEN_INTEREST` bound before the floor product can overflow.
        assert_eq!(
            Config {
                min_position_notional: i128::MAX / 2,
                max_position_notional: i128::MAX / 2 + 1,
                max_open_interest: i128::MAX,
                ..base_config()
            }
            .check_valid(),
            Err(MarketError::InvalidConfig)
        );
        // Large but sane magnitudes: min chunk 1e15 against scalar 1e19 is a
        // 0.01% rate, inside the 0.1% cap, with OI at `MAX_OPEN_INTEREST`.
        assert_eq!(
            Config {
                min_position_notional: 1_000_000_000_000_000,
                max_position_notional: 2_000_000_000_000_000,
                max_open_interest: MAX_OPEN_INTEREST,
                impact_scalar: 10_000_000_000_000_000_000,
                ..base_config()
            }
            .check_valid(),
            Ok(())
        );
    }

    /// The open-interest ceiling admits `MAX_OPEN_INTEREST` and rejects one
    /// unit above it.
    #[test]
    fn max_open_interest_admits_the_bound_and_rejects_one_unit_above() {
        assert_eq!(
            Config {
                max_open_interest: MAX_OPEN_INTEREST,
                ..base_config()
            }
            .check_valid(),
            Ok(())
        );
        assert_eq!(
            Config {
                max_open_interest: MAX_OPEN_INTEREST + 1,
                ..base_config()
            }
            .check_valid(),
            Err(MarketError::InvalidConfig)
        );
    }

    /// The redeem gate must sit at or below the ADL clear target, so a
    /// permitted redeem leaves every side at or below it. The boundary sits
    /// at equality.
    #[test]
    fn redeem_gate_above_adl_clear_target_rejected_and_boundary_passes() {
        // Gate 45% above the 40% clear target (still below the 50% trigger).
        assert_eq!(
            Config {
                max_pnl_withdraw: 45 * SCALAR_18 / 100,
                ..base_config()
            }
            .check_valid(),
            Err(MarketError::InvalidConfig)
        );
        // Boundary: gate exactly at the clear target.
        assert_eq!(
            Config {
                max_pnl_withdraw: 4 * SCALAR_18 / 10,
                ..base_config()
            }
            .check_valid(),
            Ok(())
        );
    }

    /// The keeper execution fee cannot exceed the smallest order's margin, or
    /// dust orders are net-negative escrow. The boundary sits at equality.
    #[test]
    fn exec_fee_above_min_order_margin_rejected_and_boundary_passes() {
        // One unit above the 1_000_000 min_order_margin.
        assert_eq!(
            Config {
                exec_fee: 1_000_001,
                ..base_config()
            }
            .check_valid(),
            Err(MarketError::InvalidConfig)
        );
        // Boundary: fee exactly at the margin floor.
        assert_eq!(
            Config {
                exec_fee: 1_000_000,
                ..base_config()
            }
            .check_valid(),
            Ok(())
        );
    }

    use crate::engine::market_data::MarketData;
    use crate::engine::status::Status;
    use soroban_sdk::testutils::Ledger;
    use soroban_sdk::{contract, contractimpl};

    #[contract]
    struct Harness;

    // A thin entry point so a test can capture `set_config`'s error through
    // the generated client's `try_` method. The trap reverts, so the stored
    // config stays in place for the next case.
    #[contractimpl]
    impl Harness {
        pub fn set(e: Env, config: Config) {
            set_config(&e, &config);
        }
    }

    /// Seeds the storage `set_config` reads: the current config, a market
    /// record whose last borrowing accrual is stale (update 0 against ledger
    /// timestamp 100), and `status`.
    fn seed_stale_accrual(e: &Env, status: Status) {
        storage::set_config(e, &test_config());
        storage::set_market_data(e, &MarketData::default());
        storage::set_status(e, &status);
        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
    }

    /// A borrowing-param change against `test_config` (both kink rates raised).
    fn raised_borrow_config() -> Config {
        Config {
            borrow_rate: 1,
            increased_borrow_rate: 1,
            ..test_config()
        }
    }

    /// A funding-velocity-param change against `test_config`.
    fn raised_funding_config() -> Config {
        Config {
            funding_increase: test_config().funding_increase + 1,
            ..test_config()
        }
    }

    // Funding params share the accrual guard: a velocity change over a stale
    // accrual traps like a borrowing change (`set_config` no longer accrues
    // funding inline. Only the price-bearing `accrue` moves the shared
    // clock).
    #[test]
    #[should_panic(expected = "#703")]
    fn set_config_funding_change_requires_accrual_when_active() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            seed_stale_accrual(&e, Status::Active);
            set_config(&e, &raised_funding_config());
        });
    }

    // A same-ledger accrual admits the funding change, mirroring borrowing.
    #[test]
    fn set_config_funding_change_passes_with_same_ledger_accrual() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            storage::set_config(&e, &test_config());
            e.ledger().with_mut(|ledger| ledger.timestamp = 100);
            storage::set_market_data(
                &e,
                &MarketData {
                    accrued_at: 100,
                    ..Default::default()
                },
            );
            storage::set_status(&e, &Status::Active);
            set_config(&e, &raised_funding_config());
            assert_eq!(
                storage::get_config(&e).funding_increase,
                test_config().funding_increase + 1
            );
        });
    }

    // `Frozen` waives the guard for funding params exactly as for borrowing.
    #[test]
    fn set_config_funding_change_waived_when_frozen() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            seed_stale_accrual(&e, Status::Frozen);
            set_config(&e, &raised_funding_config());
            assert_eq!(
                storage::get_config(&e).funding_increase,
                test_config().funding_increase + 1
            );
        });
    }

    // `Frozen` waives the same-ledger accrual requirement: the borrowing
    // change lands, and the first accrual at unfreeze prices the whole frozen
    // window at the new rates.
    #[test]
    fn set_config_borrowing_change_waived_when_frozen() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            seed_stale_accrual(&e, Status::Frozen);
            set_config(&e, &raised_borrow_config());
            assert_eq!(storage::get_config(&e).borrow_rate, 1);
        });
    }

    // The guard is unchanged on an active market: a borrowing change over a
    // stale accrual still traps.
    #[test]
    #[should_panic(expected = "#703")]
    fn set_config_borrowing_change_requires_accrual_when_active() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            seed_stale_accrual(&e, Status::Active);
            set_config(&e, &raised_borrow_config());
        });
    }

    // `Retired` is terminal, not the emergency freeze: no waiver.
    #[test]
    #[should_panic(expected = "#703")]
    fn set_config_borrowing_change_requires_accrual_when_retired() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            seed_stale_accrual(&e, Status::Retired);
            set_config(&e, &raised_borrow_config());
        });
    }

    // A same-ledger accrual still satisfies the guard on an active market.
    #[test]
    fn set_config_borrowing_change_passes_with_same_ledger_accrual() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            storage::set_config(&e, &test_config());
            e.ledger().with_mut(|ledger| ledger.timestamp = 100);
            storage::set_market_data(
                &e,
                &MarketData {
                    accrued_at: 100,
                    ..Default::default()
                },
            );
            storage::set_status(&e, &Status::Active);
            set_config(&e, &raised_borrow_config());
            assert_eq!(storage::get_config(&e).borrow_rate, 1);
        });
    }

    /// `test_config` with every rate parameter non-zero, so each one can move
    /// alone and stay valid.
    fn live_rates_config() -> Config {
        Config {
            borrow_rate: 1_000,
            increased_borrow_rate: 10_000,
            funding_increase: 1_000,
            funding_decrease: 500,
            threshold_stable_funding: 5 * SCALAR_18 / 100,
            threshold_decrease_funding: 2 * SCALAR_18 / 100,
            funding_min: 10,
            funding_max: 1_000_000,
            ..test_config()
        }
    }

    // Each rate parameter guards the accrual on its own. A one-unit move of
    // any single borrowing-curve or funding-velocity field over a stale
    // accrual traps MarketNotAccrued on an active market.
    #[test]
    fn set_config_each_rate_param_alone_requires_accrual_when_active() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        let base = live_rates_config();
        e.as_contract(&contract_id, || {
            storage::set_config(&e, &base);
            storage::set_market_data(&e, &MarketData::default());
            storage::set_status(&e, &Status::Active);
        });
        e.ledger().with_mut(|ledger| ledger.timestamp = 100);

        let cases = [
            (
                "target_util",
                Config {
                    target_util: base.target_util + 1,
                    ..base.clone()
                },
            ),
            (
                "borrow_rate",
                Config {
                    borrow_rate: base.borrow_rate + 1,
                    ..base.clone()
                },
            ),
            (
                "increased_borrow_rate",
                Config {
                    increased_borrow_rate: base.increased_borrow_rate + 1,
                    ..base.clone()
                },
            ),
            (
                "max_util_open",
                Config {
                    max_util_open: base.max_util_open + 1,
                    ..base.clone()
                },
            ),
            (
                "funding_increase",
                Config {
                    funding_increase: base.funding_increase + 1,
                    ..base.clone()
                },
            ),
            (
                "funding_decrease",
                Config {
                    funding_decrease: base.funding_decrease + 1,
                    ..base.clone()
                },
            ),
            (
                "threshold_stable_funding",
                Config {
                    threshold_stable_funding: base.threshold_stable_funding + 1,
                    ..base.clone()
                },
            ),
            (
                "threshold_decrease_funding",
                Config {
                    threshold_decrease_funding: base.threshold_decrease_funding + 1,
                    ..base.clone()
                },
            ),
            (
                "funding_min",
                Config {
                    funding_min: base.funding_min + 1,
                    ..base.clone()
                },
            ),
            (
                "funding_max",
                Config {
                    funding_max: base.funding_max + 1,
                    ..base.clone()
                },
            ),
        ];
        let client = HarnessClient::new(&e, &contract_id);
        for (label, changed) in cases {
            assert_eq!(changed.check_valid(), Ok(()), "{} stays valid", label);
            assert_eq!(
                client.try_set(&changed).unwrap_err().unwrap(),
                MarketError::MarketNotAccrued.into(),
                "{}",
                label
            );
        }
    }

    // Only the rate parameters need the accrual. The keeper cut and the
    // withdrawal cap price no accrual window, so changing them lands over the
    // same stale accrual.
    #[test]
    fn set_config_non_rate_change_skips_the_accrual_guard() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            seed_stale_accrual(&e, Status::Active);
            set_config(
                &e,
                &Config {
                    keeper_rate: SCALAR_18 / 5,
                    max_util_withdraw: SCALAR_18,
                    ..test_config()
                },
            );
            let stored = storage::get_config(&e);
            assert_eq!(stored.keeper_rate, SCALAR_18 / 5);
            assert_eq!(stored.max_util_withdraw, SCALAR_18);
        });
    }

    /// The fill floor is bounded to a fraction of the vault cap, including on
    /// a multiply overflow.
    #[test]
    fn min_deposit_cap_rejected_and_boundary_passes() {
        let invalid = Err(MarketError::InvalidConfig);
        // one unit past max_vault_balance / 100
        assert_eq!(
            Config {
                min_deposit: 100_000_000_001,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        // an overflowing product is rejected, not wrapped
        assert_eq!(
            Config {
                min_deposit: i128::MAX / 50,
                ..base_config()
            }
            .check_valid(),
            invalid
        );
        // boundary: exactly max_vault_balance / 100
        assert_eq!(
            Config {
                min_deposit: 100_000_000_000,
                ..base_config()
            }
            .check_valid(),
            Ok(())
        );
    }
}
