#![no_main]

//! Stateful fuzz target for forced closure: liquidation, ADL, and the delist
//! wind-down.
//!
//! Where `fuzz_market_general` fuzzes what a trader can ask for, this target
//! fuzzes what the protocol can do to a trader against their will:
//! `execute_liquidation`, `update_adl_state`, `execute_adl`, and the owner's
//! `set_status` / `set_terminal_price` wind-down levers. Each run seeds a
//! leveraged long and short so the first price move already has something to
//! break, then lets the fuzzer swing the mark, jump the clock (up to twelve
//! days, enough to clear the seven-day delist deadline), and race keepers
//! against the book.
//!
//! Beyond the shared invariant ledger, this target asserts:
//!
//! - a landed liquidation leaves the position fully closed.
//! - an armed ADL side really is above its clear target (`check_adl_armed`).
//! - a landed `execute_adl` strictly reduced the side's pending PnL. The
//!   contract enforces this with `AdlNotEligible`/`AdlOvershoot`, so a landed
//!   call that did not move the measure would mean a gate read the wrong book.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

use zenex_fuzz::codes;
use zenex_fuzz::fixture::{Fixture, SCALAR_7, USER_COUNT};
use zenex_fuzz::invariants::{effective_book, position, side_pnl_maximized, Ledger};
use zenex_fuzz::price::{Oracle, PriceMove};

const COMMANDS: usize = 20;

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    /// Size of the seeded long, in whole settlement units.
    seed_long_units: u16,
    /// Size of the seeded short, in whole settlement units.
    seed_short_units: u16,
    /// Margin cushion above the 10% floor for both seeds, in 30-bps steps.
    seed_margin_steps: u8,
    commands: [Command; COMMANDS],
}

#[derive(Arbitrary, Debug, Clone, Copy)]
enum Command {
    /// Open (or grow) a position with an immediately-filled market increase.
    Open {
        user: u8,
        is_long: bool,
        notional_units: u16,
        margin_steps: u8,
    },
    /// Force-close a position at the current mark.
    Liquidate { user: u8, is_long: bool },
    /// Recompute the ADL flags.
    UpdateAdl,
    /// Deleverage a winner on an armed side.
    ExecuteAdl {
        user: u8,
        is_long: bool,
        /// Slice of the position's notional to close, `(amount_pct + 1) / 256`
        /// of it.
        amount_pct: u8,
        /// Ask for the whole position instead of a slice.
        full: bool,
    },
    /// Price-bearing accrual of both indices.
    Accrue,
    /// Owner status transition, wrapped into `Active`/`OnIce`/`Frozen`/`Delisted`.
    /// `Retired` is excluded: it is absorbing and would end the run's coverage.
    SetStatus { status: u8 },
    /// Owner flat settlement price, valid once the delist grace window expires.
    SetTerminalPrice { bps: i16 },
    /// Advance the ledger clock.
    PassTime { units: u16 },
    /// Step the oracle walk.
    MovePrice(PriceMove),
}

fuzz_target!(|input: FuzzInput| {
    let f = Fixture::create();
    let mut oracle = Oracle::default();
    let mut ledger = Ledger::new(USER_COUNT);

    seed(
        &f,
        &oracle,
        input.seed_long_units,
        input.seed_short_units,
        input.seed_margin_steps,
    );
    ledger.check(&f, "seed");

    for (step, command) in input.commands.iter().enumerate() {
        run(&f, &mut oracle, &mut ledger, *command, step);
        let context = format!("after step {step}: {command:?}");
        ledger.check(&f, &context);
    }

    teardown(&f, &oracle, &mut ledger);
});

/// Open one leveraged long and one leveraged short at the starting mark.
///
/// Without a seeded book most runs would spend their whole budget bouncing off
/// `PositionNotFound`. With it, the very first `MovePrice` can push a side
/// under maintenance margin or over the ADL trigger.
fn seed(f: &Fixture, oracle: &Oracle, long_units: u16, short_units: u16, margin_steps: u8) {
    let price = oracle.fresh_bytes(&f.env);
    for (idx, is_long, units) in [(0usize, true, long_units), (1usize, false, short_units)] {
        let user = f.user(idx);
        let notional = (units as i128 + 1) * SCALAR_7;
        let margin = margin_for(notional, margin_steps);
        let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
        let created = f.market.try_create_order(
            &user,
            &is_long,
            &0u32, // MarketIncrease
            &notional,
            &margin,
            &0,
            &0,
            &f.far_expiration(),
        );
        zenex_fuzz::observer::check(
            &f.env,
            &rollback_before,
            &created,
            codes::CREATE_ORDER,
            false,
            "seed create",
        );
        if let Ok(Ok(id)) = created {
            let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
            let filled = f.market.try_execute_order(&f.keeper, &user, &id, &price);
            zenex_fuzz::observer::check(
                &f.env,
                &rollback_before,
                &filled,
                codes::EXECUTE_ORDER,
                true,
                "seed fill",
            );
            if !codes::succeeded(&filled) {
                let _ = f.market.try_cancel_order(&user, &id);
            }
        }
    }
}

fn run(f: &Fixture, oracle: &mut Oracle, ledger: &mut Ledger, command: Command, step: usize) {
    let context = format!("step {step}");
    match command {
        Command::Open {
            user,
            is_long,
            notional_units,
            margin_steps,
        } => {
            let user = f.user(user as usize);
            let notional = (notional_units as i128 + 1) * SCALAR_7;
            let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
            let created = f.market.try_create_order(
                &user,
                &is_long,
                &0u32,
                &notional,
                &margin_for(notional, margin_steps),
                &0,
                &0,
                &f.far_expiration(),
            );
            zenex_fuzz::observer::check(
                &f.env,
                &rollback_before,
                &created,
                codes::CREATE_ORDER,
                false,
                &context,
            );
            if let Ok(Ok(id)) = created {
                let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
                let filled =
                    f.market
                        .try_execute_order(&f.keeper, &user, &id, &oracle.bytes(&f.env));
                zenex_fuzz::observer::check(
                    &f.env,
                    &rollback_before,
                    &filled,
                    codes::EXECUTE_ORDER,
                    true,
                    &context,
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
        }

        Command::Liquidate { user, is_long } => {
            let user = f.user(user as usize);
            let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
            let outcome =
                f.market
                    .try_execute_liquidation(&f.keeper, &user, &is_long, &oracle.bytes(&f.env));
            zenex_fuzz::observer::check(
                &f.env,
                &rollback_before,
                &outcome,
                codes::EXECUTE_LIQUIDATION,
                true,
                &context,
            );
            if codes::succeeded(&outcome) {
                assert_eq!(
                    position(f, &user, is_long).notional,
                    0,
                    "[{context}] liquidation left the position open"
                );
            }
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

        Command::ExecuteAdl {
            user,
            is_long,
            amount_pct,
            full,
        } => {
            let user = f.user(user as usize);
            let size = position(f, &user, is_long).notional;
            let amount = if full || size == 0 {
                i128::MAX
            } else {
                (size * (amount_pct as i128 + 1) / 256).max(1)
            };
            let before = f.market.get_market_data();
            let (bid, ask) =
                effective_book(f, oracle.bid(), oracle.ask(), oracle.publish_time(&f.env));
            let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
            let outcome = f.market.try_execute_adl(
                &f.keeper,
                &user,
                &is_long,
                &amount,
                &oracle.bytes(&f.env),
            );
            zenex_fuzz::observer::check(
                &f.env,
                &rollback_before,
                &outcome,
                codes::EXECUTE_ADL,
                true,
                &context,
            );
            if codes::succeeded(&outcome) {
                let after = f.market.get_market_data();
                let before_pnl = side_pnl_maximized(&before, bid, ask, is_long);
                let after_pnl = side_pnl_maximized(&after, bid, ask, is_long);
                assert!(
                    after_pnl < before_pnl,
                    "[{context}] ADL landed without reducing the {} side's pending PnL: \
                     {before_pnl} -> {after_pnl}",
                    if is_long { "long" } else { "short" }
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

        Command::SetStatus { status } => {
            let status = (status % 4) as u32; // Active | OnIce | Frozen | Delisted
            let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
            let outcome = f.market.try_set_status(&status);
            zenex_fuzz::observer::check(
                &f.env,
                &rollback_before,
                &outcome,
                codes::SET_STATUS,
                false,
                &context,
            );
            if codes::succeeded(&outcome) {
                assert_eq!(
                    f.market.get_status(),
                    status,
                    "[{context}] status transition landed but did not stick"
                );
            }
        }

        Command::SetTerminalPrice { bps } => {
            let price = offset_price(oracle.mid, bps);
            let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
            let outcome = f.market.try_set_terminal_price(&price);
            zenex_fuzz::observer::check(
                &f.env,
                &rollback_before,
                &outcome,
                codes::SET_TERMINAL_PRICE,
                false,
                &context,
            );
        }

        // Sixteen seconds per unit reaches roughly twelve days, so a single
        // jump can clear the seven-day delist deadline that unlocks the
        // wind-down forced close.
        Command::PassTime { units } => f.jump(units as u64 * 16),

        Command::MovePrice(mv) => oracle.apply(mv),
    }
}

/// Margin at the 10% initial-margin floor plus 30 bps per cushion step.
fn margin_for(notional: i128, margin_steps: u8) -> i128 {
    (notional * (1_000 + margin_steps as i128 * 30)) / 10_000
}

fn offset_price(mid: i128, bps: i16) -> i128 {
    let bps = (bps as i128).clamp(-9_000, 9_000);
    ((mid * (10_000 + bps)) / 10_000).max(1)
}

/// Try to leave the market tradable and flatten the book.
///
/// A run can end `Frozen` (every keeper path dead) or `Delisted` past its grace
/// window (no way back to `Active`), so every step here is best-effort. The
/// terminal funding invariant only fires when the unwind actually succeeded.
fn teardown(f: &Fixture, oracle: &Oracle, ledger: &mut Ledger) {
    let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
    let activated = f.market.try_set_status(&0u32);
    zenex_fuzz::observer::check(
        &f.env,
        &rollback_before,
        &activated,
        codes::SET_STATUS,
        false,
        "cleanup activate",
    );
    f.jump(3_600);
    let price = oracle.fresh_bytes(&f.env);

    for idx in 0..USER_COUNT {
        let user = f.user(idx);
        for is_long in [true, false] {
            if position(f, &user, is_long).notional == 0 {
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
            if position(f, &user, is_long).notional != 0 {
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
