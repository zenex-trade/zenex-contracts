//! The v2 invariant ledger: the properties every stateful target re-checks
//! after every command.
//!
//! [`Ledger::check`] runs the whole set after every command: the always-on
//! properties, plus the flat-book ones (which only hold once every position has
//! realized its accruals) whenever the run happens to have emptied the book.
//!
//! Each check names the property it defends.
//!
//! State is read through [`Snapshot::take`], which enters the market
//! contract's frame once with `Env::as_contract` and reads its storage module
//! directly. Going through the contract's view entries instead would mean one
//! full host invocation per read, around sixty per command. That alone costs
//! an order of magnitude in fuzzing throughput. The reads are the same ones
//! the view entries perform.

use market::storage::DataKey;
use market::{MarketData, Order, Position, VaultOrder};
use soroban_sdk::{Address, Env};

use crate::fixture::{Fixture, SCALAR_18};

/// Highest order id the ledger walks per user when summing live escrow.
///
/// Ids come from a strictly increasing per-user counter, so walking
/// `1..counter` enumerates every id ever handed out. The cap keeps a
/// pathological run from turning the invariant check into the bottleneck. A run
/// that exceeds it fails explicitly, so increasing trace lengths cannot
/// silently disable escrow or custody assertions.
const MAX_TRACKED_ORDERS: u32 = 512;

/// One trader's state as of a check.
pub struct UserState {
    pub address: Address,
    pub long: Position,
    pub short: Position,
    pub claimable: i128,
    pub order_counter: u32,
    /// Live trade orders, by id.
    pub orders: std::vec::Vec<(u32, Order)>,
    /// Live vault orders, by id.
    pub vault_orders: std::vec::Vec<(u32, VaultOrder)>,
}

impl UserState {
    fn position(&self, is_long: bool) -> &Position {
        if is_long {
            &self.long
        } else {
            &self.short
        }
    }

    fn order(&self, id: u32) -> Option<&Order> {
        self.orders
            .iter()
            .find(|(oid, _)| *oid == id)
            .map(|(_, o)| o)
    }
}

/// Everything the invariant set reads, gathered in one contract frame.
pub struct Snapshot {
    pub market: MarketData,
    /// `publish_time` of the temporary `PriceCache` entry, `None` when the
    /// lazy entry is absent (never seeded, or lapsed).
    pub price_cache_time: Option<u64>,
    pub users: std::vec::Vec<UserState>,
}

/// Read a position without writing: the snapshot is an observer and must not
/// create rows or extend TTLs mid-run. An absent entry reads as the zeroed row.
fn probe_position(e: &Env, user: &Address, is_long: bool) -> Position {
    e.storage()
        .persistent()
        .get(&DataKey::Position(user.clone(), is_long))
        .unwrap_or_else(|| Position {
            margin: 0,
            notional: 0,
            tokens: 0,
            funding_idx: 0,
            borrowing_idx: 0,
            locked_notional: 0,
            unlocks_at: 0,
            priced_at: 0,
            decrease_orders: soroban_sdk::Vec::new(e),
        })
}

impl Snapshot {
    pub fn take(f: &Fixture) -> Snapshot {
        f.env.as_contract(&f.market_address, || {
            let e: &Env = &f.env;
            let market = market::storage::get_market_data(e);
            let mut users = std::vec::Vec::with_capacity(f.users.len());
            for address in &f.users {
                let order_counter = market::storage::get_order_counter(e, address);
                let mut orders = std::vec::Vec::new();
                let mut vault_orders = std::vec::Vec::new();
                if order_counter > MAX_TRACKED_ORDERS {
                    panic!("observer order capacity exceeded: {order_counter}");
                } else {
                    for id in 1..order_counter {
                        if let Some(order) = e
                            .storage()
                            .persistent()
                            .get::<_, Order>(&DataKey::Order(address.clone(), id))
                        {
                            orders.push((id, order));
                        }
                        if let Some(vault_order) = e
                            .storage()
                            .persistent()
                            .get::<_, VaultOrder>(&DataKey::VaultOrder(address.clone(), id))
                        {
                            vault_orders.push((id, vault_order));
                        }
                    }
                }
                users.push(UserState {
                    address: address.clone(),
                    long: probe_position(e, address, true),
                    short: probe_position(e, address, false),
                    claimable: market::storage::get_claimable_credit(e, address),
                    order_counter,
                    orders,
                    vault_orders,
                });
            }
            Snapshot {
                market,
                price_cache_time: market::storage::get_price_cache(e).map(|p| p.publish_time),
                users,
            }
        })
    }

    /// Settlement token still escrowed by pending orders: an increase order's
    /// `margin + exec_fee`, a decrease order's `exec_fee`, a vault deposit's
    /// `amount + exec_fee`, and a vault redeem's `exec_fee` (its principal is
    /// escrowed in shares, not settlement token).
    fn live_escrow(&self) -> i128 {
        let mut escrow = 0i128;
        for user in &self.users {
            for (_, order) in &user.orders {
                // OrderKind discriminants 0..=2 are the increase kinds.
                escrow += if order.kind < 3 {
                    order.margin + order.exec_fee
                } else {
                    order.exec_fee
                };
            }
            for (_, vault_order) in &user.vault_orders {
                // VaultOrderKind::Deposit == 0 escrows assets. Redeem == 1
                // escrows shares, so only its exec_fee is settlement token.
                escrow += if vault_order.kind == 0 {
                    vault_order.amount + vault_order.exec_fee
                } else {
                    vault_order.exec_fee
                };
            }
        }
        escrow
    }
}

/// Monotone quantities carried across a run.
#[derive(Debug, Default)]
pub struct Ledger {
    borrowing_idx_long: i128,
    borrowing_idx_short: i128,
    accrued_at: u64,
    last_price_time: u64,
    order_counters: std::vec::Vec<u32>,
    /// Each open position's last seen borrowing checkpoint, per user as
    /// `[long, short]`. `None` while the position is closed.
    checkpoints: std::vec::Vec<[Option<i128>; 2]>,
}

impl Ledger {
    pub fn new(user_count: usize) -> Self {
        Ledger {
            order_counters: std::vec![0; user_count],
            checkpoints: std::vec![[None; 2]; user_count],
            ..Default::default()
        }
    }

    /// The full invariant set: the always-on checks, plus the flat-book ones
    /// whenever the run happens to have emptied the book. `context` names the
    /// command that just ran.
    pub fn check(&mut self, f: &Fixture, context: &str) {
        let snapshot = Snapshot::take(f);
        self.check_non_negative(&snapshot, context);
        self.check_monotone(&snapshot, context);
        self.check_positions(&snapshot, context);
        self.check_borrowing(f, &snapshot, context);
        self.check_funding_ledger(&snapshot, context);
        self.check_solvency(f, &snapshot, context);
        self.check_flat(&snapshot, context);
    }

    /// **Non-negativity.** No side of the book, and no funding ledger total,
    /// may go negative. A negative aggregate means a position unwound more
    /// than it contributed. A negative `credit_pool` means the pool recorded
    /// paying out money it never collected.
    fn check_non_negative(&self, s: &Snapshot, context: &str) {
        let m = &s.market;
        for (name, value) in [
            ("notional.long", m.notional.long),
            ("notional.short", m.notional.short),
            ("margin.long", m.margin.long),
            ("margin.short", m.margin.short),
            ("tokens.long", m.tokens.long),
            ("tokens.short", m.tokens.short),
            ("borrowing_weight.long", m.borrowing_weight.long),
            ("borrowing_weight.short", m.borrowing_weight.short),
            ("credit_pool", m.credit_pool),
            ("credit_owed", m.credit_owed),
        ] {
            assert!(value >= 0, "[{context}] {name} went negative: {value}");
        }
        // Every checkpoint index is at most its side's current index, so the
        // unsettled borrowing `notional * borrowing_idx - borrowing_weight`
        // is non-negative per side. A negative side would mean the weight
        // recorded a checkpoint ahead of the index, and `pending_borrowing`
        // would clamp it to zero and hide the LP-side mispricing. On an
        // empty side this forces the weight, and so the pending borrowing,
        // to zero: a flat book has nothing left for `lp_net_pnl` to price.
        for is_long in [true, false] {
            let pending = side_pending_borrowing(m, is_long);
            assert!(
                pending >= 0,
                "[{context}] {} pending borrowing went negative: {pending}",
                side_name(is_long)
            );
        }
    }

    /// **Monotone clocks and indices.** Borrowing accrues a non-negative rate
    /// over non-negative elapsed time on both sides, so neither index may
    /// fall. The accrual timestamp only moves forward. The consumed-price
    /// clock is the temporary `PriceCache` entry: monotonic on `publish_time`
    /// while the entry lives, but the entry is lazy and may lapse. After that
    /// an older payload may legitimately re-seed it, so absence resets the
    /// watermark. (The funding indices are deliberately bidirectional: the
    /// receiving side's index falls. They are not checked here.)
    fn check_monotone(&mut self, s: &Snapshot, context: &str) {
        let m = &s.market;
        for (name, seen, current) in [
            (
                "borrowing_idx.long",
                &mut self.borrowing_idx_long,
                m.borrowing_idx.long,
            ),
            (
                "borrowing_idx.short",
                &mut self.borrowing_idx_short,
                m.borrowing_idx.short,
            ),
        ] {
            assert!(
                current >= *seen,
                "[{context}] {name} moved backward: {seen} -> {current}"
            );
            *seen = current;
        }
        for (name, seen, current) in [("accrued_at", &mut self.accrued_at, m.accrued_at)] {
            assert!(
                current >= *seen,
                "[{context}] {name} moved backward: {seen} -> {current}"
            );
            *seen = current;
        }
        match s.price_cache_time {
            Some(current) => {
                assert!(
                    current >= self.last_price_time,
                    "[{context}] price cache publish_time moved backward: {} -> {current}",
                    self.last_price_time,
                );
                self.last_price_time = current;
            }
            // The lazy entry lapsed (or was never seeded): the next consumed
            // payload may carry any admissible publish_time, so the watermark
            // restarts from it.
            None => self.last_price_time = 0,
        }
    }

    /// **Aggregate consistency**, **position coherence**, and the
    /// pending-decrease list's integrity.
    ///
    /// Every per-side market total must equal the sum over the open positions
    /// on that side: the utilization, funding, ADL and share-pricing gates all
    /// read the aggregates, so drift between them and the positions silently
    /// misprices the whole book.
    ///
    /// A position with no size must be the canonical all-zero row. Its
    /// `decrease_orders` list may still be non-empty (a trader can rest a
    /// decrease before ever opening), but every id on it must resolve to a live
    /// decrease order on that side. The list is the escrow-refund funnel a
    /// closure walks, so a stale id strands that escrow permanently and burns
    /// one of the side's eight slots.
    fn check_positions(&mut self, s: &Snapshot, context: &str) {
        let mut sum = MarketTotals::default();
        for user in &s.users {
            for is_long in [true, false] {
                let p = user.position(is_long);
                assert!(
                    p.notional >= 0 && p.margin >= 0 && p.tokens >= 0,
                    "[{context}] position ({is_long}) has a negative field: \
                     notional {} margin {} tokens {}",
                    p.notional,
                    p.margin,
                    p.tokens
                );
                if p.notional == 0 {
                    assert_eq!(
                        (p.margin, p.tokens, p.locked_notional, p.unlocks_at),
                        (0, 0, 0, 0),
                        "[{context}] closed position ({is_long}) is not the zeroed row"
                    );
                } else {
                    assert!(
                        p.tokens > 0,
                        "[{context}] open position ({is_long}) has no base size"
                    );
                }
                for id in p.decrease_orders.iter() {
                    let order = user.order(id).unwrap_or_else(|| {
                        panic!(
                            "[{context}] position ({is_long}) lists order {id}, \
                             which no longer exists"
                        )
                    });
                    assert!(
                        order.kind >= 3,
                        "[{context}] position ({is_long}) lists order {id}, an increase kind"
                    );
                    assert_eq!(
                        order.is_long, is_long,
                        "[{context}] position ({is_long}) lists order {id} from the other side"
                    );
                }
                sum.add(is_long, p);
            }
        }
        let m = &s.market;
        assert_eq!(
            (m.notional.long, m.notional.short),
            (sum.notional_long, sum.notional_short),
            "[{context}] market notional drifted from the sum of positions"
        );
        assert_eq!(
            (m.margin.long, m.margin.short),
            (sum.margin_long, sum.margin_short),
            "[{context}] market margin drifted from the sum of positions"
        );
        assert_eq!(
            (m.tokens.long, m.tokens.short),
            (sum.tokens_long, sum.tokens_short),
            "[{context}] market tokens drifted from the sum of positions"
        );
        // The weight is exact (no rounding), so it must equal the sum of
        // every open row's `notional * borrowing_idx` at all times, not just
        // at a flat book. Drift misprices LP shares through `lp_net_pnl`.
        assert_eq!(
            (m.borrowing_weight.long, m.borrowing_weight.short),
            (sum.borrowing_weight_long, sum.borrowing_weight_short),
            "[{context}] market borrowing weight drifted from the sum of position checkpoints"
        );
    }

    /// **Borrowing checkpoints and the pending-borrowing mark.**
    ///
    /// Per open position: its checkpoint is at most its side's current
    /// index, since the index only grows and a checkpoint is a past value of
    /// it, and it never moves backward while the position stays open. A
    /// checkpoint ahead of the index or moving backward would undercharge
    /// or recharge the trader at the next settle. Checked per position
    /// because a side sum lets one bad row hide behind the others.
    ///
    /// Then the contract's own `MarketData::pending_borrowing`, the figure
    /// `lp_net_pnl` prices LP shares with, must equal the same amount
    /// recomputed from the positions rather than the stored weight: the
    /// exact sum of `notional * (index - checkpoint)`, floored for the
    /// trader-best mark and ceiled for the trader-worst.
    fn check_borrowing(&mut self, f: &Fixture, s: &Snapshot, context: &str) {
        let m = &s.market;
        let mut exact = 0i128;
        for (idx, user) in s.users.iter().enumerate() {
            for (slot, is_long) in [(0usize, true), (1usize, false)] {
                let p = user.position(is_long);
                if p.notional == 0 {
                    self.checkpoints[idx][slot] = None;
                    continue;
                }
                let index = m.borrowing_idx.get(is_long);
                let side = side_name(is_long);
                assert!(
                    p.borrowing_idx <= index,
                    "[{context}] a {side} checkpoint {} is ahead of the side index {index}",
                    p.borrowing_idx
                );
                if let Some(seen) = self.checkpoints[idx][slot] {
                    assert!(
                        p.borrowing_idx >= seen,
                        "[{context}] an open {side} checkpoint moved backward: {seen} -> {}",
                        p.borrowing_idx
                    );
                }
                self.checkpoints[idx][slot] = Some(p.borrowing_idx);
                exact += p.notional * (index - p.borrowing_idx);
            }
        }
        let floor = exact / SCALAR_18;
        let ceil = (exact + SCALAR_18 - 1) / SCALAR_18;
        assert_eq!(
            (
                m.pending_borrowing(&f.env, true),
                m.pending_borrowing(&f.env, false)
            ),
            (floor, ceil),
            "[{context}] pending_borrowing (trader-best, trader-worst) does not match \
             the positions' unsettled borrowing"
        );
    }

    /// **Funding ledger conservation**, plus the order-counter clock.
    ///
    /// `credit_owed` is the protocol's single record of the funding and parked
    /// failed payouts it owes traders. The per-user `ClaimableCredit` balances
    /// split it. Every write to one is matched 1:1 by the same write to the
    /// other, so they must stay equal. Drift either strands funding a trader
    /// earned, or lets the pool pay out more than it recorded owing.
    fn check_funding_ledger(&mut self, s: &Snapshot, context: &str) {
        let mut claimable_total = 0i128;
        for (idx, user) in s.users.iter().enumerate() {
            assert!(
                user.claimable >= 0,
                "[{context}] claimable funding went negative: {}",
                user.claimable
            );
            claimable_total += user.claimable;
            assert!(
                user.order_counter >= self.order_counters[idx],
                "[{context}] order counter moved backward: {} -> {}",
                self.order_counters[idx],
                user.order_counter
            );
            self.order_counters[idx] = user.order_counter;
        }
        assert_eq!(
            s.market.credit_owed, claimable_total,
            "[{context}] credit_owed does not match the sum of claimable balances"
        );
    }

    /// **Settlement conservation.** The market contract's settlement-token
    /// balance is fully accounted for at every instant:
    ///
    /// ```text
    /// balance == posted margin + funding pool + live escrow
    /// ```
    ///
    /// Everything the contract holds is one of those three: margin posted
    /// against open positions, unclaimed credit (funding and parked failed
    /// payouts), and escrow taken at order creation. A fill that pays a leg
    /// twice, forgets a refund, keeps a fee it was meant to forward, or draws
    /// more from the vault than it settles breaks this immediately. It is the
    /// generalization of "zero residual when the book is flat", and unlike that
    /// one it holds mid-flight, which is where the interesting bugs are.
    fn check_solvency(&mut self, f: &Fixture, s: &Snapshot, context: &str) {
        let escrow = s.live_escrow();
        let posted = s.market.margin.long + s.market.margin.short;
        let expected = posted + s.market.credit_pool + escrow;
        let actual = f.contract_balance();
        assert_eq!(
            actual, expected,
            "[{context}] contract balance {actual} != posted margin {posted} \
             + funding pool {} + live escrow {escrow}",
            s.market.credit_pool
        );
    }

    /// **Funding pool solvency.** Once every position has realized its
    /// accrued funding, the pool must have collected at least as much as it
    /// credited: `credit_pool >= credit_owed`.
    ///
    /// This is the one-sided-market guard. Per accrual the receiving side is
    /// credited `floor(pay_delta * payer_notional / receiver_notional)`, and is
    /// not credited at all when it is empty. Aggregate credits can then never
    /// exceed aggregate charges, unless a rounding error, a side flip, or a
    /// notional the accrual and the realization disagree about lets them. The
    /// failure it guards against is concrete: credits outrunning collections
    /// means `claim_credit` hands out money that came from posted margin or
    /// from another trader's escrow.
    ///
    /// It is only assertable at a flat book because realization is lazy: a
    /// receiver touched before its payer legitimately credits `credit_owed`
    /// ahead of the matching `credit_pool` inflow.
    fn check_flat(&mut self, s: &Snapshot, context: &str) {
        if s.market.notional.total() != 0 || s.market.tokens.total() != 0 {
            return;
        }
        assert!(
            s.market.credit_pool >= s.market.credit_owed,
            "[{context}] flat book credits more funding than it collected: \
             pool {} < owed {}",
            s.market.credit_pool,
            s.market.credit_owed
        );
        assert_eq!(
            s.market.margin.long + s.market.margin.short,
            0,
            "[{context}] flat book still carries posted margin"
        );
    }

    /// **An armed ADL side is genuinely over its clear target.**
    ///
    /// `update_adl_state` arms a side whose pending PnL exceeds `adl_max_pnl`
    /// of half the vault balance, and holds it armed while it exceeds
    /// `adl_clear_target` of the same. A valid config always has
    /// `adl_clear_target <= adl_max_pnl`, so an armed side must in every case
    /// sit strictly above the clear target.
    ///
    /// Arming is not cosmetic: an armed side blocks every size-growing
    /// increase on it and licenses any keeper to force-close its winners.
    /// Arming a side that is not over the line is a griefing vector. Failing
    /// to disarm one that has fallen back under it freezes that side out of
    /// the market.
    ///
    /// Call immediately after a successful `update_adl_state` with the bid/ask
    /// and `publish_time` of the report it verified: the accrual it runs moves
    /// the indices, not the size aggregates this recomputes from.
    pub fn check_adl_armed(
        &self,
        f: &Fixture,
        bid: i128,
        ask: i128,
        publish_time: u64,
        context: &str,
    ) {
        let (bid, ask) = effective_book(f, bid, ask, publish_time);
        let (adl, clear_target, market) = f.env.as_contract(&f.market_address, || {
            let e: &Env = &f.env;
            let config = market::storage::get_config(e);
            (
                market::storage::get_adl(e),
                config.adl_clear_target,
                market::storage::get_market_data(e),
            )
        });
        if !adl.long && !adl.short {
            return;
        }
        let clear_target = half_factor(f.vault.total_assets(), clear_target);
        for (is_long, armed) in [(true, adl.long), (false, adl.short)] {
            if !armed {
                continue;
            }
            let pnl = side_pnl_maximized(&market, bid, ask, is_long);
            assert!(
                pnl > clear_target,
                "[{context}] ADL is armed on the {} side at pending PnL {pnl}, \
                 at or under the {clear_target} clear target",
                if is_long { "long" } else { "short" }
            );
        }
    }
}

#[derive(Default)]
struct MarketTotals {
    notional_long: i128,
    notional_short: i128,
    margin_long: i128,
    margin_short: i128,
    tokens_long: i128,
    tokens_short: i128,
    borrowing_weight_long: i128,
    borrowing_weight_short: i128,
}

impl MarketTotals {
    fn add(&mut self, is_long: bool, p: &Position) {
        if is_long {
            self.notional_long += p.notional;
            self.margin_long += p.margin;
            self.tokens_long += p.tokens;
            self.borrowing_weight_long += p.notional * p.borrowing_idx;
        } else {
            self.notional_short += p.notional;
            self.margin_short += p.margin;
            self.tokens_short += p.tokens;
            self.borrowing_weight_short += p.notional * p.borrowing_idx;
        }
    }
}

/// `is_long`'s accrued but unsettled borrowing before `pending_borrowing`
/// rounds and clamps it: `notional * borrowing_idx - borrowing_weight`
/// (token-dec times `SCALAR_18`, exact).
fn side_pending_borrowing(market: &MarketData, is_long: bool) -> i128 {
    market.notional.get(is_long) * market.borrowing_idx.get(is_long)
        - market.borrowing_weight.get(is_long)
}

fn side_name(is_long: bool) -> &'static str {
    if is_long {
        "long"
    } else {
        "short"
    }
}

/// `math::half_factor`: `floor((balance / 2) * factor / SCALAR_18)`.
fn half_factor(balance: i128, factor: i128) -> i128 {
    ((balance / 2) * factor) / crate::fixture::SCALAR_18
}

/// `MarketData::side_pnl(.., maximize = true)`: a long marks its base size at
/// the ask rounded up, a short its entry notional against the bid-marked size
/// rounded down, and either side's loss floors at its posted margin.
pub fn side_pnl_maximized(market: &MarketData, bid: i128, ask: i128, is_long: bool) -> i128 {
    let scalar = crate::fixture::SCALAR_18;
    let pnl = if is_long {
        // Both operands are non-negative, so `(x + scalar - 1) / scalar` is the
        // ceiling `soroban_fixed_point_math` computes.
        (market.tokens.long * ask + scalar - 1) / scalar - market.notional.long
    } else {
        market.notional.short - (market.tokens.short * bid) / scalar
    };
    pnl.max(-market.margin.get(is_long))
}

/// Read one position without taking a full snapshot (targets use this to pick
/// decrease sizes).
pub fn position(f: &Fixture, user: &Address, is_long: bool) -> Position {
    f.env
        .as_contract(&f.market_address, || probe_position(&f.env, user, is_long))
}

/// Returns the bid/ask a substitution route prices at for a report carrying
/// `bid`, `ask` and `publish_time`.
///
/// Only the routes `Market::load` opens with `newest_price` (accrue,
/// liquidation, ADL arming and execution, vault fills) price this way. A fill
/// prices as submitted, so a model of `execute_order` must not call this.
pub fn effective_book(f: &Fixture, bid: i128, ask: i128, publish_time: u64) -> (i128, i128) {
    f.env.as_contract(&f.market_address, || {
        // A flat settlement price overrides every payload. Otherwise the
        // contract upgrades an older report to its strictly newer cache, so
        // a model that keeps the submitted pair recomputes at a mark the
        // contract never used.
        if market::storage::has_terminal_price(&f.env) {
            let terminal = market::storage::get_terminal_price(&f.env);
            return (terminal, terminal);
        }
        match market::storage::get_price_cache(&f.env) {
            Some(cached) if cached.publish_time > publish_time => (cached.bid, cached.ask),
            _ => (bid, ask),
        }
    })
}
