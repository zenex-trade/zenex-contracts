//! The [`Position`] row and its increase, decrease, and liquidate paths
//! behind the keeper entries. Margin, notional, and token changes mirror
//! the market's per-side aggregates.

use crate::engine::constants::MAX_ORDERS_PER_SIDE;
use crate::engine::market::Market;
use crate::engine::math;
use crate::engine::order::OrderKind;
use crate::errors::MarketError;
use crate::events::{CancelOrder, CloseFill, DecreaseFill, IncreaseFill, OpenFill};
use crate::storage;
use soroban_sdk::{contracttype, panic_with_error, Address, Env, Vec};

/// A netted trader position, one per `(user, is_long)`, which is the
/// storage key.
///
/// PnL is implied: `tokens * price - notional` for a long, and the inverse
/// for a short. Zero `notional` means no open position, and the all-zero
/// row is the canonical closed state.
#[derive(Clone, Debug)]
#[contracttype]
pub struct Position {
    /// The posted margin (token-dec).
    pub margin: i128,
    /// The size in quote (token-dec).
    pub notional: i128,
    /// The size in base (base-dec). The implied entry is `notional / tokens`.
    pub tokens: i128,
    /// The funding index snapshot at the last change (`SCALAR_18`).
    pub funding_idx: i128,
    /// The borrowing index snapshot at the last change (`SCALAR_18`).
    pub borrowing_idx: i128,
    /// The notional under the decrease lock (token-dec).
    pub locked_notional: i128,
    /// The lock deadline timestamp (seconds). `locked_notional` counts while
    /// `now < unlocks_at`.
    pub unlocks_at: u64,
    /// The `publish_time` of the last fill's price (seconds). It is the fill
    /// and force-close price floor.
    pub priced_at: u64,
    /// The pending decrease order ids on the side, at most
    /// `MAX_ORDERS_PER_SIDE`. The list is swept, with refunds, when the
    /// position closes.
    pub decrease_orders: Vec<u32>,
}

/// The itemized costs one action settles (token-dec). The caller debits the
/// margin through [`Fees::debit`].
pub struct Fees {
    /// The skew-split trade fee.
    pub base: i128,
    /// The size-quadratic impact fee on the fill's full notional.
    pub impact: i128,
    /// The signed funding accrual. Positive is paid and banked to
    /// `credit_pool`. Negative is earned and claimable.
    pub funding: i128,
    /// The borrowing accrual, non-negative.
    pub borrowing: i128,
}

impl Fees {
    /// Returns the total debited from the margin (token-dec):
    /// `base + impact + borrowing`, plus `funding` when paid.
    pub fn debit(&self) -> i128 {
        self.base + self.impact + self.borrowing + self.funding.max(0)
    }
}

/// A position's close-grade settlement numbers from [`Position::settle`]:
/// the full-size trade fees plus accruals, the haircut PnL, and the settled
/// equity `margin - fees.debit() + pnl` (signed, token-dec).
struct Settled {
    fees: Fees,
    pnl: i128,
    equity: i128,
}

/// The outcome of an increase (token-dec).
pub struct Increase {
    /// The base size bought at `price.entry` (base-dec).
    pub tokens: i128,
    /// The itemized costs the increase settled.
    pub fees: Fees,
}

impl Increase {
    /// Publishes the fill receipt. With `opened` set, the side was empty
    /// before the fill, and the receipt is an [`OpenFill`]. Otherwise it is
    /// an [`IncreaseFill`]. `notional` and `margin` are the filled order's
    /// magnitudes.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn publish_receipt(
        &self,
        e: &Env,
        user: &Address,
        id: u32,
        is_long: bool,
        keeper: &Address,
        price: i128,
        notional: i128,
        margin: i128,
        opened: bool,
    ) {
        if opened {
            // There are no funding and borrowing keys. Accruals settle over
            // the pre-fill notional, which is 0 when the side was empty.
            OpenFill {
                user: user.clone(),
                id,
                is_long,
                keeper: keeper.clone(),
                price,
                notional,
                tokens: self.tokens,
                margin,
                base_fee: self.fees.base,
                impact_fee: self.fees.impact,
            }
            .publish(e);
        } else {
            IncreaseFill {
                user: user.clone(),
                id,
                is_long,
                keeper: keeper.clone(),
                price,
                notional,
                tokens: self.tokens,
                margin,
                base_fee: self.fees.base,
                impact_fee: self.fees.impact,
                funding: self.fees.funding,
                borrowing: self.fees.borrowing,
            }
            .publish(e);
        }
    }
}

/// The outcome of a decrease (token-dec).
pub struct Decrease {
    /// The closed size: the request clamped to the position.
    pub notional: i128,
    /// The base size closed (base-dec).
    pub tokens: i128,
    /// The gross margin leg: the requested withdrawal on a partial close, or
    /// the freed margin on a full close.
    pub margin: i128,
    /// The realized PnL on the closed fraction, post-haircut, signed, gross
    /// of the settled costs.
    pub pnl: i128,
    /// The payout owed to the trader, net of the settled costs.
    pub returned: i128,
    /// The shortfall the vault absorbs: fees and losses past the freed
    /// margin.
    pub bad_debt: i128,
    /// The itemized costs the decrease settled.
    pub fees: Fees,
}

impl Decrease {
    /// Publishes the fill receipt. With `closed` set it is a [`CloseFill`].
    /// Otherwise it is a [`DecreaseFill`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn publish_receipt(
        &self,
        e: &Env,
        user: &Address,
        id: u32,
        is_long: bool,
        keeper: &Address,
        price: i128,
        closed: bool,
    ) {
        if closed {
            CloseFill {
                user: user.clone(),
                id,
                is_long,
                keeper: keeper.clone(),
                price,
                notional: self.notional,
                tokens: self.tokens,
                margin: self.margin,
                pnl: self.pnl,
                base_fee: self.fees.base,
                impact_fee: self.fees.impact,
                funding: self.fees.funding,
                borrowing: self.fees.borrowing,
                bad_debt: self.bad_debt,
                returned: self.returned,
            }
            .publish(e);
        } else {
            DecreaseFill {
                user: user.clone(),
                id,
                is_long,
                keeper: keeper.clone(),
                price,
                notional: self.notional,
                tokens: self.tokens,
                margin: self.margin,
                pnl: self.pnl,
                base_fee: self.fees.base,
                impact_fee: self.fees.impact,
                funding: self.fees.funding,
                borrowing: self.fees.borrowing,
                returned: self.returned,
            }
            .publish(e);
        }
    }
}

/// The outcome of a liquidation (token-dec).
pub struct Liquidation {
    /// The force-closed size.
    pub notional: i128,
    /// The force-closed base size (base-dec).
    pub tokens: i128,
    /// The freed margin, gross of the settled costs.
    pub margin: i128,
    /// The realized PnL over the whole position, post-haircut, signed.
    pub pnl: i128,
    /// The shortfall the vault absorbs: fees and losses past the freed
    /// margin.
    pub bad_debt: i128,
    /// The post-fee remainder owed to the trader, net of the liquidation
    /// fee.
    pub returned: i128,
    /// The liquidation fee charged: `min(max(equity, 0), ceil(liq_fee * notional))`.
    pub liq_fee: i128,
    /// The itemized costs the liquidation settled.
    pub fees: Fees,
}

impl Position {
    /// Loads the netted position for `(user, is_long)` from storage, zeroed
    /// if none exists.
    pub(crate) fn load(e: &Env, user: &Address, is_long: bool) -> Position {
        storage::get_position(e, user, is_long)
    }

    /// Returns a zeroed position with an empty decrease list, the canonical
    /// closed and never-opened state.
    pub(crate) fn zeroed(e: &Env) -> Position {
        Position {
            margin: 0,
            notional: 0,
            tokens: 0,
            funding_idx: 0,
            borrowing_idx: 0,
            locked_notional: 0,
            unlocks_at: 0,
            priced_at: 0,
            decrease_orders: Vec::new(e),
        }
    }

    /// Appends a pending decrease order id to the side's list.
    ///
    /// # Errors
    /// - [`MarketError::TooManyOrders`] if the side already holds
    ///   [`MAX_ORDERS_PER_SIDE`] ids.
    pub(crate) fn push_decrease(&mut self, e: &Env, id: u32) {
        // The cap bounds the closure sweep, so a max-parked liquidation
        // stays inside per-tx limits.
        if self.decrease_orders.len() >= MAX_ORDERS_PER_SIDE {
            panic_with_error!(e, MarketError::TooManyOrders);
        }
        self.decrease_orders.push_back(id);
    }

    /// Drops a pending decrease order id from the side's list and leaves the
    /// rest in order.
    pub(crate) fn remove_decrease(&mut self, e: &Env, id: u32) {
        let mut kept = Vec::new(e);
        for existing in self.decrease_orders.iter() {
            if existing != id {
                kept.push_back(existing);
            }
        }
        self.decrease_orders = kept;
    }

    /// Traps unless this position is open.
    ///
    /// # Errors
    /// - [`MarketError::PositionNotFound`] if `notional` is zero.
    pub(crate) fn require_exists(&self, e: &Env) {
        if self.notional == 0 {
            panic_with_error!(e, MarketError::PositionNotFound);
        }
    }

    /// Traps if `publish_time` is older than the price the position was last
    /// marked against. This is the fill and force-close price floor.
    ///
    /// # Errors
    /// - [`MarketError::StalePrice`] if `publish_time` is before `priced_at`.
    pub(crate) fn require_price_not_stale(&self, e: &Env, publish_time: u64) {
        if publish_time < self.priced_at {
            panic_with_error!(e, MarketError::StalePrice);
        }
    }

    /// Persists the position and returns the escrow (token-dec) the caller
    /// owes `user`. It is `0` when the position survives.
    ///
    /// A closed position persists as the zeroed row, which keeps the entry's
    /// prepaid rent alive for a later open. Closure sweeps every pending
    /// decrease order on the side: each emits its [`CancelOrder`] receipt,
    /// and the escrows sum into the refund.
    pub(crate) fn store(&self, e: &Env, user: &Address, is_long: bool) -> i128 {
        let closed = self.notional == 0 && self.margin == 0 && self.tokens == 0;
        let mut refund = 0;
        if closed {
            // The side is flat, so every listed decrease order is stale.
            for id in self.decrease_orders.iter() {
                let order = storage::get_order(e, user, id);
                let escrow = order.escrow_amount(OrderKind::from_u32(e, order.kind));
                refund += escrow;
                storage::remove_order(e, user, id);
                CancelOrder {
                    user: user.clone(),
                    id,
                    refund: escrow,
                }
                .publish(e);
            }
        }
        let position = if closed {
            Position::zeroed(e)
        } else {
            self.clone()
        };
        storage::set_position(e, user, is_long, &position);
        refund
    }

    /// Returns the notional still under the decrease lock at `now`
    /// (token-dec). It is zero once the `unlocks_at` deadline has passed.
    fn locked(&self, now: u64) -> i128 {
        if now < self.unlocks_at {
            self.locked_notional
        } else {
            0
        }
    }

    /// Returns the margin required at `margin_ratio` of notional:
    /// `ceil(margin_ratio * notional)` (token-dec).
    fn margin_requirement(&self, e: &Env, margin_ratio: i128) -> i128 {
        math::apply_factor_ceil(e, self.notional, margin_ratio)
    }

    /// Checks that the position sits within the notional band, its margin
    /// covers the initial margin, and its settled equity clears the
    /// maintenance margin. It traps on the first violation.
    ///
    /// # Errors
    /// - [`MarketError::NotionalBelowMinimum`] if notional is below
    ///   `min_position_notional`.
    /// - [`MarketError::NotionalAboveMaximum`] if notional exceeds
    ///   `max_position_notional`.
    /// - [`MarketError::InsufficientMargin`] if `margin` is below
    ///   `init_margin * notional`, which is skipped when `is_adl`.
    /// - [`MarketError::PositionLiquidatable`] if the settled equity is below
    ///   `maintenance_margin * notional`.
    fn require_valid(
        &mut self,
        e: &Env,
        market: &mut Market,
        user: &Address,
        is_long: bool,
        is_adl: bool,
    ) {
        // Size stays inside the position band.
        if self.notional < market.config.min_position_notional {
            panic_with_error!(e, MarketError::NotionalBelowMinimum);
        }
        if self.notional > market.config.max_position_notional {
            panic_with_error!(e, MarketError::NotionalAboveMaximum);
        }
        // The initial margin is measured PnL-free. An ADL remainder is not a
        // voluntary restructure, so it skips this floor.
        if !is_adl && self.margin < self.margin_requirement(e, market.config.init_margin) {
            panic_with_error!(e, MarketError::InsufficientMargin);
        }
        // The maintenance line is settled and fee-inclusive. A repeat settle
        // banks nothing. It re-prices the current state.
        let settled = self.settle(e, market, user, is_long);
        // No action may leave a liquidatable position behind.
        if self.is_liquidatable(e, market, &settled) {
            panic_with_error!(e, MarketError::PositionLiquidatable);
        }
    }

    /// Grows the position by `notional` size, posts `margin`, and returns
    /// the [`Increase`] outcome.
    ///
    /// Size is bought at `price.entry`, and the implied entry blends. Added
    /// size is locked against decreases for `notional_lock` seconds. A new
    /// increase folds into the live lock and resets its deadline.
    ///
    /// # Errors
    /// - Refer to [`Position::require_valid`] errors.
    /// - [`MarketError::OpenInterestExceeded`] if a positive `notional` leaves
    ///   the side's open interest above `max_open_interest`.
    /// - [`MarketError::SizeRoundsToZero`] if a positive `notional` buys no
    ///   base size at the entry price.
    pub(crate) fn increase(
        &mut self,
        e: &Env,
        market: &mut Market,
        user: &Address,
        is_long: bool,
        notional: i128,
        margin: i128,
    ) -> Increase {
        let entry_price = market.price.entry(is_long);
        // Entry size rounds against the trader, like the close mark in
        // [`math::pnl`]. A floored short size is worth less than its
        // notional, so an unchanged-price round trip would pay the trader
        // up to one base unit.
        let tokens_added = if is_long {
            math::to_tokens_floor(e, notional, entry_price)
        } else {
            math::to_tokens_ceil(e, notional, entry_price)
        };
        // A sized increase that buys no base unit would settle at its whole
        // notional with no exposure behind it. A margin-only top-up carries
        // no notional and no size, so it passes.
        if notional > 0 && tokens_added == 0 {
            panic_with_error!(e, MarketError::SizeRoundsToZero);
        }
        let (funding, borrowing) = self.settle_accruals(e, market, user, is_long);
        let (base, impact) = market.trade_fees(e, is_long, notional, tokens_added);
        let fees = Fees {
            base,
            impact,
            funding,
            borrowing,
        };

        self.tokens += tokens_added;
        market.data.tokens.add(is_long, tokens_added);

        self.set_checkpoint(
            market,
            is_long,
            self.notional + notional,
            self.borrowing_idx,
        );

        // The added margin, less the settled costs.
        let margin_added = margin - fees.debit();
        self.margin += margin_added;
        market.data.margin.add(is_long, margin_added);

        if notional > 0 {
            // An expired bucket stops counting and is replaced. A live one
            // grows.
            self.locked_notional = self.locked(market.now) + notional;
            self.unlocks_at = market.now + market.config.notional_lock;
        }
        // The position's price floor anchors at this fill's price.
        self.priced_at = market.price.publish_time;

        self.require_valid(e, market, user, is_long, false);

        // The per-side open interest caps size growth. A pure margin top-up,
        // with no notional, does not grow it.
        if notional > 0 && market.data.notional.get(is_long) > market.config.max_open_interest {
            panic_with_error!(e, MarketError::OpenInterestExceeded);
        }

        Increase {
            tokens: tokens_added,
            fees,
        }
    }

    /// Closes `notional` of size, clamped to the position, withdraws `margin`,
    /// and returns the [`Decrease`] outcome.
    ///
    /// The closed fraction realizes PnL pro-rata at `price.exit` with the
    /// implied entry preserved, and a profit is subject to
    /// [`Market::haircut_pnl`]. The settled costs pay from the realized
    /// profit first, then the surviving margin, and the withdrawal caps at
    /// the margin that survives. A request for the whole position, or one
    /// whose survivor would sit below `min_position_notional`, clamps to a
    /// full close. With `is_adl` the remainder skips the initial-margin
    /// floor.
    ///
    /// # Errors
    /// - [`MarketError::PositionNotFound`] if no position is open.
    /// - [`MarketError::PositionLiquidatable`] if the settled equity is below
    ///   the maintenance margin before the close. Liquidation is the only
    ///   legal transition for a liquidatable position.
    /// - [`MarketError::NotionalLocked`] if the requested close exceeds the
    ///   unlocked notional, or a close clamped to a full close finds any
    ///   notional still locked.
    /// - Refer to [`Position::require_valid`] errors for the survivor of a
    ///   partial close.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn decrease(
        &mut self,
        e: &Env,
        market: &mut Market,
        user: &Address,
        is_long: bool,
        notional: i128,
        margin: i128,
        is_adl: bool,
    ) -> Decrease {
        self.require_exists(e);

        // The position settles once. The gate, the full close, and the
        // partial's accruals all consume the same numbers.
        let settled = self.settle(e, market, user, is_long);
        if self.is_liquidatable(e, market, &settled) {
            panic_with_error!(e, MarketError::PositionLiquidatable);
        }

        let locked = self.locked(market.now);

        // A request for the whole position, or one whose survivor would sit
        // below the minimum and could never validate, is a full close.
        if notional >= self.notional
            || self.notional - notional < market.config.min_position_notional
        {
            // Any locked notional rejects a full close.
            if locked > 0 {
                panic_with_error!(e, MarketError::NotionalLocked);
            }
            return self.close_settled(market, is_long, settled);
        }

        // A partial close is only allowed on the unlocked fraction.
        if notional > self.notional - locked {
            panic_with_error!(e, MarketError::NotionalLocked);
        }

        let closed_tokens = math::prorate(e, self.tokens, notional, self.notional);
        // Impact is quadratic in fill size, so the gate's full-size fees do
        // not apply to a partial.
        let (base, impact) = market.trade_fees(e, is_long, -notional, -closed_tokens);
        let fees = Fees {
            base,
            impact,
            funding: settled.fees.funding,
            borrowing: settled.fees.borrowing,
        };

        let pnl = math::pnl(e, closed_tokens, notional, &market.price, is_long);
        let capped_pnl = market.haircut_pnl(e, is_long, pnl);

        let total_fees = fees.debit();
        let profit = capped_pnl.max(0);
        let loss = capped_pnl.min(0);
        // The fees pay from the realized profit first. The margin pays the
        // uncovered rest, so a fee never reduces the withdrawal.
        let covered = total_fees.min(profit);
        let uncovered = total_fees - covered;
        // The withdrawal claims last: it caps at the margin that survives
        // the loss and the uncovered fees, so it never mints bad debt.
        let withdrawal = margin.min((self.margin + loss - uncovered).max(0));

        // Margin floors at zero. The excess, a loss and fees past the margin,
        // is `bad_debt` drawn from the vault at settlement.
        let margin_change = loss - uncovered - withdrawal;
        let next_margin = self.margin + margin_change;
        let bad_debt = (-next_margin).max(0);
        self.margin = next_margin.max(0);
        market.data.margin.add(is_long, margin_change + bad_debt);

        // The position's price floor anchors at this fill's price.
        self.priced_at = market.price.publish_time;

        self.set_checkpoint(
            market,
            is_long,
            self.notional - notional,
            self.borrowing_idx,
        );
        self.tokens -= closed_tokens;
        market.data.tokens.add(is_long, -closed_tokens);

        // The survivor must still validate.
        self.require_valid(e, market, user, is_long, is_adl);

        Decrease {
            notional,
            tokens: closed_tokens,
            margin,
            pnl: capped_pnl,
            // The paid withdrawal plus the profit the fees did not consume.
            returned: withdrawal + (profit - covered),
            bad_debt,
            fees,
        }
    }

    /// Closes the whole position against its settled numbers and returns the
    /// [`Decrease`] outcome.
    ///
    /// It fully unwinds size, margin, and the lock, and mirrors the market
    /// aggregates. The payout is the settled equity floored at zero. Any
    /// shortfall becomes `bad_debt`.
    fn close_settled(&mut self, market: &mut Market, is_long: bool, settled: Settled) -> Decrease {
        let notional = self.notional;
        let tokens = self.tokens;
        let returned = settled.equity.max(0);
        let bad_debt = (-settled.equity).max(0);

        self.set_checkpoint(market, is_long, 0, self.borrowing_idx);
        market.data.tokens.add(is_long, -tokens);
        self.tokens = 0;

        market.data.margin.add(is_long, -self.margin);
        let gross_margin = self.margin;
        self.margin = 0;
        self.locked_notional = 0;
        self.unlocks_at = 0;

        Decrease {
            notional,
            tokens,
            margin: gross_margin,
            pnl: settled.pnl,
            returned,
            bad_debt,
            fees: settled.fees,
        }
    }

    /// Force-closes the whole position at `price.exit` and returns the
    /// [`Liquidation`] outcome.
    ///
    /// `force` is the delist wind-down waiver: it skips the eligibility
    /// check but still charges the fee. Every liquidation charges
    /// `min(max(equity, 0), ceil(liq_fee * notional))` and returns the rest.
    ///
    /// # Errors
    /// - [`MarketError::PositionNotFound`] if no position is open.
    /// - [`MarketError::NotLiquidatable`] if equity is at or above
    ///   `maintenance_margin * notional` and `force` is not set.
    pub(crate) fn liquidate(
        &mut self,
        e: &Env,
        market: &mut Market,
        user: &Address,
        is_long: bool,
        force: bool,
    ) -> Liquidation {
        self.require_exists(e);

        let settled = self.settle(e, market, user, is_long);

        if !force && !self.is_liquidatable(e, market, &settled) {
            panic_with_error!(e, MarketError::NotLiquidatable);
        }

        let closed = self.close_settled(market, is_long, settled);
        let equity = closed.returned;

        // Capped at the freed equity, so the fee can never mint bad debt or
        // a self-liquidation profit.
        let liq_fee =
            math::apply_factor_ceil(e, closed.notional, market.config.liq_fee).min(equity);

        Liquidation {
            notional: closed.notional,
            tokens: closed.tokens,
            margin: closed.margin,
            pnl: closed.pnl,
            bad_debt: closed.bad_debt,
            returned: equity - liq_fee,
            liq_fee,
            fees: closed.fees,
        }
    }

    /// Banks the position's accrued funding and borrowing against the market
    /// indices (token-dec), re-snapshots, and returns `(funding, borrowing)`.
    ///
    /// The accruals price the pre-action size, so call it before mutating
    /// the position.
    fn settle_accruals(
        &mut self,
        e: &Env,
        market: &mut Market,
        user: &Address,
        is_long: bool,
    ) -> (i128, i128) {
        let funding_idx = market.data.funding_idx.get(is_long);
        let borrowing_idx = market.data.borrowing_idx.get(is_long);
        // Already banked to these indices, so nothing accrues.
        if funding_idx == self.funding_idx && borrowing_idx == self.borrowing_idx {
            return (0, 0);
        }
        let funding = math::accrued_amount(e, self.notional, funding_idx - self.funding_idx);
        let borrowing = math::accrued_amount(e, self.notional, borrowing_idx - self.borrowing_idx);
        if funding > 0 {
            market.data.credit_pool += funding;
        } else if funding < 0 {
            let earned = funding.abs();
            market.data.credit_owed += earned;
            storage::add_claimable_credit(e, user, earned);
        }
        self.funding_idx = funding_idx;
        self.set_checkpoint(market, is_long, self.notional, borrowing_idx);
        (funding, borrowing)
    }

    /// Sets the position's `notional` (token-dec) and `borrowing_idx`
    /// (`SCALAR_18`), and mirrors the change into `is_long`'s open interest
    /// and borrowing weight.
    ///
    /// Every change to a position's `notional` or `borrowing_idx` goes
    /// through it. It is the only writer of
    /// [`MarketData::borrowing_weight`](crate::engine::market_data::MarketData::borrowing_weight).
    fn set_checkpoint(
        &mut self,
        market: &mut Market,
        is_long: bool,
        notional: i128,
        borrowing_idx: i128,
    ) {
        // The whole old contribution leaves and the whole new one enters.
        // The release profile sets `overflow-checks`, so an overflowing
        // product traps instead of wrapping.
        let weight_change = notional * borrowing_idx - self.notional * self.borrowing_idx;
        market.data.borrowing_weight.add(is_long, weight_change);
        market.data.notional.add(is_long, notional - self.notional);
        self.notional = notional;
        self.borrowing_idx = borrowing_idx;
    }

    /// Settles the position to its close-grade [`Settled`] numbers. A repeat
    /// settle banks nothing and re-prices the current state.
    fn settle(&mut self, e: &Env, market: &mut Market, user: &Address, is_long: bool) -> Settled {
        let (funding, borrowing) = self.settle_accruals(e, market, user, is_long);
        let (base, impact) = market.trade_fees(e, is_long, -self.notional, -self.tokens);
        let fees = Fees {
            base,
            impact,
            funding,
            borrowing,
        };
        let pnl = market.haircut_pnl(
            e,
            is_long,
            math::pnl(e, self.tokens, self.notional, &market.price, is_long),
        );
        let equity = self.margin - fees.debit() + pnl;
        Settled { fees, pnl, equity }
    }

    /// Returns whether the settled equity sits below the maintenance line,
    /// the liquidation-eligibility predicate.
    fn is_liquidatable(&self, e: &Env, market: &Market, settled: &Settled) -> bool {
        settled.equity < self.margin_requirement(e, market.config.maintenance_margin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dependencies::PriceData;
    use crate::engine::constants::{BASE_UNIT, PRICE_SCALAR, SCALAR_18};
    use crate::engine::market_data::{MarketData, SidePair};
    use crate::engine::order::Order;
    use crate::engine::settlement::Settlement;
    use crate::engine::status::Status;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{contract, Env};

    // A lone 10x long: 100.0 notional on 10.0 margin, entered at $10
    // (tokens = 1e9 token-dec * 1e18 / 1e19 = 1e8 base-dec). test_config:
    // fee_non_dom 0.3%, maintenance margin 5%, liq_fee 1%, zero accrual
    // indices.
    fn long_position(e: &Env) -> Position {
        Position {
            margin: 10_0000000,
            notional: 100_0000000,
            tokens: 10 * BASE_UNIT,
            funding_idx: 0,
            borrowing_idx: 0,
            locked_notional: 0,
            unlocks_at: 0,
            priced_at: 0,
            decrease_orders: Vec::new(e),
        }
    }

    fn market_with_bid(e: &Env, bid: i128) -> Market {
        Market {
            config: crate::engine::config::test_config(),
            data: MarketData {
                notional: SidePair {
                    long: 100_0000000,
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
                bid,
                ask: 10 * PRICE_SCALAR,
                publish_time: 0,
            },
            token: Address::generate(e),
            vault: Address::generate(e),
            vault_balance: 1000_0000000,
            treasury: Address::generate(e),
            treasury_rate: 0,
            now: 0,
        }
    }

    // Closing the lone long is fully improving: base fee = ceil(0.3% * 100) =
    // 0.3 plus the size impact fee 100^2 / 100_000 (the test impact_scalar) =
    // 0.1, so the 10.0 gross margin settles to a 9.6 remainder.

    // Bid $9.20: pnl = 92 - 100 = -8.0, equity 1.6 covers the 1.0 liquidation
    // fee (ceil(1% * 100)): the fee is charged in full and the 0.6 net
    // remainder is owed to the trader.
    #[test]
    fn liquidate_charges_fee_and_returns_net_remainder() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 92 * PRICE_SCALAR / 10);
        let mut position = long_position(&e);
        let user = Address::generate(&e);

        let liquidation = position.liquidate(&e, &mut market, &user, true, false);

        assert_eq!(liquidation.notional, 100_0000000);
        assert_eq!(liquidation.tokens, 10 * BASE_UNIT);
        assert_eq!(liquidation.margin, 10_0000000);
        assert_eq!(liquidation.pnl, -8_0000000);
        assert_eq!(liquidation.returned, 6000000);
        assert_eq!(liquidation.liq_fee, 1_0000000);
        assert_eq!(position.notional, 0);
        assert_eq!(position.margin, 0);
        assert_eq!(market.data.notional.long, 0);
    }

    // Bid $9.10: pnl = -9.0, equity 0.6 falls below the 1.0 liquidation fee
    // (ceil(1% * 100)). The fee saturates on the whole remainder, the old
    // hard-tier forfeit, and the trader is paid nothing.
    #[test]
    fn liquidate_fee_saturates_on_remainder() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 91 * PRICE_SCALAR / 10);
        let mut position = long_position(&e);
        let user = Address::generate(&e);

        let liquidation = position.liquidate(&e, &mut market, &user, true, false);

        assert_eq!(liquidation.margin, 10_0000000);
        assert_eq!(liquidation.pnl, -9_0000000);
        assert_eq!(liquidation.returned, 0);
        assert_eq!(liquidation.liq_fee, 6000000);
    }

    // Bid $9.10: settled equity 0.6 sits below the 5.0 maintenance margin, so
    // the liquidatable gate rejects the voluntary full close. The keeper
    // liquidation path, with the saturating fee, must run instead.
    #[test]
    #[should_panic(expected = "#723")]
    fn full_close_rejected_below_maintenance_margin() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 91 * PRICE_SCALAR / 10);
        let mut position = long_position(&e);
        let user = Address::generate(&e);

        position.decrease(&e, &mut market, &user, true, i128::MAX, 0, false);
    }

    // Bid $8.50: the raw loss (-15.0) passes through on pnl. The 5.4 past the
    // 9.6 settled remainder lands on bad_debt, which the vault absorbs, and
    // the hard tier fires on the zero equity.
    #[test]
    fn liquidate_bad_debt_floors_pnl_at_margin() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 85 * PRICE_SCALAR / 10);
        let mut position = long_position(&e);
        let user = Address::generate(&e);

        let liquidation = position.liquidate(&e, &mut market, &user, true, false);

        assert_eq!(liquidation.pnl, -15_0000000);
        assert_eq!(liquidation.bad_debt, 5_4000000);
        assert_eq!(liquidation.returned, 0);
        assert_eq!(
            liquidation.liq_fee, 0,
            "the capped fee collects nothing from zero equity"
        );
    }

    // Bid $9.80: equity 7.6 still covers the 5.0 maintenance margin.
    #[test]
    #[should_panic(expected = "#722")]
    fn liquidate_healthy_position_panics() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 98 * PRICE_SCALAR / 10);
        let mut position = long_position(&e);
        let user = Address::generate(&e);
        position.liquidate(&e, &mut market, &user, true, false);
    }

    // A full close while the long side's pending PnL (5.0 at the $10.50 mark)
    // exceeds the trader allowance (90% of half the 5.0 vault = 2.25) scales
    // the 5.0 profit by the 2.25/5.0 cap ratio and pays 2.25. The receipt
    // carries the gross 10.0 margin. The 0.4 in fees settles inside the
    // returned equity.
    #[test]
    fn close_haircuts_profit_above_trader_gate() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 105 * PRICE_SCALAR / 10);
        market.price.ask = 105 * PRICE_SCALAR / 10;
        market.vault_balance = 5_0000000;
        let mut position = long_position(&e);
        let user = Address::generate(&e);
        let decrease = position.decrease(&e, &mut market, &user, true, i128::MAX, 0, false);
        assert_eq!(decrease.pnl, 2_2500000);
        assert_eq!(decrease.margin, 10_0000000);
        // returned = 10.0 - 0.4 fees + 2.25 haircut profit.
        assert_eq!(decrease.returned, 11_8500000);
    }

    // A decrease into the dust band below the position clamps to a full
    // close: 99.5 would leave 0.5, under the 1.0 position minimum, so the
    // whole 100.0 closes at the flat $10 exit and the settled remainder
    // (10.0 margin less the 0.4 close fees) pays out instead of the partial
    // path reverting NotionalBelowMinimum.
    #[test]
    fn decrease_into_dust_band_clamps_to_full_close() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 10 * PRICE_SCALAR);
        let mut position = long_position(&e);
        let user = Address::generate(&e);

        let decrease = position.decrease(&e, &mut market, &user, true, 99_5000000, 0, false);

        assert_eq!(decrease.notional, 100_0000000, "the whole position closes");
        assert_eq!(decrease.returned, 9_6000000, "settled remainder pays out");
        assert_eq!(position.notional, 0);
        assert_eq!(position.margin, 0);
        assert_eq!(market.data.notional.long, 0);
    }

    // The clamp's boundary: 99.0 leaves exactly the 1.0 position minimum, so
    // the partial path runs and the survivor stands.
    #[test]
    fn decrease_leaving_exact_minimum_fills_partially() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 10 * PRICE_SCALAR);
        let mut position = long_position(&e);
        let user = Address::generate(&e);

        let decrease = position.decrease(&e, &mut market, &user, true, 99_0000000, 0, false);

        assert_eq!(decrease.notional, 99_0000000, "the requested partial fills");
        assert_eq!(
            position.notional, 10_000_000,
            "survivor sits exactly at the minimum"
        );
        assert_eq!(market.data.notional.long, 10_000_000);
    }

    // The clamp does not bypass the decrease lock: 99.5 clamps to a full
    // close, and with 0.4 still locked the full close rejects NotionalLocked
    // even though the unlocked 99.6 could have covered the request itself.
    #[test]
    #[should_panic(expected = "#721")]
    fn dust_band_full_close_rejects_locked_notional() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 10 * PRICE_SCALAR);
        let mut position = long_position(&e);
        position.locked_notional = 4_000_000;
        position.unlocks_at = 100;
        let user = Address::generate(&e);

        position.decrease(&e, &mut market, &user, true, 99_5000000, 0, false);
    }

    // Accrued borrowing (a 12% index move on the 100.0 notional = 12.0)
    // exceeds the 10.0 margin: settled equity is
    // 10.0 - 0.3 (base) - 0.1 (impact) - 12.0 = -2.4, below maintenance. The
    // liquidatable gate refuses the ADL. The position must be liquidated.
    // `liquidate_bad_debt_floors_pnl_at_margin` covers that path's
    // accounting.
    #[test]
    #[should_panic(expected = "#723")]
    fn adl_rejects_liquidatable_position() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 10 * PRICE_SCALAR);
        market.data.borrowing_idx.long = 12 * crate::engine::constants::SCALAR_18 / 100;
        let mut position = long_position(&e);
        let user = Address::generate(&e);

        position.decrease(&e, &mut market, &user, true, i128::MAX, 0, true);
    }

    // The fee-eroded winner the gate cannot catch: a 24% borrowing accrual
    // (24.0) dwarfs the 10.0 margin, but the +20.0 unrealized profit at the
    // $12 bid keeps settled equity (10 - 0.4 - 24 + 20 = 5.6) above the 5.0
    // maintenance line, so the position is not liquidatable and the ADL
    // proceeds. The 10% chunk realizes only 2.0 of that profit: fees debit
    // 0.03 (base) + 0.001 (impact) + 24.0 (borrowing) = 24.031, the profit
    // covers 2.0, and the 22.031 uncovered rest overruns the margin. The
    // margin floors at zero, the 12.031 excess lands on bad_debt (sizing the
    // settlement's vault draw), and the aggregate mirrors the floored value.
    #[test]
    fn adl_partial_floors_margin_and_books_bad_debt() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 12 * PRICE_SCALAR);
        market.data.borrowing_idx.long = 24 * crate::engine::constants::SCALAR_18 / 100;
        let mut position = long_position(&e);
        let user = Address::generate(&e);

        let decrease = position.decrease(&e, &mut market, &user, true, 10_0000000, 0, true);

        assert_eq!(decrease.pnl, 2_0000000); // chunk profit, unhaircut
        assert_eq!(decrease.fees.base, 300_000);
        assert_eq!(decrease.fees.impact, 10_000);
        assert_eq!(decrease.fees.borrowing, 24_0000000);
        assert_eq!(decrease.bad_debt, 12_0310000); // uncovered fees past the margin
        assert_eq!(
            decrease.returned, 0,
            "nothing pays out while fees go uncovered"
        );
        assert_eq!(position.margin, 0, "margin floors at zero, never negative");
        assert_eq!(
            position.notional, 90_0000000,
            "survivor keeps the unclosed size"
        );
        assert_eq!(
            market.data.margin.long, 0,
            "the aggregate mirrors the floored value"
        );
    }

    // Bid $9.90: a 50.0 partial close realizes a 0.5 loss with a 2.0
    // withdrawal requested. There is no profit, so the 0.175 close fees
    // (base 0.15 + impact 0.025) debit the margin, never the withdrawal. The
    // margin pays the loss, the fees, and the full withdrawal:
    // 10 - 0.5 - 0.175 - 2.0 = 7.325. The trader is paid the 2.0 in full.
    #[test]
    fn losing_partial_decrease_pays_the_withdrawal_in_full() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 99 * PRICE_SCALAR / 10);
        let mut position = long_position(&e);
        let user = Address::generate(&e);

        let decrease =
            position.decrease(&e, &mut market, &user, true, 50_0000000, 2_0000000, false);

        assert_eq!(decrease.pnl, -5_000_000);
        assert_eq!(decrease.fees.base, 1_500_000);
        assert_eq!(decrease.fees.impact, 250_000);
        assert_eq!(decrease.returned, 2_0000000, "the withdrawal pays in full");
        assert_eq!(decrease.bad_debt, 0);
        assert_eq!(
            position.margin, 7_3250000,
            "margin pays the loss, the fees, and the withdrawal"
        );
        assert_eq!(
            market.data.margin.long, 7_3250000,
            "the aggregate mirrors the row"
        );
    }

    // Bid $10.02: a 50.0 partial close with no withdrawal realizes a 0.1
    // profit against the 0.175 close fees. The profit covers 0.1, the margin
    // pays the uncovered 0.075, and nothing returns.
    #[test]
    fn zero_withdrawal_fees_past_profit_debit_the_margin() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 1002 * PRICE_SCALAR / 100);
        let mut position = long_position(&e);
        let user = Address::generate(&e);

        let decrease = position.decrease(&e, &mut market, &user, true, 50_0000000, 0, false);

        assert_eq!(decrease.pnl, 1_000_000);
        assert_eq!(
            decrease.returned, 0,
            "nothing pays out past the covered fees"
        );
        assert_eq!(decrease.bad_debt, 0);
        assert_eq!(
            position.margin, 9_9250000,
            "margin pays only the uncovered rest"
        );
    }

    // Bid $10.20: the 50.0 close realizes a 1.0 profit. The 0.175 close fees
    // pay from the profit, the margin stays untouched, and the 0.825
    // remainder returns.
    #[test]
    fn zero_withdrawal_profit_pays_fees_and_returns_the_remainder() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 102 * PRICE_SCALAR / 10);
        let mut position = long_position(&e);
        let user = Address::generate(&e);

        let decrease = position.decrease(&e, &mut market, &user, true, 50_0000000, 0, false);

        assert_eq!(decrease.pnl, 1_0000000);
        assert_eq!(
            decrease.returned, 8_250_000,
            "the profit remainder past the fees returns"
        );
        assert_eq!(position.margin, 10_0000000, "margin stays untouched");
        assert_eq!(market.data.margin.long, 10_0000000);
    }

    // Bid $12: an ADL close of 10.0 requests a 50.0 withdrawal, past the
    // whole 10.0 margin. The 0.031 fees pay from the 2.0 chunk profit. The
    // withdrawal caps at the 10.0 surviving margin and mints no bad debt:
    // returned = 10.0 + (2.0 - 0.031) = 11.969.
    #[test]
    fn withdrawal_caps_at_the_surviving_margin() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 12 * PRICE_SCALAR);
        let mut position = long_position(&e);
        let user = Address::generate(&e);

        let decrease =
            position.decrease(&e, &mut market, &user, true, 10_0000000, 50_0000000, true);

        assert_eq!(decrease.pnl, 2_0000000);
        assert_eq!(
            decrease.returned, 11_9690000,
            "the capped withdrawal plus the profit remainder"
        );
        assert_eq!(decrease.bad_debt, 0, "a withdrawal never mints bad debt");
        assert_eq!(position.margin, 0);
        assert_eq!(market.data.margin.long, 0);
    }

    // Conservation across a bad-debt ADL close with a withdrawal requested:
    // bid $12 with a 24% borrowing accrual. Fees debit 24.031, the 2.0 chunk
    // profit covers 2.0, and the 22.031 uncovered rest overruns the 10.0
    // margin. Nothing survives, so the 3.0 requested withdrawal is dropped,
    // never turned into bad debt. The identity
    // returned + fees.debit() == freed margin + pnl + bad_debt sizes the
    // settlement's legs exactly.
    #[test]
    fn decrease_conserves_tokens_when_fees_overrun_the_margin() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 12 * PRICE_SCALAR);
        market.data.borrowing_idx.long = 24 * crate::engine::constants::SCALAR_18 / 100;
        let mut position = long_position(&e);
        let margin_before = position.margin;
        let user = Address::generate(&e);

        let decrease = position.decrease(&e, &mut market, &user, true, 10_0000000, 3_0000000, true);

        assert_eq!(decrease.returned, 0, "no payout while fees go uncovered");
        assert_eq!(
            decrease.bad_debt, 12_0310000,
            "only fees past the margin, never the withdrawal"
        );
        assert_eq!(position.margin, 0);
        assert_eq!(market.data.margin.long, 0, "the aggregate mirrors the row");
        assert_eq!(
            decrease.returned + decrease.fees.debit(),
            (margin_before - position.margin) + decrease.pnl + decrease.bad_debt,
            "trader payout plus fee legs equal the freed margin, the PnL, and the bad debt"
        );
    }

    // Accrued fees alone, not price, can erode a position into the hard
    // tier. At the flat $10 exit the raw PnL is 0, but a 9% borrowing accrual
    // (9.0 on the 100.0 notional) settles the 10.0 margin to a 0.6 post-fee
    // equity: below the 1.0 liquidation fee, the same saturated remainder
    // that `liquidate_fee_saturates_on_remainder` charges goes to the fee
    // legs. The pre-close
    // equity is a healthy 10.0 and clears maintenance, so the voluntary full
    // close must reject on the settled equity, not the pre-fee book. Else
    // the trader collects the 0.6 the vault is owed.
    #[test]
    #[should_panic(expected = "#723")]
    fn full_close_rejected_below_maintenance_after_accrued_fees() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 10 * PRICE_SCALAR);
        market.data.borrowing_idx.long = 9 * crate::engine::constants::SCALAR_18 / 100;
        let mut position = long_position(&e);
        let user = Address::generate(&e);

        position.decrease(&e, &mut market, &user, true, i128::MAX, 0, false);
    }

    // Bid $9.20: a 100.0 increase on 12.0 margin at the $10 ask pays 0.6 in
    // fees (base ceil(0.5% * 100) = 0.5 on the worsening leg plus the 0.1
    // impact fee), so the 200.0 survivor holds 21.4 margin and clears its
    // 20.0 initial-margin floor. Its settled equity, 21.4 - 1.0 close fees
    // - 16.0 pnl = 4.4, sits below the 10.0 maintenance line. The survivor
    // is liquidatable, so the validity check reports PositionLiquidatable,
    // the same code as the pre-action gate, not the initial-margin
    // InsufficientMargin.
    #[test]
    #[should_panic(expected = "#723")]
    fn increase_leaving_survivor_below_maintenance_rejects_liquidatable() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 92 * PRICE_SCALAR / 10);
        let mut position = long_position(&e);
        let user = Address::generate(&e);

        position.increase(&e, &mut market, &user, true, 100_0000000, 12_0000000);
    }

    // At an ask of $1e12 a $1 long floors to no base unit, so the increase
    // traps instead of opening size that settles at its whole notional.
    #[test]
    #[should_panic(expected = "#716")]
    fn increase_rejects_a_long_that_buys_no_base_unit() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 10 * PRICE_SCALAR);
        market.price.ask = 1_000_000_000_000 * PRICE_SCALAR;
        let mut position = Position::zeroed(&e);
        let user = Address::generate(&e);

        position.increase(&e, &mut market, &user, true, 1_0000000, 1_0000000);
    }

    // A margin-only top-up carries no notional, so zero size is its normal
    // outcome and the guard stays out of its way.
    #[test]
    fn increase_with_zero_notional_adds_margin_at_any_price() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 10 * PRICE_SCALAR);
        market.price.ask = 1_000_000_000_000 * PRICE_SCALAR;
        let mut position = long_position(&e);
        let user = Address::generate(&e);

        let increase = position.increase(&e, &mut market, &user, true, 0, 5_0000000);
        assert_eq!(increase.tokens, 0);
        assert_eq!(position.tokens, 10 * BASE_UNIT);
        assert_eq!(position.margin, 15_0000000);
    }

    // A margin-only top-up adds no size, so the decrease lock stands. The
    // 100.0 locked until t = 100 keeps its amount and its deadline when 5.0
    // of margin lands at t = 50.
    #[test]
    fn margin_top_up_leaves_the_decrease_lock_untouched() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 10 * PRICE_SCALAR);
        market.now = 50;
        let mut position = long_position(&e);
        position.locked_notional = 100_0000000;
        position.unlocks_at = 100;
        let user = Address::generate(&e);

        position.increase(&e, &mut market, &user, true, 0, 5_0000000);

        assert_eq!(position.margin, 15_0000000);
        assert_eq!(position.locked_notional, 100_0000000);
        assert_eq!(position.unlocks_at, 100, "the deadline does not move");
    }

    // A margin-only top-up grows no open interest, so it stays open on a side
    // already above its ceiling. Other traders hold 200.0 more long, and the
    // owner has since lowered max_open_interest to 200.0, under the side's
    // 300.0.
    #[test]
    fn margin_top_up_passes_on_a_side_above_the_open_interest_ceiling() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 10 * PRICE_SCALAR);
        market.data.notional.long = 300_0000000;
        market.data.tokens.long = 30 * BASE_UNIT;
        market.data.margin.long = 30_0000000;
        market.config.max_position_notional = 100_0000000;
        market.config.max_open_interest = 200_0000000;
        let mut position = long_position(&e);
        let user = Address::generate(&e);

        position.increase(&e, &mut market, &user, true, 0, 5_0000000);

        assert_eq!(position.margin, 15_0000000);
        assert_eq!(market.data.notional.long, 300_0000000);
        assert_eq!(market.data.margin.long, 35_0000000);
    }

    #[contract]
    struct Harness;

    // Only the all-zero row is the closed state. A flat row that still holds
    // margin persists as it is: the store keeps the margin and sweeps none of
    // the side's resting decrease orders.
    #[test]
    fn store_keeps_a_flat_row_that_still_holds_margin() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        let user = Address::generate(&e);
        e.as_contract(&contract_id, || {
            storage::set_order(
                &e,
                &user,
                7,
                &Order {
                    is_long: true,
                    kind: OrderKind::LimitDecrease as u32,
                    notional: 50_0000000,
                    margin: 0,
                    trigger_price: 12 * PRICE_SCALAR,
                    price_bound: 0,
                    exec_fee: 1_000_000,
                    created_at: 0,
                    expiration: u32::MAX,
                },
            );
            let position = Position {
                margin: 5_0000000,
                decrease_orders: soroban_sdk::vec![&e, 7],
                ..Position::zeroed(&e)
            };

            assert_eq!(position.store(&e, &user, true), 0, "nothing is refunded");

            let stored = storage::get_position(&e, &user, true);
            assert_eq!(stored.margin, 5_0000000);
            assert_eq!(stored.decrease_orders.len(), 1);
            assert!(e
                .storage()
                .persistent()
                .has(&storage::DataKey::Order(user.clone(), 7)));
        });
    }

    // A zeroed (absent) position cannot be decreased.
    #[test]
    #[should_panic(expected = "#720")]
    fn decrease_panics_on_absent_position() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 10 * PRICE_SCALAR);
        let mut position = Position::zeroed(&e);
        let user = Address::generate(&e);
        position.decrease(&e, &mut market, &user, true, 100_0000000, 0, false);
    }

    // A zeroed (absent) position cannot be liquidated.
    #[test]
    #[should_panic(expected = "#720")]
    fn liquidate_panics_on_absent_position() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 10 * PRICE_SCALAR);
        let mut position = Position::zeroed(&e);
        let user = Address::generate(&e);
        position.liquidate(&e, &mut market, &user, true, false);
    }

    // The side accepts exactly MAX_ORDERS_PER_SIDE (8) pending decrease ids.
    #[test]
    fn push_decrease_accepts_up_to_the_side_cap() {
        let e = Env::default();
        let mut position = Position::zeroed(&e);
        for id in 1..=MAX_ORDERS_PER_SIDE {
            position.push_decrease(&e, id);
        }
        assert_eq!(position.decrease_orders.len(), 8);
    }

    // The 9th pending decrease on a side is rejected: the cap bounds the
    // closure sweep (entry restores, deletes, and cancel receipts) the
    // liquidation transaction must carry.
    #[test]
    #[should_panic(expected = "#733")]
    fn push_decrease_rejects_a_ninth_order_on_the_side() {
        let e = Env::default();
        let mut position = Position::zeroed(&e);
        for id in 1..=MAX_ORDERS_PER_SIDE {
            position.push_decrease(&e, id);
        }
        position.push_decrease(&e, 9);
    }

    // The wind-down waiver closes the same healthy position but still charges
    // the liquidation fee, because the wind-down notice window is the free
    // exit. The 7.6 equity nets 6.6 after the 1.0 fee.
    #[test]
    fn liquidate_force_waives_eligibility_for_healthy_position() {
        let e = Env::default();
        let mut market = market_with_bid(&e, 98 * PRICE_SCALAR / 10);
        let mut position = long_position(&e);
        let user = Address::generate(&e);

        let liquidation = position.liquidate(&e, &mut market, &user, true, true);

        assert_eq!(liquidation.margin, 10_0000000);
        assert_eq!(liquidation.pnl, -2_0000000);
        assert_eq!(liquidation.returned, 6_6000000);
        assert_eq!(liquidation.liq_fee, 1_0000000);
        assert_eq!(position.notional, 0);
        assert_eq!(market.data.notional.long, 0);
    }

    // An empty book at a flat $1, so base size equals notional and every side
    // marks zero PnL. The 100,000.0 vault keeps the PnL cap out of reach.
    fn flat_market(e: &Env, treasury_rate: i128) -> Market {
        Market {
            config: crate::engine::config::test_config(),
            data: MarketData::default(),
            status: Status::Active,
            price: PriceData {
                bid: PRICE_SCALAR,
                ask: PRICE_SCALAR,
                publish_time: 0,
            },
            token: Address::generate(e),
            vault: Address::generate(e),
            vault_balance: 100000_0000000,
            treasury: Address::generate(e),
            treasury_rate,
            now: 0,
        }
    }

    // Returns the vault's cut of a paid `borrowing` (token-dec) through the
    // settlement split.
    fn vault_borrowing_cut(e: &Env, market: &Market, borrowing: i128) -> i128 {
        let increase = Increase {
            tokens: 0,
            fees: Fees {
                base: 0,
                impact: 0,
                funding: 0,
                borrowing,
            },
        };
        Settlement::compute_increase_order(e, market, &increase, 0).vault
    }

    // Asserts that `is_long`'s open interest and borrowing weight equal the
    // sums over `positions`, every row on the side. It also asserts that the
    // market's pending borrowing equals the rows' exact unsettled accruals,
    // rounded once, so the other side must be empty.
    fn assert_side_matches(e: &Env, market: &Market, is_long: bool, positions: &[&Position]) {
        let index = market.data.borrowing_idx.get(is_long);
        let open_interest: i128 = positions.iter().map(|p| p.notional).sum();
        let weight: i128 = positions.iter().map(|p| p.notional * p.borrowing_idx).sum();
        let unsettled: i128 = positions
            .iter()
            .map(|p| p.notional * (index - p.borrowing_idx))
            .sum();
        assert_eq!(market.data.notional.get(is_long), open_interest);
        assert_eq!(market.data.borrowing_weight.get(is_long), weight);
        assert_eq!(
            market.data.pending_borrowing(e, true),
            unsettled / SCALAR_18
        );
        assert_eq!(
            market.data.pending_borrowing(e, false),
            (unsettled + SCALAR_18 - 1) / SCALAR_18
        );
    }

    // Two longs walk the borrowing books at a 10% treasury rate. The index
    // takes the values I0 = 0.001, I1 = 0.001500000000000333,
    // I2 = 0.002718281828459045 and I3 = 0.003141592653589793.
    //
    // - A opens 1,000.0 at I0. Nothing is pending.
    // - B opens 333.3333333 at I1. B's checkpoint offsets the index it was
    //   not open for, so only A's accrual is pending.
    // - At I2 the side owes 2.1243757712... The vault's 90% share marks at
    //   1.9119381 for a redeem and 1.9119383 for a deposit.
    // - A pays 1.7182819, of which the vault keeps 1.5464538, and reduces to
    //   600.0. Only B's accrual stays pending.
    // - At I3 both close and pay 0.2539865 and 0.5471976. The vault keeps
    //   0.7210658, at least the 0.7210656 redeem mark and the 0.7210657
    //   deposit mark taken before the closes.
    #[test]
    fn borrowing_weight_follows_two_longs_through_their_lifetimes() {
        let e = Env::default();
        let mut market = flat_market(&e, SCALAR_18 / 10);
        let (user_a, user_b) = (Address::generate(&e), Address::generate(&e));
        let mut a = Position::zeroed(&e);
        let mut b = Position::zeroed(&e);

        market.data.borrowing_idx.long = 1_000_000_000_000_000;
        a.increase(&e, &mut market, &user_a, true, 1000_0000000, 200_0000000);
        assert_eq!(
            market.data.borrowing_weight.long,
            1000_0000000 * 1_000_000_000_000_000
        );
        assert_eq!(market.data.pending_borrowing(&e, false), 0);

        market.data.borrowing_idx.long = 1_500_000_000_000_333;
        b.increase(&e, &mut market, &user_b, true, 333_3333333, 100_0000000);
        assert_eq!(market.data.notional.long, 1333_3333333);
        assert_eq!(
            market.data.borrowing_weight.long,
            14_999_999_999_501_109_999_999_889
        );
        assert_eq!(market.data.pending_borrowing(&e, true), 5_000_000);
        assert_eq!(market.data.pending_borrowing(&e, false), 5_000_001);

        market.data.borrowing_idx.long = 2_718_281_828_459_045;
        assert_eq!(market.data.pending_borrowing(&e, true), 2_1243757);
        assert_eq!(market.data.pending_borrowing(&e, false), 2_1243758);
        assert_eq!(market.lp_net_pnl(&e, true), -1_9119381);
        assert_eq!(market.lp_net_pnl(&e, false), -1_9119383);

        // Past the 30 s decrease lock.
        market.now = 100;
        let reduce = a.decrease(&e, &mut market, &user_a, true, 400_0000000, 0, false);
        assert_eq!(reduce.fees.borrowing, 1_7182819);
        assert_eq!(
            vault_borrowing_cut(&e, &market, reduce.fees.borrowing),
            1_5464538
        );
        assert_eq!(market.data.notional.long, 933_3333333);
        assert_eq!(
            market.data.borrowing_weight.long,
            21_309_690_970_255_379_999_999_889
        );
        assert_eq!(market.data.pending_borrowing(&e, true), 4060939);
        assert_eq!(market.data.pending_borrowing(&e, false), 4060940);

        market.data.borrowing_idx.long = 3_141_592_653_589_793;
        assert_eq!(market.lp_net_pnl(&e, true), -7210656);
        assert_eq!(market.lp_net_pnl(&e, false), -7210657);
        let close_a = a.decrease(&e, &mut market, &user_a, true, i128::MAX, 0, false);
        let close_b = b.decrease(&e, &mut market, &user_b, true, i128::MAX, 0, false);
        assert_eq!(close_a.fees.borrowing, 2539865);
        assert_eq!(close_b.fees.borrowing, 5471976);
        assert_eq!(
            vault_borrowing_cut(&e, &market, close_a.fees.borrowing)
                + vault_borrowing_cut(&e, &market, close_b.fees.borrowing),
            7210658
        );
        assert_eq!(market.data.notional.long, 0);
        assert_eq!(market.data.borrowing_weight.long, 0);
    }

    // Every checkpoint writer keeps the short side's weight at the sum of
    // its rows' `notional * borrowing_idx`. Three shorts open at different
    // indices, then the index moves and they leave through each writer:
    //
    // - a margin top-up re-checkpoints S2 at its unchanged size,
    // - an ADL partial shrinks S1,
    // - a voluntary full close and a forced liquidation close S1 and S2,
    // - an ADL full close, at a later index, empties the side.
    #[test]
    fn borrowing_weight_matches_the_rows_through_every_checkpoint_writer() {
        let e = Env::default();
        let mut market = flat_market(&e, 0);
        let user = Address::generate(&e);
        let mut s1 = Position::zeroed(&e);
        let mut s2 = Position::zeroed(&e);
        let mut s3 = Position::zeroed(&e);

        market.data.borrowing_idx.short = 7_000_000_000_000_001;
        s1.increase(&e, &mut market, &user, false, 250_0000000, 100_0000000);
        assert_side_matches(&e, &market, false, &[&s1, &s2, &s3]);
        market.data.borrowing_idx.short = 19_999_999_999_999_999;
        s2.increase(&e, &mut market, &user, false, 123_4567891, 50_0000000);
        assert_side_matches(&e, &market, false, &[&s1, &s2, &s3]);
        market.data.borrowing_idx.short = 31_415_926_535_897_932;
        s3.increase(&e, &mut market, &user, false, 77_7777777, 30_0000000);
        assert_side_matches(&e, &market, false, &[&s1, &s2, &s3]);

        // Past the 30 s decrease lock.
        market.now = 100;
        market.data.borrowing_idx.short = 47_123_456_789_012_345;
        s2.increase(&e, &mut market, &user, false, 0, 1_0000000);
        assert_eq!(s2.borrowing_idx, market.data.borrowing_idx.short);
        assert_side_matches(&e, &market, false, &[&s1, &s2, &s3]);

        s1.decrease(&e, &mut market, &user, false, 100_0000000, 0, true);
        assert_eq!(s1.notional, 150_0000000);
        assert_side_matches(&e, &market, false, &[&s1, &s2, &s3]);

        s1.decrease(&e, &mut market, &user, false, i128::MAX, 0, false);
        s2.liquidate(&e, &mut market, &user, false, true);
        assert_eq!((s1.notional, s2.notional), (0, 0));
        assert_side_matches(&e, &market, false, &[&s1, &s2, &s3]);

        market.data.borrowing_idx.short = 52_000_000_000_000_007;
        s3.decrease(&e, &mut market, &user, false, i128::MAX, 0, true);
        assert_side_matches(&e, &market, false, &[&s1, &s2, &s3]);
        assert_eq!(market.data.notional.short, 0);
        assert_eq!(market.data.borrowing_weight.short, 0);
        assert_eq!(market.data.pending_borrowing(&e, false), 0);
        assert_eq!(market.data.borrowing_weight.long, 0);
    }
}
