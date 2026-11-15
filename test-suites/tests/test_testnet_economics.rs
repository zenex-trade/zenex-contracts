//! Recorded testnet economics with raised position, open-interest and vault
//! caps, real LP shares and the XLM feed ID. Reports use synthetic non-round
//! XLM prices at V3 precision. The shared verifier checks real signatures from
//! public test signers.

use market::{Config, MarketError, OrderKind, VaultOrderKind};
use soroban_sdk::{token::TokenClient, xdr::ToXdr, Address};
use test_suites::carry::{borrowing_index, lone_side_carry};
use test_suites::test_fixture::TestFixture;
use test_suites::testnet_profile::TestnetProfile;
use test_suites::{SCALAR_18, SCALAR_7};

const MID: i128 = 312_345_678_901_234_567;
const HALF_SPREAD: i128 = 100_000_000_000_000;

fn ceil(numerator: i128, denominator: i128) -> i128 {
    (numerator + denominator - 1) / denominator
}

fn floor_factor(amount: i128, factor: i128) -> i128 {
    amount * factor / SCALAR_18
}

fn trade_fee(config: &Config, notional: i128, opening: bool) -> i128 {
    let rate = if opening {
        config.fee_dom
    } else {
        config.fee_non_dom
    };
    ceil(notional * rate, SCALAR_18)
        + ceil(notional * notional, config.impact_scalar).min(ceil(notional, 10))
}

fn marked_pnl(tokens: i128, notional: i128, mark: i128, is_long: bool) -> i128 {
    if is_long {
        tokens * mark / SCALAR_18 - notional
    } else {
        notional - ceil(tokens * mark, SCALAR_18)
    }
}

fn total_tokens(fixture: &TestFixture) -> i128 {
    fixture
        .users
        .iter()
        .map(|user| fixture.balance(user))
        .sum::<i128>()
        + [
            &fixture.keeper,
            &fixture.treasury.address,
            &fixture.vault.address,
            &fixture.market.address,
        ]
        .iter()
        .map(|address| fixture.balance(address))
        .sum::<i128>()
}

fn assert_flat(fixture: &TestFixture, trader: &Address, is_long: bool) {
    let position = fixture.position(trader, is_long);
    assert_eq!(
        (position.notional, position.tokens, position.margin),
        (0, 0, 0)
    );
    assert!(position.decrease_orders.is_empty());
    let data = fixture.market.get_market_data();
    for pair in [
        &data.notional,
        &data.tokens,
        &data.margin,
        &data.borrowing_weight,
    ] {
        assert_eq!((pair.long, pair.short), (0, 0));
    }
    assert_eq!(data.credit_owed, 0, "one-sided funding has no receiver");
    assert_eq!(fixture.balance(&fixture.market.address), data.credit_pool);
    assert_eq!(fixture.market.get_claimable_credit(trader), 0);
}

/// A fee-paying deposit mints real shares against the empty vault, without
/// donation.
fn seed_lp(fixture: &mut TestFixture, amount: i128) -> Address {
    let input = TestnetProfile::load();
    let config = input.test_config();
    let lp = fixture.new_user(amount + 10 * SCALAR_7);
    let fee = floor_factor(amount, config.deposit_fee);
    let expected_shares = (amount - fee) * 10_i128.pow(input.vault.decimals_offset);
    let fill = fixture.vault_deposit(&fixture.keeper, &lp, amount, expected_shares, MID);
    assert!(fill.filled, "deposit error {}", fill.error);
    assert_eq!(fixture.share_balance(&lp), expected_shares);
    assert_eq!(
        fixture.vault.total_assets(),
        amount - floor_factor(fee, config.keeper_rate) - floor_factor(fee, input.treasury.rate)
    );
    assert_eq!(
        fill.payout,
        floor_factor(fee, config.keeper_rate) + config.exec_fee
    );
    lp
}

/// Redeem all real shares after flattening. The documented virtual asset and
/// the vault's own redeem-fee cut remain. Neither is an unclosed liability.
fn exit_lp(fixture: &TestFixture, lp: &Address, mark: i128) {
    let input = TestnetProfile::load();
    let config = input.test_config();
    let before = fixture.vault.total_assets();
    let lp_before = fixture.balance(lp);
    let keeper_before = fixture.balance(&fixture.keeper);
    let treasury_before = fixture.balance(&fixture.treasury.address);
    let shares = fixture.share_balance(lp);
    let assets = shares * (before + 1) / (shares + 10_i128.pow(input.vault.decimals_offset));
    let fee = floor_factor(assets, config.redeem_fee);
    let keeper_cut = floor_factor(fee, config.keeper_rate);
    let treasury_cut = floor_factor(fee, input.treasury.rate);
    let id = fixture.market.create_vault_order(
        lp,
        &(VaultOrderKind::Redeem as u32),
        &shares,
        &(assets - fee),
    );
    assert!(
        config.redeem_lock > 5,
        "the recorded redeem_lock must leave room for a pre-lock probe"
    );
    fixture.jump(config.redeem_lock - 5);
    assert_eq!(
        fixture
            .market
            .try_execute_vault_order(&fixture.keeper, lp, &id, &fixture.price(mark)),
        Err(Ok(MarketError::VaultOrderLocked.into()))
    );
    assert_eq!(fixture.share_balance(lp), 0, "all real shares escrowed");
    assert_eq!(fixture.share_balance(&fixture.market.address), shares);
    fixture.jump(10);
    let reward = fixture
        .market
        .execute_vault_order(&fixture.keeper, lp, &id, &fixture.price(mark));
    assert_eq!(reward, keeper_cut + config.exec_fee);
    assert_eq!(
        fixture.balance(lp),
        lp_before - config.exec_fee + assets - fee
    );
    assert_eq!(fixture.balance(&fixture.keeper), keeper_before + reward);
    assert_eq!(
        fixture.balance(&fixture.treasury.address),
        treasury_before + treasury_cut
    );
    assert_eq!(
        fixture.vault.total_assets(),
        before - assets + fee - keeper_cut - treasury_cut
    );
    assert_eq!(fixture.share_balance(lp), 0);
    assert_eq!(
        fixture.share_balance(&fixture.market.address),
        0,
        "escrowed shares burned"
    );
    assert!(fixture.market.try_get_vault_order(lp, &id).is_err());
}

#[test]
fn testnet_profile_round_trips_every_config_field_and_stack_dial() {
    let input = TestnetProfile::load();
    let fixture = TestFixture::create_testnet();
    let recorded = input.market.config();
    let mut expanded = input.test_config();
    assert_eq!(input.capacity_multiplier, 1_000);
    assert_eq!(
        expanded.max_position_notional,
        recorded.max_position_notional * 1_000
    );
    assert_eq!(
        expanded.max_open_interest,
        recorded.max_open_interest * 1_000
    );
    assert_eq!(
        expanded.max_vault_balance,
        recorded.max_vault_balance * 1_000
    );
    expanded.max_position_notional = recorded.max_position_notional;
    expanded.max_open_interest = recorded.max_open_interest;
    expanded.max_vault_balance = recorded.max_vault_balance;
    assert_eq!(
        expanded.to_xdr(&fixture.env),
        recorded.to_xdr(&fixture.env),
        "capacity overrides preserve every recorded economic field"
    );
    let expected = input.test_config().to_xdr(&fixture.env);
    let stored = fixture.market.get_config().to_xdr(&fixture.env);
    assert_eq!(stored, expected, "all 34 Config fields including limits");
    assert_eq!(fixture.market.get_feed(), input.feed_id(&fixture.env));
    assert_eq!(
        fixture.oracle.trade_staleness(),
        input.oracle.trade_staleness
    );
    assert_eq!(
        fixture.oracle.close_staleness(),
        input.oracle.close_staleness
    );
    assert_eq!(
        fixture.oracle.spread_reduction_factor(),
        input.oracle.spread_reduction_factor
    );
    assert_eq!(fixture.treasury.get_rate(), input.treasury.rate);
    let shares = TokenClient::new(&fixture.env, &fixture.vault.address);
    assert_eq!(
        shares.decimals(),
        input.settlement.decimals + input.vault.decimals_offset
    );
    assert_eq!(shares.name().to_string(), input.vault.name);
    assert_eq!(shares.symbol().to_string(), input.vault.symbol);
    assert_eq!(
        TokenClient::new(&fixture.env, &fixture.token_address).decimals(),
        input.settlement.decimals
    );
    let raw = fixture.price_bid_ask(MID, MID - HALF_SPREAD, MID + HALF_SPREAD);
    let verified = fixture.oracle.verify_price(&raw, &fixture.feed_id, &false);
    let retained = HALF_SPREAD - floor_factor(HALF_SPREAD, input.oracle.spread_reduction_factor);
    assert_eq!(
        (verified.bid, verified.ask),
        (MID - retained, MID + retained)
    );
}

fn exact_limit_lifecycle(is_long: bool) {
    let input = TestnetProfile::load();
    let config = input.test_config();
    let mut fixture = TestFixture::create_testnet();
    let lp = seed_lp(&mut fixture, config.max_vault_balance);
    let trader_funds = config.max_position_notional;
    let trader = fixture.new_user(trader_funds);
    let minted = total_tokens(&fixture);
    let notional = config.max_position_notional;
    // The capacity-scale impact fee reaches 10%. Enough margin covers both
    // entry and a modeled full close while preserving the recorded 20x ladder.
    let margin = notional * 2 / 5;
    let spread = HALF_SPREAD - floor_factor(HALF_SPREAD, input.oracle.spread_reduction_factor);
    let bid = MID - spread;
    let ask = MID + spread;
    let entry = if is_long { ask } else { bid };
    let exit = if is_long { bid } else { ask };
    let tokens = if is_long {
        notional * SCALAR_18 / entry
    } else {
        ceil(notional * SCALAR_18, entry)
    };
    let mut expected_trader = trader_funds - margin - config.exec_fee;
    let open_fee = trade_fee(&config, notional, true);
    let mut expected_margin = margin - open_fee;
    let mut expected_keeper = fixture.balance(&fixture.keeper)
        + floor_factor(open_fee, config.keeper_rate)
        + config.exec_fee;
    let mut expected_treasury =
        fixture.balance(&fixture.treasury.address) + floor_factor(open_fee, input.treasury.rate);
    let mut expected_vault = fixture.vault.total_assets() + open_fee
        - floor_factor(open_fee, config.keeper_rate)
        - floor_factor(open_fee, input.treasury.rate);
    let price = fixture.price_bid_ask(MID, MID - HALF_SPREAD, MID + HALF_SPREAD);
    fixture.create_and_fill(
        &fixture.keeper,
        &trader,
        is_long,
        OrderKind::MarketIncrease as u32,
        notional,
        margin,
        0,
        0,
        fixture.far_expiration(),
        &price,
    );
    let position = fixture.position(&trader, is_long);
    assert_eq!(
        (position.notional, position.tokens, position.margin),
        (notional, tokens, expected_margin)
    );

    // An individually valid order cannot grow an existing position past the
    // exact configured limit. Its trapped fill rolls back. Cancel its escrow.
    let too_large = fixture.create_and_try_fill(
        &fixture.keeper,
        &trader,
        is_long,
        OrderKind::MarketIncrease as u32,
        config.min_order_notional,
        config.min_order_margin,
        0,
        0,
        fixture.far_expiration(),
        &price,
    );
    assert!(!too_large.filled);
    assert_eq!(too_large.error, MarketError::NotionalAboveMaximum as u32);
    fixture.market.cancel_order(&trader, &too_large.id);
    assert_eq!(fixture.position(&trader, is_long).notional, notional);

    let mut remaining_notional = notional;
    let mut remaining_tokens = tokens;
    let mut expected_pool = 0;
    let mut funding_rate = 0;
    let mut expected_borrow_idx = 0;
    let mut expected_funding_idx = 0;
    // Two real hours: the second window reaches the recorded funding cap.
    for (index, close_notional) in [notional / 3 + 1, notional - (notional / 3 + 1)]
        .into_iter()
        .enumerate()
    {
        let seconds = 3_600;
        let reserve = if is_long {
            ceil(remaining_tokens * ask, SCALAR_18)
        } else {
            remaining_notional
        };
        let borrow_idx = borrowing_index(&config, reserve, expected_vault, seconds);
        funding_rate = (funding_rate + config.funding_increase * seconds).min(config.funding_max);
        let funding_idx = funding_rate.max(config.funding_min) * seconds;
        let borrowing = ceil(remaining_notional * borrow_idx, SCALAR_18);
        let funding = ceil(remaining_notional * funding_idx, SCALAR_18);
        expected_borrow_idx += borrow_idx;
        expected_funding_idx += funding_idx;
        expected_pool += funding;
        let closed_tokens = if index == 0 {
            remaining_tokens * close_notional / remaining_notional
        } else {
            remaining_tokens
        };
        let pnl = marked_pnl(closed_tokens, close_notional, exit, is_long);
        assert!(pnl < 0, "non-round adverse spread costs the trader");
        let fee = trade_fee(&config, close_notional, false);
        let keeper = floor_factor(fee, config.keeper_rate);
        let treasury =
            floor_factor(fee, input.treasury.rate) + floor_factor(borrowing, input.treasury.rate);
        expected_margin += pnl - fee - borrowing - funding;
        expected_trader -= config.exec_fee;
        if index == 1 {
            expected_trader += expected_margin;
            expected_margin = 0;
        }
        expected_keeper += keeper + config.exec_fee;
        expected_treasury += treasury;
        expected_vault += fee + borrowing - keeper - treasury - pnl;

        fixture.jump(seconds as u64);
        let price = fixture.price_bid_ask(MID, MID - HALF_SPREAD, MID + HALF_SPREAD);
        let reward = fixture.create_and_fill(
            &fixture.keeper,
            &trader,
            is_long,
            OrderKind::MarketDecrease as u32,
            close_notional,
            0,
            0,
            0,
            fixture.far_expiration(),
            &price,
        );
        assert_eq!(reward, keeper + config.exec_fee);
        remaining_notional -= close_notional;
        remaining_tokens -= closed_tokens;
        let position = fixture.position(&trader, is_long);
        assert_eq!(
            (position.notional, position.tokens, position.margin),
            (remaining_notional, remaining_tokens, expected_margin)
        );
        let data = fixture.market.get_market_data();
        assert_eq!(data.borrowing_idx.get(is_long), expected_borrow_idx);
        assert_eq!(data.funding_idx.get(is_long), expected_funding_idx);
        assert_eq!(data.borrowing_idx.get(!is_long), 0);
        assert_eq!(data.funding_idx.get(!is_long), 0);
        assert_eq!(
            data.funding_rate,
            if is_long { funding_rate } else { -funding_rate }
        );
        assert_eq!(data.credit_pool, expected_pool);
        assert_eq!(fixture.balance(&trader), expected_trader);
        assert_eq!(fixture.balance(&fixture.keeper), expected_keeper);
        assert_eq!(
            fixture.balance(&fixture.treasury.address),
            expected_treasury
        );
        assert_eq!(fixture.vault.total_assets(), expected_vault);
        assert_eq!(total_tokens(&fixture), minted, "every token accounted for");
        fixture.assert_borrowing_books(&fixture.users);
    }
    assert_eq!(funding_rate, config.funding_max);
    assert_flat(&fixture, &trader, is_long);
    exit_lp(&fixture, &lp, MID);
    assert_flat(&fixture, &trader, is_long);
    assert_eq!(total_tokens(&fixture), minted);
}

#[test]
fn exact_long_limits_carry_partial_close_full_close_and_lp_exit() {
    exact_limit_lifecycle(true);
}

#[test]
fn exact_short_limits_carry_partial_close_full_close_and_lp_exit() {
    exact_limit_lifecycle(false);
}

#[test]
fn exact_margin_ladder_liquidates_both_sides_and_lp_exits() {
    for is_long in [true, false] {
        let input = TestnetProfile::load();
        let config = input.test_config();
        let mut fixture = TestFixture::create_testnet();
        let notional = 100 * SCALAR_7;
        let lp = seed_lp(&mut fixture, 1_000 * SCALAR_7);
        let trader = fixture.new_user(100 * SCALAR_7);
        let minted = total_tokens(&fixture);
        let margin = ceil(notional * config.init_margin, SCALAR_18);
        let open_fee = trade_fee(&config, notional, true);
        fixture.create_and_fill(
            &fixture.keeper,
            &trader,
            is_long,
            OrderKind::MarketIncrease as u32,
            notional,
            margin + open_fee,
            0,
            0,
            fixture.far_expiration(),
            &fixture.price(MID),
        );
        assert_eq!(
            fixture.position(&trader, is_long).margin,
            margin,
            "exact initial-margin floor after entry fees"
        );
        // A 3/4-initial-margin move leaves positive equity below the
        // recorded 2% maintenance line after close fees and carry.
        let price_move = floor_factor(MID, config.init_margin) * 3 / 4;
        let mark = if is_long {
            MID - price_move
        } else {
            MID + price_move
        };
        let position = fixture.position(&trader, is_long);
        let before_vault = fixture.vault.total_assets();
        let hold = config.notional_lock + 5;
        let reserve = if is_long {
            ceil(position.tokens * mark, SCALAR_18)
        } else {
            notional
        };
        let carry = lone_side_carry(&config, notional, reserve, before_vault, hold as i128);
        let pnl = marked_pnl(position.tokens, notional, mark, is_long);
        let close_fee = trade_fee(&config, notional, false);
        let equity = margin + pnl - close_fee - carry.total();
        assert!(equity > 0 && equity < ceil(notional * config.maintenance_margin, SCALAR_18));
        let liq_fee = ceil(notional * config.liq_fee, SCALAR_18).min(equity);
        let keeper = floor_factor(close_fee + liq_fee, config.keeper_rate);
        let treasury = floor_factor(close_fee + liq_fee, input.treasury.rate)
            + floor_factor(carry.borrowing, input.treasury.rate);
        let trader_before = fixture.balance(&trader);
        let keeper_before = fixture.balance(&fixture.keeper);
        let treasury_before = fixture.balance(&fixture.treasury.address);
        fixture.jump(hold);
        let reward = fixture.market.execute_liquidation(
            &fixture.keeper,
            &trader,
            &is_long,
            &fixture.price(mark),
        );
        assert_eq!(
            reward, keeper,
            "liquidation carries no execution-fee escrow"
        );
        assert_eq!(fixture.balance(&trader), trader_before + equity - liq_fee);
        assert_eq!(fixture.balance(&fixture.keeper), keeper_before + keeper);
        assert_eq!(
            fixture.balance(&fixture.treasury.address),
            treasury_before + treasury
        );
        assert_eq!(
            fixture.vault.total_assets(),
            before_vault + close_fee + liq_fee + carry.borrowing - keeper - treasury - pnl
        );
        assert_flat(&fixture, &trader, is_long);
        exit_lp(&fixture, &lp, mark);
        assert_eq!(total_tokens(&fixture), minted);
    }
}

#[test]
fn exact_adl_band_deleverages_both_sides_then_book_and_lp_settle() {
    for is_long in [true, false] {
        let input = TestnetProfile::load();
        let config = input.test_config();
        let mut fixture = TestFixture::create_testnet();
        let notional = 1_000 * SCALAR_7;
        let lp = seed_lp(&mut fixture, 3 * notional);
        let trader = fixture.new_user(notional);
        let minted = total_tokens(&fixture);
        fixture.create_and_fill(
            &fixture.keeper,
            &trader,
            is_long,
            OrderKind::MarketIncrease as u32,
            notional,
            notional * 2 / 5,
            0,
            0,
            fixture.far_expiration(),
            &fixture.price(MID),
        );
        let hold = config.notional_lock + 5;
        fixture.jump(hold);
        let mark = if is_long { MID * 2 } else { MID / 10 };
        let report = fixture.price(mark);
        let position = fixture.position(&trader, is_long);
        let before_pnl = marked_pnl(position.tokens, notional, mark, is_long);
        assert!(before_pnl > floor_factor(fixture.vault.total_assets() / 2, config.adl_max_pnl));
        assert!(fixture.market.update_adl_state(&report).get(is_long));
        let amount = notional / 4 + 1;
        let closed_tokens = position.tokens * amount / notional;
        let pnl = marked_pnl(closed_tokens, amount, mark, is_long);
        let vault_before = fixture.vault.total_assets();
        let trader_before = fixture.balance(&trader);
        let treasury_before = fixture.balance(&fixture.treasury.address);
        let keeper_before = fixture.balance(&fixture.keeper);
        let reserve = if is_long {
            ceil(position.tokens * mark, SCALAR_18)
        } else {
            notional
        };
        let carry = lone_side_carry(&config, notional, reserve, vault_before, hold as i128);
        let fee = trade_fee(&config, amount, false);
        let keeper = floor_factor(fee, config.keeper_rate);
        let treasury = floor_factor(fee, input.treasury.rate)
            + floor_factor(carry.borrowing, input.treasury.rate);
        let reward =
            fixture
                .market
                .execute_adl(&fixture.keeper, &trader, &is_long, &amount, &report);
        assert_eq!(reward, keeper);
        assert_eq!(fixture.balance(&fixture.keeper), keeper_before + reward);
        assert_eq!(
            fixture.balance(&trader),
            trader_before + pnl - fee - carry.total()
        );
        assert_eq!(
            fixture.balance(&fixture.treasury.address),
            treasury_before + treasury
        );
        assert_eq!(
            fixture.vault.total_assets(),
            vault_before + fee + carry.borrowing - keeper - treasury - pnl
        );
        assert_eq!(fixture.market.get_market_data().credit_pool, carry.funding);
        let survivor = fixture.position(&trader, is_long);
        assert_eq!(
            survivor.margin, position.margin,
            "profit covers fees and carry"
        );
        assert_eq!(survivor.notional, notional - amount);
        assert_eq!(
            survivor.tokens,
            position.tokens - position.tokens * amount / notional
        );
        let after_pnl = marked_pnl(survivor.tokens, survivor.notional, mark, is_long);
        assert!(after_pnl < before_pnl);
        assert!(
            after_pnl >= floor_factor(fixture.vault.total_assets() / 2, config.adl_clear_target)
        );
        assert_eq!(total_tokens(&fixture), minted);
        fixture.assert_borrowing_books(&fixture.users);
        // ADL forbids overshooting its clear floor. The surviving trader can
        // close voluntarily. This tests complete settlement without claiming
        // that a keeper can force every healthy winner completely flat.
        fixture.create_and_fill(
            &fixture.keeper,
            &trader,
            is_long,
            OrderKind::MarketDecrease as u32,
            survivor.notional,
            0,
            0,
            0,
            fixture.far_expiration(),
            &report,
        );
        assert_flat(&fixture, &trader, is_long);
        assert!(!fixture.market.update_adl_state(&report).get(is_long));
        exit_lp(&fixture, &lp, mark);
        assert_eq!(total_tokens(&fixture), minted);
    }
}

#[test]
fn testnet_adl_arms_strictly_above_55_percent_and_clears_at_40_percent() {
    for is_long in [true, false] {
        let config = TestnetProfile::load().test_config();
        assert_eq!(config.adl_max_pnl, 55 * SCALAR_18 / 100);
        assert_eq!(config.adl_clear_target, 40 * SCALAR_18 / 100);
        let mut fixture = TestFixture::create_testnet();
        let notional = 1_000 * SCALAR_7;
        let lp = seed_lp(&mut fixture, 3 * notional);
        let trader = fixture.new_user(notional);
        fixture.open(
            &trader,
            is_long,
            notional,
            notional / 2,
            &fixture.price(MID),
        );
        fixture.jump(config.notional_lock + 5);
        let tokens = fixture.position(&trader, is_long).tokens;
        let half_vault = fixture.vault.total_assets() / 2;
        // Set the maximized side PnL to an exact token atom. Long maximizes
        // with ceil(value). Short maximizes with notional - floor(value). These
        // marks invert those independent integer equations at XLM's 18-dec
        // scale.
        let report_for_pnl = |pnl: i128| {
            let mark = if is_long {
                (notional + pnl) * SCALAR_18 / tokens
            } else {
                ceil((notional - pnl) * SCALAR_18, tokens)
            };
            fixture.price(mark)
        };
        let between_defaults = floor_factor(half_vault, 53 * SCALAR_18 / 100);
        assert!(
            !fixture
                .market
                .update_adl_state(&report_for_pnl(between_defaults))
                .get(is_long),
            "53% must not arm the testnet 55% trigger"
        );
        let trigger = floor_factor(half_vault, config.adl_max_pnl);
        assert!(
            !fixture
                .market
                .update_adl_state(&report_for_pnl(trigger))
                .get(is_long),
            "exact trigger remains unarmed"
        );
        assert!(
            fixture
                .market
                .update_adl_state(&report_for_pnl(trigger + 1))
                .get(is_long),
            "one atom above the trigger arms"
        );
        let clear = floor_factor(half_vault, config.adl_clear_target);
        assert!(
            fixture
                .market
                .update_adl_state(&report_for_pnl(clear + 1))
                .get(is_long),
            "hysteresis holds one atom above clear"
        );
        assert!(
            !fixture
                .market
                .update_adl_state(&report_for_pnl(clear))
                .get(is_long),
            "exact clear allowance clears"
        );
        fixture.close(&trader, is_long, notional, 0, &fixture.price(MID));
        assert_flat(&fixture, &trader, is_long);
        exit_lp(&fixture, &lp, MID);
    }
}
