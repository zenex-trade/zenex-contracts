//! Broad stress coverage using the recorded testnet curves and stack dials.
//!
//! Most suites run `test_suites::default_config`, which carries the recorded
//! borrowing/funding curves but different fees and a 10x margin ladder.
//! This file retains the deliberate sizing and margin changes in
//! `test_suites::deployed_config` and the deployed oracle, treasury, and
//! vault dials. It covers the borrowing kink, funding velocity ramp and its
//! cap, the 3600-second redeem cooldown, the 30% treasury cut, and the
//! 6-decimal share offset. `test_testnet_economics` separately tests the recorded 20x ladder,
//! real LP shares, XLM feed and complete book exits.
//!
//! Refer to [`test_suites::deployed_config`] for the two categories that
//! diverge from the record, the sizing limits and the leverage ladder.
//!
//! Expected values come from independent math derived in the comment above
//! each assert, never from contract output. Prices anchor at BTC $100k with a
//! 7-dec settlement token and an 18-dec feed, so `tokens = notional * 1e18 /
//! price` puts a $25,000 notional at 2.5e6 base-dec.

use soroban_sdk::token::TokenClient;
use soroban_sdk::Address;

use market::{MarketError, OrderKind, VaultOrderKind};
use test_suites::constants::{BTC_PRICE, EXEC_FEE};
use test_suites::setup::create_deployed_fixture_with_data;
use test_suites::test_fixture::{
    TestFixture, DEPLOYED_CLOSE_STALENESS, DEPLOYED_SPREAD_REDUCTION, DEPLOYED_TRADE_STALENESS,
    DEPLOYED_TREASURY_RATE,
};
use test_suites::{deployed_config, SCALAR_18, SCALAR_7};

// ---- anchors ----

/// The whale's size, large enough to carry long-side reserve utilization past
/// the `target_util` kink against the fixture's $10M vault (token-dec).
const WHALE_NOTIONAL: i128 = 3_400_000 * SCALAR_7;

/// The whale's posted margin (token-dec). The 10% impact-fee ceiling charges
/// $340,000 on entry and the maintenance gate prices a second one on the
/// modelled close, so the whale posts far above the initial margin.
const WHALE_MARGIN: i128 = 1_000_000 * SCALAR_7;

/// The tracked trader's size, the record's `max_position_notional`
/// (token-dec).
const TRADER_NOTIONAL: i128 = 25_000 * SCALAR_7;

/// The tracked trader's posted margin (token-dec): 5% of the notional, the
/// initial margin the record deploys, at 20x.
const TRADER_MARGIN: i128 = 1_250 * SCALAR_7;

/// The accrual window each jump advances (seconds).
const WINDOW: u64 = 3_600;

// ---- helpers ----

/// The reserve utilization the borrowing curve reads for the long side
/// (`SCALAR_18`): `ceil(reserved / floor(floor(vault / 2) * max_util_open))`,
/// where a long's reserve is its base size marked at the ask.
///
/// It mirrors `market::engine::borrowing::side_utilization`, so a test can
/// state where on the curve it sits instead of asserting a rate alone.
fn long_utilization(fixture: &TestFixture) -> i128 {
    let data = fixture.market.get_market_data();
    let reserved = (data.tokens.long * BTC_PRICE + SCALAR_18 - 1) / SCALAR_18;
    let capacity = (fixture.vault.total_assets() / 2) * deployed_config().max_util_open / SCALAR_18;
    (reserved * SCALAR_18 + capacity - 1) / capacity
}

/// Creates a redeem order and returns its id. The fill waits for the
/// `redeem_lock` cooldown, so the caller owns the clock.
fn create_redeem(fixture: &TestFixture, lp: &Address, shares: i128) -> u32 {
    fixture
        .market
        .create_vault_order(lp, &(VaultOrderKind::Redeem as u32), &shares, &0)
}

// ---- the config the stack deploys ----

/// The factory accepts `deployed_config`, and every field this test reads
/// back matches it. The stack dials reach the oracle, the treasury, and the
/// vault.
///
/// A stored config is a config that passed `Config::check_valid` at deploy,
/// which is the only way a market exists.
#[test]
fn the_deployed_config_deploys_and_round_trips() {
    let fixture = TestFixture::create_deployed();
    let stored = fixture.market.get_config();
    let expected = deployed_config();

    assert_eq!(stored.keeper_rate, expected.keeper_rate, "keeper_rate");
    assert_eq!(stored.fee_dom, expected.fee_dom, "fee_dom");
    assert_eq!(stored.fee_non_dom, expected.fee_non_dom, "fee_non_dom");
    assert_eq!(
        stored.impact_scalar, expected.impact_scalar,
        "impact_scalar"
    );
    assert_eq!(
        stored.max_util_open, expected.max_util_open,
        "max_util_open"
    );
    assert_eq!(
        stored.max_util_withdraw, expected.max_util_withdraw,
        "max_util_withdraw"
    );
    assert_eq!(stored.init_margin, expected.init_margin, "init_margin");
    assert_eq!(
        stored.maintenance_margin, expected.maintenance_margin,
        "maintenance_margin"
    );
    assert_eq!(stored.liq_fee, expected.liq_fee, "liq_fee");
    assert_eq!(stored.target_util, expected.target_util, "target_util");
    assert_eq!(stored.borrow_rate, expected.borrow_rate, "borrow_rate");
    assert_eq!(
        stored.increased_borrow_rate, expected.increased_borrow_rate,
        "increased_borrow_rate"
    );
    assert_eq!(
        stored.funding_increase, expected.funding_increase,
        "funding_increase"
    );
    assert_eq!(stored.funding_max, expected.funding_max, "funding_max");
    assert_eq!(stored.redeem_lock, expected.redeem_lock, "redeem_lock");
    assert_eq!(stored.deposit_fee, expected.deposit_fee, "deposit_fee");
    assert_eq!(stored.redeem_fee, expected.redeem_fee, "redeem_fee");
    assert_eq!(
        stored.max_vault_balance, expected.max_vault_balance,
        "max_vault_balance"
    );

    // The margin ladder the deviation opens up still satisfies the ordering
    // rule `liq_fee < maintenance_margin < init_margin`, which is what
    // `check_valid` enforces.
    assert!(stored.liq_fee < stored.maintenance_margin, "ladder rung 1");
    assert!(
        stored.maintenance_margin < stored.init_margin,
        "ladder rung 2"
    );
    // 1 / init_margin is the maximum leverage: 1e18 / 1e16 = 100x.
    assert_eq!(SCALAR_18 / stored.init_margin, 100, "100x max leverage");

    assert_eq!(
        fixture.oracle.trade_staleness(),
        DEPLOYED_TRADE_STALENESS,
        "oracle trade staleness"
    );
    assert_eq!(
        fixture.oracle.close_staleness(),
        DEPLOYED_CLOSE_STALENESS,
        "oracle close staleness"
    );
    assert_eq!(
        fixture.oracle.spread_reduction_factor(),
        DEPLOYED_SPREAD_REDUCTION,
        "oracle spread reduction"
    );
    assert_eq!(
        fixture.treasury.get_rate(),
        DEPLOYED_TREASURY_RATE,
        "treasury rate"
    );
    // The vault stamps its share decimals as the asset's 7 plus its fixed 6.
    let shares = TokenClient::new(&fixture.env, &fixture.vault.address);
    assert_eq!(shares.decimals(), 13, "vault share decimals");
}

/// `deployed_config` carries the record's own value in every field except
/// the six in the sizing limits and the leverage ladder.
///
/// Every literal here is transcribed from the record's `pair-manifest.json`,
/// so a drift in either the fixture or the transcription fails. The two
/// divergent categories are written as the record's value times their
/// factor, which keeps the record visible at the point of divergence.
#[test]
fn deployed_config_diverges_from_the_record_in_six_fields() {
    let config = deployed_config();

    // The 28 fields copied exactly: the borrowing and funding curve, the ADL
    // and profit band, the fee rates, the utilization ratios, the timing
    // locks, and the dust floors.
    assert_eq!(config.keeper_rate, 100_000_000_000_000_000, "keeper_rate");
    assert_eq!(config.min_position_notional, 10_000_000, "min_pos_notional");
    assert_eq!(config.min_order_notional, 10_000_000, "min_order_notional");
    assert_eq!(config.min_order_margin, 10_000_000, "min_order_margin");
    assert_eq!(config.exec_fee, 100_000, "exec_fee");
    assert_eq!(config.fee_dom, 600_000_000_000_000, "fee_dom");
    assert_eq!(config.fee_non_dom, 400_000_000_000_000, "fee_non_dom");
    assert_eq!(config.impact_scalar, 100_000_000_000_000, "impact_scalar");
    assert_eq!(
        config.max_util_open, 800_000_000_000_000_000,
        "max_util_open"
    );
    assert_eq!(
        config.max_util_withdraw, 900_000_000_000_000_000,
        "max_util_withdraw"
    );
    assert_eq!(config.notional_lock, 30, "notional_lock");
    assert_eq!(config.target_util, 800_000_000_000_000_000, "target_util");
    assert_eq!(config.borrow_rate, 15_854_895_992, "borrow_rate");
    assert_eq!(
        config.increased_borrow_rate, 15_854_895_992,
        "increased_borrow_rate"
    );
    assert_eq!(config.funding_increase, 885_267, "funding_increase");
    assert_eq!(config.funding_decrease, 36_885, "funding_decrease");
    assert_eq!(
        config.threshold_stable_funding, 40_000_000_000_000_000,
        "threshold_stable_funding"
    );
    assert_eq!(
        config.threshold_decrease_funding, 0,
        "threshold_decrease_funding"
    );
    assert_eq!(config.funding_min, 317_097_920, "funding_min");
    assert_eq!(config.funding_max, 6_373_921_868, "funding_max");
    assert_eq!(config.adl_max_pnl, 550_000_000_000_000_000, "adl_max_pnl");
    assert_eq!(
        config.adl_clear_target, 400_000_000_000_000_000,
        "adl_clear_target"
    );
    assert_eq!(
        config.max_pnl_trader, 900_000_000_000_000_000,
        "max_pnl_trader"
    );
    assert_eq!(
        config.max_pnl_withdraw, 150_000_000_000_000_000,
        "max_pnl_withdraw"
    );
    assert_eq!(config.redeem_lock, 3_600, "redeem_lock");
    assert_eq!(config.deposit_fee, 1_000_000_000_000_000, "deposit_fee");
    assert_eq!(config.redeem_fee, 2_500_000_000_000_000, "redeem_fee");
    assert_eq!(config.min_deposit, 10_000_000, "min_deposit");

    // Category one: the sizing limits at 1000x the record, which holds the
    // record's 1 : 2 : 4 proportion between the three.
    assert_eq!(
        config.max_position_notional,
        250_000_000_000 * 1_000,
        "max_position_notional"
    );
    assert_eq!(
        config.max_open_interest,
        500_000_000_000 * 1_000,
        "max_open_interest"
    );
    assert_eq!(
        config.max_vault_balance,
        1_000_000_000_000 * 1_000,
        "max_vault_balance"
    );

    // Category two: the leverage ladder at the intended 100x maximum. One
    // divisor moves all three rungs, which holds the record's 5 : 2 : 0.5
    // proportion and the `liq_fee < maintenance_margin < init_margin` rule.
    assert_eq!(
        config.init_margin,
        50_000_000_000_000_000 / 5,
        "init_margin"
    );
    assert_eq!(
        config.maintenance_margin,
        20_000_000_000_000_000 / 5,
        "maintenance_margin"
    );
    assert_eq!(config.liq_fee, 5_000_000_000_000_000 / 5, "liq_fee");
}

// ---- the position lifecycle ----

/// A position opens, accrues over two real hours on the live borrowing and
/// funding curves, decreases in part, and closes in full, with long-side
/// reserve utilization held past the `target_util` kink throughout.
///
/// The whale carries utilization. The tracked trader is sized at the record's
/// `max_position_notional` and posts the record's 5% initial margin. Its
/// legs therefore read at deployment scale while the curves run in the region
/// the deployment runs them.
#[test]
fn position_lifecycle_accrues_past_the_borrowing_kink() {
    let mut fixture = create_deployed_fixture_with_data();
    let whale = fixture.new_user(20_000_000 * SCALAR_7);
    let trader = fixture.new_user(10_000 * SCALAR_7);
    let price = fixture.price(BTC_PRICE);

    // --- the whale lifts utilization into the kink region ---

    // tokens = floor(3.4e13 * 1e18 / 1e23) = 3.4e8 base-dec.
    // base = ceil(0.06% * 3.4e13) = 2.04e10 on the worsening side.
    // The quadratic impact would be 3.4e13^2 / 1e14 = 34% of the notional, so
    // the 10% `MAX_IMPACT_RATE` ceiling binds: impact = 3.4e12.
    // keeper = floor(10% * (2.04e10 + 3.4e12)) + exec_fee.
    let whale_payout = fixture.open(&whale, true, WHALE_NOTIONAL, WHALE_MARGIN, &price);
    assert_eq!(whale_payout, 342_040_000_000 + EXEC_FEE, "whale fee cut");
    assert_eq!(
        fixture.position(&whale, true).margin,
        6_579_600_000_000,
        "whale margin net of the entry fees"
    );

    // --- the tracked trader opens ---

    // tokens = floor(2.5e11 * 1e18 / 1e23) = 2.5e6 base-dec.
    // base = ceil(0.06% * 2.5e11) = 1.5e8. impact = 2.5e11^2 / 1e14 = 6.25e8,
    // a 0.25% rate, which is the impact the record's largest position pays.
    let payout = fixture.open(&trader, true, TRADER_NOTIONAL, TRADER_MARGIN, &price);
    assert_eq!(payout, 77_500_000 + EXEC_FEE, "trader open fee cut");

    let position = fixture.position(&trader, true);
    assert_eq!(position.tokens, 2_500_000, "position tokens (base-dec)");
    // margin = 1.25e10 - 1.5e8 - 6.25e8.
    assert_eq!(
        position.margin, 11_725_000_000,
        "margin net of the open fees"
    );
    // The vault banks 60% of each fee: 100% less the 10% keeper cut and the
    // 30% treasury cut. $10M seed + floor(60% * 3.4204e12) + 60% * 7.75e8.
    assert_eq!(
        fixture.vault.total_assets(),
        102_052_705_000_000,
        "vault banks the fee remainder of both opens"
    );

    // reserved = ceil(3.425e8 base-dec * 1e23 / 1e18) = 3.425e13 token-dec.
    // capacity = floor(floor(1.02052705e14 / 2) * 80%) = 4.0821082e13.
    // util = ceil(3.425e13 * 1e18 / 4.0821082e13).
    let utilization = long_utilization(&fixture);
    assert_eq!(utilization, 839_027_245_774_622_045, "long reserve util");
    assert!(
        utilization > deployed_config().target_util,
        "the book sits past the kink"
    );

    // --- window one: the curves run for an hour ---

    fixture.jump(WINDOW);
    let data = fixture.market.accrue(&fixture.price(BTC_PRICE));

    // The kink is flat, `increased_borrow_rate == borrow_rate`, so the rate
    // past the kink stays `ceil(borrow_rate * util)`:
    // ceil(15_854_895_992 * 8.39027245774622045e17 / 1e18) = 13_302_689_717
    // per second, and the index advances by rate * 3600.
    assert_eq!(
        data.borrowing_idx.long, 47_889_682_981_200,
        "long borrowing index after one hour"
    );
    // Only the dominant side pays. The shorts are empty.
    assert_eq!(data.borrowing_idx.short, 0, "short borrowing index");

    // A one-sided book is full skew, so the velocity is the full
    // `funding_increase`: rate = 885_267 * 3600, under the 6_373_921_868 cap.
    assert_eq!(
        data.funding_rate, 3_186_961_200,
        "funding rate after an hour"
    );
    // The charged magnitude clears `funding_min`, so the index advances by
    // rate * 3600.
    assert_eq!(
        data.funding_idx.long, 11_473_060_320_000,
        "long funding index after one hour"
    );
    // The receiving side is empty, so nothing is redistributed.
    assert_eq!(data.funding_idx.short, 0, "short funding index");

    // --- a partial decrease in the same ledger ---

    let price = fixture.price(BTC_PRICE);
    // funding = ceil(2.5e11 * 11_473_060_320_000 / 1e18) = 2_868_266.
    // borrowing = ceil(2.5e11 * 47_889_682_981_200 / 1e18) = 11_972_421.
    // closed tokens = floor(2.5e6 * 1e11 / 2.5e11) = 1e6.
    // base = ceil(0.04% * 1e11) = 4e7 on the improving side. impact = 1e22 /
    // 1e14 = 1e8. keeper = floor(10% * (4e7 + 1e8)) + exec_fee.
    let payout = fixture.close(&trader, true, 10_000 * SCALAR_7, 200 * SCALAR_7, &price);
    assert_eq!(payout, 14_000_000 + EXEC_FEE, "decrease fee cut");

    let position = fixture.position(&trader, true);
    assert_eq!(position.notional, 15_000 * SCALAR_7, "surviving notional");
    assert_eq!(position.tokens, 1_500_000, "surviving tokens (base-dec)");
    // The withdrawal claims last, so the margin pays the 1.54840687e8 of fees
    // and then the $200: 11_725_000_000 - 154_840_687 - 2_000_000_000.
    assert_eq!(
        position.margin, 9_570_159_313,
        "margin after fees and payout"
    );
    // The funding the trader paid banks to the pool. With no shorts to
    // receive it, it stays there as surplus.
    assert_eq!(
        fixture.market.get_market_data().credit_pool,
        2_868_266,
        "paid funding banks to the credit pool"
    );

    // --- window two: the funding rate reaches its cap ---

    fixture.jump(WINDOW);
    let data = fixture.market.accrue(&fixture.price(BTC_PRICE));

    // reserved falls to 3.415e13 as the trader's tokens shrink, so
    // util = 836_576_773_897_941_245 and rate = ceil(borrow_rate * util) =
    // 13_263_837_740. The index adds rate * 3600 to the first window's.
    assert_eq!(
        data.borrowing_idx.long, 95_639_498_845_200,
        "long borrowing index after two hours"
    );
    // A second full-skew hour would ramp the rate to 6_373_922_400, so the
    // 6_373_921_868 cap binds. Two hours of one-sided skew is all it takes.
    assert_eq!(
        data.funding_rate,
        deployed_config().funding_max,
        "funding rate clamps at funding_max"
    );
    // 11_473_060_320_000 + 6_373_921_868 * 3600.
    assert_eq!(
        data.funding_idx.long, 34_419_179_044_800,
        "long funding index after two hours"
    );

    // --- the full close ---

    let price = fixture.price(BTC_PRICE);
    // funding = ceil(1.5e11 * 22_946_118_724_800 / 1e18) = 3_441_918.
    // borrowing = ceil(1.5e11 * 47_749_815_864_000 / 1e18) = 7_162_473.
    // base = ceil(0.04% * 1.5e11) = 6e7. impact = 2.25e22 / 1e14 = 2.25e8.
    // The mark never moved, so the PnL is zero and the payout is the settled
    // equity: 9_570_159_313 - 6e7 - 2.25e8 - 7_162_473 - 3_441_918.
    let payout = fixture.close(&trader, true, 15_000 * SCALAR_7, 0, &price);
    assert_eq!(payout, 28_500_000 + EXEC_FEE, "close fee cut");

    let position = fixture.position(&trader, true);
    assert_eq!(position.notional, 0, "the position closed");
    assert_eq!(position.margin, 0, "no margin survives a close");
    assert_eq!(position.tokens, 0, "no size survives a close");

    // The trader posted $1,250 and got back $200 plus the settled equity, and
    // earned every keeper cut back by filling its own orders. The three
    // exec_fees round-trip.
    assert_eq!(
        fixture.balance(&trader),
        98_894_554_922,
        "trader balance after the round trip"
    );
    // Both windows' funding sits in the pool: 2_868_266 + 3_441_918.
    assert_eq!(
        fixture.market.get_market_data().credit_pool,
        6_310_184,
        "the pool holds both windows of paid funding"
    );
    // The treasury takes 30% of every trade fee and of the borrowing fee,
    // gross of the keeper's cut.
    assert_eq!(
        fixture.balance(&fixture.treasury.address),
        1_026_485_740_467,
        "treasury cut of every fee"
    );
    // The vault banks the fee remainders and funds no PnL.
    assert_eq!(
        fixture.vault.total_assets(),
        102_052_973_394_427,
        "vault balance after the lifecycle"
    );
    // The whale still holds its position, so its accrued funding and
    // borrowing stay in the indices and bank only when it acts.
    assert_eq!(
        fixture.position(&whale, true).notional,
        WHALE_NOTIONAL,
        "the whale is untouched"
    );
}

// ---- vault liquidity ----

/// A deposit mints shares at the deployed 0.1% fill fee and the 6-decimal
/// share offset, and a redeem pays out only after the 3600-second cooldown.
#[test]
fn vault_deposit_and_redeem_wait_out_the_deployed_cooldown() {
    let mut fixture = create_deployed_fixture_with_data();
    let lp = fixture.new_user(1_000_000 * SCALAR_7);

    // fee = floor(0.1% * 5e12) = 5e9, so 4.995e12 of assets reach the vault.
    // The vault holds a $10M donation and no shares, so the mint prices
    // against the virtual shares alone: shares = floor(4.995e12 * 1e6 /
    // (1e14 + 1)) = 49_949. One share is worth $10 at this seeding.
    let deposit = 500_000 * SCALAR_7;
    let attempt = fixture.vault_deposit(&lp, &lp, deposit, 0, BTC_PRICE);

    assert!(attempt.filled, "the deposit fills a ledger after creation");
    // keeper = floor(10% * 5e9) + exec_fee.
    assert_eq!(attempt.payout, 500_000_000 + EXEC_FEE, "deposit fee cut");
    assert_eq!(fixture.share_balance(&lp), 49_949, "minted shares");
    // 1e14 + 4.995e12 + floor(60% * 5e9).
    assert_eq!(
        fixture.vault.total_assets(),
        104_998_000_000_000,
        "vault banks the net deposit and the fee remainder"
    );

    // --- the cooldown ---

    let shares = fixture.share_balance(&lp);
    let id = create_redeem(&fixture, &lp, shares);
    // Five seconds short of the 3600-second cooldown.
    fixture.jump(3_595);
    let early = fixture
        .market
        .try_execute_vault_order(&lp, &lp, &id, &fixture.price(BTC_PRICE));
    assert_eq!(
        early,
        Err(Ok(MarketError::VaultOrderLocked.into())),
        "a redeem inside the cooldown is locked"
    );
    // The order survives its trapped fill with the shares still escrowed.
    assert_eq!(fixture.share_balance(&lp), 0, "shares stay escrowed");

    fixture.jump(10);
    // assets = floor(49_949 * (1.04998e14 + 1) / (49_949 + 1e6)) =
    // 4_995_047_475_639. fee = floor(0.25% * that) = 12_487_618_689.
    // keeper = floor(10% * fee) + exec_fee.
    let payout = fixture
        .market
        .execute_vault_order(&lp, &lp, &id, &fixture.price(BTC_PRICE));
    assert_eq!(payout, 1_248_761_868 + EXEC_FEE, "redeem fee cut");
    assert_eq!(fixture.share_balance(&lp), 0, "every share burns");
    // The LP paid a $500 deposit fee and a $1,248 redeem fee, and took back
    // its keeper cuts.
    assert_eq!(
        fixture.balance(&lp),
        9_984_308_618_818,
        "LP balance after the round trip"
    );
    // The vault keeps the fee minus the separately floored keeper and
    // treasury cuts: 12_487_618_689 - 1_248_761_868 - 3_746_285_606
    // = 7_492_571_215, two atoms more than floor(60% * fee).
    // 1.04998e14 - 4_995_047_475_639 + 7_492_571_215.
    assert_eq!(
        fixture.vault.total_assets(),
        100_010_445_095_576,
        "vault keeps the redeem fee remainder"
    );
}

// ---- the utilization gates ----

/// An open past `max_util_open` of half the vault is rejected, at the
/// deployed 80% ratio.
#[test]
fn an_open_past_max_util_open_is_rejected() {
    let mut fixture = create_deployed_fixture_with_data();
    let whale = fixture.new_user(20_000_000 * SCALAR_7);
    let price = fixture.price(BTC_PRICE);

    // The fill banks its own fees first, so the gate reads a 1.0259548e14
    // vault: capacity = floor(floor(1.0259548e14 / 2) * 80%) =
    // 4.1038192e13, under the 4.3e13 this open reserves. The margin clears
    // the initial and maintenance gates, which run before the reserve gate.
    let attempt = fixture.create_and_try_fill(
        &whale,
        &whale,
        true,
        OrderKind::MarketIncrease as u32,
        4_300_000 * SCALAR_7,
        2_200_000 * SCALAR_7,
        0,
        0,
        fixture.far_expiration(),
        &price,
    );

    assert!(!attempt.filled, "the open does not fill");
    assert_eq!(
        attempt.error,
        MarketError::UtilizationExceeded as u32,
        "the reserve gate rejects it"
    );
}

/// A redeem that would leave the open reserve under-backed traps, at the
/// deployed 90% withdrawal ratio.
///
/// The vault holds LP shares only, with no donation, so a redeem can move the
/// balance far enough for the gate to bind.
#[test]
fn a_redeem_past_max_util_withdraw_is_rejected() {
    let mut fixture = TestFixture::create_deployed();
    let lp = fixture.new_user(11_000_000 * SCALAR_7);
    let whale = fixture.new_user(20_000_000 * SCALAR_7);

    // The vault is empty, so the mint prices at the virtual shares: shares =
    // 9.99e13 assets * 1e6. The vault banks 9.996e13 with the fee remainder.
    let deposit = 10_000_000 * SCALAR_7;
    let attempt = fixture.vault_deposit(&lp, &lp, deposit, 0, BTC_PRICE);
    assert!(attempt.filled, "the seed deposit fills");
    assert_eq!(
        fixture.vault.total_assets(),
        99_960_000_000_000,
        "vault balance after the seed deposit"
    );

    let price = fixture.price(BTC_PRICE);
    fixture.open(&whale, true, WHALE_NOTIONAL, WHALE_MARGIN, &price);
    // reserved = 3.4e13 against a capacity of floor(floor(1.0201224e14 / 2) *
    // 80%) = 4.0804896e13, which is 83.3% utilized and past the kink.
    assert!(
        long_utilization(&fixture) > deployed_config().target_util,
        "the whale holds the book past the kink"
    );

    // Half the shares redeem for 51_006_119_999_999, which would leave
    // 51_006_120_000_001 in the vault. The redeem fee's 76_509_180_001 vault
    // leg banks back first, so the gate reads 51_082_629_180_002. The
    // withdrawal capacity there is floor(floor(that / 2) * 90%) =
    // 22_987_183_131_000, far under the 3.4e13 the whale reserves.
    let id = create_redeem(&fixture, &lp, fixture.share_balance(&lp) / 2);
    fixture.jump(3_605);
    let blocked = fixture
        .market
        .try_execute_vault_order(&lp, &lp, &id, &fixture.price(BTC_PRICE));
    assert_eq!(
        blocked,
        Err(Ok(MarketError::UtilizationExceeded.into())),
        "the withdrawal gate rejects the redeem"
    );
}
