//! Trade orders: the [`Order`] row and its create and cancel entries.
//! Keepers fill them in [`crate::engine::keeper`].

use crate::dependencies::PriceData;
use crate::engine::config::Config;
use crate::engine::position::Position;
use crate::engine::status::Status;
use crate::errors::MarketError;
use crate::events::{CancelOrder, CreateOrder};
use crate::storage;
use soroban_sdk::{contracttype, panic_with_error, token::TokenClient, Address, Env, MuxedAddress};

/// The intent of a keeper order: increase or decrease, and how it becomes
/// eligible, immediately or on a price crossing. It crosses the ABI as its
/// `u32` discriminant, [`Order::kind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum OrderKind {
    /// Grows the position now. `trigger_price` is unused.
    MarketIncrease = 0,
    /// Grows the position when the price crosses `trigger_price` favorably.
    LimitIncrease = 1,
    /// Grows the position when the price crosses `trigger_price` adversely.
    StopIncrease = 2,
    /// Shrinks the position now. `trigger_price` is unused.
    MarketDecrease = 3,
    /// Take profit. Shrinks the position when the price crosses
    /// `trigger_price` favorably.
    LimitDecrease = 4,
    /// Stop loss. Shrinks the position when the price crosses
    /// `trigger_price` adversely.
    StopDecrease = 5,
}

impl OrderKind {
    /// Decodes a raw `u32` order-kind discriminant.
    ///
    /// # Errors
    /// - [`MarketError::UnknownKind`] if `raw` is not an order-kind
    ///   discriminant.
    pub(crate) fn from_u32(e: &Env, raw: u32) -> OrderKind {
        match raw {
            0 => OrderKind::MarketIncrease,
            1 => OrderKind::LimitIncrease,
            2 => OrderKind::StopIncrease,
            3 => OrderKind::MarketDecrease,
            4 => OrderKind::LimitDecrease,
            5 => OrderKind::StopDecrease,
            _ => panic_with_error!(e, MarketError::UnknownKind),
        }
    }

    /// Returns `true` for the three increase kinds.
    pub(crate) fn is_increase(self) -> bool {
        matches!(
            self,
            OrderKind::MarketIncrease | OrderKind::LimitIncrease | OrderKind::StopIncrease
        )
    }

    /// Returns `true` for the three decrease kinds.
    pub(crate) fn is_decrease(self) -> bool {
        !self.is_increase()
    }

    /// Returns `true` for the two market kinds. They fill on eligibility,
    /// with no trigger crossing.
    pub(crate) fn is_market(self) -> bool {
        matches!(self, OrderKind::MarketIncrease | OrderKind::MarketDecrease)
    }

    /// Returns `true` for the four limit and stop kinds. They fill on a
    /// `trigger_price` crossing.
    pub(crate) fn is_trigger(self) -> bool {
        !self.is_market()
    }
}

/// A keeper-executed order. Persistent user-tier storage, keyed
/// `(user, id)`.
#[derive(Clone, Debug)]
#[contracttype]
pub struct Order {
    /// The side this order targets.
    pub is_long: bool,
    /// The `OrderKind` discriminant.
    pub kind: u32,
    /// The size change magnitude, at least `0` (token-dec).
    pub notional: i128,
    /// The margin change magnitude, at least `0` (token-dec).
    pub margin: i128,
    /// The crossing level for a trigger kind, in the feed's native
    /// precision. A market kind's fill never reads it.
    pub trigger_price: i128,
    /// The fill slippage limit, in the feed's native precision. `0` is
    /// unbounded.
    pub price_bound: i128,
    /// The keeper execution fee escrowed at creation (token-dec).
    pub exec_fee: i128,
    /// The submission ledger timestamp (seconds), the fill's anti-replay anchor.
    pub created_at: u64,
    /// The last ledger sequence the order is fillable at.
    pub expiration: u32,
}

impl Order {
    /// Returns the settlement-token escrow this order locks at creation
    /// (token-dec): an increase's `margin` plus its `exec_fee`, or a
    /// decrease's `exec_fee`.
    pub(crate) fn escrow_amount(&self, kind: OrderKind) -> i128 {
        if kind.is_increase() {
            self.margin + self.exec_fee
        } else {
            self.exec_fee
        }
    }

    /// Refunds this order's escrow, [`Order::escrow_amount`], to `user`,
    /// deletes the `(user, id)` entry, and returns the refunded amount
    /// (token-dec).
    pub(crate) fn refund_and_remove(&self, e: &Env, user: &Address, id: u32) -> i128 {
        let refund = self.escrow_amount(OrderKind::from_u32(e, self.kind));
        if refund > 0 {
            let token = TokenClient::new(e, &storage::get_token(e));
            let market_address = e.current_contract_address();
            token.transfer(&market_address, &MuxedAddress::from(user.clone()), &refund);
        }
        storage::remove_order(e, user, id);
        refund
    }

    /// Traps on the first structural rule this order violates at submission
    /// and returns the decoded kind.
    ///
    /// # Errors
    /// - [`MarketError::UnknownKind`] if `kind` is not an order-kind
    ///   discriminant.
    /// - Refer to [`Order::check_valid`] errors.
    pub(crate) fn require_valid(&self, e: &Env, config: &Config) -> OrderKind {
        let kind = OrderKind::from_u32(e, self.kind);
        self.check_valid(kind, config, e.ledger().sequence())
            .unwrap_or_else(|err| panic_with_error!(e, err));
        kind
    }

    /// Returns the first structural rule this order violates, or `Ok(())`.
    /// Sizing, margin, utilization, and the trigger and bound consistency
    /// against the resulting position are enforced at execute.
    ///
    /// # Errors
    /// - [`MarketError::NegativeValueNotAllowed`] if a magnitude,
    ///   `trigger_price`, or `price_bound` is negative.
    /// - [`MarketError::InvalidOrder`] if:
    ///   - a moved value is below its dust floor,
    ///   - the order moves nothing,
    ///   - a trigger kind carries a non-positive `trigger_price`, or
    ///   - an increase's `margin + exec_fee` escrow sum overflows.
    /// - [`MarketError::NotionalAboveMaximum`] if an increase's `notional`
    ///   exceeds `max_position_notional`.
    /// - [`MarketError::OrderExpired`] if `expiration` is already behind
    ///   `ledger_seq`.
    fn check_valid(
        &self,
        kind: OrderKind,
        config: &Config,
        ledger_seq: u32,
    ) -> Result<(), MarketError> {
        // No negative inputs.
        if self.notional < 0 || self.margin < 0 || self.trigger_price < 0 || self.price_bound < 0 {
            return Err(MarketError::NegativeValueNotAllowed);
        }
        // A moved value must clear its dust floor. The band (0, floor) is
        // rejected.
        if (self.notional > 0 && self.notional < config.min_order_notional)
            || (self.margin > 0 && self.margin < config.min_order_margin)
        {
            return Err(MarketError::InvalidOrder);
        }
        // An order that moves neither size nor margin is a no-op.
        if self.notional == 0 && self.margin == 0 {
            return Err(MarketError::InvalidOrder);
        }
        // A trigger kind fires on a crossing, so it needs a positive trigger
        // price. A market kind's fill never reads trigger_price.
        if kind.is_trigger() && self.trigger_price == 0 {
            return Err(MarketError::InvalidOrder);
        }
        // An increase past the max position notional can never fill. A
        // decrease never grows the position, so it is uncapped and clamps to
        // a full close at fill.
        if kind.is_increase() && self.notional > config.max_position_notional {
            return Err(MarketError::NotionalAboveMaximum);
        }
        // The escrow sum `margin + exec_fee` is checked here rather than
        // trapping in the transfer arithmetic.
        if kind.is_increase() && self.margin.checked_add(self.exec_fee).is_none() {
            return Err(MarketError::InvalidOrder);
        }
        // The expiration is in the past.
        if self.expiration < ledger_seq {
            return Err(MarketError::OrderExpired);
        }
        Ok(())
    }

    /// Traps on the first eligibility rule this order fails at `price`.
    ///
    /// # Errors
    /// - Refer to [`Order::check_trigger`] errors.
    pub(crate) fn require_price_trigger_met(&self, e: &Env, price: &PriceData) {
        let kind = OrderKind::from_u32(e, self.kind);
        self.check_trigger(kind, price, e.ledger().sequence(), e.ledger().timestamp())
            .unwrap_or_else(|err| panic_with_error!(e, err));
    }

    /// Returns the first fill-eligibility rule this order fails at the
    /// verified `price`, or `Ok(())`. The trigger and the slippage bound are
    /// both judged on the execution-side price, `entry` for an increase and
    /// `exit` for a decrease. A stop or limit therefore fires on the price the
    /// fill will touch.
    ///
    /// # Errors
    /// - [`MarketError::OrderExpired`] if `ledger_seq` is past `expiration`.
    /// - [`MarketError::StalePrice`] if `price.publish_time` predates
    ///   `created_at`, unless a market kind fills in its creation ledger.
    /// - [`MarketError::TriggerNotMet`] if a trigger kind's `trigger_price`
    ///   is not crossed.
    /// - [`MarketError::PriceBoundExceeded`] if the execution price is worse
    ///   than `price_bound`.
    fn check_trigger(
        &self,
        kind: OrderKind,
        price: &PriceData,
        ledger_seq: u32,
        now: u64,
    ) -> Result<(), MarketError> {
        // Expiration. Time may have advanced since submission.
        if ledger_seq > self.expiration {
            return Err(MarketError::OrderExpired);
        }
        // Anti-replay: the verified price must be at least as fresh as the
        // order. A market kind that fills in its creation ledger is an atomic
        // create-and-fill. Ledger close times strictly increase, so an equal
        // timestamp identifies that ledger.
        let atomic_market_fill = kind.is_market() && self.created_at == now;
        if !atomic_market_fill && price.publish_time < self.created_at {
            return Err(MarketError::StalePrice);
        }
        // The execution-side price the fill will touch: entry for an
        // increase, exit for a decrease.
        let execution_price = if kind.is_increase() {
            price.entry(self.is_long)
        } else {
            price.exit(self.is_long)
        };
        if kind.is_trigger() {
            // StopIncrease and LimitDecrease fill on the upside for a long
            // and the downside for a short. LimitIncrease and StopDecrease
            // are inverted.
            let fills_above =
                matches!(kind, OrderKind::StopIncrease | OrderKind::LimitDecrease) == self.is_long;
            let crossed = if fills_above {
                execution_price >= self.trigger_price
            } else {
                execution_price <= self.trigger_price
            };
            if !crossed {
                return Err(MarketError::TriggerNotMet);
            }
        }
        if self.price_bound != 0 {
            // A long increase or a short decrease buys base.
            let is_buy = kind.is_increase() == self.is_long;
            let within = if is_buy {
                // A buy reads `price_bound` as a ceiling.
                execution_price <= self.price_bound
            } else {
                // A sell reads `price_bound` as a floor.
                execution_price >= self.price_bound
            };
            if !within {
                return Err(MarketError::PriceBoundExceeded);
            }
        }
        Ok(())
    }
}

/// Creates an [`Order`] for the keeper to fill and returns its id. Refer to
/// [`Market::create_order`](crate::Market::create_order).
#[allow(clippy::too_many_arguments)]
pub fn create_order(
    e: &Env,
    user: &Address,
    is_long: bool,
    kind: u32,
    notional: i128,
    margin: i128,
    trigger_price: i128,
    price_bound: i128,
    expiration: u32,
) -> u32 {
    user.require_auth();
    let status = storage::get_status(e);
    if status == Status::Frozen || status == Status::Retired {
        panic_with_error!(e, MarketError::MarketFrozen);
    }

    let config = storage::get_config(e);
    let created_at = e.ledger().timestamp();
    let order = Order {
        is_long,
        kind,
        notional,
        margin,
        trigger_price,
        price_bound,
        exec_fee: config.exec_fee,
        created_at,
        expiration,
    };

    let order_kind = order.require_valid(e, &config);

    // The target position row loads here, so its TTL is extended on the
    // trader's transaction and a decrease can join its list. A missing row
    // is created here, so the trader pays for it rather than the fill.
    let stored = storage::find_position(e, user, is_long);
    let missing = stored.is_none();
    let mut position = stored.unwrap_or_else(|| Position::zeroed(e));

    let escrow = order.escrow_amount(order_kind);
    if escrow > 0 {
        let token = TokenClient::new(e, &storage::get_token(e));
        let market_address = e.current_contract_address();
        token.transfer(user, &MuxedAddress::from(market_address), &escrow);
    }

    let id = storage::next_order_id(e, user);
    storage::set_order(e, user, id, &order);
    if order_kind.is_decrease() {
        // A decrease may rest before its position opens.
        position.push_decrease(e, id);
        storage::set_position(e, user, is_long, &position);
    } else if missing {
        // An increase leaves an existing row untouched, so only a missing
        // row is stored.
        storage::set_position(e, user, is_long, &position);
    }

    CreateOrder {
        user: user.clone(),
        id,
        order,
    }
    .publish(e);

    id
}

/// Cancels the pending [`Order`] `(user, id)` and returns the refunded
/// escrow (token-dec). Refer to
/// [`Market::cancel_order`](crate::Market::cancel_order).
pub fn cancel_order(e: &Env, user: &Address, id: u32) -> i128 {
    user.require_auth();
    if storage::get_status(e) == Status::Frozen {
        panic_with_error!(e, MarketError::MarketFrozen);
    }
    // `get_order` traps with `OrderNotFound` on a missing entry.
    let order = storage::get_order(e, user, id);
    let refund = order.refund_and_remove(e, user, id);
    if OrderKind::from_u32(e, order.kind).is_decrease() {
        let mut position = storage::get_position(e, user, order.is_long);
        position.remove_decrease(e, id);
        storage::set_position(e, user, order.is_long, &position);
    }

    CancelOrder {
        user: user.clone(),
        id,
        refund,
    }
    .publish(e);
    refund
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::constants::{PRICE_SCALAR, SCALAR_18};

    // Config floors and cap that the order validator reads.
    const MIN_ORDER_NOTIONAL: i128 = 1_000_000;
    const MIN_ORDER_MARGIN: i128 = 1_000_000;
    const MAX_POSITION_NOTIONAL: i128 = 1_000_000_000_000;

    /// Returns a config that supplies the fields `Order::check_valid` reads:
    /// the two dust floors and `max_position_notional`. The rest are filler,
    /// because order validation never touches them.
    fn base_config() -> Config {
        Config {
            keeper_rate: SCALAR_18 / 10,
            min_position_notional: 10_000_000,
            max_position_notional: MAX_POSITION_NOTIONAL,
            max_open_interest: 10 * MAX_POSITION_NOTIONAL,
            min_order_notional: MIN_ORDER_NOTIONAL,
            min_order_margin: MIN_ORDER_MARGIN,
            exec_fee: 100_000,
            fee_dom: SCALAR_18 / 200,
            fee_non_dom: 3 * SCALAR_18 / 1000,
            impact_scalar: 1_000_000_000_000,
            max_util_open: 8 * SCALAR_18 / 10,
            max_util_withdraw: 9 * SCALAR_18 / 10,
            init_margin: SCALAR_18 / 10,
            maintenance_margin: SCALAR_18 / 20,
            liq_fee: SCALAR_18 / 100,
            notional_lock: 30,
            target_util: SCALAR_18 / 2,
            borrow_rate: 0,
            increased_borrow_rate: 0,
            funding_increase: 0,
            funding_decrease: 0,
            threshold_stable_funding: 0,
            threshold_decrease_funding: 0,
            funding_min: 0,
            funding_max: 0,
            adl_max_pnl: 0,
            adl_clear_target: 0,
            max_pnl_trader: 9 * SCALAR_18 / 10,
            max_pnl_withdraw: 9 * SCALAR_18 / 10,
            redeem_lock: 0,
            deposit_fee: SCALAR_18 / 1000,
            redeem_fee: SCALAR_18 / 1000,
            min_deposit: 1_000_000,
            max_vault_balance: 10_000_000_000_000,
        }
    }

    /// Returns a valid market-increase order at ledger sequence 1, with
    /// expiration 1000. Callers override only the fields under test.
    fn base_order() -> Order {
        Order {
            is_long: true,
            kind: OrderKind::MarketIncrease as u32,
            notional: 5 * MIN_ORDER_NOTIONAL,
            margin: 2 * MIN_ORDER_MARGIN,
            trigger_price: 0,
            price_bound: 0,
            exec_fee: 0,
            created_at: 0,
            expiration: 1000,
        }
    }

    fn order_with(kind: OrderKind, notional: i128, margin: i128) -> Order {
        Order {
            kind: kind as u32,
            notional,
            margin,
            ..base_order()
        }
    }

    /// Every `(kind, notional, margin)` shape plus each dust-floor boundary.
    /// Exactly at the floor passes, and one unit inside rejects.
    /// MarketIncrease and MarketDecrease share the same structural rules
    /// here. Only the max-notional cap, which has its own test, differs.
    /// Each case keeps a valid expiration and a zero trigger and bound, so
    /// only the shape and floor logic is exercised.
    #[test]
    fn shape_and_dust_floor_matrix() {
        use OrderKind::{MarketDecrease, MarketIncrease};
        let config = base_config();
        let invalid_order = Err(MarketError::InvalidOrder);
        let cases = [
            // --- valid increase shapes ---
            (
                "increase: size and margin",
                MarketIncrease,
                5 * MIN_ORDER_NOTIONAL,
                5 * MIN_ORDER_MARGIN,
                Ok(()),
            ),
            (
                "increase: leverage up (no margin)",
                MarketIncrease,
                5 * MIN_ORDER_NOTIONAL,
                0,
                Ok(()),
            ),
            (
                "increase: pure deposit (no size)",
                MarketIncrease,
                0,
                5 * MIN_ORDER_MARGIN,
                Ok(()),
            ),
            // --- valid decrease shapes ---
            (
                "decrease: size and margin",
                MarketDecrease,
                5 * MIN_ORDER_NOTIONAL,
                5 * MIN_ORDER_MARGIN,
                Ok(()),
            ),
            (
                "decrease: reduce keeping margin",
                MarketDecrease,
                5 * MIN_ORDER_NOTIONAL,
                0,
                Ok(()),
            ),
            (
                "decrease: pure withdrawal (no size)",
                MarketDecrease,
                0,
                5 * MIN_ORDER_MARGIN,
                Ok(()),
            ),
            // --- no-op: both deltas zero, either kind ---
            ("increase: no-op", MarketIncrease, 0, 0, invalid_order),
            ("decrease: no-op", MarketDecrease, 0, 0, invalid_order),
            // --- notional dust floor (when size moves) ---
            (
                "increase: notional at floor",
                MarketIncrease,
                MIN_ORDER_NOTIONAL,
                MIN_ORDER_MARGIN,
                Ok(()),
            ),
            (
                "increase: notional below floor",
                MarketIncrease,
                MIN_ORDER_NOTIONAL - 1,
                MIN_ORDER_MARGIN,
                invalid_order,
            ),
            (
                "decrease: notional at floor",
                MarketDecrease,
                MIN_ORDER_NOTIONAL,
                MIN_ORDER_MARGIN,
                Ok(()),
            ),
            (
                "decrease: notional below floor",
                MarketDecrease,
                MIN_ORDER_NOTIONAL - 1,
                MIN_ORDER_MARGIN,
                invalid_order,
            ),
            // --- margin dust floor (when margin moves) ---
            (
                "increase: margin at floor",
                MarketIncrease,
                5 * MIN_ORDER_NOTIONAL,
                MIN_ORDER_MARGIN,
                Ok(()),
            ),
            (
                "increase: margin below floor",
                MarketIncrease,
                5 * MIN_ORDER_NOTIONAL,
                MIN_ORDER_MARGIN - 1,
                invalid_order,
            ),
            (
                "decrease: margin at floor",
                MarketDecrease,
                5 * MIN_ORDER_NOTIONAL,
                MIN_ORDER_MARGIN,
                Ok(()),
            ),
            (
                "decrease: margin below floor",
                MarketDecrease,
                5 * MIN_ORDER_NOTIONAL,
                MIN_ORDER_MARGIN - 1,
                invalid_order,
            ),
            // --- margin-only dust floor (no size) ---
            (
                "increase: deposit at floor",
                MarketIncrease,
                0,
                MIN_ORDER_MARGIN,
                Ok(()),
            ),
            (
                "increase: deposit below floor",
                MarketIncrease,
                0,
                MIN_ORDER_MARGIN - 1,
                invalid_order,
            ),
            (
                "decrease: withdrawal below floor",
                MarketDecrease,
                0,
                MIN_ORDER_MARGIN - 1,
                invalid_order,
            ),
        ];
        for (label, kind, notional, margin, expected) in cases {
            assert_eq!(
                order_with(kind, notional, margin).check_valid(kind, &config, 1),
                expected,
                "{}",
                label
            );
        }
    }

    /// An increase is capped at `max_position_notional`, because a delta over
    /// it can never fill. A decrease never grows the position, so the same
    /// magnitude is allowed and clamps to a full close at fill. `i128::MAX`
    /// is the conventional full-close signal.
    #[test]
    fn max_position_notional_caps_increase_only() {
        let config = base_config();
        assert_eq!(
            order_with(OrderKind::MarketIncrease, MAX_POSITION_NOTIONAL, 0).check_valid(
                OrderKind::MarketIncrease,
                &config,
                1
            ),
            Ok(())
        );
        assert_eq!(
            order_with(OrderKind::MarketIncrease, MAX_POSITION_NOTIONAL + 1, 0).check_valid(
                OrderKind::MarketIncrease,
                &config,
                1
            ),
            Err(MarketError::NotionalAboveMaximum)
        );
        assert_eq!(
            order_with(OrderKind::MarketDecrease, MAX_POSITION_NOTIONAL + 1, 0).check_valid(
                OrderKind::MarketDecrease,
                &config,
                1
            ),
            Ok(())
        );
        assert_eq!(
            order_with(OrderKind::MarketDecrease, i128::MAX, 0).check_valid(
                OrderKind::MarketDecrease,
                &config,
                1
            ),
            Ok(())
        );
    }

    /// An increase whose `margin + exec_fee` escrow sum would overflow i128
    /// is rejected at validation instead of trapping in the escrow
    /// arithmetic. A decrease escrows only its `exec_fee`, so the same margin
    /// passes.
    #[test]
    fn increase_escrow_overflow_rejected() {
        let config = base_config();
        let overflowing = Order {
            exec_fee: 1,
            ..order_with(OrderKind::MarketIncrease, 5 * MIN_ORDER_NOTIONAL, i128::MAX)
        };
        assert_eq!(
            overflowing.check_valid(OrderKind::MarketIncrease, &config, 1),
            Err(MarketError::InvalidOrder)
        );
        let withdrawal = Order {
            exec_fee: 1,
            ..order_with(OrderKind::MarketDecrease, 5 * MIN_ORDER_NOTIONAL, i128::MAX)
        };
        assert_eq!(
            withdrawal.check_valid(OrderKind::MarketDecrease, &config, 1),
            Ok(())
        );
    }

    /// A trigger kind, limit or stop, with no `trigger_price` set is
    /// rejected. The same shape on a market kind is fine, because a market
    /// kind never reads it.
    #[test]
    fn trigger_kind_requires_positive_trigger_price() {
        let config = base_config();
        assert_eq!(
            order_with(OrderKind::LimitDecrease, 5 * MIN_ORDER_NOTIONAL, 0).check_valid(
                OrderKind::LimitDecrease,
                &config,
                1
            ),
            Err(MarketError::InvalidOrder)
        );
        let order = Order {
            trigger_price: PRICE_SCALAR,
            ..order_with(OrderKind::LimitDecrease, 5 * MIN_ORDER_NOTIONAL, 0)
        };
        assert_eq!(
            order.check_valid(OrderKind::LimitDecrease, &config, 1),
            Ok(())
        );
    }

    /// Eligible while `ledger_seq <= expiration`. The first sequence past it
    /// rejects.
    #[test]
    fn expiration_boundary() {
        let config = base_config();
        let order = Order {
            expiration: 100,
            ..base_order()
        };
        assert_eq!(
            order.check_valid(OrderKind::MarketIncrease, &config, 100),
            Ok(())
        );
        assert_eq!(
            order.check_valid(OrderKind::MarketIncrease, &config, 101),
            Err(MarketError::OrderExpired)
        );
    }

    /// Any negative input (magnitude, trigger, or bound) is rejected up front.
    #[test]
    fn negative_inputs_rejected() {
        let config = base_config();
        let negative = Err(MarketError::NegativeValueNotAllowed);
        let cases = [
            (
                "negative notional",
                Order {
                    notional: -1,
                    ..base_order()
                },
            ),
            (
                "negative margin",
                Order {
                    margin: -1,
                    ..base_order()
                },
            ),
            (
                "negative trigger",
                Order {
                    trigger_price: -1,
                    ..base_order()
                },
            ),
            (
                "negative bound",
                Order {
                    price_bound: -1,
                    ..base_order()
                },
            ),
        ];
        for (label, order) in cases {
            assert_eq!(
                order.check_valid(OrderKind::MarketIncrease, &config, 1),
                negative,
                "{}",
                label
            );
        }
    }

    // --- check_trigger (fill eligibility) ---

    /// Ledger sequence and close time used across the trigger tests unless the
    /// case injects its own.
    const TEST_LEDGER_SEQ: u32 = 50;
    const TEST_NOW: u64 = 500;

    // A verified price straddling a $10 mid (bid 9.90, ask 10.10), published
    // at timestamp 500 (seconds).
    fn eligibility_price() -> PriceData {
        PriceData {
            bid: 99 * PRICE_SCALAR / 10,
            ask: 101 * PRICE_SCALAR / 10,
            publish_time: 500,
        }
    }

    // A plain market order (no trigger, no bound) filling at a fresh price passes.
    #[test]
    fn check_trigger_market_order_passes() {
        let order = Order {
            expiration: 100,
            created_at: 500,
            ..base_order()
        };
        assert_eq!(
            order.check_trigger(
                OrderKind::MarketIncrease,
                &eligibility_price(),
                TEST_LEDGER_SEQ,
                TEST_NOW
            ),
            Ok(())
        );
    }

    // A ledger sequence past the expiration rejects.
    #[test]
    fn check_trigger_expired_rejects() {
        let order = Order {
            expiration: 100,
            created_at: 500,
            ..base_order()
        };
        assert_eq!(
            order.check_trigger(
                OrderKind::MarketIncrease,
                &eligibility_price(),
                101,
                TEST_NOW
            ),
            Err(MarketError::OrderExpired)
        );
    }

    // The expiration ledger itself still fills. Only a sequence past it
    // expires the order.
    #[test]
    fn check_trigger_fills_on_the_expiration_ledger() {
        let order = Order {
            expiration: 100,
            created_at: 500,
            ..base_order()
        };
        assert_eq!(
            order.check_trigger(
                OrderKind::MarketIncrease,
                &eligibility_price(),
                100,
                TEST_NOW
            ),
            Ok(())
        );
    }

    // A price published before the order, on a later ledger, is stale.
    #[test]
    fn check_trigger_stale_price_rejects() {
        let order = Order {
            expiration: 100,
            created_at: 550, // price.publish_time 500 < 550, and now 600 != 550
            ..base_order()
        };
        assert_eq!(
            order.check_trigger(
                OrderKind::MarketIncrease,
                &eligibility_price(),
                TEST_LEDGER_SEQ,
                600
            ),
            Err(MarketError::StalePrice)
        );
    }

    // The atomic market-fill exemption: a market order (no trigger) whose
    // created_at equals the fill timestamp accepts a price published earlier.
    #[test]
    fn check_trigger_same_ledger_market_fill_exempts_staleness() {
        let order = Order {
            expiration: 100,
            created_at: 550, // == now, so the publish_time 500 < 550 check is skipped
            ..base_order()
        };
        assert_eq!(
            order.check_trigger(
                OrderKind::MarketIncrease,
                &eligibility_price(),
                TEST_LEDGER_SEQ,
                550
            ),
            Ok(())
        );
    }

    // A trigger order gets no same-ledger exemption: even with created_at
    // equal to the fill timestamp, a price published earlier is stale.
    #[test]
    fn check_trigger_same_ledger_trigger_order_stays_stale() {
        let order = Order {
            kind: OrderKind::LimitDecrease as u32,
            trigger_price: 95 * PRICE_SCALAR / 10,
            expiration: 100,
            created_at: 550, // == now, but publish_time 500 < 550 still rejects
            ..base_order()
        };
        assert_eq!(
            order.check_trigger(
                OrderKind::LimitDecrease,
                &eligibility_price(),
                TEST_LEDGER_SEQ,
                550
            ),
            Err(MarketError::StalePrice)
        );
    }

    // A trigger order filling in its creation ledger passes on a price at
    // least as fresh as the order.
    #[test]
    fn check_trigger_same_ledger_trigger_order_fresh_price_passes() {
        let order = Order {
            kind: OrderKind::LimitDecrease as u32,
            trigger_price: 95 * PRICE_SCALAR / 10, // crossed: exit bid 9.90 >= 9.50
            expiration: 100,
            created_at: 500, // == publish_time, not stale
            ..base_order()
        };
        assert_eq!(
            order.check_trigger(
                OrderKind::LimitDecrease,
                &eligibility_price(),
                TEST_LEDGER_SEQ,
                TEST_NOW
            ),
            Ok(())
        );
    }

    // A long take-profit (LimitDecrease) fires only once the exit price (the
    // 9.90 bid) reaches the trigger. Below it the order holds.
    #[test]
    fn check_trigger_long_decrease_above_trigger() {
        let unmet = Order {
            kind: OrderKind::LimitDecrease as u32,
            is_long: true,
            trigger_price: 10 * PRICE_SCALAR, // bid 9.90 < 10.00: not crossed
            expiration: 100,
            created_at: 500,
            ..base_order()
        };
        assert_eq!(
            unmet.check_trigger(
                OrderKind::LimitDecrease,
                &eligibility_price(),
                TEST_LEDGER_SEQ,
                TEST_NOW
            ),
            Err(MarketError::TriggerNotMet)
        );
        let met = Order {
            trigger_price: 95 * PRICE_SCALAR / 10, // bid 9.90 >= 9.50: crossed
            ..unmet
        };
        assert_eq!(
            met.check_trigger(
                OrderKind::LimitDecrease,
                &eligibility_price(),
                TEST_LEDGER_SEQ,
                TEST_NOW
            ),
            Ok(())
        );
    }

    // A long stop loss (StopDecrease) fires only once the exit price (the
    // 9.90 bid) falls to the trigger. Above it the order holds.
    #[test]
    fn check_trigger_long_stop_decrease_below_trigger() {
        let unmet = Order {
            kind: OrderKind::StopDecrease as u32,
            is_long: true,
            trigger_price: 95 * PRICE_SCALAR / 10, // bid 9.90 > 9.50: not crossed
            expiration: 100,
            created_at: 500,
            ..base_order()
        };
        assert_eq!(
            unmet.check_trigger(
                OrderKind::StopDecrease,
                &eligibility_price(),
                TEST_LEDGER_SEQ,
                TEST_NOW
            ),
            Err(MarketError::TriggerNotMet)
        );
        let met = Order {
            trigger_price: 10 * PRICE_SCALAR, // bid 9.90 <= 10.00: crossed
            ..unmet
        };
        assert_eq!(
            met.check_trigger(
                OrderKind::StopDecrease,
                &eligibility_price(),
                TEST_LEDGER_SEQ,
                TEST_NOW
            ),
            Ok(())
        );
    }

    // A long increase buys base at the ask (10.10) and caps it at
    // price_bound. A bound below the ask rejects. At or above it passes.
    #[test]
    fn check_trigger_long_increase_price_bound() {
        let too_tight = Order {
            kind: OrderKind::MarketIncrease as u32,
            is_long: true,
            price_bound: 10 * PRICE_SCALAR, // ask 10.10 > 10.00: exceeded
            expiration: 100,
            created_at: 500,
            ..base_order()
        };
        assert_eq!(
            too_tight.check_trigger(
                OrderKind::MarketIncrease,
                &eligibility_price(),
                TEST_LEDGER_SEQ,
                TEST_NOW
            ),
            Err(MarketError::PriceBoundExceeded)
        );
        let ok = Order {
            price_bound: 102 * PRICE_SCALAR / 10, // ask 10.10 <= 10.20: within
            ..too_tight
        };
        assert_eq!(
            ok.check_trigger(
                OrderKind::MarketIncrease,
                &eligibility_price(),
                TEST_LEDGER_SEQ,
                TEST_NOW
            ),
            Ok(())
        );
    }

    // A short increase sells base at the bid (9.90) and floors it at
    // price_bound. A bound above the bid rejects. At or below it passes.
    #[test]
    fn check_trigger_short_increase_price_bound() {
        let too_tight = Order {
            kind: OrderKind::MarketIncrease as u32,
            is_long: false,
            price_bound: 10 * PRICE_SCALAR, // bid 9.90 < 10.00: exceeded
            expiration: 100,
            created_at: 500,
            ..base_order()
        };
        assert_eq!(
            too_tight.check_trigger(
                OrderKind::MarketIncrease,
                &eligibility_price(),
                TEST_LEDGER_SEQ,
                TEST_NOW
            ),
            Err(MarketError::PriceBoundExceeded)
        );
        let ok = Order {
            price_bound: 98 * PRICE_SCALAR / 10, // bid 9.90 >= 9.80: within
            ..too_tight
        };
        assert_eq!(
            ok.check_trigger(
                OrderKind::MarketIncrease,
                &eligibility_price(),
                TEST_LEDGER_SEQ,
                TEST_NOW
            ),
            Ok(())
        );
    }

    // OrderKind::from_u32 round-trips every discriminant and panics on an
    // unknown one. The is_increase, is_decrease, is_market, and is_trigger
    // predicates partition the six kinds correctly.
    #[test]
    fn order_kind_decode_and_predicates() {
        let e = Env::default();
        let kinds = [
            (OrderKind::MarketIncrease, true, false, true),
            (OrderKind::LimitIncrease, true, false, false),
            (OrderKind::StopIncrease, true, false, false),
            (OrderKind::MarketDecrease, false, true, true),
            (OrderKind::LimitDecrease, false, true, false),
            (OrderKind::StopDecrease, false, true, false),
        ];
        for (kind, is_increase, is_decrease, is_market) in kinds {
            assert_eq!(OrderKind::from_u32(&e, kind as u32), kind);
            assert_eq!(kind.is_increase(), is_increase);
            assert_eq!(kind.is_decrease(), is_decrease);
            assert_eq!(kind.is_market(), is_market);
            assert_eq!(kind.is_trigger(), !is_market);
        }
    }

    // An unrecognized discriminant panics with UnknownKind.
    #[test]
    #[should_panic(expected = "#734")]
    fn order_kind_from_u32_panics_on_unknown_discriminant() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            OrderKind::from_u32(&e, 6);
        });
    }

    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{contract, contractimpl, Address};

    #[contract]
    struct Harness;

    // A thin entry point so a test can capture `cancel_order`'s error through
    // the generated client's `try_` method (the crate is `no_std`, so there is
    // no `catch_unwind`) and go on to assert the post-call escrow invariants.
    #[contractimpl]
    impl Harness {
        pub fn cancel(e: Env, user: Address, id: u32) -> i128 {
            cancel_order(&e, &user, id)
        }
    }

    // Order creation halts while the market is Frozen.
    #[test]
    #[should_panic(expected = "#704")]
    fn create_order_rejected_when_frozen() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let user = Address::generate(&e);
        e.as_contract(&contract_id, || {
            storage::set_status(&e, &Status::Frozen);
            create_order(
                &e,
                &user,
                true,
                OrderKind::MarketIncrease as u32,
                10_000_000,
                10_000_000,
                0,
                0,
                100,
            );
        });
    }

    fn submit_setup(e: &Env) {
        storage::set_status(e, &Status::Active);
        storage::set_config(e, &crate::engine::config::test_config());
    }

    // Registers a settlement token, mints `amount` to `user`, and records it
    // on `contract_id` as the settlement token, covering the exec_fee escrow
    // every `create_order` call takes.
    fn fund_escrow_token(e: &Env, contract_id: &Address, user: &Address, amount: i128) {
        let token_admin = Address::generate(e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        soroban_sdk::token::StellarAssetClient::new(e, &token.address()).mint(user, &amount);
        e.as_contract(contract_id, || {
            storage::set_token(e, &token.address());
        });
    }

    // A cancel returns the refunded escrow: an increase pays back its
    // margin plus the exec_fee, a decrease its exec_fee alone, and the
    // user's wallet is made whole.
    #[test]
    fn cancel_order_returns_the_refunded_escrow() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let user = Address::generate(&e);
        fund_escrow_token(&e, &contract_id, &user, 20_000_000);
        let increase_id = e.as_contract(&contract_id, || {
            submit_setup(&e);
            create_order(
                &e,
                &user,
                true,
                OrderKind::MarketIncrease as u32,
                0,
                10_000_000,
                0,
                0,
                100,
            )
        });
        // The increase escrowed its 10_000_000 margin plus the config's
        // 100_000 exec_fee.
        e.as_contract(&contract_id, || {
            assert_eq!(cancel_order(&e, &user, increase_id), 10_100_000);
        });
        let decrease_id = e.as_contract(&contract_id, || {
            create_order(
                &e,
                &user,
                true,
                OrderKind::MarketDecrease as u32,
                10_000_000,
                0,
                0,
                0,
                100,
            )
        });
        // The decrease escrowed the exec_fee alone.
        e.as_contract(&contract_id, || {
            assert_eq!(cancel_order(&e, &user, decrease_id), 100_000);
        });
        let token = e.as_contract(&contract_id, || storage::get_token(&e));
        assert_eq!(
            soroban_sdk::token::Client::new(&e, &token).balance(&user),
            20_000_000,
            "both cancels made the wallet whole",
        );
    }

    // A cancel is halted while the market is Frozen, and the halt leaves the
    // escrow untouched: no refund is paid and the pending order survives, so
    // the cancel can still run once the market thaws.
    #[test]
    fn cancel_rejected_while_frozen() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let user = Address::generate(&e);
        fund_escrow_token(&e, &contract_id, &user, 20_000_000);

        let id = e.as_contract(&contract_id, || {
            submit_setup(&e);
            create_order(
                &e,
                &user,
                true,
                OrderKind::MarketIncrease as u32,
                0,
                10_000_000,
                0,
                0,
                100,
            )
        });

        // The increase escrowed its 10.0 margin plus the 0.1 exec_fee.
        let token = e.as_contract(&contract_id, || storage::get_token(&e));
        let token_client = soroban_sdk::token::Client::new(&e, &token);
        let wallet_after_escrow = token_client.balance(&user);
        assert_eq!(wallet_after_escrow, 9_900_000);

        // The frozen guard rejects the cancel with MarketFrozen (#704) before
        // it can refund or delete.
        e.as_contract(&contract_id, || storage::set_status(&e, &Status::Frozen));
        let outcome = HarnessClient::new(&e, &contract_id).try_cancel(&user, &id);
        assert_eq!(
            outcome.unwrap_err().unwrap(),
            MarketError::MarketFrozen.into()
        );

        // Neither the escrow nor the pending order moves, so the cancel can
        // still run once the market thaws.
        assert_eq!(
            token_client.balance(&user),
            wallet_after_escrow,
            "the escrow stays put",
        );
        e.as_contract(&contract_id, || {
            assert!(
                e.storage()
                    .persistent()
                    .has(&storage::DataKey::Order(user.clone(), id)),
                "the pending order survives the frozen cancel",
            );
        });
    }

    // A decrease may be submitted before any position is open on that side:
    // the order is stored and the zeroed position row is created.
    #[test]
    fn submit_decrease_accepted_without_open_position() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let user = Address::generate(&e);
        fund_escrow_token(&e, &contract_id, &user, 1_000_000);
        e.as_contract(&contract_id, || {
            submit_setup(&e);
            let id = create_order(
                &e,
                &user,
                true,
                OrderKind::MarketDecrease as u32,
                10_000_000,
                0,
                0,
                0,
                100,
            );
            assert!(e
                .storage()
                .persistent()
                .has(&storage::DataKey::Order(user.clone(), id)));
            let key = storage::DataKey::Position(user.clone(), true);
            assert!(e.storage().persistent().has(&key));
            let position = storage::get_position(&e, &user, true);
            assert_eq!(position.notional, 0);
            assert_eq!(position.margin, 0);
            assert_eq!(position.tokens, 0);
        });
    }

    // Submitting against an existing position leaves its fields untouched and
    // tops its entry TTL back up to the user tier.
    #[test]
    fn submit_extends_existing_position_ttl() {
        use soroban_sdk::testutils::storage::Persistent as _;
        use soroban_sdk::testutils::Ledger as _;

        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let user = Address::generate(&e);
        let thirty_days_ledgers = 30 * 17280;
        fund_escrow_token(&e, &contract_id, &user, 1_000_000);
        e.as_contract(&contract_id, || {
            submit_setup(&e);
            storage::extend_instance(&e);
            let position = crate::engine::position::Position {
                notional: 50_000_000,
                margin: 25_000_000,
                tokens: 5_000,
                ..crate::engine::position::Position::zeroed(&e)
            };
            storage::set_position(&e, &user, true, &position);
        });

        // Age the entry below the 100-day threshold so the submit touch fires.
        e.ledger()
            .with_mut(|ledger| ledger.sequence_number += thirty_days_ledgers);

        e.as_contract(&contract_id, || {
            let sequence = e.ledger().sequence();
            create_order(
                &e,
                &user,
                true,
                OrderKind::MarketDecrease as u32,
                10_000_000,
                0,
                0,
                0,
                sequence + 100,
            );
            let position = storage::get_position(&e, &user, true);
            assert_eq!(position.notional, 50_000_000);
            assert_eq!(position.margin, 25_000_000);
            let key = storage::DataKey::Position(user.clone(), true);
            assert_eq!(
                e.storage().persistent().get_ttl(&key),
                storage::LEDGER_BUMP_USER
            );
        });
    }
}
