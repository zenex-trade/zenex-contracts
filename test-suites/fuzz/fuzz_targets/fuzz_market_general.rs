#![no_main]

//! Stateful fuzz target for the v2 order and position lifecycle.
//!
//! Drives the keeper-mediated order surface across three traders and one
//! keeper on a single market, interleaved with ledger jumps and a bounded
//! oracle walk: `create_order` (all six kinds, escrowed at creation),
//! `execute_order`, `cancel_order`, `claim_credit`, `accrue`, and
//! `update_adl_state`.
//!
//! Scope note: this target stays on an `Active` market so its teardown can
//! always flatten the book and assert the terminal funding invariant. Status
//! transitions, forced liquidation and ADL execution belong to
//! `fuzz_liquidation`. Vault deposit/redeem orders belong to
//! `fuzz_vault_orders`.
//!
//! Invariants live in `zenex_fuzz::invariants` and are documented in
//! `README.md`. The headline ones here are settlement conservation (the
//! contract's balance is always exactly posted margin + funding pool + live
//! escrow), aggregate consistency (market totals equal the sum of positions),
//! credit-ledger conservation (`credit_owed` equals the sum of claimable
//! balances), and known-error discipline on every entry.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use soroban_sdk::Address;

use zenex_fuzz::codes;
use zenex_fuzz::fixture::{Fixture, SCALAR_7, USER_COUNT};
use zenex_fuzz::invariants::Ledger;
use zenex_fuzz::orders::Orders;
use zenex_fuzz::price::{Oracle, PriceMove};

/// Commands per run. Long enough for a position to be opened, accrue funding
/// and borrowing across a jump, and be unwound. Short enough that libFuzzer
/// still gets a useful iteration rate.
const COMMANDS: usize = 24;

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    commands: [Command; COMMANDS],
}

#[derive(Arbitrary, Debug, Clone, Copy)]
enum Command {
    /// Submit an order, escrowing its margin (increase) or exec fee (decrease).
    CreateOrder {
        user: u8,
        is_long: bool,
        /// `OrderKind` discriminant, wrapped into `0..=5`.
        kind: u8,
        /// Size in whole settlement units before scaling.
        notional_units: u16,
        /// Posted margin as a fraction of notional, from the 10% initial-margin
        /// floor upward.
        margin_bps: u8,
        /// Trigger level as a basis-point offset from the current mark.
        trigger_bps: i16,
        /// Slippage bound as a basis-point offset from the mark (0 = unbounded).
        bound_bps: i16,
        /// Drop the size leg, leaving a margin-only order (a top-up or a
        /// withdrawal).
        margin_only: bool,
        /// Ask for the whole position (`i128::MAX`) on a decrease.
        full_close: bool,
        /// Expire at the current sequence, so any later jump that advances the
        /// sequence makes the order unfillable.
        expires_now: bool,
    },
    /// Keeper-fill one of the user's orders at the current mark.
    ExecuteOrder { user: u8, id: u8 },
    /// Cancel one of the user's orders and refund its escrow.
    CancelOrder { user: u8, id: u8 },
    /// Pay out the user's accrued funding.
    ClaimCredit { user: u8 },
    /// Price-bearing accrual of both indices.
    Accrue,
    /// Recompute the ADL flags.
    UpdateAdl,
    /// Advance the ledger clock.
    PassTime { units: u16 },
    /// Step the oracle walk.
    MovePrice(PriceMove),
}

fuzz_target!(|input: FuzzInput| {
    let f = Fixture::create();
    let mut oracle = Oracle::default();
    let mut ledger = Ledger::new(USER_COUNT);
    // The trade orders this run created, and the ids a fill or cancel has
    // spent. A second landing on a spent id would pay its escrow twice.
    let mut orders = Orders::default();

    ledger.check(&f, "setup");

    for (step, command) in input.commands.iter().enumerate() {
        run(&f, &mut oracle, &mut ledger, &mut orders, *command, step);
        let context = format!("after step {step}: {command:?}");
        ledger.check(&f, &context);
    }

    teardown(&f, &oracle, &mut ledger);
});

fn run(
    f: &Fixture,
    oracle: &mut Oracle,
    ledger: &mut Ledger,
    orders: &mut Orders,
    command: Command,
    step: usize,
) {
    let context = format!("step {step}");
    match command {
        Command::CreateOrder {
            user,
            is_long,
            kind,
            notional_units,
            margin_bps,
            trigger_bps,
            bound_bps,
            margin_only,
            full_close,
            expires_now,
        } => {
            let kind = (kind % 6) as u32;
            let is_decrease = kind >= 3;
            let notional = if margin_only {
                0
            } else if is_decrease && full_close {
                // The documented full-close sentinel: a decrease above the
                // position size clamps to the whole position at fill.
                i128::MAX
            } else {
                (notional_units as i128 + 1) * SCALAR_7
            };
            let margin = if is_decrease && full_close {
                0
            } else {
                margin_for(notional_units, margin_bps)
            };
            let trigger_price = if kind == 0 || kind == 3 {
                0
            } else {
                offset_price(oracle.mid, trigger_bps)
            };
            let price_bound = if bound_bps == 0 {
                0
            } else {
                offset_price(oracle.mid, bound_bps)
            };
            let expiration = if expires_now {
                f.env.ledger().sequence()
            } else {
                f.far_expiration()
            };
            let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
            let outcome = f.market.try_create_order(
                &f.user(user as usize),
                &is_long,
                &kind,
                &notional,
                &margin,
                &trigger_price,
                &price_bound,
                &expiration,
            );
            zenex_fuzz::observer::check(
                &f.env,
                &rollback_before,
                &outcome,
                codes::CREATE_ORDER,
                false,
                &context,
            );
            if let Ok(Ok(id)) = outcome {
                orders.created(user as usize % USER_COUNT, id);
            }
        }

        Command::ExecuteOrder { user, id } => {
            let (idx, id) = orders.pick(f, user as usize, id, |owner, candidate| {
                order_rests(f, &f.user(owner), candidate)
            });
            let user = f.user(idx);
            let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
            let outcome = f
                .market
                .try_execute_order(&f.keeper, &user, &id, &oracle.bytes(&f.env));
            zenex_fuzz::observer::check(
                &f.env,
                &rollback_before,
                &outcome,
                codes::EXECUTE_ORDER,
                true,
                &context,
            );
            orders.record(idx, id, codes::succeeded(&outcome), &context);
        }

        Command::CancelOrder { user, id } => {
            let (idx, id) = orders.pick(f, user as usize, id, |owner, candidate| {
                order_rests(f, &f.user(owner), candidate)
            });
            let user = f.user(idx);
            let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
            let outcome = f.market.try_cancel_order(&user, &id);
            zenex_fuzz::observer::check(
                &f.env,
                &rollback_before,
                &outcome,
                codes::CANCEL_ORDER,
                false,
                &context,
            );
            orders.record(idx, id, codes::succeeded(&outcome), &context);
        }

        Command::ClaimCredit { user } => {
            let user = f.user(user as usize);
            let before = f.market.get_claimable_credit(&user);
            let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
            let outcome = f.market.try_claim_credit(&user);
            zenex_fuzz::observer::check(
                &f.env,
                &rollback_before,
                &outcome,
                codes::CLAIM_CREDIT,
                false,
                &context,
            );
            if let Ok(Ok(paid)) = outcome {
                assert!(paid > 0, "[{context}] a landed claim paid nothing");
                assert!(
                    paid <= before,
                    "[{context}] claim paid {paid} against a {before} claimable balance"
                );
            }
        }

        Command::Accrue => {
            let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
            let outcome = f.market.try_accrue(&oracle.bytes(&f.env));
            zenex_fuzz::observer::check(
                &f.env,
                &rollback_before,
                &outcome,
                codes::ACCRUE,
                true,
                &context,
            );
        }

        Command::UpdateAdl => {
            let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
            let outcome = f.market.try_update_adl_state(&oracle.bytes(&f.env));
            zenex_fuzz::observer::check(
                &f.env,
                &rollback_before,
                &outcome,
                codes::UPDATE_ADL_STATE,
                true,
                &context,
            );
            if codes::succeeded(&outcome) {
                ledger.check_adl_armed(
                    f,
                    oracle.bid(),
                    oracle.ask(),
                    oracle.publish_time(&f.env),
                    &context,
                );
            }
        }

        // Four seconds per unit spans a single ledger up to roughly three days,
        // so a run crosses the notional lock, the funding hold bands, and a
        // meaningful borrowing accrual.
        Command::PassTime { units } => f.jump(units as u64 * 4),

        Command::MovePrice(mv) => oracle.apply(mv),
    }
}

/// Posted margin for a fill: the 10% initial-margin floor plus a fuzzer-chosen
/// cushion, so most orders are fillable while the thin end still probes
/// `InsufficientMargin`.
fn margin_for(notional_units: u16, margin_bps: u8) -> i128 {
    let notional = (notional_units as i128 + 1) * SCALAR_7;
    (notional * (1_000 + margin_bps as i128 * 30)) / 10_000
}

/// A price `bps` basis points away from `mid`, floored at one unit so it stays
/// a valid (positive) trigger.
fn offset_price(mid: i128, bps: i16) -> i128 {
    let bps = (bps as i128).clamp(-9_000, 9_000);
    ((mid * (10_000 + bps)) / 10_000).max(1)
}

/// Whether `user`'s trade order `id` still rests.
fn order_rests(f: &Fixture, user: &Address, id: u32) -> bool {
    matches!(f.market.try_get_order(user, &id), Ok(Ok(_)))
}

/// Flatten the book and assert the terminal invariants.
///
/// Unwinding matters: funding-pool solvency is only assertable once every
/// position has realized its accrued funding, and a run that leaves positions
/// open never reaches it. Each side is offered a voluntary full close first and
/// a liquidation second, which between them cover the healthy and the
/// underwater book.
fn teardown(f: &Fixture, oracle: &Oracle, ledger: &mut Ledger) {
    // Clear the decrease lock so a full close is not rejected out of hand.
    f.jump(3_600);
    let price = oracle.fresh_bytes(&f.env);

    for idx in 0..USER_COUNT {
        let user = f.user(idx);
        for is_long in [true, false] {
            if f.market.get_position(&user, &is_long).notional == 0 {
                continue;
            }
            let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
            let close = f.market.try_create_order(
                &user,
                &is_long,
                &3u32, // MarketDecrease
                &i128::MAX,
                &0,
                &0,
                &0,
                &f.far_expiration(),
            );
            zenex_fuzz::observer::check(
                &f.env,
                &rollback_before,
                &close,
                codes::CREATE_ORDER,
                false,
                "teardown close",
            );
            if let Ok(Ok(id)) = close {
                let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
                let filled = f.market.try_execute_order(&f.keeper, &user, &id, &price);
                zenex_fuzz::observer::check(
                    &f.env,
                    &rollback_before,
                    &filled,
                    codes::EXECUTE_ORDER,
                    true,
                    "teardown fill",
                );
                if !codes::succeeded(&filled) {
                    let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
                    let cancelled = f.market.try_cancel_order(&user, &id);
                    zenex_fuzz::observer::check(
                        &f.env,
                        &rollback_before,
                        &cancelled,
                        codes::CANCEL_ORDER,
                        false,
                        "cleanup cancel",
                    );
                }
            }

            if f.market.get_position(&user, &is_long).notional != 0 {
                let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
                let liquidated = f
                    .market
                    .try_execute_liquidation(&f.keeper, &user, &is_long, &price);
                zenex_fuzz::observer::check(
                    &f.env,
                    &rollback_before,
                    &liquidated,
                    codes::EXECUTE_LIQUIDATION,
                    true,
                    "teardown liquidation",
                );
            }
        }
    }

    ledger.check(f, "teardown");
    zenex_fuzz::observer::finish_stress(f);
}
