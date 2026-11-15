//! Share pricing at the vault-order boundary: `lp_net_pnl`'s netted
//! mark-to-market is INTENDED behavior, matching GMX V2's GM pricing
//! (`MarketUtils.getPoolValueInfo`: signed net PnL subtracted, trader losses
//! credit the pool uncapped, only trader profit is capped).
//!
//! What the netted mark means, pinned here:
//!
//! - A net-losing book prices a redeem at `balance + pending losses`. The
//!   losses are backed by margin escrowed in the market contract. Whether
//!   they realize as marked depends on the trader's close timing, and both
//!   the exiting and the staying LP hold the same lottery either way.
//! - A balanced book (profits == losses) with no pending borrowing nets to
//!   zero and prices redeems at the token balance: entry equals exit.
//! - The vault's share of the borrowing that open positions owe but have not
//!   paid counts as backing, as GMX's pending borrowing fees do. It counts in
//!   full even where a position's margin cannot cover it.
//! - Pending losses floor at posted margin, not at the vault balance, so a
//!   large-margin loser can price the backing above the vault's tokens.
//!   Pending borrowing does the same for a full exit. A payout beyond the
//!   vault balance cannot land: the vault traps it with `PayoutExceedsAssets`
//!   before its token transfer, as GMX's `applyDeltaToPoolAmount` guard does.
//!   A sized partial exit correctly realizes the marked share value.
//!
//! Config anchors (`test_suites::default_config`): BTC $100k, 7-dec token,
//! `max_util_open` 80%, `max_util_withdraw` 90%, `max_pnl_withdraw` 15%,
//! `max_pnl_trader` 90% (all per side on half the vault), maintenance 5%,
//! 30s notional lock, the deployed borrowing and funding curves, 10 bps vault
//! fill fees.

use soroban_sdk::Address;

use market::engine::constants::MAX_BORROW_RATE;
use market::Config;
use strategy_vault::strategy::StrategyVaultError;
use test_suites::carry::borrowing_index;
use test_suites::constants::{BTC_PRICE, EXEC_FEE, PRICE_SCALAR};
use test_suites::default_config;
use test_suites::test_fixture::{TestFixture, TREASURY_RATE};
use test_suites::{SCALAR_18, SCALAR_7};

/// Marked BTC prices in `price_scalar` units (dollars * 1e18).
const P80K: i128 = 80_000 * PRICE_SCALAR;
const P100K: i128 = 100_000 * PRICE_SCALAR;
const P130K: i128 = 130_000 * PRICE_SCALAR;
const P190K: i128 = 190_000 * PRICE_SCALAR;
const P450K: i128 = 450_000 * PRICE_SCALAR;

const PAYOUT_EXCEEDS_ASSETS: u32 = StrategyVaultError::PayoutExceedsAssets as u32;

/// Makes two equal LP deposits at the $100k anchor and returns their share
/// balances.
fn seed_two_lps(fixture: &TestFixture, a: &Address, b: &Address, amount: i128) -> (i128, i128) {
    let first = fixture.vault_deposit(&fixture.keeper, a, amount, 0, BTC_PRICE);
    assert!(first.filled, "LP A's seed deposit fills");
    let second = fixture.vault_deposit(&fixture.keeper, b, amount, 0, BTC_PRICE);
    assert!(second.filled, "LP B's seed deposit fills");
    (fixture.share_balance(a), fixture.share_balance(b))
}

/// `shares`' pro-rata claim on the vault's actual token balance.
fn pro_rata(fixture: &TestFixture, shares: i128, supply: i128) -> i128 {
    fixture.vault.total_assets() * shares / supply
}

// ---- the net-losing book: exits price the mark, not the settlement ----

/// A short is $13.5k underwater (margin posted, loss unrealized). LP A's
/// full redeem prices against `balance + 13.5k`. That is ~$6.75k above its
/// pro-rata share of the tokens held. The short then recovers and closes
/// flat, so LP B exits ~$13.5k under A. The fair-value mark pays each exit
/// the expected value at its own instant. The later price path decides the
/// realized split, exactly as it would have had both LPs stayed.
#[test]
fn redeem_on_a_net_losing_book_pays_out_losses_never_collected() {
    let mut fixture = TestFixture::create();
    let lp_a = fixture.new_user(51_000 * SCALAR_7);
    let lp_b = fixture.new_user(51_000 * SCALAR_7);
    let trader = fixture.new_user(16_000 * SCALAR_7);

    let (shares_a, shares_b) = seed_two_lps(&fixture, &lp_a, &lp_b, 50_000 * SCALAR_7);
    let supply = shares_a + shares_b;

    // A 1x short: $15k notional on $15k margin at $100k. At $190k the marked
    // loss is $13.5k. Equity ~$1.4k stays above the 5% maintenance line, so
    // no keeper can force the loss to realize.
    let open_price = fixture.price(P100K);
    fixture.open(
        &trader,
        false,
        15_000 * SCALAR_7,
        15_000 * SCALAR_7,
        &open_price,
    );

    fixture.jump(5);
    let fair_a = pro_rata(&fixture, shares_a, supply);
    let wallet_a_before = fixture.balance(&lp_a);
    let redeem_a = fixture.vault_redeem(&fixture.keeper, &lp_a, shares_a, 0, P190K);
    assert!(
        redeem_a.filled,
        "the net-losing-book redeem passes every gate"
    );
    let paid_a = fixture.balance(&lp_a) - wallet_a_before;

    // A's exit prices the $13.5k marked loss as backing: roughly half of it
    // (A holds ~half the shares) on top of A's pro-rata share of the tokens.
    assert!(
        paid_a > fair_a + 6_000 * SCALAR_7,
        "A was paid {} against a {} pro-rata share of the tokens held",
        paid_a,
        fair_a,
    );

    // The short recovers to entry and closes: the loss never realizes, the
    // vault collects nothing but the close fee.
    fixture.jump(35);
    let close_price = fixture.price(P100K);
    fixture.close(&trader, false, 15_000 * SCALAR_7, 0, &close_price);
    let trader_fees_only = 16_000 * SCALAR_7 - fixture.balance(&trader);
    assert!(
        trader_fees_only < 300 * SCALAR_7,
        "the trader lost only fees ({}), never the marked $13.5k",
        trader_fees_only,
    );

    fixture.jump(5);
    let wallet_b_before = fixture.balance(&lp_b);
    let redeem_b = fixture.vault_redeem(&fixture.keeper, &lp_b, shares_b, 0, P100K);
    assert!(redeem_b.filled, "B's exit on the emptied book fills");
    let paid_b = fixture.balance(&lp_b) - wallet_b_before;

    // Identical deposits, ~$13.5k apart: the price path after A's exit
    // settled the lottery in A's favor.
    assert!(
        paid_a - paid_b > 12_000 * SCALAR_7,
        "A exited with {}, B with {}",
        paid_a,
        paid_b,
    );
}

/// The control: the same book and the same redeem by A at $190k. The short
/// closes AT the marked price, so the $13.5k does arrive. B then exits within
/// fee noise of A. Together with the test above: the netted mark is fair
/// exactly when the loss realizes as marked. The losing trader, not the
/// protocol, decides whether it does.
#[test]
fn the_same_redeem_is_fair_when_the_loss_realizes_as_marked() {
    let mut fixture = TestFixture::create();
    let lp_a = fixture.new_user(51_000 * SCALAR_7);
    let lp_b = fixture.new_user(51_000 * SCALAR_7);
    let trader = fixture.new_user(16_000 * SCALAR_7);

    let (shares_a, shares_b) = seed_two_lps(&fixture, &lp_a, &lp_b, 50_000 * SCALAR_7);

    let open_price = fixture.price(P100K);
    fixture.open(
        &trader,
        false,
        15_000 * SCALAR_7,
        15_000 * SCALAR_7,
        &open_price,
    );

    fixture.jump(5);
    let wallet_a_before = fixture.balance(&lp_a);
    let redeem_a = fixture.vault_redeem(&fixture.keeper, &lp_a, shares_a, 0, P190K);
    assert!(redeem_a.filled, "A's redeem fills at the marked loss");
    let paid_a = fixture.balance(&lp_a) - wallet_a_before;

    // The short realizes the full marked loss into the vault.
    fixture.jump(35);
    let close_price = fixture.price(P190K);
    fixture.close(&trader, false, 15_000 * SCALAR_7, 0, &close_price);

    fixture.jump(5);
    let wallet_b_before = fixture.balance(&lp_b);
    let redeem_b = fixture.vault_redeem(&fixture.keeper, &lp_b, shares_b, 0, P190K);
    assert!(redeem_b.filled, "B's exit fills after the loss landed");
    let paid_b = fixture.balance(&lp_b) - wallet_b_before;

    assert!(
        (paid_a - paid_b).abs() < 300 * SCALAR_7,
        "with the loss collected the exits match: A {}, B {}",
        paid_a,
        paid_b,
    );
}

// ---- the balanced book: netting prices fair ----

/// Equal profits and losses ($1.5k each side) net to zero: a redeem prices
/// at the vault's token balance, entry equals exit, no spread. Flooring
/// either side separately would recognize the long's $1.5k as liability
/// while ignoring the short's, taxing every redeem on every balanced price
/// move.
#[test]
fn balanced_book_redeems_price_at_the_token_balance() {
    let mut fixture = TestFixture::create();
    let lp_a = fixture.new_user(51_000 * SCALAR_7);
    let lp_b = fixture.new_user(51_000 * SCALAR_7);
    let long_trader = fixture.new_user(6_000 * SCALAR_7);
    let short_trader = fixture.new_user(6_000 * SCALAR_7);

    let (shares_a, shares_b) = seed_two_lps(&fixture, &lp_a, &lp_b, 50_000 * SCALAR_7);
    let supply = shares_a + shares_b;

    // Mirrored 1x positions: $5k notional on $5k margin each side at $100k.
    // At $130k the long is +$1.5k and the short −$1.5k.
    let open_price = fixture.price(P100K);
    fixture.open(
        &long_trader,
        true,
        5_000 * SCALAR_7,
        5_000 * SCALAR_7,
        &open_price,
    );
    fixture.jump(5);
    let open_price = fixture.price(P100K);
    fixture.open(
        &short_trader,
        false,
        5_000 * SCALAR_7,
        5_000 * SCALAR_7,
        &open_price,
    );

    fixture.jump(5);
    let redeem_shares = 25_000 * SCALAR_7;
    let fair = pro_rata(&fixture, redeem_shares, supply);
    let wallet_before = fixture.balance(&lp_a);
    let redeem = fixture.vault_redeem(&fixture.keeper, &lp_a, redeem_shares, 0, P130K);
    assert!(redeem.filled, "the balanced-book redeem fills");
    let paid = fixture.balance(&lp_a) - wallet_before;

    // Paid the pro-rata token-balance claim, net of the 10 bps redeem fee.
    let fee_and_dust = fair / 1000 + SCALAR_7;
    assert!(
        paid <= fair && paid > fair - fee_and_dust,
        "paid {} against a fair {} (10 bps fee tolerance)",
        paid,
        fair,
    );
}

// ---- losses beyond the vault balance ----

/// Pending losses floor at posted margin, which can exceed the vault
/// balance. A $150k-margin short marked $122.5k underwater (equity $27k,
/// nowhere near liquidation) prices the backing at ~2.2x the vault's
/// tokens. That value is backed by margin escrowed in the market contract,
/// not yet in the vault. A payout above the vault balance cannot land: the
/// vault traps it with `PayoutExceedsAssets` before its token transfer, as
/// GMX's `applyDeltaToPoolAmount` guard does. A sized partial redeem
/// realizes the marked share value from the tokens present.
#[test]
fn losses_beyond_the_vault_balance_price_redeems_above_it() {
    let mut fixture = TestFixture::create();
    let lp_a = fixture.new_user(51_000 * SCALAR_7);
    let lp_b = fixture.new_user(51_000 * SCALAR_7);
    let trader = fixture.new_user(151_000 * SCALAR_7);

    let (shares_a, shares_b) = seed_two_lps(&fixture, &lp_a, &lp_b, 50_000 * SCALAR_7);
    let supply = shares_a + shares_b;

    // A 0.23x short: $35k notional on $150k margin at $100k. At $450k the
    // marked loss is $122.5k (within margin), against a ~$100k vault.
    let open_price = fixture.price(P100K);
    fixture.open(
        &trader,
        false,
        35_000 * SCALAR_7,
        150_000 * SCALAR_7,
        &open_price,
    );

    fixture.jump(5);
    let liq_price = fixture.price(P450K);
    assert!(
        fixture
            .market
            .try_execute_liquidation(&fixture.keeper, &trader, &false, &liq_price)
            .is_err(),
        "equity sits far above maintenance: no keeper can realize the loss",
    );

    // The full exit: priced at ~2.2x the tokens held, the payout exceeds the
    // vault balance, and the vault traps it before its token transfer.
    let vault_tokens = fixture.vault.total_assets();
    let full = fixture.vault_redeem(&fixture.keeper, &lp_a, shares_a, 0, P450K);
    assert!(!full.filled, "a payout above the vault balance cannot land");
    assert_eq!(
        full.error, PAYOUT_EXCEEDS_ASSETS,
        "the vault names the shortfall before any exit gate runs"
    );

    // The declined order rests with A's escrowed shares. Free them.
    fixture.market.cancel_vault_order(&lp_a, &full.id);

    // The sized exit: ~9% of supply (13-dec shares) stays under the
    // utilization gate and extracts ~2.2x its pro-rata share of the actual
    // tokens.
    let partial_shares = 9_000 * SCALAR_7 * 1_000_000;
    let fair = vault_tokens * partial_shares / supply;
    let wallet_before = fixture.balance(&lp_a);
    let partial = fixture.vault_redeem(&fixture.keeper, &lp_a, partial_shares, 0, P450K);
    assert!(partial.filled, "the sized exit passes every gate");
    let paid = fixture.balance(&lp_a) - wallet_before;
    assert!(
        paid > 2 * fair,
        "the sized exit took {} against a {} pro-rata claim",
        paid,
        fair,
    );
    // Silence the unused warning while documenting B holds the residual claim.
    let _ = shares_b;
}

/// Pending borrowing alone prices the last LP's full exit above the vault's
/// tokens. After 30 days the $10k long owes the vault its share of the
/// borrowing, backing the vault does not hold yet. The full exit traps with
/// `PayoutExceedsAssets`, nothing moves, and the order rests.
#[test]
fn pending_borrowing_prices_the_last_lps_full_exit_above_the_vault_tokens() {
    let mut fixture = TestFixture::create();
    let lp = fixture.new_user(100_010 * SCALAR_7);
    let trader = fixture.new_user(3_000 * SCALAR_7);
    let seed = fixture.vault_deposit(&fixture.keeper, &lp, 100_000 * SCALAR_7, 0, P100K);
    assert!(seed.filled, "the LP's seed deposit fills");
    let shares = fixture.share_balance(&lp);
    let open_price = fixture.price(P100K);
    fixture.open(
        &trader,
        true,
        10_000 * SCALAR_7,
        2_000 * SCALAR_7,
        &open_price,
    );
    fixture.jump(30 * 86_400);

    let vault_tokens = fixture.vault.total_assets();
    let full = fixture.vault_redeem(&fixture.keeper, &lp, shares, 0, P100K);
    assert!(!full.filled, "the payout exceeds the vault's tokens");
    assert_eq!(full.error, PAYOUT_EXCEEDS_ASSETS);
    assert_eq!(fixture.vault.total_assets(), vault_tokens, "nothing moved");
    assert_eq!(
        fixture.market.get_vault_order(&lp, &full.id).amount,
        shares,
        "the order rests"
    );
}

/// A small book's reserve fits in the redeem fee the fill returns to the
/// vault, so no exit gate rejects before the vault's tokens run out. A $20
/// long marked 20% down owes the vault $4. The full exit traps with
/// `PayoutExceedsAssets`, and a 99.99% exit fills.
#[test]
fn a_small_book_redeems_up_to_the_vault_tokens() {
    let mut fixture = TestFixture::create();
    let lp = fixture.new_user(100_010 * SCALAR_7);
    let trader = fixture.new_user(100 * SCALAR_7);
    let seed = fixture.vault_deposit(&fixture.keeper, &lp, 100_000 * SCALAR_7, 0, P100K);
    assert!(seed.filled, "the LP's seed deposit fills");
    let shares = fixture.share_balance(&lp);
    let open_price = fixture.price(P100K);
    fixture.open(&trader, true, 20 * SCALAR_7, 10 * SCALAR_7, &open_price);

    let full = fixture.vault_redeem(&fixture.keeper, &lp, shares, 0, P80K);
    assert_eq!(full.error, PAYOUT_EXCEEDS_ASSETS, "the full exit traps");
    fixture.market.cancel_vault_order(&lp, &full.id);

    let most = fixture.vault_redeem(&fixture.keeper, &lp, shares / 10_000 * 9_999, 0, P80K);
    assert!(most.filled, "the exit gates pass on the returned fee");
}

// ---- the mint side of the same mark ----

/// On the same net-losing book a deposit is charged for the pending
/// collections: the mint prices against `balance + loss`, so the depositor
/// gets fewer shares than the token balance alone would mint. Adverse to the
/// entrant and protective of standing LPs, the mint-side mark is already
/// pointing the right way and must keep netting.
#[test]
fn deposit_on_a_net_losing_book_charges_for_the_pending_collections() {
    let mut fixture = TestFixture::create();
    let lp_a = fixture.new_user(51_000 * SCALAR_7);
    let lp_b = fixture.new_user(51_000 * SCALAR_7);
    let lp_c = fixture.new_user(11_000 * SCALAR_7);
    let trader = fixture.new_user(16_000 * SCALAR_7);

    let (shares_a, shares_b) = seed_two_lps(&fixture, &lp_a, &lp_b, 50_000 * SCALAR_7);
    let supply = shares_a + shares_b;

    let open_price = fixture.price(P100K);
    fixture.open(
        &trader,
        false,
        15_000 * SCALAR_7,
        15_000 * SCALAR_7,
        &open_price,
    );

    fixture.jump(5);
    let deposit_assets = 10_000 * SCALAR_7;
    // Shares the token balance alone would mint (fee-net), before marking.
    let unmarked = (deposit_assets - deposit_assets / 1000) * supply / fixture.vault.total_assets();
    let deposit = fixture.vault_deposit(&fixture.keeper, &lp_c, deposit_assets, 0, P190K);
    assert!(deposit.filled, "the deposit fills on the marked book");
    let minted = fixture.share_balance(&lp_c);

    // The $13.5k mark on a ~$100k vault prices shares ~13% dearer.
    assert!(
        minted < unmarked - unmarked / 10,
        "minted {} shares against {} unmarked: the entrant pays the premium",
        minted,
        unmarked,
    );
}

// ---- pending borrowing ----

/// The vault's share of pending borrowing counts as backing in full, even
/// once a position owes more than its margin can pay. An 8x long opens at
/// 75% utilization on a curve at the maximum rate. After a week its
/// borrowing exceeds its margin, and a deposit still marks the vault's share
/// of all of it, rounded up.
#[test]
fn pending_borrowing_counts_as_backing_beyond_the_margin_that_pays_it() {
    let config = Config {
        borrow_rate: MAX_BORROW_RATE,
        increased_borrow_rate: MAX_BORROW_RATE,
        funding_increase: 0,
        funding_decrease: 0,
        threshold_stable_funding: 0,
        funding_min: 0,
        funding_max: 0,
        ..default_config()
    };
    let mut fixture = TestFixture::create_with(config.clone());
    let lp = fixture.new_user(10_000 * SCALAR_7 + EXEC_FEE);
    let seed = fixture.vault_deposit(&fixture.keeper, &lp, 10_000 * SCALAR_7, 0, BTC_PRICE);
    assert!(seed.filled);

    let notional = 3_000 * SCALAR_7;
    let trader = fixture.new_user(notional / 8 + EXEC_FEE);
    fixture.open(
        &trader,
        true,
        notional,
        notional / 8,
        &fixture.price(BTC_PRICE),
    );
    let margin = fixture.position(&trader, true).margin;

    let week = 7 * 86_400;
    fixture.jump(week);
    let vault_balance = fixture.vault.total_assets();
    let depositor = fixture.new_user(100 * SCALAR_7 + EXEC_FEE);
    let deposit = fixture.vault_deposit(&fixture.keeper, &depositor, 100 * SCALAR_7, 0, BTC_PRICE);
    assert!(deposit.filled);

    // The long marks zero PnL at its entry price. The fill accrues the week
    // plus the five seconds the order rests.
    let index = borrowing_index(&config, notional, vault_balance, week as i128 + 5);
    let pending = (notional * index + SCALAR_18 - 1) / SCALAR_18;
    let income = (pending * (SCALAR_18 - TREASURY_RATE) + SCALAR_18 - 1) / SCALAR_18;
    assert!(
        income > margin,
        "the vault's share alone exceeds the margin"
    );
    assert_eq!(fixture.last_vault_fill_net_pnl(), -income);
    assert_eq!(fixture.market.get_market_data().borrowing_idx.long, index);
}
