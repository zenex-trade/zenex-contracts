#![no_main]

//! Stateful fuzz target for the LP side: vault deposit and redeem orders
//! priced against the traders' unrealized PnL.
//!
//! This surface is where an LP's money meets a trader's paper profit.
//! `create_vault_order` escrows assets (deposit) or shares (redeem).
//! `execute_vault_order` prices the mint or burn against `lp_net_pnl`, the
//! book's pending PnL less the vault's pending borrowing income, marked
//! adverse to whoever is transacting. The target
//! therefore interleaves LP flow with trader flow, so deposits and redeems
//! land while the book is carrying PnL rather than flat.
//!
//! Beyond the shared invariant ledger it asserts:
//!
//! - **share conservation** — the vault's `total_supply` equals the sum of
//!   share balances across every holder the harness can create (the LPs plus
//!   the market contract, which custodies redeem escrow).
//! - **`PnlExceedsAssets` is unreachable from a fill** — the vault refuses to
//!   price shares when the marked profit exceeds its assets. The market only
//!   ever hands it `lp_net_pnl`, which caps each side's profit at
//!   `max_pnl_trader` of half the balance and subtracts borrowing income, so
//!   it is structurally below it. Code 801 is deliberately absent
//!   from the `execute_vault_order` allowlist: seeing it means the cap does not
//!   hold, which would brick every deposit and redeem on that market.
//! - **single consumption**: a landed fill or cancel spends the vault order's
//!   id, and no later call on it lands.
//!
//! Fills and cancels aim at a resting vault order through [`Orders`]. Trade
//! orders share the id counter, so a pick over the whole counter would mostly
//! miss.
//!
//! The `Retired` transition is fuzzed here rather than in `fuzz_liquidation`
//! because retiring is an LP event: it sweeps the funding-pool surplus to the
//! vault and switches redeems to the immediate, order-free path. `Retired` is
//! final, so `Retire` only takes effect in the second half of the commands.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use market::VaultOrder;
use soroban_sdk::Address;

use zenex_fuzz::codes;
use zenex_fuzz::fixture::{Fixture, SCALAR_7, USER_COUNT};
use zenex_fuzz::invariants::{position, Ledger};
use zenex_fuzz::orders::Orders;
use zenex_fuzz::price::{Oracle, PriceMove};

const COMMANDS: usize = 22;

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    commands: [Command; COMMANDS],
}

#[derive(Arbitrary, Debug, Clone, Copy)]
enum Command {
    /// Escrow a deposit (assets) or a redeem (shares) for the keeper to fill.
    CreateVaultOrder {
        user: u8,
        /// `VaultOrderKind` discriminant, wrapped into `0..=1`.
        kind: u8,
        /// Deposit size in whole settlement units, or the fraction of the LP's
        /// share balance to redeem.
        amount_units: u16,
        /// Slippage bound as a fraction of the naive expected output (0 = unset).
        min_out_pct: u8,
    },
    /// Keeper-fill one of the user's vault orders. `wait` first moves the
    /// clock to the order's earliest fill, as a keeper would fill it.
    ExecuteVaultOrder { user: u8, id: u8, wait: bool },
    /// Cancel a pending vault order and pay back its escrow.
    CancelVaultOrder { user: u8, id: u8 },
    /// Open a trader position, so the next fill prices against live PnL.
    OpenPosition {
        user: u8,
        is_long: bool,
        notional_units: u16,
        margin_bps: u8,
    },
    /// Fully close a trader position.
    ClosePosition { user: u8, is_long: bool },
    /// Price-bearing accrual of both indices.
    Accrue,
    /// Wind the market down: sweeps the funding surplus and switches redeems to
    /// the immediate path. Requires an empty book. Ignored in the first half
    /// of the run, because `Retired` is final and would waste the rest.
    Retire,
    /// Advance the ledger clock.
    PassTime { units: u16 },
    /// Step the oracle walk.
    MovePrice(PriceMove),
}

fuzz_target!(|input: FuzzInput| {
    let f = Fixture::create();
    let mut oracle = Oracle::default();
    let mut ledger = Ledger::new(USER_COUNT);
    let mut orders = Orders::default();

    ledger.check(&f, "setup");
    check_shares(&f, "setup");

    for (step, command) in input.commands.iter().enumerate() {
        run(&f, &mut oracle, &mut orders, *command, step);
        let context = format!("after step {step}: {command:?}");
        ledger.check(&f, &context);
        check_shares(&f, &context);
    }

    teardown(&f, &oracle, &mut ledger);
});

fn run(f: &Fixture, oracle: &mut Oracle, orders: &mut Orders, command: Command, step: usize) {
    let context = format!("step {step}");
    match command {
        Command::CreateVaultOrder {
            user,
            kind,
            amount_units,
            min_out_pct,
        } => {
            let idx = user as usize % USER_COUNT;
            let user = f.user(idx);
            let kind = (kind % 2) as u32;
            let amount = if kind == 0 {
                (amount_units as i128 + 1) * SCALAR_7
            } else {
                // Redeem a fuzzer-chosen slice of what this LP actually holds,
                // so the escrow transfer usually lands and the fill gates
                // (redeem_lock, min_out, utilization, pending PnL) are what the
                // run spends its time on.
                let held = f.vault.balance(&user);
                (held * (amount_units as i128 + 1) / 65_536).max(1)
            };
            // A naive slippage bound: the output at par, where an asset unit
            // buys `share_scale` share units, scaled down by the fuzzer's
            // fraction. Most bounds hold, and the top of the range rejects a
            // fill priced against the LP.
            let naive = if kind == 0 {
                amount * f.share_scale
            } else {
                amount / f.share_scale
            };
            let min_out = naive * (min_out_pct as i128) / 256;
            let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
            let outcome = f
                .market
                .try_create_vault_order(&user, &kind, &amount, &min_out);
            zenex_fuzz::observer::check(
                &f.env,
                &rollback_before,
                &outcome,
                codes::CREATE_VAULT_ORDER,
                false,
                &context,
            );
            if let Ok(Ok(id)) = outcome {
                orders.created(idx, id);
            }
        }

        Command::ExecuteVaultOrder { user, id, wait } => {
            let (idx, id) = orders.pick(f, user as usize, id, |owner, candidate| {
                vault_order(f, &f.user(owner), candidate).is_some()
            });
            let user = f.user(idx);
            if wait {
                if let Some(order) = vault_order(f, &user, id) {
                    wait_for_fill(f, &order);
                }
            }
            let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
            let outcome =
                f.market
                    .try_execute_vault_order(&f.keeper, &user, &id, &oracle.bytes(&f.env));
            zenex_fuzz::observer::check(
                &f.env,
                &rollback_before,
                &outcome,
                codes::EXECUTE_VAULT_ORDER,
                true,
                &context,
            );
            orders.record(idx, id, codes::succeeded(&outcome), &context);
        }

        Command::CancelVaultOrder { user, id } => {
            let (idx, id) = orders.pick(f, user as usize, id, |owner, candidate| {
                vault_order(f, &f.user(owner), candidate).is_some()
            });
            let user = f.user(idx);
            let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
            let outcome = f.market.try_cancel_vault_order(&user, &id);
            zenex_fuzz::observer::check(
                &f.env,
                &rollback_before,
                &outcome,
                codes::CANCEL_VAULT_ORDER,
                false,
                &context,
            );
            orders.record(idx, id, codes::succeeded(&outcome), &context);
        }

        Command::OpenPosition {
            user,
            is_long,
            notional_units,
            margin_bps,
        } => {
            let user = f.user(user as usize);
            let notional = (notional_units as i128 + 1) * SCALAR_7;
            let margin = (notional * (1_000 + margin_bps as i128 * 30)) / 10_000;
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

        Command::ClosePosition { user, is_long } => {
            let user = f.user(user as usize);
            if position(f, &user, is_long).notional == 0 {
                return;
            }
            let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
            let created = f.market.try_create_order(
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

        // A no-op in the first half: an early retire would freeze the run.
        Command::Retire if step < COMMANDS / 2 => {}

        Command::Retire => {
            let rollback_before = zenex_fuzz::observer::Values::take(&f.env);
            let outcome = f.market.try_set_status(&4u32); // Retired
            zenex_fuzz::observer::check(
                &f.env,
                &rollback_before,
                &outcome,
                codes::SET_STATUS,
                false,
                &context,
            );
        }

        Command::PassTime { units } => f.jump(units as u64 * 4),

        Command::MovePrice(mv) => oracle.apply(mv),
    }
}

/// Every vault share is held by an LP, by the bootstrap seed LP, or custodied
/// by the market contract as redeem escrow. Nothing else in the fixture can
/// hold one. A supply above the holders' sum means shares were minted to
/// nobody. A supply below it means a burn missed the ledger.
fn check_shares(f: &Fixture, context: &str) {
    let mut held = f.vault.balance(&f.market_address) + f.vault.balance(&f.seed_lp);
    for user in &f.users {
        let balance = f.vault.balance(user);
        assert!(
            balance >= 0,
            "[{context}] share balance went negative: {balance}"
        );
        held += balance;
    }
    assert_eq!(
        f.vault.total_supply(),
        held,
        "[{context}] vault share supply does not match the sum of holder balances"
    );
}

/// The vault order `user` holds under `id`, if it still rests.
fn vault_order(f: &Fixture, user: &Address, id: u32) -> Option<VaultOrder> {
    match f.market.try_get_vault_order(user, &id) {
        Ok(Ok(order)) => Some(order),
        _ => None,
    }
}

/// Moves the clock to `order`'s earliest fill: a later ledger for a deposit,
/// the end of `redeem_lock` for a redeem. A clock already past it stays.
fn wait_for_fill(f: &Fixture, order: &VaultOrder) {
    let lock = if order.kind == 1 {
        f.market.get_config().redeem_lock
    } else {
        0
    };
    // A fill must land in a later ledger than the order, five seconds on.
    let ready = order.created_at + lock.max(5);
    let now = f.env.ledger().timestamp();
    if now < ready {
        f.jump(ready - now);
    }
}

/// Flatten the book so the terminal funding invariant can fire. Vault orders
/// are left where they are: their escrow is covered by the settlement identity
/// in the shared ledger, which is checked either way.
fn teardown(f: &Fixture, oracle: &Oracle, ledger: &mut Ledger) {
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
                &3u32,
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
    check_shares(f, "teardown");
}
