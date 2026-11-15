//! The permissionless keeper entries: the price-bearing fills of trade and
//! vault orders, the liquidation, and the accrual poke. Each loads the
//! [`Market`] once and runs exactly one action. The entries are
//! [`execute_order`], [`execute_vault_order`], [`execute_liquidation`], and
//! [`accrue`].

use crate::dependencies::VaultClient;
use crate::engine::market::Market;
use crate::engine::market_data::MarketData;
use crate::engine::math;
use crate::engine::order::{Order, OrderKind};
use crate::engine::position::Position;
use crate::engine::settlement::{pay_trader, Settlement};
use crate::engine::status::{self, Status};
use crate::engine::vault_order::{VaultOrder, VaultOrderKind};
use crate::errors::MarketError;
use crate::events::{AccrualUpdate, DepositFill, Liquidation, RedeemFill, RejectVaultOrder};
use crate::storage;
use soroban_sdk::auth::{ContractContext, InvokerContractAuthEntry, SubContractInvocation};
use soroban_sdk::{panic_with_error, token, vec, Address, Bytes, Env, IntoVal, Symbol};

/// Fills the keeper order `(user, id)` at the effective price, settles it,
/// and returns the keeper's payout (token-dec). Refer to
/// [`Market::execute_order`](crate::Market::execute_order).
pub fn execute_order(e: &Env, keeper: &Address, user: &Address, id: u32, price: &Bytes) -> i128 {
    // As-submitted price, trade class: the triggers and bounds judge the
    // payload the keeper submitted, under the strict window.
    let mut market = Market::load(e, price, false, false);
    let order = storage::get_order(e, user, id);
    let kind = OrderKind::from_u32(e, order.kind);

    // A zero-notional increase adds only margin, growing no size or reserve,
    // so it stays available for margin defense when opens are halted.
    if kind.is_increase() && order.notional > 0 {
        if !market.status.allows_open() || storage::get_adl(e).get(order.is_long) {
            panic_with_error!(e, MarketError::IncreaseHalted);
        }
    }
    let mut position = Position::load(e, user, order.is_long);
    // No fill may price behind the position's own last mark. `priced_at`
    // stays monotone, so the force-close floor never moves backwards.
    position.require_price_not_stale(e, market.price.publish_time);
    order.require_price_trigger_met(e, &market.price);

    let reward = if kind.is_increase() {
        fill_increase(e, &mut market, &mut position, keeper, user, id, &order)
    } else {
        fill_decrease(e, &mut market, &mut position, keeper, user, id, &order)
    };
    market.store(e);
    reward
}

/// Force-closes the position `(user, is_long)` at the effective price,
/// settles it, and returns the keeper's payout (token-dec). Refer to
/// [`Market::execute_liquidation`](crate::Market::execute_liquidation).
pub fn execute_liquidation(
    e: &Env,
    keeper: &Address,
    user: &Address,
    is_long: bool,
    price: &Bytes,
) -> i128 {
    // Close class: a liquidation that arrives one ledger late must still
    // land. The cache substitution and the position's `priced_at` floor
    // bound what any in-window payload can price it at.
    let mut market = Market::load(e, price, true, true);
    let mut position = Position::load(e, user, is_long);
    position.require_price_not_stale(e, market.price.publish_time);

    let force = market.status == Status::Delisted && status::deadline_passed(e);
    let liquidation = position.liquidate(e, &mut market, user, is_long, force);

    let refund = position.store(e, user, is_long);
    Liquidation {
        user: user.clone(),
        is_long,
        keeper: keeper.clone(),
        price: market.price.exit(is_long),
        notional: liquidation.notional,
        tokens: liquidation.tokens,
        margin: liquidation.margin,
        pnl: liquidation.pnl,
        base_fee: liquidation.fees.base,
        impact_fee: liquidation.fees.impact,
        funding: liquidation.fees.funding,
        borrowing: liquidation.fees.borrowing,
        bad_debt: liquidation.bad_debt,
        returned: liquidation.returned,
        liq_fee: liquidation.liq_fee,
    }
    .publish(e);

    let settlement = Settlement::compute_liquidation(e, &market, &liquidation, refund);
    let reward = settlement.settle(e, &mut market, keeper, user);
    market.store(e);
    reward
}

/// Fills the vault order `(user, id)` at the effective price and returns
/// the keeper's payout (token-dec). Refer to
/// [`Market::execute_vault_order`](crate::Market::execute_vault_order).
pub fn execute_vault_order(
    e: &Env,
    keeper: &Address,
    user: &Address,
    id: u32,
    price: &Bytes,
) -> i128 {
    // Trade class: a vault fill is a voluntary user fill, so it keeps the
    // strict staleness window even though it executes at the newest price.
    let mut market = Market::load(e, price, true, false);
    let order = storage::get_vault_order(e, user, id);
    // Commit-then-execute, two checks. The price check: no payload that
    // predates the commitment. Equality passes, as a price published at the
    // commit ledger's close was not knowable to the committer.
    if market.price.publish_time < order.created_at {
        panic_with_error!(e, MarketError::StalePrice);
    }
    // The ledger check: the fill must land in a later ledger than the
    // commitment, regardless of what the payload's timestamp claims.
    if market.now <= order.created_at {
        panic_with_error!(e, MarketError::StalePrice);
    }

    let kind = VaultOrderKind::from_u32(e, order.kind);
    let reward = match kind {
        VaultOrderKind::Deposit => fill_deposit(e, &mut market, keeper, user, id, &order),
        VaultOrderKind::Redeem => fill_redeem(e, &mut market, keeper, user, id, &order),
    };
    market.store(e);
    reward
}

/// Advances the accrual indices to now at the effective price and returns
/// the accrued market data. Refer to [`Market::accrue`](crate::Market::accrue).
pub fn accrue(e: &Env, price: &Bytes) -> MarketData {
    // Close class: the poke keeps the price cache fresh, which keeps the wide
    // window dormant elsewhere, so it must itself survive a feed gap.
    let market = Market::load(e, price, true, true);
    market.store(e);
    AccrualUpdate {}.publish(e);
    market.data
}

/// Fills an increase order, grows the position, settles the fee legs, and
/// returns the keeper's payout (token-dec).
///
/// # Security Warning
/// Moves protocol funds. Assumes the caller ran the open-halt and
/// price-eligibility gates.
///
/// # Errors
/// - [`MarketError::UtilizationExceeded`] if a size-growing fill leaves
///   the increased side's reserve above `max_util_open` of half the settled vault
///   balance.
/// - Refer to [`Position::increase`] errors.
fn fill_increase(
    e: &Env,
    market: &mut Market,
    position: &mut Position,
    keeper: &Address,
    user: &Address,
    id: u32,
    order: &Order,
) -> i128 {
    // Read before the increase mutates the row: zero notional means this
    // fill is the open.
    let opened = position.notional == 0;
    let increase = position.increase(e, market, user, order.is_long, order.notional, order.margin);
    increase.publish_receipt(
        e,
        user,
        id,
        order.is_long,
        keeper,
        market.price.entry(order.is_long),
        order.notional,
        order.margin,
        opened,
    );
    // An increase never closes, so the store refunds nothing.
    storage::remove_order(e, user, id);
    position.store(e, user, order.is_long);
    // An increase has no trader payout, PnL, bad debt, or liquidation fee.
    let legs = Settlement::compute_increase_order(e, market, &increase, order.exec_fee);
    let reward = legs.settle(e, market, keeper, user);
    // The reserve gate runs on the settled backing, which now holds this
    // open's banked fee. A pure margin top-up, with no notional, reserves
    // nothing. The opposite side may already be above its cap after a price
    // move or config change. It must not block an increase on this side.
    if order.notional > 0 {
        market.require_utilization(e, market.config.max_util_open, order.is_long);
    }
    reward
}

/// Fills a decrease order, shrinks the position, settles the legs, pays the
/// freed margin and PnL, and returns the keeper's payout (token-dec).
///
/// # Security Warning
/// Moves trader and protocol funds. Assumes the caller ran the
/// price-eligibility gates.
///
/// # Errors
/// - Refer to [`Position::decrease`] and [`Settlement::settle`] errors.
fn fill_decrease(
    e: &Env,
    market: &mut Market,
    position: &mut Position,
    keeper: &Address,
    user: &Address,
    id: u32,
    order: &Order,
) -> i128 {
    let decrease = position.decrease(
        e,
        market,
        user,
        order.is_long,
        order.notional,
        order.margin,
        false,
    );
    decrease.publish_receipt(
        e,
        user,
        id,
        order.is_long,
        keeper,
        market.price.exit(order.is_long),
        position.notional == 0,
    );
    // The filled order leaves the decrease list before the store, so the
    // closure sweep refunds only the stale siblings. The refund rides the
    // trader leg.
    storage::remove_order(e, user, id);
    position.remove_decrease(e, id);
    let refund = position.store(e, user, order.is_long);
    let settlement =
        Settlement::compute_decrease_order(e, market, &decrease, refund, order.exec_fee);
    settlement.settle(e, market, keeper, user)
}

/// Fills a deposit `order`, mints vault shares to `user` net of the vault
/// fee at the uPnL-marked share price, and returns the keeper's payout
/// (token-dec). A quote below `order.min_out` rejects the order instead.
///
/// # Security Warning
/// Moves escrowed and protocol funds. Assumes the caller ran the fill-price
/// postdate gate. `order.amount` was floored at `min_deposit` at creation.
///
/// # Errors
/// - [`MarketError::PendingPnlExceeded`] if a side's pending PnL overhangs
///   `max_pnl_trader` of half the vault balance.
/// - [`MarketError::VaultBalanceExceeded`] if the deposit would push the
///   vault above `max_vault_balance`.
fn fill_deposit(
    e: &Env,
    market: &mut Market,
    keeper: &Address,
    user: &Address,
    id: u32,
    order: &VaultOrder,
) -> i128 {
    // The deposit PnL gate: above the cap, a deposit raises the haircut
    // allowance and pays part of itself to the winning side. It measures
    // the pre-deposit book, so no fill enters an overhang.
    let trader_allowance = math::half_factor(e, market.vault_balance, market.config.max_pnl_trader);
    if market.data.side_pnl(e, &market.price, true, true) > trader_allowance
        || market.data.side_pnl(e, &market.price, false, true) > trader_allowance
    {
        panic_with_error!(e, MarketError::PendingPnlExceeded);
    }
    let market_address = e.current_contract_address();
    let vault = VaultClient::new(e, &market.vault);
    // The cut rounds down, in the depositor's favor.
    let vault_fee = math::apply_factor_floor(e, order.amount, market.config.deposit_fee);
    let deposit_assets = order.amount - vault_fee;
    // The mint marks pending PnL minimized, adverse to the depositor, so the
    // spread accrues to the standing shareholders.
    let net_pnl = market.lp_net_pnl(e, false);
    // The depositor's slippage bound, checked on the vault's own quote before
    // anything moves. The quote is exactly what the mint below produces.
    if order.min_out > 0 {
        let quoted = vault.preview_deposit(&deposit_assets, &net_pnl);
        if quoted < order.min_out {
            return reject(e, market, keeper, user, id, order, quoted, net_pnl);
        }
    }
    // The depositor is paid in shares, so the trader leg is zero.
    let fee = Settlement::compute_vault_order(e, market, vault_fee, 0, order.exec_fee);
    // The vault pulls the escrowed assets inside its own frame (market ->
    // vault -> token.transfer), where invoker auto-authorization does not
    // reach, so exactly that transfer is pre-authorized.
    e.authorize_as_current_contract(vec![
        e,
        InvokerContractAuthEntry::Contract(SubContractInvocation {
            context: ContractContext {
                contract: market.token.clone(),
                fn_name: Symbol::new(e, "transfer"),
                args: (market_address.clone(), market.vault.clone(), deposit_assets).into_val(e),
            },
            sub_invocations: vec![e],
        }),
    ]);
    let shares = vault.strategy_deposit(&deposit_assets, user, &market_address, &net_pnl);
    // Tracked so the size cap reads the post-deposit balance.
    market.vault_balance += deposit_assets;
    storage::remove_vault_order(e, user, id);
    DepositFill {
        user: user.clone(),
        id,
        keeper: keeper.clone(),
        assets: order.amount,
        shares,
        fee: vault_fee,
        net_pnl,
    }
    .publish(e);
    // The fee split settles after the mint, so the donated vault cut prices
    // against the pre-fee balance.
    let reward = fee.settle(e, market, keeper, user);
    // The vault size cap gates the settled balance, fee donation included.
    if market.vault_balance > market.config.max_vault_balance {
        panic_with_error!(e, MarketError::VaultBalanceExceeded);
    }
    reward
}

/// Fills a redeem `order`, pays `user` the redeemed assets net of the vault
/// fee at the uPnL-marked share price, and returns the keeper's payout
/// (token-dec). A quote below `order.min_out` rejects the order instead.
///
/// # Security Warning
/// Moves vault and protocol funds. Assumes the caller ran the fill-price
/// postdate gate.
///
/// # Errors
/// - [`MarketError::VaultOrderLocked`] if the `redeem_lock` cooldown has not
///   elapsed.
/// - [`MarketError::UtilizationExceeded`] if the redeem would leave the
///   vault under-reserved.
/// - [`MarketError::PendingPnlExceeded`] if a side's pending PnL would
///   overhang `max_pnl_withdraw` of half the remaining balance.
fn fill_redeem(
    e: &Env,
    market: &mut Market,
    keeper: &Address,
    user: &Address,
    id: u32,
    order: &VaultOrder,
) -> i128 {
    if market.now < order.created_at.saturating_add(market.config.redeem_lock) {
        panic_with_error!(e, MarketError::VaultOrderLocked);
    }
    let market_address = e.current_contract_address();
    let vault = VaultClient::new(e, &market.vault);
    // The burn marks pending PnL maximized, adverse to the redeemer, so the
    // spread accrues to the standing shareholders. The cap keeps the backing
    // positive.
    let net_pnl = market.lp_net_pnl(e, true);
    // The redeemer's slippage bound, checked on the vault's own quote net of
    // the vault fee before anything moves. The quote is exactly what the
    // burn below pays. The exit gates stay after the burn: a capacity
    // rejection traps and leaves the order to fill later.
    if order.min_out > 0 {
        let quoted = vault.preview_redeem(&order.amount, &net_pnl);
        let quoted_net = quoted - math::apply_factor_floor(e, quoted, market.config.redeem_fee);
        if quoted_net < order.min_out {
            return reject(e, market, keeper, user, id, order, quoted_net, net_pnl);
        }
    }
    // The vault traps a payout above its asset balance, so the tracked
    // balance below stays non-negative.
    let redeemed = vault.strategy_redeem(&order.amount, &market_address, &market_address, &net_pnl);
    // Tracked so the exit gates read the remaining backing.
    market.vault_balance -= redeemed;
    // The cut rounds down, in the redeemer's favor.
    let vault_fee = math::apply_factor_floor(e, redeemed, market.config.redeem_fee);
    let to_user = redeemed - vault_fee;
    // The redeemer's net payout rides the trader leg.
    let settlement = Settlement::compute_vault_order(e, market, vault_fee, to_user, order.exec_fee);
    storage::remove_vault_order(e, user, id);
    RedeemFill {
        user: user.clone(),
        id,
        keeper: keeper.clone(),
        shares: order.amount,
        assets: redeemed,
        fee: vault_fee,
        net_pnl,
    }
    .publish(e);
    let reward = settlement.settle(e, market, keeper, user);

    // Both exit gates measure the settled balance, fee donation included.
    // What remains must still back the reserve, which bounds every pending
    // trader profit from above.
    market.require_utilization(e, market.config.max_util_withdraw, true);
    market.require_utilization(e, market.config.max_util_withdraw, false);
    // The redeem PnL gate: no exit while either side's pending profit
    // overhangs `max_pnl_withdraw` of its half of the remaining balance.
    let withdraw_allowance =
        math::half_factor(e, market.vault_balance, market.config.max_pnl_withdraw);
    if market.data.side_pnl(e, &market.price, true, true) > withdraw_allowance
        || market.data.side_pnl(e, &market.price, false, true) > withdraw_allowance
    {
        panic_with_error!(e, MarketError::PendingPnlExceeded);
    }
    reward
}

/// Rejects a mature vault `order` whose fill would pay less than its
/// `min_out` and returns the keeper's payout (token-dec). It refunds the
/// escrowed principal to `user`, pays the escrowed `exec_fee` to the keeper,
/// and removes the order.
///
/// Paying the keeper makes the first attempt settle the order either way, so
/// `min_out` bounds slippage and cannot hold a resting price condition. The
/// refund cannot block the rejection. A deposit's assets that the user cannot
/// receive, for example after dropping its trustline, park as claimable
/// credit, and a redeem's shares need no trustline.
#[allow(clippy::too_many_arguments)]
fn reject(
    e: &Env,
    market: &mut Market,
    keeper: &Address,
    user: &Address,
    id: u32,
    order: &VaultOrder,
    quoted: i128,
    net_pnl: i128,
) -> i128 {
    match VaultOrderKind::from_u32(e, order.kind) {
        VaultOrderKind::Deposit => {
            let token = token::Client::new(e, &market.token);
            pay_trader(e, &token, &mut market.data, user, order.amount);
        }
        VaultOrderKind::Redeem => {
            let this = e.current_contract_address();
            VaultClient::new(e, &market.vault).transfer(&this, user, &order.amount);
        }
    }
    storage::remove_vault_order(e, user, id);
    RejectVaultOrder {
        user: user.clone(),
        id,
        keeper: keeper.clone(),
        quoted,
        net_pnl,
    }
    .publish(e);
    // Only the keeper leg is non-zero.
    Settlement::compute_vault_order(e, market, 0, 0, order.exec_fee).settle(e, market, keeper, user)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::constants::{BASE_UNIT, PRICE_SCALAR, SCALAR_18};
    use crate::engine::market_data::SidePair;
    use soroban_sdk::testutils::{Address as _, Events as _, Ledger};
    use soroban_sdk::{contract, contractimpl, map, symbol_short, token, Map, Val};

    #[contract]
    struct Harness;

    // Rejects every transfer to the constructor-set address and records the
    // latest approve per spender. It is the pay_trader fallback harness.
    #[contract]
    struct RejectingToken;

    #[contractimpl]
    impl RejectingToken {
        pub fn __constructor(e: Env, rejected: Address) {
            e.storage()
                .instance()
                .set(&symbol_short!("rejected"), &rejected);
        }

        pub fn transfer(e: Env, _from: Address, to: Address, _amount: i128) {
            let rejected: Address = e
                .storage()
                .instance()
                .get(&symbol_short!("rejected"))
                .unwrap();
            if to == rejected {
                panic_with_error!(&e, MarketError::InvalidOrder);
            }
        }

        pub fn allowance(e: Env, _from: Address, spender: Address) -> i128 {
            e.storage()
                .instance()
                .get(&(symbol_short!("allow"), spender))
                .unwrap_or(0)
        }

        pub fn approve(
            e: Env,
            _from: Address,
            spender: Address,
            amount: i128,
            _live_until_ledger: u32,
        ) {
            e.storage()
                .instance()
                .set(&(symbol_short!("allow"), spender), &amount);
        }
    }

    // Reports a zero treasury fee rate.
    #[contract]
    struct MockTreasury;

    #[contractimpl]
    impl MockTreasury {
        pub fn get_rate(_e: Env) -> i128 {
            0
        }
    }

    // Reports the constructor-set total. Deposit, redeem, and withdraw only
    // ever mutate the tracked total. No real token ever moves through this mock:
    // tests that expect a settled leg to reach the vault's own token balance
    // pre-fund the market contract directly and rely on `Settlement::settle`
    // (not this mock) to move it.
    #[contract]
    struct MockVault;

    #[contractimpl]
    impl MockVault {
        pub fn __constructor(e: Env, total: i128) {
            e.storage().instance().set(&symbol_short!("total"), &total);
        }

        pub fn total_assets(e: Env) -> i128 {
            e.storage().instance().get(&symbol_short!("total")).unwrap()
        }

        // A 1:1 deposit grows the tracked total. The mock ignores `net_pnl`.
        pub fn strategy_deposit(
            e: Env,
            assets: i128,
            _receiver: Address,
            _from: Address,
            _net_pnl: i128,
        ) -> i128 {
            let total: i128 = e.storage().instance().get(&symbol_short!("total")).unwrap();
            e.storage()
                .instance()
                .set(&symbol_short!("total"), &(total + assets));
            assets
        }

        // A 1:1 redeem shrinks the tracked total. The mock ignores `net_pnl`.
        pub fn strategy_redeem(
            e: Env,
            shares: i128,
            _receiver: Address,
            _owner: Address,
            _net_pnl: i128,
        ) -> i128 {
            let total: i128 = e.storage().instance().get(&symbol_short!("total")).unwrap();
            e.storage()
                .instance()
                .set(&symbol_short!("total"), &(total - shares));
            shares
        }

        pub fn strategy_withdraw(e: Env, amount: i128) {
            let total: i128 = e.storage().instance().get(&symbol_short!("total")).unwrap();
            e.storage()
                .instance()
                .set(&symbol_short!("total"), &(total - amount));
        }

        // Quotes match the 1:1 mint and burn above.
        pub fn preview_deposit(_e: Env, assets: i128, _net_pnl: i128) -> i128 {
            assets
        }

        pub fn preview_redeem(_e: Env, shares: i128, _net_pnl: i128) -> i128 {
            shares
        }

        // Records the last share transfer's recipient and amount.
        pub fn transfer(e: Env, _from: Address, to: Address, amount: i128) {
            e.storage()
                .instance()
                .set(&symbol_short!("xfer"), &(to, amount));
        }

        pub fn last_transfer(e: Env) -> Option<(Address, i128)> {
            e.storage().instance().get(&symbol_short!("xfer"))
        }
    }

    // A Delisted market past its deadline at the $9.50 flat price with a lone
    // healthy 10x long (100.0 notional on 10.0 margin, entry $10): the
    // wind-down waiver lets execute_liquidation close it through the soft tier.
    // Base fee ceil(0.3% * 100) = 0.3 on the improving leg (a lone long closing
    // moves skew fully toward balance) plus the 100^2 / 100_000 = 0.1 impact
    // fee, settled margin 9.6, pnl -5.0. The trader receives the 4.6
    // remainder, the keeper takes the 10% cut (0.04), and the vault banks the
    // 5.0 loss plus the 0.36 fee remainder (the mock treasury rate is 0). The
    // legs sum exactly to the 10.0 posted margin held by the contract.
    #[test]
    fn liquidate_wind_down_closes_healthy_position_at_flat_price() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let vault = e.register(MockVault, (1000_0000000_i128,));
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &10_0000000);

        let user = Address::generate(&e);
        let keeper = Address::generate(&e);
        let treasury = e.register(MockTreasury, ());

        let now = crate::engine::constants::DELIST_DEADLINE;
        e.ledger().with_mut(|ledger| ledger.timestamp = now);
        e.as_contract(&contract_id, || {
            storage::set_token(&e, &token.address());
            storage::set_vault(&e, &vault);
            storage::set_treasury(&e, &treasury);
            storage::set_feed_id(&e, &soroban_sdk::BytesN::from_array(&e, &[1u8; 32]));
            storage::set_status(&e, &Status::Delisted);
            storage::set_delisted_at(&e, 0);
            storage::set_terminal_price(&e, 95 * PRICE_SCALAR / 10);
            storage::set_config(&e, &crate::engine::config::test_config());
            storage::set_market_data(
                &e,
                &MarketData {
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
                    // Accrued to now: the close settles no funding or borrowing.
                    accrued_at: now,
                    ..Default::default()
                },
            );
            storage::set_position(
                &e,
                &user,
                true,
                &Position {
                    margin: 10_0000000,
                    notional: 100_0000000,
                    tokens: 10 * BASE_UNIT,
                    funding_idx: 0,
                    borrowing_idx: 0,
                    // A live lock. The wind-down close never trips it.
                    locked_notional: 100_0000000,
                    unlocks_at: u64::MAX,
                    priced_at: 0,
                    decrease_orders: soroban_sdk::Vec::new(&e),
                },
            );
            let reward = execute_liquidation(&e, &keeper, &user, true, &Bytes::new(&e));
            // keeper_rate (10%) of the 0.4 trade fee plus 10% of the 1.0
            // liquidation fee.
            assert_eq!(reward, 1_400_000);

            // The closed position persists as the zeroed row.
            let closed = storage::get_position(&e, &user, true);
            assert_eq!(closed.notional, 0);
            assert_eq!(closed.margin, 0);
            assert_eq!(closed.tokens, 0);
            let data = storage::get_market_data(&e);
            assert_eq!(data.notional.long, 0);
            assert_eq!(data.tokens.long, 0);
            assert_eq!(data.margin.long, 0);
        });

        // The single receipt: the itemized liquidation priced at the $9.50
        // flat exit.
        let liquidation_data: Map<Symbol, Val> = map![
            &e,
            (Symbol::new(&e, "keeper"), keeper.clone().into_val(&e)),
            (
                Symbol::new(&e, "price"),
                (95 * PRICE_SCALAR / 10).into_val(&e)
            ),
            (Symbol::new(&e, "notional"), 100_0000000_i128.into_val(&e)),
            (Symbol::new(&e, "tokens"), (10 * BASE_UNIT).into_val(&e)),
            (Symbol::new(&e, "margin"), 10_0000000_i128.into_val(&e)),
            (Symbol::new(&e, "pnl"), (-5_0000000_i128).into_val(&e)),
            (Symbol::new(&e, "base_fee"), 3000000_i128.into_val(&e)),
            (Symbol::new(&e, "impact_fee"), 1_000_000_i128.into_val(&e)),
            (Symbol::new(&e, "funding"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "borrowing"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "bad_debt"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "returned"), 3_6000000_i128.into_val(&e)),
            (Symbol::new(&e, "liq_fee"), 1_0000000_i128.into_val(&e)),
        ];
        assert_eq!(
            e.events().all().filter_by_contract(&contract_id),
            vec![
                &e,
                (
                    contract_id.clone(),
                    (Symbol::new(&e, "liquidation"), user.clone(), true).into_val(&e),
                    liquidation_data.into_val(&e),
                ),
            ],
        );

        let token_client = token::Client::new(&e, &token.address());
        assert_eq!(token_client.balance(&user), 3_6000000);
        assert_eq!(token_client.balance(&vault), 6_2600000);
        assert_eq!(token_client.balance(&keeper), 1_400_000);
        assert_eq!(token_client.balance(&treasury), 0);
        assert_eq!(token_client.balance(&contract_id), 0);
    }

    // The same wind-down liquidation with two decrease orders (escrows 0.1 and
    // 0.25) still resting on the side: the closure sweep emits one
    // `cancel_order` receipt per swept order before the liquidation receipt,
    // whose `refund` carries the 0.35 aggregate escrow, and the trader's
    // transfer pays exactly returned + refund in one leg.
    #[test]
    fn liquidation_sweep_emits_cancel_receipts_and_the_aggregate_refund() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let vault = e.register(MockVault, (1000_0000000_i128,));
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        // The 10.0 margin plus the 0.35 escrow the two resting orders locked.
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &10_3500000);

        let user = Address::generate(&e);
        let keeper = Address::generate(&e);
        let treasury = e.register(MockTreasury, ());

        let now = crate::engine::constants::DELIST_DEADLINE;
        e.ledger().with_mut(|ledger| ledger.timestamp = now);
        e.as_contract(&contract_id, || {
            storage::set_token(&e, &token.address());
            storage::set_vault(&e, &vault);
            storage::set_treasury(&e, &treasury);
            storage::set_feed_id(&e, &soroban_sdk::BytesN::from_array(&e, &[1u8; 32]));
            storage::set_status(&e, &Status::Delisted);
            storage::set_delisted_at(&e, 0);
            storage::set_terminal_price(&e, 95 * PRICE_SCALAR / 10);
            storage::set_config(&e, &crate::engine::config::test_config());
            storage::set_market_data(
                &e,
                &MarketData {
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
                    accrued_at: now,
                    ..Default::default()
                },
            );
            // Two never-crossing take-profits parked on the side.
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
            storage::set_order(
                &e,
                &user,
                9,
                &Order {
                    is_long: true,
                    kind: OrderKind::LimitDecrease as u32,
                    notional: 50_0000000,
                    margin: 0,
                    trigger_price: 13 * PRICE_SCALAR,
                    price_bound: 0,
                    exec_fee: 2_500_000,
                    created_at: 0,
                    expiration: u32::MAX,
                },
            );
            storage::set_position(
                &e,
                &user,
                true,
                &Position {
                    margin: 10_0000000,
                    notional: 100_0000000,
                    tokens: 10 * BASE_UNIT,
                    funding_idx: 0,
                    borrowing_idx: 0,
                    locked_notional: 0,
                    unlocks_at: 0,
                    priced_at: 0,
                    decrease_orders: soroban_sdk::vec![&e, 7, 9],
                },
            );
            execute_liquidation(&e, &keeper, &user, true, &Bytes::new(&e));

            // Both swept rows are gone. The zeroed position dropped the list.
            assert!(!e
                .storage()
                .persistent()
                .has(&storage::DataKey::Order(user.clone(), 7)));
            assert!(!e
                .storage()
                .persistent()
                .has(&storage::DataKey::Order(user.clone(), 9)));
            assert_eq!(
                storage::get_position(&e, &user, true).decrease_orders.len(),
                0
            );
        });

        // One `cancel_order` per swept order, each carrying the escrow it
        // refunds, then the liquidation receipt. The payout transfer adds
        // the summed 0.35 escrow.
        let cancel_7_data: Map<Symbol, Val> =
            map![&e, (Symbol::new(&e, "refund"), 1_000_000_i128.into_val(&e))];
        let cancel_9_data: Map<Symbol, Val> =
            map![&e, (Symbol::new(&e, "refund"), 2_500_000_i128.into_val(&e))];
        let liquidation_data: Map<Symbol, Val> = map![
            &e,
            (Symbol::new(&e, "keeper"), keeper.clone().into_val(&e)),
            (
                Symbol::new(&e, "price"),
                (95 * PRICE_SCALAR / 10).into_val(&e)
            ),
            (Symbol::new(&e, "notional"), 100_0000000_i128.into_val(&e)),
            (Symbol::new(&e, "tokens"), (10 * BASE_UNIT).into_val(&e)),
            (Symbol::new(&e, "margin"), 10_0000000_i128.into_val(&e)),
            (Symbol::new(&e, "pnl"), (-5_0000000_i128).into_val(&e)),
            (Symbol::new(&e, "base_fee"), 3000000_i128.into_val(&e)),
            (Symbol::new(&e, "impact_fee"), 1_000_000_i128.into_val(&e)),
            (Symbol::new(&e, "funding"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "borrowing"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "bad_debt"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "returned"), 3_6000000_i128.into_val(&e)),
            (Symbol::new(&e, "liq_fee"), 1_0000000_i128.into_val(&e)),
        ];
        assert_eq!(
            e.events().all().filter_by_contract(&contract_id),
            vec![
                &e,
                (
                    contract_id.clone(),
                    (Symbol::new(&e, "cancel_order"), user.clone(), 7_u32).into_val(&e),
                    cancel_7_data.into_val(&e),
                ),
                (
                    contract_id.clone(),
                    (Symbol::new(&e, "cancel_order"), user.clone(), 9_u32).into_val(&e),
                    cancel_9_data.into_val(&e),
                ),
                (
                    contract_id.clone(),
                    (Symbol::new(&e, "liquidation"), user.clone(), true).into_val(&e),
                    liquidation_data.into_val(&e),
                ),
            ],
        );

        // The trader's single transfer reconciles exactly: returned + refund.
        let token_client = token::Client::new(&e, &token.address());
        assert_eq!(token_client.balance(&user), 3_6000000 + 3_500_000);
        assert_eq!(token_client.balance(&vault), 6_2600000);
        assert_eq!(token_client.balance(&keeper), 1_400_000);
        assert_eq!(token_client.balance(&contract_id), 0);
    }

    // Before the delist deadline the waiver is off: a healthy position stays
    // protected even on a Delisted market. Equity marks at 7.6 (9.6 post-fee
    // margin plus -2.0 pnl at the $9.80 flat price), still above the 5.0
    // maintenance margin.
    #[test]
    #[should_panic(expected = "#722")]
    fn liquidate_healthy_position_rejected_before_deadline() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        let vault = e.register(MockVault, (1000_0000000_i128,));
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);
        let treasury = e.register(MockTreasury, ());

        e.as_contract(&contract_id, || {
            storage::set_token(&e, &Address::generate(&e));
            storage::set_vault(&e, &vault);
            storage::set_treasury(&e, &treasury);
            storage::set_feed_id(&e, &soroban_sdk::BytesN::from_array(&e, &[1u8; 32]));
            storage::set_status(&e, &Status::Delisted);
            storage::set_delisted_at(&e, 0);
            // Fixture-set flat price so the load stays price-free.
            storage::set_terminal_price(&e, 98 * PRICE_SCALAR / 10);
            storage::set_config(&e, &crate::engine::config::test_config());
            storage::set_market_data(
                &e,
                &MarketData {
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
            );
            storage::set_position(
                &e,
                &user,
                true,
                &Position {
                    margin: 10_0000000,
                    notional: 100_0000000,
                    tokens: 10 * BASE_UNIT,
                    funding_idx: 0,
                    borrowing_idx: 0,
                    locked_notional: 0,
                    unlocks_at: 0,
                    priced_at: 0,
                    decrease_orders: soroban_sdk::Vec::new(&e),
                },
            );
            execute_liquidation(&e, &keeper, &user, true, &Bytes::new(&e));
        });
    }

    fn has_vault_order(e: &Env, user: &Address, id: u32) -> bool {
        e.storage()
            .persistent()
            .has(&storage::DataKey::VaultOrder(user.clone(), id))
    }

    // A deposit fill that would push the vault past `max_vault_balance` is
    // rejected: 1000.0 held plus the 0.4995000 net deposit (5_000_000 minus
    // its 0.001 vault fee) exceeds the 1000.0000050 cap.
    #[test]
    #[should_panic(expected = "#753")]
    fn deposit_fill_rejected_above_vault_balance_cap() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let vault = e.register(MockVault, (1000_0000000_i128,));
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &1_0000000);

        let user = Address::generate(&e);
        let keeper = Address::generate(&e);
        let treasury = e.register(MockTreasury, ());

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        e.as_contract(&contract_id, || {
            storage::set_token(&e, &token.address());
            storage::set_vault(&e, &vault);
            storage::set_treasury(&e, &treasury);
            storage::set_feed_id(&e, &soroban_sdk::BytesN::from_array(&e, &[1u8; 32]));
            storage::set_status(&e, &Status::Delisted);
            storage::set_delisted_at(&e, 0);
            storage::set_terminal_price(&e, 10 * PRICE_SCALAR);
            storage::set_config(
                &e,
                &crate::engine::config::Config {
                    max_vault_balance: 1000_0000050,
                    ..crate::engine::config::test_config()
                },
            );
            storage::set_market_data(&e, &MarketData::default());
            storage::set_vault_order(
                &e,
                &user,
                1,
                &VaultOrder {
                    kind: VaultOrderKind::Deposit as u32,
                    amount: 5_000_000,
                    min_out: 0,
                    exec_fee: 0,
                    created_at: 0,
                },
            );
            execute_vault_order(&e, &keeper, &user, 1, &Bytes::new(&e));
        });
    }

    // A redeem fill is rejected while pending trader profit exceeds the
    // withdraw gate. The lone long (80.0 notional, 10.0 base tokens, entry $8)
    // marks +20.0 pending profit at the flat $10 price. A 230.0 vault easily
    // clears the 90% reserve gate (the long's 100.0 mark-value reserve stays
    // under 90% of half the 227.0 post-redeem balance), but the profit still
    // exceeds the 15% withdraw gate (17.025 = 15% of half 227.0).
    #[test]
    #[should_panic(expected = "#754")]
    fn redeem_fill_rejected_above_pending_pnl_gate() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let vault = e.register(MockVault, (230_0000000_i128,));
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &5_0000000);
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);
        let treasury = e.register(MockTreasury, ());

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        e.as_contract(&contract_id, || {
            storage::set_token(&e, &token.address());
            storage::set_vault(&e, &vault);
            storage::set_treasury(&e, &treasury);
            storage::set_feed_id(&e, &soroban_sdk::BytesN::from_array(&e, &[1u8; 32]));
            storage::set_status(&e, &Status::Delisted);
            storage::set_delisted_at(&e, 0);
            storage::set_terminal_price(&e, 10 * PRICE_SCALAR);
            storage::set_config(&e, &crate::engine::config::test_config());
            storage::set_market_data(
                &e,
                &MarketData {
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
            );
            storage::set_vault_order(
                &e,
                &user,
                1,
                &VaultOrder {
                    kind: VaultOrderKind::Redeem as u32,
                    amount: 3_0000000,
                    min_out: 0,
                    exec_fee: 0,
                    created_at: 0,
                },
            );
            execute_vault_order(&e, &keeper, &user, 1, &Bytes::new(&e));
        });
    }

    /// Wire a Delisted flat-$10 market with a `MockVault` holding
    /// `vault_total`, the given config, an empty book, and `contract_funds`
    /// minted to the market contract. Returns `(contract, token, user, keeper)`.
    fn vault_fill_setup(
        e: &Env,
        vault_total: i128,
        config: crate::engine::config::Config,
        contract_funds: i128,
    ) -> (Address, Address, Address, Address) {
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let vault = e.register(MockVault, (vault_total,));
        let token_admin = Address::generate(e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        if contract_funds > 0 {
            token::StellarAssetClient::new(e, &token.address()).mint(&contract_id, &contract_funds);
        }
        let user = Address::generate(e);
        let keeper = Address::generate(e);
        let treasury = e.register(MockTreasury, ());
        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        e.as_contract(&contract_id, || {
            storage::set_token(e, &token.address());
            storage::set_vault(e, &vault);
            storage::set_treasury(e, &treasury);
            storage::set_feed_id(e, &soroban_sdk::BytesN::from_array(e, &[1u8; 32]));
            storage::set_status(e, &Status::Delisted);
            storage::set_delisted_at(e, 0);
            storage::set_terminal_price(e, 10 * PRICE_SCALAR);
            storage::set_config(e, &config);
            storage::set_market_data(e, &MarketData::default());
        });
        (contract_id, token.address(), user, keeper)
    }

    // A full deposit fill mints shares (net of the 0.001 vault fee) and pays
    // the keeper its 10% cut, plus banks the fee remainder to the vault: fee
    // 0.0050000, keeper cut 0.0005000, vault remainder 0.0045000.
    #[test]
    fn deposit_fill_mints_shares_and_completes() {
        let e = Env::default();
        let (contract_id, token, user, keeper) = vault_fill_setup(
            &e,
            100_0000000,
            crate::engine::config::test_config(),
            100_0000000,
        );
        e.as_contract(&contract_id, || {
            storage::set_vault_order(
                &e,
                &user,
                1,
                &VaultOrder {
                    kind: VaultOrderKind::Deposit as u32,
                    amount: 5_0000000,
                    min_out: 0,
                    exec_fee: 0,
                    created_at: 40,
                },
            );
            let fee = execute_vault_order(&e, &keeper, &user, 1, &Bytes::new(&e));
            assert_eq!(fee, 5_000); // keeper: 10% of the 0.0050000 vault fee
            assert!(!has_vault_order(&e, &user, 1));
        });
        let token_client = token::Client::new(&e, &token);
        assert_eq!(token_client.balance(&keeper), 5_000);
        assert_eq!(token_client.balance(&vault(&e, &contract_id)), 45_000);
    }

    // The vault cap admits its exact value. The vault holds 100.0. The 4.995
    // net deposit (5.0 less its 0.005 vault fee) and the 0.0045 fee remainder
    // the vault banks settle it at 104.9995, the cap itself.
    #[test]
    fn deposit_fill_landing_exactly_on_the_vault_cap_fills() {
        let e = Env::default();
        let config = crate::engine::config::Config {
            max_vault_balance: 104_9995000,
            ..crate::engine::config::test_config()
        };
        let (contract_id, token, user, keeper) =
            vault_fill_setup(&e, 100_0000000, config, 100_0000000);
        e.as_contract(&contract_id, || {
            storage::set_vault_order(
                &e,
                &user,
                1,
                &VaultOrder {
                    kind: VaultOrderKind::Deposit as u32,
                    amount: 5_0000000,
                    min_out: 0,
                    exec_fee: 0,
                    created_at: 40,
                },
            );
            execute_vault_order(&e, &keeper, &user, 1, &Bytes::new(&e));
            assert!(!has_vault_order(&e, &user, 1));
        });
        assert_eq!(
            token::Client::new(&e, &token).balance(&vault(&e, &contract_id)),
            45_000
        );
    }

    // The redeem PnL gate admits pending profit exactly at the allowance, on
    // either side. Redeeming 30.0 of a 1000.0 vault leaves 970.0 plus the
    // 0.027 fee remainder, so the allowance is floor(15% * 485.0135) =
    // 72.752025. At the flat $10, the long (10 base tokens on 27.247975) and
    // the short (10 base tokens on 172.752025) each mark exactly that profit.
    #[test]
    fn redeem_fill_with_pending_profit_exactly_at_the_gate_fills() {
        let e = Env::default();
        let (contract_id, _, user, keeper) = vault_fill_setup(
            &e,
            1000_0000000,
            crate::engine::config::test_config(),
            100_0000000,
        );
        e.as_contract(&contract_id, || {
            storage::set_market_data(
                &e,
                &MarketData {
                    notional: SidePair {
                        long: 27_2479750,
                        short: 172_7520250,
                    },
                    margin: SidePair {
                        long: 10_0000000,
                        short: 10_0000000,
                    },
                    tokens: SidePair {
                        long: 10 * BASE_UNIT,
                        short: 10 * BASE_UNIT,
                    },
                    accrued_at: 100,
                    ..Default::default()
                },
            );
            storage::set_vault_order(
                &e,
                &user,
                1,
                &VaultOrder {
                    kind: VaultOrderKind::Redeem as u32,
                    amount: 30_0000000,
                    min_out: 0,
                    exec_fee: 0,
                    created_at: 40,
                },
            );
            execute_vault_order(&e, &keeper, &user, 1, &Bytes::new(&e));
            assert!(!has_vault_order(&e, &user, 1));
        });
    }

    // The deposit-gate fixture's terminal price, $10.000000005. It values 10
    // base tokens at 100.00000005, half a unit off the grid, so a side's
    // maximized mark reads one unit above its minimized mark.
    const GATE_PRICE: i128 = 10 * PRICE_SCALAR + 5_000_000_000;

    // A book with 10 base tokens and 10.0 margin per side at the given
    // entry notionals, accrued to the fixture's timestamp 100.
    fn deposit_gate_book(long_notional: i128, short_notional: i128) -> MarketData {
        MarketData {
            notional: SidePair {
                long: long_notional,
                short: short_notional,
            },
            margin: SidePair {
                long: 10_0000000,
                short: 10_0000000,
            },
            tokens: SidePair {
                long: 10 * BASE_UNIT,
                short: 10 * BASE_UNIT,
            },
            accrued_at: 100,
            ..Default::default()
        }
    }

    // Fills a 5.0 deposit into the 100.0 vault fixture against `book`, priced
    // flat at `GATE_PRICE`.
    fn fill_deposit_against(e: &Env, book: MarketData) {
        let (contract_id, _, user, keeper) = vault_fill_setup(
            e,
            100_0000000,
            crate::engine::config::test_config(),
            100_0000000,
        );
        e.as_contract(&contract_id, || {
            storage::set_terminal_price(e, GATE_PRICE);
            storage::set_market_data(e, &book);
            storage::set_vault_order(
                e,
                &user,
                1,
                &VaultOrder {
                    kind: VaultOrderKind::Deposit as u32,
                    amount: 5_0000000,
                    min_out: 0,
                    exec_fee: 0,
                    created_at: 40,
                },
            );
            execute_vault_order(e, &keeper, &user, 1, &Bytes::new(e));
            assert!(!has_vault_order(e, &user, 1));
        });
    }

    // The deposit PnL gate admits pending profit exactly at the trader
    // allowance, on either side. The 100.0 vault allows floor(90% * 50.0) =
    // 45.0 per side. Maximized, the long (on 55.0000001) and the short (on
    // 145.0) each mark exactly 45.0.
    #[test]
    fn deposit_fill_with_pending_profit_exactly_at_the_gate_fills() {
        let e = Env::default();
        fill_deposit_against(&e, deposit_gate_book(55_0000001, 145_0000000));
    }

    // The long on 55.0 marks 45.0000001 maximized, one unit over the
    // allowance, and traps. Minimized it marks 45.0, so only the maximized
    // mark traps. The gate also reads the pre-deposit vault: the 4.995 net
    // deposit would lift the allowance to 47.24775.
    #[test]
    #[should_panic(expected = "#754")]
    fn deposit_fill_rejected_one_unit_of_long_profit_above_the_gate() {
        let e = Env::default();
        fill_deposit_against(&e, deposit_gate_book(55_0000000, 145_0000000));
    }

    // The short on 145.0000001 marks 45.0000001 maximized and traps.
    // Minimized it marks 45.0.
    #[test]
    #[should_panic(expected = "#754")]
    fn deposit_fill_rejected_one_unit_of_short_profit_above_the_gate() {
        let e = Env::default();
        fill_deposit_against(&e, deposit_gate_book(55_0000001, 145_0000001));
    }

    // Returns the `net_pnl` of the market's last event, the vault fill's
    // receipt.
    fn last_fill_net_pnl(e: &Env, contract_id: &Address) -> i128 {
        use soroban_sdk::{xdr, TryFromVal};
        let events = e.events().all().filter_by_contract(contract_id);
        let xdr::ContractEventBody::V0(body) = &events.events().last().unwrap().body;
        let data = Val::try_from_val(e, &body.data).unwrap();
        let data = Map::<Symbol, Val>::try_from_val(e, &data).unwrap();
        i128::try_from_val(e, &data.get(Symbol::new(e, "net_pnl")).unwrap()).unwrap()
    }

    // Each vault fill prices the pending borrowing against the transacting
    // LP. The long side marks zero PnL at the flat $10 and owes 5.0 and a
    // fraction of a unit: 100.0 checkpointed at index 0 and marked at
    // 0.050000000000000001. The mock treasury takes no cut. The deposit
    // prices at -5.0000001, rounded up, and the redeem at -5.0, rounded down.
    #[test]
    fn vault_fills_price_pending_borrowing_against_the_lp() {
        let e = Env::default();
        let (contract_id, _, user, keeper) = vault_fill_setup(
            &e,
            1000_0000000,
            crate::engine::config::test_config(),
            100_0000000,
        );
        let book = MarketData {
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
            borrowing_idx: SidePair {
                long: SCALAR_18 / 20 + 1,
                short: 0,
            },
            accrued_at: 100,
            ..Default::default()
        };
        for (id, kind, net_pnl) in [
            (1, VaultOrderKind::Deposit as u32, -5_0000001),
            (2, VaultOrderKind::Redeem as u32, -5_0000000),
        ] {
            e.as_contract(&contract_id, || {
                storage::set_market_data(&e, &book);
                storage::set_vault_order(
                    &e,
                    &user,
                    id,
                    &VaultOrder {
                        kind,
                        amount: 5_0000000,
                        min_out: 0,
                        exec_fee: 0,
                        created_at: 40,
                    },
                );
                execute_vault_order(&e, &keeper, &user, id, &Bytes::new(&e));
            });
            assert_eq!(last_fill_net_pnl(&e, &contract_id), net_pnl);
        }
    }

    /// Read back the vault address stored on the market contract.
    fn vault(e: &Env, contract_id: &Address) -> Address {
        e.as_contract(contract_id, || storage::get_vault(e))
    }

    // A full redeem fill burns shares (mock 1:1) and pays the redeemer net of
    // the 0.001 vault fee: fee 0.0300000, keeper cut 0.0030000, vault
    // remainder 0.0270000, user payout 29.9700000.
    #[test]
    fn redeem_fill_pays_and_completes() {
        let e = Env::default();
        let (contract_id, token, user, keeper) = vault_fill_setup(
            &e,
            100_0000000,
            crate::engine::config::test_config(),
            100_0000000,
        );
        e.as_contract(&contract_id, || {
            storage::set_vault_order(
                &e,
                &user,
                1,
                &VaultOrder {
                    kind: VaultOrderKind::Redeem as u32,
                    amount: 30_0000000,
                    min_out: 0,
                    exec_fee: 0,
                    created_at: 40,
                },
            );
            let fee = execute_vault_order(&e, &keeper, &user, 1, &Bytes::new(&e));
            assert_eq!(fee, 30_000); // keeper: 10% of the 0.0300000 vault fee
            assert!(!has_vault_order(&e, &user, 1));
        });
        let token_client = token::Client::new(&e, &token);
        assert_eq!(token_client.balance(&user), 29_9700000);
        assert_eq!(token_client.balance(&keeper), 30_000);
        assert_eq!(token_client.balance(&vault(&e, &contract_id)), 270_000);
    }

    // A deposit quoted below its slippage bound is rejected: the mock mints
    // 1:1, so a min_out one stroop above the 49_950_000 net deposit misses.
    // The assets return to the user, the exec_fee pays the keeper, the order
    // is gone, and the vault mints nothing.
    #[test]
    fn deposit_fill_below_min_out_rejects_the_order() {
        let e = Env::default();
        let (contract_id, token, user, keeper) = vault_fill_setup(
            &e,
            100_0000000,
            crate::engine::config::test_config(),
            100_0000000,
        );
        let vault = e.as_contract(&contract_id, || storage::get_vault(&e));
        e.as_contract(&contract_id, || {
            storage::set_vault_order(
                &e,
                &user,
                1,
                &VaultOrder {
                    kind: VaultOrderKind::Deposit as u32,
                    amount: 5_0000000,
                    min_out: 49_950_001,
                    exec_fee: 100_000,
                    created_at: 40,
                },
            );
            let reward = execute_vault_order(&e, &keeper, &user, 1, &Bytes::new(&e));
            assert_eq!(reward, 100_000, "the keeper earns the exec_fee");
            assert!(!has_vault_order(&e, &user, 1));
        });
        let token_client = token::Client::new(&e, &token);
        assert_eq!(token_client.balance(&user), 5_0000000, "principal refunded");
        assert_eq!(token_client.balance(&keeper), 100_000);
        assert_eq!(MockVaultClient::new(&e, &vault).total_assets(), 100_0000000);
    }

    // A redeem quoted below its slippage bound is rejected once mature: a
    // min_out one stroop above the 299_700_000 net payout misses. The shares
    // return to the user, the exec_fee pays the keeper, and nothing burns.
    #[test]
    fn redeem_fill_below_min_out_rejects_the_order() {
        let e = Env::default();
        let (contract_id, token, user, keeper) = vault_fill_setup(
            &e,
            100_0000000,
            crate::engine::config::test_config(),
            100_0000000,
        );
        let vault = e.as_contract(&contract_id, || storage::get_vault(&e));
        e.as_contract(&contract_id, || {
            storage::set_vault_order(
                &e,
                &user,
                1,
                &VaultOrder {
                    kind: VaultOrderKind::Redeem as u32,
                    amount: 30_0000000,
                    min_out: 299_700_001,
                    exec_fee: 100_000,
                    created_at: 40,
                },
            );
            let reward = execute_vault_order(&e, &keeper, &user, 1, &Bytes::new(&e));
            assert_eq!(reward, 100_000, "the keeper earns the exec_fee");
            assert!(!has_vault_order(&e, &user, 1));
        });
        let vault_client = MockVaultClient::new(&e, &vault);
        assert_eq!(
            vault_client.last_transfer(),
            Some((user.clone(), 30_0000000))
        );
        assert_eq!(vault_client.total_assets(), 100_0000000, "nothing burned");
        assert_eq!(token::Client::new(&e, &token).balance(&keeper), 100_000);
    }

    // A quote exactly at the bound fills.
    #[test]
    fn redeem_fill_at_min_out_fills() {
        let e = Env::default();
        let (contract_id, _, user, keeper) = vault_fill_setup(
            &e,
            100_0000000,
            crate::engine::config::test_config(),
            100_0000000,
        );
        let vault = e.as_contract(&contract_id, || storage::get_vault(&e));
        e.as_contract(&contract_id, || {
            storage::set_vault_order(
                &e,
                &user,
                1,
                &VaultOrder {
                    kind: VaultOrderKind::Redeem as u32,
                    amount: 30_0000000,
                    min_out: 299_700_000,
                    exec_fee: 0,
                    created_at: 40,
                },
            );
            execute_vault_order(&e, &keeper, &user, 1, &Bytes::new(&e));
            assert!(!has_vault_order(&e, &user, 1));
        });
        assert_eq!(MockVaultClient::new(&e, &vault).total_assets(), 70_0000000);
    }

    // A redeem fill before the cooldown elapsed is locked: the live
    // `redeem_lock` (50) from `created_at` (80) is 130, past the ledger's now
    // of 100.
    #[test]
    #[should_panic(expected = "#751")]
    fn redeem_fill_locked_before_cooldown() {
        let e = Env::default();
        let config = crate::engine::config::Config {
            redeem_lock: 50,
            ..crate::engine::config::test_config()
        };
        let (contract_id, _, user, keeper) = vault_fill_setup(&e, 100_0000000, config, 100_0000000);
        e.as_contract(&contract_id, || {
            storage::set_vault_order(
                &e,
                &user,
                1,
                &VaultOrder {
                    kind: VaultOrderKind::Redeem as u32,
                    amount: 30_0000000,
                    min_out: 0,
                    exec_fee: 0,
                    created_at: 80,
                },
            );
            execute_vault_order(&e, &keeper, &user, 1, &Bytes::new(&e));
        });
    }

    // The redeem cooldown is not snapshotted at order creation: it is
    // `order.created_at + market.config.redeem_lock` read live at fill time.
    // The order is created under a 10_000-second lock (which would still be
    // locked at now = 100), but lowering the live config to a zero lock
    // before the fill lets it through immediately.
    #[test]
    fn redeem_lock_reads_live_config_not_a_snapshot_at_creation() {
        let e = Env::default();
        let config = crate::engine::config::Config {
            redeem_lock: 10_000,
            ..crate::engine::config::test_config()
        };
        let (contract_id, _, user, keeper) = vault_fill_setup(&e, 100_0000000, config, 100_0000000);
        e.as_contract(&contract_id, || {
            storage::set_vault_order(
                &e,
                &user,
                1,
                &VaultOrder {
                    kind: VaultOrderKind::Redeem as u32,
                    amount: 30_0000000,
                    min_out: 0,
                    exec_fee: 0,
                    created_at: 80,
                },
            );
            storage::set_config(
                &e,
                &crate::engine::config::Config {
                    redeem_lock: 0,
                    ..crate::engine::config::test_config()
                },
            );
            execute_vault_order(&e, &keeper, &user, 1, &Bytes::new(&e));
            assert!(!has_vault_order(&e, &user, 1));
        });
    }

    // A margin-only Increase (notional 0) fills even on a Delisted market
    // where opens are halted, so a trader can still defend margin. The order's
    // margin is escrowed at creation, so this fill only grows the
    // position's bookkept margin. No trade fee is charged, because no size
    // moves. A size-growing Increase in the same state is rejected below.
    #[test]
    fn margin_top_up_fills_while_opens_halted() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let vault = e.register(MockVault, (1000_0000000_i128,));
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);
        let treasury = e.register(MockTreasury, ());

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);

        e.as_contract(&contract_id, || {
            storage::set_token(&e, &token.address());
            storage::set_vault(&e, &vault);
            storage::set_treasury(&e, &treasury);
            storage::set_feed_id(&e, &soroban_sdk::BytesN::from_array(&e, &[1u8; 32]));
            storage::set_status(&e, &Status::Delisted);
            storage::set_delisted_at(&e, 0);
            storage::set_terminal_price(&e, 10 * PRICE_SCALAR);
            storage::set_config(&e, &crate::engine::config::test_config());
            storage::set_market_data(
                &e,
                &MarketData {
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
                    accrued_at: 100,
                    ..Default::default()
                },
            );
            storage::set_position(
                &e,
                &user,
                true,
                &Position {
                    margin: 10_0000000,
                    notional: 100_0000000,
                    tokens: 10 * BASE_UNIT,
                    funding_idx: 0,
                    borrowing_idx: 0,
                    locked_notional: 0,
                    unlocks_at: 0,
                    priced_at: 0,
                    decrease_orders: soroban_sdk::Vec::new(&e),
                },
            );
            storage::set_order(
                &e,
                &user,
                1,
                &Order {
                    is_long: true,
                    kind: OrderKind::MarketIncrease as u32,
                    notional: 0,
                    margin: 5_0000000,
                    trigger_price: 0,
                    price_bound: 0,
                    exec_fee: 0,
                    created_at: 100,
                    expiration: 1000,
                },
            );
            execute_order(&e, &keeper, &user, 1, &Bytes::new(&e));

            let position = storage::get_position(&e, &user, true);
            assert_eq!(position.margin, 15_0000000);
            assert_eq!(position.notional, 100_0000000);
        });

        // The single receipt: the fee-free top-up priced at the $10 flat entry.
        let increase_data: Map<Symbol, Val> = map![
            &e,
            (Symbol::new(&e, "keeper"), keeper.clone().into_val(&e)),
            (Symbol::new(&e, "price"), (10 * PRICE_SCALAR).into_val(&e)),
            (Symbol::new(&e, "notional"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "tokens"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "margin"), 5_0000000_i128.into_val(&e)),
            (Symbol::new(&e, "base_fee"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "impact_fee"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "funding"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "borrowing"), 0_i128.into_val(&e)),
        ];
        assert_eq!(
            e.events().all().filter_by_contract(&contract_id),
            vec![
                &e,
                (
                    contract_id.clone(),
                    (Symbol::new(&e, "increase_fill"), user.clone(), 1_u32, true).into_val(&e),
                    increase_data.into_val(&e),
                ),
            ],
        );
    }

    // A force-close price that predates the position's last entry change is
    // rejected: the position was entered at t=200 but the flat price is stamped
    // at now=100, so the anti-replay guard fires before the (otherwise
    // liquidatable, underwater at $8.50) position can be closed.
    #[test]
    #[should_panic(expected = "#740")]
    fn liquidate_rejected_on_price_predating_entry() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        let vault = e.register(MockVault, (1000_0000000_i128,));
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);
        let treasury = e.register(MockTreasury, ());

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        e.as_contract(&contract_id, || {
            storage::set_token(&e, &Address::generate(&e));
            storage::set_vault(&e, &vault);
            storage::set_treasury(&e, &treasury);
            storage::set_feed_id(&e, &soroban_sdk::BytesN::from_array(&e, &[1u8; 32]));
            storage::set_status(&e, &Status::Active);
            storage::set_terminal_price(&e, 85 * PRICE_SCALAR / 10);
            storage::set_config(&e, &crate::engine::config::test_config());
            storage::set_market_data(
                &e,
                &MarketData {
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
            );
            storage::set_position(
                &e,
                &user,
                true,
                &Position {
                    margin: 10_0000000,
                    notional: 100_0000000,
                    tokens: 10 * BASE_UNIT,
                    funding_idx: 0,
                    borrowing_idx: 0,
                    locked_notional: 0,
                    unlocks_at: 0,
                    priced_at: 200,
                    decrease_orders: soroban_sdk::Vec::new(&e),
                },
            );
            execute_liquidation(&e, &keeper, &user, true, &Bytes::new(&e));
        });
    }

    // A full open-then-close cycle through execute_order at a flat $10. The
    // order's margin is escrowed at creation (this test writes the order
    // directly, so the market contract is pre-funded with it in place of a
    // real `create_order` pull). The open posts 25.0, paying 0.3 base +
    // 60^2 / 100_000 = 0.036 impact fee, so the position banks 24.664
    // margin on 60.0 notional. Jumping past the lock, a full-close
    // Decrease realizes zero PnL, charges 0.18 improving fee plus the same
    // 0.036 impact, and returns 24.448 to the trader.
    #[test]
    fn execute_order_open_then_close_cycle() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let vault = e.register(MockVault, (1000_0000000_i128,));
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);
        let treasury = e.register(MockTreasury, ());
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &25_0000000);

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);

        e.as_contract(&contract_id, || {
            storage::set_token(&e, &token.address());
            storage::set_vault(&e, &vault);
            storage::set_treasury(&e, &treasury);
            storage::set_feed_id(&e, &soroban_sdk::BytesN::from_array(&e, &[1u8; 32]));
            storage::set_status(&e, &Status::Active);
            storage::set_terminal_price(&e, 10 * PRICE_SCALAR);
            storage::set_config(&e, &crate::engine::config::test_config());
            storage::set_market_data(
                &e,
                &MarketData {
                    accrued_at: 100,
                    ..Default::default()
                },
            );
            storage::set_order(
                &e,
                &user,
                1,
                &Order {
                    is_long: true,
                    kind: OrderKind::MarketIncrease as u32,
                    notional: 60_0000000,
                    margin: 25_0000000,
                    trigger_price: 0,
                    price_bound: 0,
                    exec_fee: 0,
                    created_at: 100,
                    expiration: 1000,
                },
            );
            execute_order(&e, &keeper, &user, 1, &Bytes::new(&e));

            let opened = storage::get_position(&e, &user, true);
            assert_eq!(opened.notional, 60_0000000);
            assert_eq!(opened.tokens, 6 * BASE_UNIT);
            assert_eq!(opened.margin, 24_6640000);
            assert_eq!(opened.priced_at, 100);

            // Past the notional lock, close the whole position.
            e.ledger().with_mut(|ledger| ledger.timestamp = 200);
            storage::set_order(
                &e,
                &user,
                2,
                &Order {
                    is_long: true,
                    kind: OrderKind::MarketDecrease as u32,
                    notional: i128::MAX,
                    margin: 0,
                    trigger_price: 0,
                    price_bound: 0,
                    exec_fee: 0,
                    created_at: 200,
                    expiration: 1000,
                },
            );
            execute_order(&e, &keeper, &user, 2, &Bytes::new(&e));
            // The closed position persists as the zeroed row.
            let closed = storage::get_position(&e, &user, true);
            assert_eq!(closed.notional, 0);
            assert_eq!(closed.margin, 0);
            assert_eq!(closed.tokens, 0);
        });

        // Posted 25.0, received the 24.448 settled remainder back.
        assert_eq!(
            token::Client::new(&e, &token.address()).balance(&user),
            24_4480000
        );
    }

    // The first fill on an empty side emits `open_fill`. The next size-growing
    // fill on the now-open position emits `increase_fill`. The zero-to-size
    // case is attested by its own event name, exactly as size-to-zero is by
    // `close_fill`. The open receipt drops the structurally-zero funding and
    // borrowing keys. The increase keeps them. At the flat $10: the 60.0 open
    // pays 0.3 base + 0.036 impact (margin banks 24.664), the 40.0 increase
    // (a lone long worsening skew) pays 0.2 base + 0.016 impact.
    #[test]
    fn first_fill_emits_open_fill_then_increase_fill() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let vault = e.register(MockVault, (1000_0000000_i128,));
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);
        let treasury = e.register(MockTreasury, ());
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &35_0000000);

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);

        e.as_contract(&contract_id, || {
            storage::set_token(&e, &token.address());
            storage::set_vault(&e, &vault);
            storage::set_treasury(&e, &treasury);
            storage::set_feed_id(&e, &soroban_sdk::BytesN::from_array(&e, &[1u8; 32]));
            storage::set_status(&e, &Status::Active);
            storage::set_terminal_price(&e, 10 * PRICE_SCALAR);
            storage::set_config(&e, &crate::engine::config::test_config());
            storage::set_market_data(
                &e,
                &MarketData {
                    accrued_at: 100,
                    ..Default::default()
                },
            );
            storage::set_order(
                &e,
                &user,
                1,
                &Order {
                    is_long: true,
                    kind: OrderKind::MarketIncrease as u32,
                    notional: 60_0000000,
                    margin: 25_0000000,
                    trigger_price: 0,
                    price_bound: 0,
                    exec_fee: 0,
                    created_at: 100,
                    expiration: 1000,
                },
            );
            execute_order(&e, &keeper, &user, 1, &Bytes::new(&e));

            let opened = storage::get_position(&e, &user, true);
            assert_eq!(opened.notional, 60_0000000);
            assert_eq!(opened.margin, 24_6640000);

            storage::set_order(
                &e,
                &user,
                2,
                &Order {
                    is_long: true,
                    kind: OrderKind::MarketIncrease as u32,
                    notional: 40_0000000,
                    margin: 10_0000000,
                    trigger_price: 0,
                    price_bound: 0,
                    exec_fee: 0,
                    created_at: 100,
                    expiration: 1000,
                },
            );
            execute_order(&e, &keeper, &user, 2, &Bytes::new(&e));

            let grown = storage::get_position(&e, &user, true);
            assert_eq!(grown.notional, 100_0000000);
            assert_eq!(grown.margin, 34_4480000);
        });

        let open_data: Map<Symbol, Val> = map![
            &e,
            (Symbol::new(&e, "keeper"), keeper.clone().into_val(&e)),
            (Symbol::new(&e, "price"), (10 * PRICE_SCALAR).into_val(&e)),
            (Symbol::new(&e, "notional"), 60_0000000_i128.into_val(&e)),
            (Symbol::new(&e, "tokens"), (6 * BASE_UNIT).into_val(&e)),
            (Symbol::new(&e, "margin"), 25_0000000_i128.into_val(&e)),
            (Symbol::new(&e, "base_fee"), 3_000_000_i128.into_val(&e)),
            (Symbol::new(&e, "impact_fee"), 360_000_i128.into_val(&e)),
        ];
        let increase_data: Map<Symbol, Val> = map![
            &e,
            (Symbol::new(&e, "keeper"), keeper.clone().into_val(&e)),
            (Symbol::new(&e, "price"), (10 * PRICE_SCALAR).into_val(&e)),
            (Symbol::new(&e, "notional"), 40_0000000_i128.into_val(&e)),
            (Symbol::new(&e, "tokens"), (4 * BASE_UNIT).into_val(&e)),
            (Symbol::new(&e, "margin"), 10_0000000_i128.into_val(&e)),
            (Symbol::new(&e, "base_fee"), 2_000_000_i128.into_val(&e)),
            (Symbol::new(&e, "impact_fee"), 160_000_i128.into_val(&e)),
            (Symbol::new(&e, "funding"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "borrowing"), 0_i128.into_val(&e)),
        ];
        assert_eq!(
            e.events().all().filter_by_contract(&contract_id),
            vec![
                &e,
                (
                    contract_id.clone(),
                    (Symbol::new(&e, "open_fill"), user.clone(), 1_u32, true).into_val(&e),
                    open_data.into_val(&e),
                ),
                (
                    contract_id.clone(),
                    (Symbol::new(&e, "increase_fill"), user.clone(), 2_u32, true).into_val(&e),
                    increase_data.into_val(&e),
                ),
            ],
        );
    }

    // A full-close Decrease fill emits the `close_fill` receipt priced at the
    // flat $10 exit. The lone 10x long (100.0 notional on 10.0 margin,
    // entry $10) closes flat: zero PnL, the 0.3 improving base fee plus the
    // 100^2 / 100_000 = 0.1 impact fee, 9.6 returned.
    #[test]
    fn full_close_emits_the_close_fill_receipt() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let vault = e.register(MockVault, (1000_0000000_i128,));
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &10_0000000);
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);
        let treasury = e.register(MockTreasury, ());

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        e.as_contract(&contract_id, || {
            storage::set_token(&e, &token.address());
            storage::set_vault(&e, &vault);
            storage::set_treasury(&e, &treasury);
            storage::set_feed_id(&e, &soroban_sdk::BytesN::from_array(&e, &[1u8; 32]));
            storage::set_status(&e, &Status::Active);
            storage::set_terminal_price(&e, 10 * PRICE_SCALAR);
            storage::set_config(&e, &crate::engine::config::test_config());
            storage::set_market_data(
                &e,
                &MarketData {
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
                    accrued_at: 100,
                    ..Default::default()
                },
            );
            storage::set_position(
                &e,
                &user,
                true,
                &Position {
                    margin: 10_0000000,
                    notional: 100_0000000,
                    tokens: 10 * BASE_UNIT,
                    funding_idx: 0,
                    borrowing_idx: 0,
                    locked_notional: 0,
                    unlocks_at: 0,
                    priced_at: 0,
                    decrease_orders: soroban_sdk::Vec::new(&e),
                },
            );
            storage::set_order(
                &e,
                &user,
                1,
                &Order {
                    is_long: true,
                    kind: OrderKind::MarketDecrease as u32,
                    notional: i128::MAX,
                    margin: 0,
                    trigger_price: 0,
                    price_bound: 0,
                    exec_fee: 0,
                    created_at: 100,
                    expiration: 1000,
                },
            );
            execute_order(&e, &keeper, &user, 1, &Bytes::new(&e));
        });

        let close_data: Map<Symbol, Val> = map![
            &e,
            (Symbol::new(&e, "keeper"), keeper.clone().into_val(&e)),
            (Symbol::new(&e, "price"), (10 * PRICE_SCALAR).into_val(&e)),
            (Symbol::new(&e, "notional"), 100_0000000_i128.into_val(&e)),
            (Symbol::new(&e, "tokens"), (10 * BASE_UNIT).into_val(&e)),
            (Symbol::new(&e, "margin"), 10_0000000_i128.into_val(&e)),
            (Symbol::new(&e, "pnl"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "base_fee"), 3000000_i128.into_val(&e)),
            (Symbol::new(&e, "impact_fee"), 1_000_000_i128.into_val(&e)),
            (Symbol::new(&e, "funding"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "borrowing"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "bad_debt"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "returned"), 9_6000000_i128.into_val(&e)),
        ];
        assert_eq!(
            e.events().all().filter_by_contract(&contract_id),
            vec![
                &e,
                (
                    contract_id.clone(),
                    (Symbol::new(&e, "close_fill"), user.clone(), 1_u32, true).into_val(&e),
                    close_data.into_val(&e),
                ),
            ],
        );

        assert_eq!(
            token::Client::new(&e, &token.address()).balance(&user),
            9_6000000
        );
    }

    // The same full close with a never-crossing take-profit (escrow 0.2) still
    // resting on the side: the sweep announces its `cancel_order` before the
    // `close_fill` receipt, whose `refund` carries the 0.2 escrow, and the
    // trader's single transfer pays returned + refund.
    #[test]
    fn full_close_sweep_folds_the_sibling_escrow_into_the_receipt_refund() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let vault = e.register(MockVault, (1000_0000000_i128,));
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        // The 10.0 margin plus the 0.2 escrow the resting order locked.
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &10_2000000);
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);
        let treasury = e.register(MockTreasury, ());

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        e.as_contract(&contract_id, || {
            storage::set_token(&e, &token.address());
            storage::set_vault(&e, &vault);
            storage::set_treasury(&e, &treasury);
            storage::set_feed_id(&e, &soroban_sdk::BytesN::from_array(&e, &[1u8; 32]));
            storage::set_status(&e, &Status::Active);
            storage::set_terminal_price(&e, 10 * PRICE_SCALAR);
            storage::set_config(&e, &crate::engine::config::test_config());
            storage::set_market_data(
                &e,
                &MarketData {
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
                    accrued_at: 100,
                    ..Default::default()
                },
            );
            storage::set_position(
                &e,
                &user,
                true,
                &Position {
                    margin: 10_0000000,
                    notional: 100_0000000,
                    tokens: 10 * BASE_UNIT,
                    funding_idx: 0,
                    borrowing_idx: 0,
                    locked_notional: 0,
                    unlocks_at: 0,
                    priced_at: 0,
                    decrease_orders: soroban_sdk::vec![&e, 1, 3],
                },
            );
            storage::set_order(
                &e,
                &user,
                1,
                &Order {
                    is_long: true,
                    kind: OrderKind::MarketDecrease as u32,
                    notional: i128::MAX,
                    margin: 0,
                    trigger_price: 0,
                    price_bound: 0,
                    exec_fee: 0,
                    created_at: 100,
                    expiration: 1000,
                },
            );
            // The parked take-profit the close must sweep.
            storage::set_order(
                &e,
                &user,
                3,
                &Order {
                    is_long: true,
                    kind: OrderKind::LimitDecrease as u32,
                    notional: 50_0000000,
                    margin: 0,
                    trigger_price: 12 * PRICE_SCALAR,
                    price_bound: 0,
                    exec_fee: 2_000_000,
                    created_at: 100,
                    expiration: u32::MAX,
                },
            );
            execute_order(&e, &keeper, &user, 1, &Bytes::new(&e));

            // The swept row is gone. The zeroed position dropped the list.
            assert!(!e
                .storage()
                .persistent()
                .has(&storage::DataKey::Order(user.clone(), 3)));
            assert_eq!(
                storage::get_position(&e, &user, true).decrease_orders.len(),
                0
            );
        });

        // The close receipt, then the sweep's cancel receipt carrying the
        // escrow it refunds (fill_decrease publishes before the store sweeps).
        let cancel_data: Map<Symbol, Val> =
            map![&e, (Symbol::new(&e, "refund"), 2_000_000_i128.into_val(&e))];
        let close_data: Map<Symbol, Val> = map![
            &e,
            (Symbol::new(&e, "keeper"), keeper.clone().into_val(&e)),
            (Symbol::new(&e, "price"), (10 * PRICE_SCALAR).into_val(&e)),
            (Symbol::new(&e, "notional"), 100_0000000_i128.into_val(&e)),
            (Symbol::new(&e, "tokens"), (10 * BASE_UNIT).into_val(&e)),
            (Symbol::new(&e, "margin"), 10_0000000_i128.into_val(&e)),
            (Symbol::new(&e, "pnl"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "base_fee"), 3000000_i128.into_val(&e)),
            (Symbol::new(&e, "impact_fee"), 1_000_000_i128.into_val(&e)),
            (Symbol::new(&e, "funding"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "borrowing"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "bad_debt"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "returned"), 9_6000000_i128.into_val(&e)),
        ];
        assert_eq!(
            e.events().all().filter_by_contract(&contract_id),
            vec![
                &e,
                (
                    contract_id.clone(),
                    (Symbol::new(&e, "close_fill"), user.clone(), 1_u32, true).into_val(&e),
                    close_data.into_val(&e),
                ),
                (
                    contract_id.clone(),
                    (Symbol::new(&e, "cancel_order"), user.clone(), 3_u32).into_val(&e),
                    cancel_data.into_val(&e),
                ),
            ],
        );

        // The trader's single transfer reconciles exactly: returned + refund.
        assert_eq!(
            token::Client::new(&e, &token.address()).balance(&user),
            9_6000000 + 2_000_000
        );
    }

    // A partial Decrease fill emits the `decrease_fill` receipt, which carries
    // no `bad_debt` entry. Closing 50.0 of the lone 10x long at the flat $10
    // exit realizes zero PnL and withdraws nothing, so the 0.15 improving base
    // fee plus the 50^2 / 100_000 = 0.025 impact fee debit the surviving
    // margin and the payout is zero.
    #[test]
    fn partial_decrease_emits_the_decrease_fill_receipt() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let vault = e.register(MockVault, (1000_0000000_i128,));
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &10_0000000);
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);
        let treasury = e.register(MockTreasury, ());

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        e.as_contract(&contract_id, || {
            storage::set_token(&e, &token.address());
            storage::set_vault(&e, &vault);
            storage::set_treasury(&e, &treasury);
            storage::set_feed_id(&e, &soroban_sdk::BytesN::from_array(&e, &[1u8; 32]));
            storage::set_status(&e, &Status::Active);
            storage::set_terminal_price(&e, 10 * PRICE_SCALAR);
            storage::set_config(&e, &crate::engine::config::test_config());
            storage::set_market_data(
                &e,
                &MarketData {
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
                    accrued_at: 100,
                    ..Default::default()
                },
            );
            storage::set_position(
                &e,
                &user,
                true,
                &Position {
                    margin: 10_0000000,
                    notional: 100_0000000,
                    tokens: 10 * BASE_UNIT,
                    funding_idx: 0,
                    borrowing_idx: 0,
                    locked_notional: 0,
                    unlocks_at: 0,
                    priced_at: 0,
                    decrease_orders: soroban_sdk::Vec::new(&e),
                },
            );
            storage::set_order(
                &e,
                &user,
                1,
                &Order {
                    is_long: true,
                    kind: OrderKind::MarketDecrease as u32,
                    notional: 50_0000000,
                    margin: 0,
                    trigger_price: 0,
                    price_bound: 0,
                    exec_fee: 0,
                    created_at: 100,
                    expiration: 1000,
                },
            );
            execute_order(&e, &keeper, &user, 1, &Bytes::new(&e));

            // The survivor keeps half the size. The fees debited the margin.
            let position = storage::get_position(&e, &user, true);
            assert_eq!(position.notional, 50_0000000);
            assert_eq!(position.tokens, 5 * BASE_UNIT);
            assert_eq!(position.margin, 9_8250000);
        });

        let decrease_data: Map<Symbol, Val> = map![
            &e,
            (Symbol::new(&e, "keeper"), keeper.clone().into_val(&e)),
            (Symbol::new(&e, "price"), (10 * PRICE_SCALAR).into_val(&e)),
            (Symbol::new(&e, "notional"), 50_0000000_i128.into_val(&e)),
            (Symbol::new(&e, "tokens"), (5 * BASE_UNIT).into_val(&e)),
            (Symbol::new(&e, "margin"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "pnl"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "base_fee"), 1_500_000_i128.into_val(&e)),
            (Symbol::new(&e, "impact_fee"), 250_000_i128.into_val(&e)),
            (Symbol::new(&e, "funding"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "borrowing"), 0_i128.into_val(&e)),
            (Symbol::new(&e, "returned"), 0_i128.into_val(&e)),
        ];
        assert_eq!(
            e.events().all().filter_by_contract(&contract_id),
            vec![
                &e,
                (
                    contract_id.clone(),
                    (Symbol::new(&e, "decrease_fill"), user.clone(), 1_u32, true).into_val(&e),
                    decrease_data.into_val(&e),
                ),
            ],
        );
    }

    // A size-growing Increase on an ADL-flagged side is rejected by the
    // open-stop even while the market is Active.
    #[test]
    #[should_panic(expected = "#705")]
    fn increase_on_adl_flagged_side_rejects() {
        use crate::engine::adl::AdlState;
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let vault = e.register(MockVault, (1000_0000000_i128,));
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);
        let treasury = e.register(MockTreasury, ());

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        e.as_contract(&contract_id, || {
            storage::set_token(&e, &Address::generate(&e));
            storage::set_vault(&e, &vault);
            storage::set_treasury(&e, &treasury);
            storage::set_feed_id(&e, &soroban_sdk::BytesN::from_array(&e, &[1u8; 32]));
            storage::set_status(&e, &Status::Active);
            storage::set_terminal_price(&e, 10 * PRICE_SCALAR);
            storage::set_config(&e, &crate::engine::config::test_config());
            storage::set_market_data(&e, &MarketData::default());
            storage::set_adl(
                &e,
                &AdlState {
                    long: true,
                    short: false,
                },
            );
            storage::set_order(
                &e,
                &user,
                1,
                &Order {
                    is_long: true,
                    kind: OrderKind::MarketIncrease as u32,
                    notional: 50_0000000,
                    margin: 10_0000000,
                    trigger_price: 0,
                    price_bound: 0,
                    exec_fee: 0,
                    created_at: 100,
                    expiration: 1000,
                },
            );
            execute_order(&e, &keeper, &user, 1, &Bytes::new(&e));
        });
    }

    // A size-growing Increase is still halted on a non-Active market.
    #[test]
    #[should_panic(expected = "#705")]
    fn positive_increase_rejected_while_opens_halted() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let vault = e.register(MockVault, (1000_0000000_i128,));
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);
        let treasury = e.register(MockTreasury, ());

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        e.as_contract(&contract_id, || {
            storage::set_token(&e, &Address::generate(&e));
            storage::set_vault(&e, &vault);
            storage::set_treasury(&e, &treasury);
            storage::set_feed_id(&e, &soroban_sdk::BytesN::from_array(&e, &[1u8; 32]));
            storage::set_status(&e, &Status::Delisted);
            storage::set_delisted_at(&e, 0);
            storage::set_terminal_price(&e, 10 * PRICE_SCALAR);
            storage::set_config(&e, &crate::engine::config::test_config());
            storage::set_market_data(&e, &MarketData::default());
            storage::set_order(
                &e,
                &user,
                1,
                &Order {
                    is_long: true,
                    kind: OrderKind::MarketIncrease as u32,
                    notional: 50_0000000,
                    margin: 5_0000000,
                    trigger_price: 0,
                    price_bound: 0,
                    exec_fee: 0,
                    created_at: 100,
                    expiration: 1000,
                },
            );
            execute_order(&e, &keeper, &user, 1, &Bytes::new(&e));
        });
    }

    // Every accrue call emits the `accrual_update` marker, including a
    // same-timestamp repeat. A fill through `Market::load` does not emit it.
    // `events().all()` holds only the last invocation's events.
    #[test]
    fn accrue_always_emits_the_accrual_marker() {
        let e = Env::default();
        // The fixture's market data starts with both accrual clocks at 0
        // against the ledger's now of 100. The empty book moves no index.
        let (contract_id, _, _, _) =
            vault_fill_setup(&e, 100_0000000, crate::engine::config::test_config(), 0);
        e.as_contract(&contract_id, || {
            accrue(&e, &Bytes::new(&e));
        });
        let accrual_data: Map<Symbol, Val> = map![&e];
        assert_eq!(
            e.events().all().filter_by_contract(&contract_id),
            vec![
                &e,
                (
                    contract_id.clone(),
                    (Symbol::new(&e, "accrual_update"),).into_val(&e),
                    accrual_data.into_val(&e),
                ),
            ],
        );
        e.as_contract(&contract_id, || {
            accrue(&e, &Bytes::new(&e));
        });
        assert_eq!(
            e.events()
                .all()
                .filter_by_contract(&contract_id)
                .events()
                .len(),
            1,
            "a same-timestamp re-accrual still reports the marker",
        );
    }

    // A payable receiver is paid directly. No claimable credit appears.
    #[test]
    fn pay_trader_transfers_directly() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &1_000);
        let user = Address::generate(&e);

        let client = token::Client::new(&e, &token.address());
        let mut data = MarketData::default();
        e.as_contract(&contract_id, || {
            crate::engine::settlement::pay_trader(&e, &client, &mut data, &user, 400);
            assert_eq!(storage::get_claimable_credit(&e, &user), 0);
        });
        assert_eq!(client.balance(&user), 400);
        assert_eq!(data.credit_owed, 0);
        assert_eq!(data.credit_pool, 0);
    }

    // Failed payouts accumulate into one claimable credit. Owed and pool rise
    // together, so the pool surplus holds, and no token allowance appears.
    #[test]
    fn pay_trader_failure_parks_claimable_credit() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let user = Address::generate(&e);
        let token = e.register(RejectingToken, (user.clone(),));

        let client = token::Client::new(&e, &token);
        let mut data = MarketData::default();
        e.as_contract(&contract_id, || {
            crate::engine::settlement::pay_trader(&e, &client, &mut data, &user, 400);
            crate::engine::settlement::pay_trader(&e, &client, &mut data, &user, 250);
            assert_eq!(storage::get_claimable_credit(&e, &user), 650);
        });
        assert_eq!(data.credit_owed, 650);
        assert_eq!(data.credit_pool, 650);
        assert_eq!(client.allowance(&contract_id, &user), 0);
    }

    // A parked payout is collectable through the credit claim: once the
    // receiver is re-authorized, `claim_credit` pays the full credit, fires
    // `ClaimCredit`, and drains the pool ledger the payout parked into.
    #[test]
    fn parked_payout_round_trips_through_claim_credit() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let user = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        // `set_authorized` requires the issuer's AUTH_REVOCABLE flag.
        token
            .issuer()
            .set_flag(soroban_sdk::testutils::IssuerFlags::RevocableFlag);
        let admin_client = token::StellarAssetClient::new(&e, &token.address());
        admin_client.mint(&contract_id, &1_000);
        // A deauthorized trustline rejects the incoming transfer, so the
        // payout parks instead of aborting.
        admin_client.set_authorized(&user, &false);

        let client = token::Client::new(&e, &token.address());
        e.as_contract(&contract_id, || {
            storage::set_token(&e, &token.address());
            storage::set_status(&e, &Status::Active);
            let mut data = MarketData::default();
            crate::engine::settlement::pay_trader(&e, &client, &mut data, &user, 400);
            storage::set_market_data(&e, &data);
        });

        admin_client.set_authorized(&user, &true);
        e.as_contract(&contract_id, || {
            assert_eq!(crate::engine::funding::claim_credit(&e, &user), 400);
        });
        let claim_data: Map<Symbol, Val> =
            map![&e, (Symbol::new(&e, "amount"), 400_i128.into_val(&e))];
        assert_eq!(
            e.events().all().filter_by_contract(&contract_id),
            vec![
                &e,
                (
                    contract_id.clone(),
                    (Symbol::new(&e, "claim_credit"), user.clone()).into_val(&e),
                    claim_data.into_val(&e),
                ),
            ],
        );
        assert_eq!(client.balance(&user), 400);
        e.as_contract(&contract_id, || {
            assert_eq!(storage::get_claimable_credit(&e, &user), 0);
            let data = storage::get_market_data(&e);
            assert_eq!(data.credit_pool, 0);
            assert_eq!(data.credit_owed, 0);
        });
    }

    // Reports a 5% treasury fee rate, so the draw carries a real treasury leg.
    #[contract]
    struct RatedTreasury;

    #[contractimpl]
    impl RatedTreasury {
        pub fn get_rate(_e: Env) -> i128 {
            crate::engine::constants::SCALAR_18 / 20
        }
    }

    // A fee-crushed wind-down book: a lone 10x long (100.0 notional on 10.0
    // margin, entry $10) at the $10 flat terminal price, with 300.0 of
    // accrued borrowing (index 3.0 against the position's 0 snapshot) plus
    // `funding_idx` of accrued funding. The close debits far past the margin,
    // so equity is 0 and the fee cuts (keeper 0.04, treasury 15.02 at the 5%
    // mock rate) can only be funded by a vault draw against `vault_total`.
    // The contract is pre-funded with the 10.0 margin plus `expected_draw`
    // (the mock vault moves no tokens of its own).
    struct CrushedEnv {
        contract_id: Address,
        token: Address,
        vault: Address,
        user: Address,
        keeper: Address,
        treasury: Address,
    }

    fn crushed_liquidation_env(
        e: &Env,
        vault_total: i128,
        funding_idx: i128,
        expected_draw: i128,
    ) -> CrushedEnv {
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let vault = e.register(MockVault, (vault_total,));
        let token_admin = Address::generate(e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        token::StellarAssetClient::new(e, &token.address())
            .mint(&contract_id, &(10_0000000 + expected_draw));

        let user = Address::generate(e);
        let keeper = Address::generate(e);
        let treasury = e.register(RatedTreasury, ());

        let now = crate::engine::constants::DELIST_DEADLINE;
        e.ledger().with_mut(|ledger| ledger.timestamp = now);
        e.as_contract(&contract_id, || {
            storage::set_token(e, &token.address());
            storage::set_vault(e, &vault);
            storage::set_treasury(e, &treasury);
            storage::set_feed_id(e, &soroban_sdk::BytesN::from_array(e, &[1u8; 32]));
            storage::set_status(e, &Status::Delisted);
            storage::set_delisted_at(e, 0);
            storage::set_terminal_price(e, 10 * PRICE_SCALAR);
            storage::set_config(e, &crate::engine::config::test_config());
            storage::set_market_data(
                e,
                &MarketData {
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
                    borrowing_idx: SidePair {
                        long: 3 * crate::engine::constants::SCALAR_18,
                        short: 0,
                    },
                    funding_idx: SidePair {
                        long: funding_idx,
                        short: 0,
                    },
                    accrued_at: now,
                    ..Default::default()
                },
            );
            storage::set_position(
                e,
                &user,
                true,
                &Position {
                    margin: 10_0000000,
                    notional: 100_0000000,
                    tokens: 10 * BASE_UNIT,
                    funding_idx: 0,
                    borrowing_idx: 0,
                    locked_notional: 0,
                    unlocks_at: 0,
                    priced_at: 0,
                    decrease_orders: soroban_sdk::Vec::new(e),
                },
            );
        });
        CrushedEnv {
            contract_id,
            token: token.address(),
            vault,
            user,
            keeper,
            treasury,
        }
    }

    // Draw 5.06 (fee cuts 15.06 less the 10.0 margin) against a vault
    // holding exactly 5.06: the draw is covered, every leg pays in full
    // (keeper 0.04, treasury 15.02), the vault empties exactly, and the
    // liquidation lands. Behaviour is identical to an unguarded settle. The
    // insolvency check only types the failure. It never reshapes legs.
    #[test]
    fn crushed_liquidation_lands_when_the_vault_covers_the_draw() {
        let e = Env::default();
        let env = crushed_liquidation_env(&e, 5_0600000, 0, 5_0600000);
        let CrushedEnv {
            contract_id,
            token,
            vault,
            user,
            keeper,
            treasury,
        } = env;

        e.as_contract(&contract_id, || {
            let reward = execute_liquidation(&e, &keeper, &user, true, &Bytes::new(&e));
            // Fee-crushed equity is zero, so the capped liquidation fee
            // collects nothing: the keeper earns only its trade-fee cut.
            assert_eq!(reward, 400_000);
            assert_eq!(storage::get_position(&e, &user, true).notional, 0);
        });

        let token_client = token::Client::new(&e, &token);
        assert_eq!(token_client.balance(&keeper), 400_000);
        assert_eq!(token_client.balance(&treasury), 15_0200000);
        assert_eq!(token_client.balance(&contract_id), 0);
        assert_eq!(
            crate::dependencies::VaultClient::new(&e, &vault).total_assets(),
            0
        );
    }

    // The same crushed book against a vault that cannot cover the draw:
    // the settlement aborts with the typed VaultInsolvent error instead of
    // trapping in the vault's own token transfer, and never pays any leg
    // from the contract's co-mingled margin escrow. Recovery is operational
    // (recapitalize the vault, retry).
    #[test]
    #[should_panic(expected = "#755")]
    fn crushed_liquidation_draw_past_the_vault_balance_panics_vault_insolvent() {
        let e = Env::default();
        let CrushedEnv {
            contract_id,
            user,
            keeper,
            ..
        } = crushed_liquidation_env(&e, 3_0000000, 0, 0);

        e.as_contract(&contract_id, || {
            execute_liquidation(&e, &keeper, &user, true, &Bytes::new(&e));
        });
    }
}
