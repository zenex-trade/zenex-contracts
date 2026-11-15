//! Auto-deleveraging: the per-side [`AdlState`] flags, armed by
//! [`update_adl_state`] and consumed by [`execute_adl`].

use crate::engine::market::Market;
use crate::engine::math;
use crate::engine::position::Position;
use crate::engine::settlement::Settlement;
use crate::errors::MarketError;
use crate::events::AdlUpdate;
use crate::storage;
use soroban_sdk::{contracttype, panic_with_error, Address, Bytes, Env};

/// The per-side ADL flags. A side's flag sets while its pending PnL exceeds
/// `adl_max_pnl` of half the vault balance, and holds until it falls to
/// `adl_clear_target`.
#[derive(Clone, Debug, Default)]
#[contracttype]
pub struct AdlState {
    /// Long-side ADL is enabled. Size-growing long increases are blocked.
    pub long: bool,
    /// Short-side ADL is enabled. Size-growing short increases are blocked.
    pub short: bool,
}

impl AdlState {
    /// Returns the enable flag on `is_long`'s side.
    pub fn get(&self, is_long: bool) -> bool {
        if is_long {
            self.long
        } else {
            self.short
        }
    }
}

/// Recomputes both sides' pending PnL at the effective price, sets or
/// clears the per-side [`AdlState`] flags, and returns the resulting state.
/// Refer to [`Market::update_adl_state`](crate::Market::update_adl_state).
pub fn update_adl_state(e: &Env, price: &Bytes) -> AdlState {
    // Newest price, close class: arming is a solvency protection, so it
    // reads the freshest mark and stays live through a feed gap.
    let market = Market::load(e, price, true, true);
    // Both allowances round down, so the trigger errs early and the clear
    // errs late.
    let trigger = math::half_factor(e, market.vault_balance, market.config.adl_max_pnl);
    let clear = math::half_factor(e, market.vault_balance, market.config.adl_clear_target);
    let long_pnl = market.data.side_pnl(e, &market.price, true, true);
    let short_pnl = market.data.side_pnl(e, &market.price, false, true);
    let current = storage::get_adl(e);
    // Arms above the trigger, holds above the clear allowance.
    let state = AdlState {
        long: long_pnl > trigger || (current.long && long_pnl > clear),
        short: short_pnl > trigger || (current.short && short_pnl > clear),
    };
    storage::set_adl(e, &state);
    market.store(e);
    AdlUpdate {
        long: state.long,
        short: state.short,
    }
    .publish(e);
    state
}

/// Deleverages the winning position `(user, is_long)` by `amount` at the
/// effective price and returns the keeper's payout (token-dec). Refer to
/// [`Market::execute_adl`](crate::Market::execute_adl).
///
/// # Security Warning
/// `user`, `is_long`, and `amount` are caller-supplied, and nothing checks
/// that this is the side's largest winner. Only the side-level PnL bound is
/// enforced, which protects the vault whichever eligible position closes.
/// Winner selection is keeper policy, and fairness among the deleveraged
/// side's traders is not chain-enforced.
pub fn execute_adl(
    e: &Env,
    keeper: &Address,
    user: &Address,
    is_long: bool,
    amount: i128,
    price: &Bytes,
) -> i128 {
    // Newest price, close class: de-risking must land even when the freshest
    // report outlasts the strict fill window.
    let mut market = Market::load(e, price, true, true);
    // The order dust floor: a permissionless keeper cannot slice a winner in
    // dust.
    if amount < market.config.min_order_notional {
        panic_with_error!(e, MarketError::InvalidOrder);
    }
    // Only a side armed by `update_adl_state`, the sole flag writer, may be
    // deleveraged.
    if !storage::get_adl(e).get(is_long) {
        panic_with_error!(e, MarketError::AdlNotTriggered);
    }
    // A side at or under the clear allowance has nothing left to deleverage.
    let before = market.data.side_pnl(e, &market.price, is_long, true);
    if before <= math::half_factor(e, market.vault_balance, market.config.adl_clear_target) {
        panic_with_error!(e, MarketError::AdlNotTriggered);
    }

    let mut position = Position::load(e, user, is_long);
    position.require_price_not_stale(e, market.price.publish_time);

    let decrease = position.decrease(e, &mut market, user, is_long, amount, 0, true);
    let refund = position.store(e, user, is_long);
    decrease.publish_receipt(
        e,
        user,
        0,
        is_long,
        keeper,
        market.price.exit(is_long),
        position.notional == 0,
    );

    // An ADL close carries no order, so no `exec_fee`.
    let settlement = Settlement::compute_decrease_order(e, &market, &decrease, refund, 0);
    let reward = settlement.settle(e, &mut market, keeper, user);

    // Act-then-check on the trigger's own measure. A failing gate traps, and
    // the revert unwinds the close and its settlement.
    let after = market.data.side_pnl(e, &market.price, is_long, true);
    // The overhang must fall. An unchanged or risen measure closed a
    // non-winner.
    if after >= before {
        panic_with_error!(e, MarketError::AdlNotEligible);
    }
    // The clear allowance is re-measured on the settled vault balance.
    if after < math::half_factor(e, market.vault_balance, market.config.adl_clear_target) {
        panic_with_error!(e, MarketError::AdlOvershoot);
    }
    market.store(e);
    reward
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::config::Config;
    use crate::engine::constants::{BASE_UNIT, PRICE_SCALAR, SCALAR_18};
    use crate::engine::market_data::{MarketData, SidePair};
    use crate::engine::position::Position;
    use crate::engine::status::Status;
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::{contract, contractimpl, symbol_short, token, Address, Bytes, Env};

    #[contract]
    struct Harness;

    // Reports a zero treasury fee rate.
    #[contract]
    struct MockTreasury;

    #[contractimpl]
    impl MockTreasury {
        pub fn get_rate(_e: Env) -> i128 {
            0
        }
    }

    // Holds a constructor-set balance and pays the strategy on withdraw,
    // deducting the withdrawal from total_assets.
    #[contract]
    struct MockVault;

    #[contractimpl]
    impl MockVault {
        pub fn __constructor(e: Env, token: Address, strategy: Address, total: i128) {
            e.storage().instance().set(&symbol_short!("token"), &token);
            e.storage()
                .instance()
                .set(&symbol_short!("strat"), &strategy);
            e.storage().instance().set(&symbol_short!("total"), &total);
        }

        pub fn total_assets(e: Env) -> i128 {
            e.storage().instance().get(&symbol_short!("total")).unwrap()
        }

        pub fn strategy_withdraw(e: Env, amount: i128) {
            let total: i128 = e.storage().instance().get(&symbol_short!("total")).unwrap();
            e.storage()
                .instance()
                .set(&symbol_short!("total"), &(total - amount));
            let token: Address = e.storage().instance().get(&symbol_short!("token")).unwrap();
            let strategy: Address = e.storage().instance().get(&symbol_short!("strat")).unwrap();
            token::Client::new(&e, &token).transfer(
                &e.current_contract_address(),
                &strategy,
                &amount,
            );
        }
    }

    // Seeds a triggering book: a lone winning 10x long (80.0 notional, 10
    // base tokens = entry $8) marked flat at $10 carries +20.0 pending PnL
    // against a 30.0 vault. The 50%/30% band triggers ADL.
    fn seed_adl(
        e: &Env,
        contract_id: &Address,
        vault: &Address,
        token: &Address,
        user: &Address,
        margin: i128,
    ) {
        let treasury = e.register(MockTreasury, ());
        e.as_contract(contract_id, || {
            storage::set_token(e, token);
            storage::set_vault(e, vault);
            storage::set_treasury(e, &treasury);
            storage::set_feed_id(e, &soroban_sdk::BytesN::from_array(e, &[1u8; 32]));
            storage::set_status(e, &Status::Active);
            storage::set_terminal_price(e, 10 * PRICE_SCALAR);
            // Arm the winning long side: execution consumes a flag set by
            // `update_adl_state`.
            storage::set_adl(
                e,
                &AdlState {
                    long: true,
                    short: false,
                },
            );
            storage::set_config(
                e,
                &Config {
                    adl_max_pnl: 5 * SCALAR_18 / 10,
                    adl_clear_target: 3 * SCALAR_18 / 10,
                    ..crate::engine::config::test_config()
                },
            );
            storage::set_market_data(
                e,
                &MarketData {
                    notional: SidePair {
                        long: 80_0000000,
                        short: 0,
                    },
                    margin: SidePair {
                        long: margin,
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
                e,
                user,
                true,
                &Position {
                    margin,
                    notional: 80_0000000,
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
    }

    // Registers a harness, seeds the standard +20.0 book against a vault of
    // `vault_total`, and returns the market contract id.
    fn seed_state(e: &Env, vault_total: i128) -> Address {
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let vault = e.register(
            MockVault,
            (token.address(), contract_id.clone(), vault_total),
        );
        let user = Address::generate(e);
        seed_adl(e, &contract_id, &vault, &token.address(), &user, 20_0000000);
        contract_id
    }

    // Pending long PnL 20.0 > trigger floor(50% * 30.0 / 2) = 7.5: the winning
    // side flags, the empty short side does not.
    #[test]
    fn update_adl_state_flags_winning_side_above_trigger() {
        let e = Env::default();
        let contract_id = seed_state(&e, 30_0000000);
        e.as_contract(&contract_id, || {
            storage::set_adl(&e, &AdlState::default());
            let state = update_adl_state(&e, &Bytes::new(&e));
            assert!(state.long);
            assert!(!state.short);
        });
    }

    // 20.0 sits between clear 30% * 50.0 = 15.0 and trigger 50% * 50.0 = 25.0
    // of half the 100.0 vault: the armed seed flag holds, an unset flag stays
    // unset (hysteresis).
    #[test]
    fn update_adl_state_holds_inside_hysteresis_band() {
        let e = Env::default();
        let contract_id = seed_state(&e, 100_0000000);
        e.as_contract(&contract_id, || {
            assert!(update_adl_state(&e, &Bytes::new(&e)).long);
            storage::set_adl(&e, &AdlState::default());
            assert!(!update_adl_state(&e, &Bytes::new(&e)).long);
        });
    }

    // 20.0 <= clear 30% * 150.0 / 2 = 22.5: the armed seed flag clears.
    #[test]
    fn update_adl_state_clears_at_or_below_target() {
        let e = Env::default();
        let contract_id = seed_state(&e, 150_0000000);
        e.as_contract(&contract_id, || {
            assert!(!update_adl_state(&e, &Bytes::new(&e)).long);
        });
    }

    // A losing side is never flagged: the armed seed flag clears once the
    // side is under water, whatever the ratio says.
    #[test]
    fn update_adl_state_never_flags_losing_side() {
        let e = Env::default();
        let contract_id = seed_state(&e, 30_0000000);
        e.as_contract(&contract_id, || {
            let mut data = storage::get_market_data(&e);
            data.notional.long = 120_0000000; // pending long PnL = -20.0
            storage::set_market_data(&e, &data);
            assert!(!update_adl_state(&e, &Bytes::new(&e)).long);
        });
    }

    // A keeper-sized partial close reduces the winner toward the clear target:
    // closing 60.0 removes 15.0 of the +20.0 pending PnL. The vault settles
    // to 30.0 - 9.963 = 20.037 (it funds the 10.125 haircut-scaled payout
    // less the 0.162 fee remainder), and the 5.0 left clears the re-measured
    // allowance floor(30% * 20.037 / 2) = 3.0055500.
    #[test]
    fn execute_adl_partial_close_reduces_toward_clear_target() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let vault = e.register(
            MockVault,
            (token.address(), contract_id.clone(), 30_0000000_i128),
        );
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &100_0000000);
        token::StellarAssetClient::new(&e, &token.address()).mint(&vault, &100_0000000);
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        seed_adl(
            &e,
            &contract_id,
            &vault,
            &token.address(),
            &user,
            20_0000000,
        );

        e.as_contract(&contract_id, || {
            execute_adl(&e, &keeper, &user, true, 60_0000000, &Bytes::new(&e));
            let position = storage::get_position(&e, &user, true);
            assert_eq!(position.notional, 20_0000000);
        });
    }

    // A partial deleveraging leaves the position open, so the closure sweep
    // cancels nothing. The tx carries exactly one receipt, the decrease
    // fill, and no `cancel_order`.
    #[test]
    fn execute_adl_partial_sweeps_no_orders() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::xdr::ContractEventBody;
        use soroban_sdk::{Symbol, TryFromVal};

        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let vault = e.register(
            MockVault,
            (token.address(), contract_id.clone(), 30_0000000_i128),
        );
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &100_0000000);
        token::StellarAssetClient::new(&e, &token.address()).mint(&vault, &100_0000000);
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        seed_adl(
            &e,
            &contract_id,
            &vault,
            &token.address(),
            &user,
            20_0000000,
        );

        e.as_contract(&contract_id, || {
            execute_adl(&e, &keeper, &user, true, 60_0000000, &Bytes::new(&e));
        });

        let all = e.events().all().filter_by_contract(&contract_id);
        let events = all.events();
        assert_eq!(events.len(), 1, "one receipt, no cancel_order");
        let ContractEventBody::V0(body) = &events[0].body;
        assert_eq!(
            Symbol::try_from_val(&e, &body.topics[0]).unwrap(),
            Symbol::new(&e, "decrease_fill")
        );
    }

    // ADL honors the decrease lock: a close touching locked notional is
    // rejected, so a freshly-filled position keeps its lock tail. Increases on
    // an armed side are blocked, so the lock cannot be refreshed past it.
    #[test]
    #[should_panic(expected = "#721")]
    fn execute_adl_rejects_locked_position() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let vault = e.register(
            MockVault,
            (token.address(), contract_id.clone(), 30_0000000_i128),
        );
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        seed_adl(
            &e,
            &contract_id,
            &vault,
            &token.address(),
            &user,
            20_0000000,
        );

        e.as_contract(&contract_id, || {
            let mut position = storage::get_position(&e, &user, true);
            position.locked_notional = 80_0000000;
            position.unlocks_at = u64::MAX;
            storage::set_position(&e, &user, true, &position);
            execute_adl(&e, &keeper, &user, true, i128::MAX, &Bytes::new(&e));
        });
    }

    // A winning position whose margin has been eroded below the initial
    // margin by accrued fees can still be deleveraged: the forced close skips
    // the remainder's initial-margin floor (only the maintenance line applies),
    // so ADL is not blocked on exactly the winner it must reduce. Margin
    // 1.8 on the 20.0 remainder is below the 2.0 init floor but the healthy
    // equity clears maintenance, so the close lands.
    #[test]
    fn execute_adl_deleverages_a_fee_eroded_winner() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let vault = e.register(
            MockVault,
            (token.address(), contract_id.clone(), 30_0000000_i128),
        );
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &100_0000000);
        token::StellarAssetClient::new(&e, &token.address()).mint(&vault, &100_0000000);
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        seed_adl(&e, &contract_id, &vault, &token.address(), &user, 1_8000000);

        e.as_contract(&contract_id, || {
            execute_adl(&e, &keeper, &user, true, 60_0000000, &Bytes::new(&e));
            let position = storage::get_position(&e, &user, true);
            assert_eq!(position.notional, 20_0000000);
        });
    }

    // A smaller close that leaves the side above the clear target is a valid
    // step: closing 30.0 removes 7.5 pending PnL, the vault settles to
    // 30.0 - 4.9815 = 25.0185, and the 12.5 left stays over the re-measured
    // allowance floor(30% * 25.0185 / 2) = 3.7527750.
    #[test]
    fn execute_adl_smaller_close_leaves_side_above_target() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let vault = e.register(
            MockVault,
            (token.address(), contract_id.clone(), 30_0000000_i128),
        );
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &100_0000000);
        token::StellarAssetClient::new(&e, &token.address()).mint(&vault, &100_0000000);
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        seed_adl(
            &e,
            &contract_id,
            &vault,
            &token.address(),
            &user,
            20_0000000,
        );

        e.as_contract(&contract_id, || {
            execute_adl(&e, &keeper, &user, true, 30_0000000, &Bytes::new(&e));
            let position = storage::get_position(&e, &user, true);
            assert_eq!(position.notional, 50_0000000);
        });
    }

    // A close leaving a sub-minimum remainder clamps to a full close inside
    // the decrease: a 79.5 request leaves 0.5, under the position minimum, so
    // the whole 80.0 closes. In this lone-winner book the full close zeroes
    // the side measure and trips the same overshoot guard as an explicit full
    // close (#771), no longer the dead-end NotionalBelowMinimum (#711).
    #[test]
    #[should_panic(expected = "#771")]
    fn execute_adl_sub_minimum_remainder_clamps_to_full_close() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let vault = e.register(
            MockVault,
            (token.address(), contract_id.clone(), 30_0000000_i128),
        );
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &100_0000000);
        token::StellarAssetClient::new(&e, &token.address()).mint(&vault, &100_0000000);
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        seed_adl(
            &e,
            &contract_id,
            &vault,
            &token.address(),
            &user,
            20_0000000,
        );

        e.as_contract(&contract_id, || {
            execute_adl(&e, &keeper, &user, true, 79_5000000, &Bytes::new(&e));
        });
    }

    // The clamped full close lands when the side keeps enough overhang: a
    // second winner (8.0 notional, 1 base token = entry $8, +2.0 pending PnL)
    // joins the book, and a 7.5 request against it leaves 0.5, under the
    // position minimum, so its whole 8.0 closes. The side measure falls from
    // 22.0 to the seed winner's 20.0, over the re-measured clear allowance,
    // and the fill lands instead of reverting #711.
    #[test]
    fn execute_adl_dust_band_request_closes_position_in_full() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let vault = e.register(
            MockVault,
            (token.address(), contract_id.clone(), 30_0000000_i128),
        );
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &100_0000000);
        token::StellarAssetClient::new(&e, &token.address()).mint(&vault, &100_0000000);
        let user = Address::generate(&e);
        let winner = Address::generate(&e);
        let keeper = Address::generate(&e);

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        seed_adl(
            &e,
            &contract_id,
            &vault,
            &token.address(),
            &user,
            20_0000000,
        );

        e.as_contract(&contract_id, || {
            // Fold the second winner into the armed side's aggregates.
            let mut data = storage::get_market_data(&e);
            data.notional.long += 8_0000000;
            data.tokens.long += BASE_UNIT;
            data.margin.long += 2_0000000;
            storage::set_market_data(&e, &data);
            storage::set_position(
                &e,
                &winner,
                true,
                &Position {
                    margin: 2_0000000,
                    notional: 8_0000000,
                    tokens: BASE_UNIT,
                    funding_idx: 0,
                    borrowing_idx: 0,
                    locked_notional: 0,
                    unlocks_at: 0,
                    priced_at: 0,
                    decrease_orders: soroban_sdk::Vec::new(&e),
                },
            );
            execute_adl(&e, &keeper, &winner, true, 7_5000000, &Bytes::new(&e));
            assert_eq!(
                storage::get_position(&e, &winner, true).notional,
                0,
                "the dust-band request closed the winner in full",
            );
            assert_eq!(
                storage::get_position(&e, &user, true).notional,
                80_0000000,
                "the seed winner is untouched",
            );
        });
    }

    // Removing more pending PnL than the distance to the clear target
    // overshoots: a full close zeroes the side measure, under the re-measured
    // allowance floor(30% * (30.0 - 13.284) / 2) = 2.5074000 on the settled
    // vault.
    #[test]
    #[should_panic(expected = "#771")]
    fn execute_adl_full_close_overshoot_rejected() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let vault = e.register(
            MockVault,
            (token.address(), contract_id.clone(), 30_0000000_i128),
        );
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &100_0000000);
        token::StellarAssetClient::new(&e, &token.address()).mint(&vault, &100_0000000);
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        seed_adl(
            &e,
            &contract_id,
            &vault,
            &token.address(),
            &user,
            20_0000000,
        );

        e.as_contract(&contract_id, || {
            execute_adl(&e, &keeper, &user, true, i128::MAX, &Bytes::new(&e));
        });
    }

    // The overshoot line is strict: a close that leaves the side exactly on
    // the re-measured clear allowance lands. The vault holds 48.1389334. The
    // +20.0 side sits under the trader allowance floor(90% * 24.0694667) =
    // 21.66252, so closing 60.0 pays its 15.0 profit unhaircut. The vault
    // funds it and banks 0.1944 of the 0.216 in fees (0.18 base, 0.036
    // impact), settling to 33.3333334. The clear allowance
    // floor(30% * 16.6666667) = 5.0 equals the 5.0 left.
    #[test]
    fn execute_adl_lands_exactly_on_the_clear_allowance() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let vault = e.register(
            MockVault,
            (token.address(), contract_id.clone(), 48_1389334_i128),
        );
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &100_0000000);
        token::StellarAssetClient::new(&e, &token.address()).mint(&vault, &100_0000000);
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        seed_adl(
            &e,
            &contract_id,
            &vault,
            &token.address(),
            &user,
            20_0000000,
        );

        e.as_contract(&contract_id, || {
            execute_adl(&e, &keeper, &user, true, 60_0000000, &Bytes::new(&e));
            assert_eq!(storage::get_position(&e, &user, true).notional, 20_0000000);
        });
        assert_eq!(MockVaultClient::new(&e, &vault).total_assets(), 33_3333334);
    }

    // The overshoot line binds partials too: closing 69.5 leaves 2.625 pending
    // PnL, under the re-measured allowance floor(30% * (30.0 - 11.540475) / 2)
    // = 2.7689287 on the settled vault.
    #[test]
    #[should_panic(expected = "#771")]
    fn execute_adl_partial_overshoot_rejected() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let vault = e.register(
            MockVault,
            (token.address(), contract_id.clone(), 30_0000000_i128),
        );
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &100_0000000);
        token::StellarAssetClient::new(&e, &token.address()).mint(&vault, &100_0000000);
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        seed_adl(
            &e,
            &contract_id,
            &vault,
            &token.address(),
            &user,
            20_0000000,
        );

        e.as_contract(&contract_id, || {
            execute_adl(&e, &keeper, &user, true, 69_5000000, &Bytes::new(&e));
        });
    }

    // A slice exactly at the order dust floor passes the guard and lands:
    // closing 0.1 removes floor(1e8 * 1e6 / 8e8) = 1.25e5 tokens carrying
    // 0.025 pending PnL, and the 19.975 left clears the re-measured allowance.
    #[test]
    fn execute_adl_accepts_amount_at_order_floor() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let vault = e.register(
            MockVault,
            (token.address(), contract_id.clone(), 30_0000000_i128),
        );
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &100_0000000);
        token::StellarAssetClient::new(&e, &token.address()).mint(&vault, &100_0000000);
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        seed_adl(
            &e,
            &contract_id,
            &vault,
            &token.address(),
            &user,
            20_0000000,
        );

        e.as_contract(&contract_id, || {
            let floor = storage::get_config(&e).min_order_notional;
            assert_eq!(floor, 1_000_000);
            execute_adl(&e, &keeper, &user, true, floor, &Bytes::new(&e));
            let position = storage::get_position(&e, &user, true);
            assert_eq!(position.notional, 79_9000000);
            assert_eq!(position.tokens, 10 * BASE_UNIT - 125_000);
        });
    }

    // A slice one unit under the order dust floor traps before any ADL gate:
    // a permissionless keeper cannot shave a winner in dust for fees and a
    // receipt.
    #[test]
    #[should_panic(expected = "#732")]
    fn execute_adl_rejects_amount_below_order_floor() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let vault = e.register(
            MockVault,
            (token.address(), contract_id.clone(), 30_0000000_i128),
        );
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        seed_adl(
            &e,
            &contract_id,
            &vault,
            &token.address(),
            &user,
            20_0000000,
        );

        e.as_contract(&contract_id, || {
            let floor = storage::get_config(&e).min_order_notional;
            execute_adl(&e, &keeper, &user, true, floor - 1, &Bytes::new(&e));
        });
    }

    // A negative close amount sits under the positive order floor, so it
    // traps before the signed decrease math could grow the position.
    #[test]
    #[should_panic(expected = "#732")]
    fn execute_adl_rejects_negative_amount() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let vault = e.register(
            MockVault,
            (token.address(), contract_id.clone(), 30_0000000_i128),
        );
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        seed_adl(
            &e,
            &contract_id,
            &vault,
            &token.address(),
            &user,
            20_0000000,
        );

        e.as_contract(&contract_id, || {
            execute_adl(&e, &keeper, &user, true, -10_0000000, &Bytes::new(&e));
        });
    }

    // A zero close amount fails the same floor.
    #[test]
    #[should_panic(expected = "#732")]
    fn execute_adl_rejects_zero_amount() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let vault = e.register(
            MockVault,
            (token.address(), contract_id.clone(), 30_0000000_i128),
        );
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        seed_adl(
            &e,
            &contract_id,
            &vault,
            &token.address(),
            &user,
            20_0000000,
        );

        e.as_contract(&contract_id, || {
            execute_adl(&e, &keeper, &user, true, 0, &Bytes::new(&e));
        });
    }

    // With the vault large enough (150.0) that the +20.0 side PnL sits at or
    // below the 30% clear target (22.5), an armed side has nothing left to
    // deleverage and execution is rejected.
    #[test]
    #[should_panic(expected = "#770")]
    fn execute_adl_below_clear_target_rejects() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let vault = e.register(
            MockVault,
            (token.address(), contract_id.clone(), 150_0000000_i128),
        );
        let user = Address::generate(&e);
        let keeper = Address::generate(&e);

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        seed_adl(
            &e,
            &contract_id,
            &vault,
            &token.address(),
            &user,
            20_0000000,
        );

        e.as_contract(&contract_id, || {
            execute_adl(&e, &keeper, &user, true, i128::MAX, &Bytes::new(&e));
        });
    }

    // ADL must reduce the side's overhang: closing a losing position on the
    // armed side raises the measure instead and reverts. A second long entered
    // at $15 (30.0 notional, 2 base tokens) is 10.0 under water at the $10
    // mark. Its full close lifts the side PnL from +10.0 to +20.0.
    #[test]
    #[should_panic(expected = "#772")]
    fn execute_adl_on_losing_position_rejects() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        let vault = e.register(
            MockVault,
            (token.address(), contract_id.clone(), 30_0000000_i128),
        );
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &100_0000000);
        let user = Address::generate(&e);
        let loser = Address::generate(&e);
        let keeper = Address::generate(&e);

        e.ledger().with_mut(|ledger| ledger.timestamp = 100);
        seed_adl(
            &e,
            &contract_id,
            &vault,
            &token.address(),
            &user,
            20_0000000,
        );

        e.as_contract(&contract_id, || {
            // Fold the losing long into the armed side's aggregates.
            let mut data = storage::get_market_data(&e);
            data.notional.long += 30_0000000;
            data.tokens.long += 2 * BASE_UNIT;
            data.margin.long += 12_0000000;
            storage::set_market_data(&e, &data);
            storage::set_position(
                &e,
                &loser,
                true,
                &Position {
                    margin: 12_0000000,
                    notional: 30_0000000,
                    tokens: 2 * BASE_UNIT,
                    funding_idx: 0,
                    borrowing_idx: 0,
                    locked_notional: 0,
                    unlocks_at: 0,
                    priced_at: 0,
                    decrease_orders: soroban_sdk::Vec::new(&e),
                },
            );
            execute_adl(&e, &keeper, &loser, true, i128::MAX, &Bytes::new(&e));
        });
    }
}
