//! Integration coverage for vault-order FILLS against the full deployed stack:
//! keeper execution (`execute_vault_order`, always a whole-order fill), the
//! redeem-side exit PnL gate (`max_pnl_withdraw`), and the per-order `min_out`
//! slippage bound.
//!
//! Creation and cancel semantics live in `test_vault_orders`. Every fill here
//! is two-step: create the order, then fill it a ledger later (the
//! commit-then-execute price-time gate requires the fill to strictly
//! postdate the order's creation), via `TestFixture::vault_deposit` /
//! `vault_redeem` or a direct `execute_vault_order` call.
//!
//! Share price is kept at par by seeding liquidity through a real LP deposit
//! (an empty vault mints the net deposit times `10^6`, the vault's fixed
//! share offset) rather than a donation.
//! Pending-PnL episodes are produced by opening a trader position and then
//! marking it at a moved price.

use soroban_sdk::Address;

use market::{MarketError, VaultOrderKind};
use test_suites::constants::{BTC_PRICE, EXEC_FEE, PRICE_SCALAR};
use test_suites::test_fixture::TestFixture;
use test_suites::SCALAR_7;

// ---- config anchors mirrored from `test_suites::default_config` ----

/// Proportional vault-order fill fee: `deposit_fee` and `redeem_fee` are both
/// `SCALAR_18 / 1000`, so `floor(assets * rate / SCALAR_18)` collapses to
/// `assets / 1000` on either leg.
fn vault_fill_fee(assets: i128) -> i128 {
    assets / 1000
}

/// The keeper's cut of a fill fee (`keeper_rate` 10%).
fn keeper_cut(fee: i128) -> i128 {
    fee / 10
}

/// The vault's donated remainder of a fill fee after the 10% keeper and the
/// fixture treasury's 5% cuts.
fn vault_cut(fee: i128) -> i128 {
    fee - keeper_cut(fee) - fee / 20
}

/// The keeper payout for a vault order fill: the fill-fee cut plus the flat
/// `exec_fee` every vault order escrows at creation.
fn vault_order_keeper_payout(fee: i128) -> i128 {
    keeper_cut(fee) + EXEC_FEE
}

/// Marked BTC prices in `price_scalar` units (dollars * 1e18).
const PRICE_150K: i128 = 150_000 * PRICE_SCALAR; // 1.5x the $100k anchor

// Market error discriminants surfaced through the fixture's `FillAttempt.error`.
// Direct `try_execute_vault_order` rejections assert the typed `MarketError`.
const PENDING_PNL_EXCEEDED: u32 = MarketError::PendingPnlExceeded as u32;

// ---- helpers ----

/// Seed the vault with a real LP deposit so shares exist at a par mint.
/// Returns the resulting vault balance (the net deposit plus the donated
/// vault share of the fill fee). Two-step: creates, then fills a ledger later.
fn seed_vault_via_deposit(fixture: &TestFixture, lp: &Address, amount: i128) -> i128 {
    let attempt = fixture.vault_deposit(&fixture.keeper, lp, amount, 0, BTC_PRICE);
    assert!(attempt.filled, "seed deposit fills a ledger after creation");
    amount - vault_fill_fee(amount) + vault_cut(vault_fill_fee(amount))
}

// ---- deposit fill ----

/// A deposit fills a ledger after creation, mints shares at par to the LP,
/// splits the proportional fill fee, and banks the net assets plus the
/// vault's fee share.
#[test]
fn deposit_fill_mints_shares_pays_keeper_and_releases_escrow() {
    let mut fixture = TestFixture::create();
    let lp = fixture.new_user(200 * SCALAR_7);

    let amount = 100 * SCALAR_7; // 1_000_000_000 token-dec
                                 // fee = floor(1e9 / 1000) = 1_000_000. The net deposit banks the rest.
    let fee = vault_fill_fee(amount); // 1_000_000
    let net = amount - fee; // 999_000_000

    let attempt = fixture.vault_deposit(&fixture.keeper, &lp, amount, 0, BTC_PRICE);

    assert!(attempt.filled, "the deposit fills a ledger after creation");
    assert_eq!(attempt.error, 0, "no fill error");
    assert_eq!(
        attempt.payout,
        vault_order_keeper_payout(fee),
        "keeper payout is its fee cut plus exec_fee"
    );

    // First deposit into an empty vault mints shares at par with the net, at
    // the vault's six extra share decimals. The vault fee share lands after
    // as a donation.
    assert_eq!(
        fixture.share_balance(&lp),
        net * 1_000_000,
        "shares minted to the LP"
    );
    assert_eq!(
        fixture.balance(&fixture.keeper),
        vault_order_keeper_payout(fee),
        "keeper banks its cut of the fill fee plus exec_fee",
    );
    assert_eq!(
        fixture.vault.total_assets(),
        net + vault_cut(fee),
        "vault backing rose by the net plus the donated fee share",
    );
}

// ---- full deposit then redeem cycle ----

/// A deposit followed by a full redeem returns the assets net of both fill fees
/// and burns every share. `redeem_lock` is 0 by default, so the redeem fills in
/// the same flow.
#[test]
fn deposit_then_full_redeem_returns_assets_minus_fees() {
    let mut fixture = TestFixture::create();
    let lp = fixture.new_user(200 * SCALAR_7);

    let amount = 100 * SCALAR_7; // 1_000_000_000
    let deposit_fee = vault_fill_fee(amount); // 1_000_000
    let shares = (amount - deposit_fee) * 1_000_000; // 999_000_000 at par, 13-dec shares

    let deposit = fixture.vault_deposit(&fixture.keeper, &lp, amount, 0, BTC_PRICE);
    assert!(deposit.filled, "deposit fills");
    assert_eq!(fixture.share_balance(&lp), shares, "shares minted");

    // Redeem every share against the donated deposit-fee share: the vault holds
    // 999_850_000 (net + 850_000 donation) against the par shares, so
    // redeemed = floor(shares * (assets + 1) / (supply + 1_000_000)) = 999_849_999.
    let redeemed = 999_849_999;
    let redeem_fee = vault_fill_fee(redeemed); // 999_849
    let assets_back = redeemed - redeem_fee; // 998_850_150

    let redeem = fixture.vault_redeem(&fixture.keeper, &lp, shares, 0, BTC_PRICE);
    assert!(redeem.filled, "redeem fills under a zero lock");
    assert_eq!(
        redeem.payout,
        vault_order_keeper_payout(redeem_fee),
        "keeper payout is its redeem-fee cut plus exec_fee"
    );

    assert_eq!(fixture.share_balance(&lp), 0, "every share burned");
    // LP wallet: started 2_000_000_000, spent the 1_000_000_000 deposit plus
    // both orders' exec_fee, got the 998_850_150 redemption back.
    assert_eq!(
        fixture.balance(&lp),
        2 * amount - amount + assets_back - 2 * EXEC_FEE,
        "LP recovered the assets net of both fees and both orders' exec_fee",
    );
    assert_eq!(
        fixture.balance(&fixture.keeper),
        vault_order_keeper_payout(deposit_fee) + vault_order_keeper_payout(redeem_fee),
        "keeper banked its cut of both fill fees plus both exec_fees",
    );
}

// ---- withdraw PnL gate ----

/// Pending trader profit above the withdraw gate (15% of half the post-redeem
/// vault) rejects the redeem with `PendingPnlExceeded` (754). Once the trader
/// closes and realizes the profit, the resting redeem fills.
#[test]
fn withdraw_gate_rejects_pending_profit_then_fills_after_trader_closes() {
    let mut fixture = TestFixture::create();
    // A little headroom above the round deposit/margin figures for the
    // flat exec_fee each order escrows on top of its principal.
    let lp1 = fixture.new_user(10_000 * SCALAR_7 + 1_000_000);
    let trader = fixture.new_user(2_000 * SCALAR_7 + 1_000_000);

    let vault_balance = seed_vault_via_deposit(&fixture, &lp1, 10_000 * SCALAR_7);
    assert_eq!(vault_balance, 99_985_000_000, "seeded vault balance");

    // A winning long: $2,000 notional on $800, opened at $100k. At the $150k
    // close price both exit gates measure the ask-marked reserve and the
    // pending gain. A smaller notional keeps the marked reserve
    // (notional * 1.5 = 3e10) under the utilization cap while the gain
    // (notional * 0.5 = 1e10) still overhangs the narrower PnL allowance
    // (both halved per side). Utilization allows up to 0.45 * post_balance,
    // and PnL gates above 0.075 * post_balance.
    let open_price = fixture.price(BTC_PRICE);
    fixture.open(&trader, true, 20_000_000_000, 8_000_000_000, &open_price);
    let pending_gain = 10_000_000_000_i128;

    // Redeem $500 of shares: post-balance ~= 94_985_000_000. The marked
    // reserve at $150k (3e10) stays under 45% of it (~42_743_250_000), but the
    // 1e10 gain exceeds 7.5% of it (~7_123_875_000), so only the PnL gate trips.
    let redeem_assets = 500 * SCALAR_7; // 5_000_000_000
    let redeem_shares = redeem_assets * 1_000_000; // the par mint of $500
    let post_balance = vault_balance - redeem_assets;
    assert!(
        pending_gain > post_balance * 75 / 1000,
        "gain exceeds the withdraw gate"
    );

    let gated = fixture.vault_redeem(&fixture.keeper, &lp1, redeem_shares, 0, PRICE_150K);
    assert!(!gated.filled, "redeem blocked while profit is pending");
    assert_eq!(
        gated.error, PENDING_PNL_EXCEEDED,
        "blocked by the withdraw gate"
    );

    // Realize the profit: close the whole long past its 30-second notional lock.
    fixture.jump(35);
    let close_price = fixture.price(PRICE_150K);
    fixture.close(&trader, true, 20_000_000_000, 0, &close_price);

    // With no open position the gate clears and the resting redeem fills.
    let filled_price = fixture.price(PRICE_150K);
    let fee = fixture
        .market
        .execute_vault_order(&fixture.keeper, &lp1, &gated.id, &filled_price);
    assert!(
        fee > 0,
        "the resting redeem fills after the profit is realized"
    );
    assert!(
        fixture.market.try_get_vault_order(&lp1, &gated.id).is_err(),
        "the filled redeem is removed",
    );
}

// ---- per-order min_out slippage bound (deposit) ----

/// A deposit whose `min_out` exceeds the shares the fill would mint is
/// rejected on the first keeper attempt: the order is removed, the assets
/// return to the LP, the keeper earns the exec_fee and the vault mints
/// nothing. A recreated order with a bound at the minted shares fills.
#[test]
fn deposit_bound_rejects_a_min_out_above_the_minted_shares() {
    let mut fixture = TestFixture::create();
    let lp = fixture.new_user(200 * SCALAR_7);

    let amount = 100 * SCALAR_7; // 1_000_000_000
    let fee = vault_fill_fee(amount); // 1_000_000
    let minted = (amount - fee) * 1_000_000; // 999_000_000 at par, 13-dec shares

    let wallet = fixture.balance(&lp);
    let keeper_before = fixture.balance(&fixture.keeper);
    let rejected = fixture.vault_deposit(&fixture.keeper, &lp, amount, minted + 1, BTC_PRICE);
    assert!(
        rejected.rejected && !rejected.filled,
        "a bound above the minted shares rejects"
    );
    assert_eq!(rejected.payout, EXEC_FEE, "the keeper earns the exec_fee");
    assert!(
        fixture
            .market
            .try_get_vault_order(&lp, &rejected.id)
            .is_err(),
        "the order is removed"
    );
    assert_eq!(
        fixture.balance(&lp),
        wallet - EXEC_FEE,
        "the principal returns"
    );
    assert_eq!(fixture.balance(&fixture.keeper), keeper_before + EXEC_FEE);
    assert_eq!(fixture.share_balance(&lp), 0, "nothing minted");
    assert_eq!(
        fixture.balance(&fixture.vault.address),
        0,
        "nothing deposited"
    );

    let admitted = fixture.vault_deposit(&fixture.keeper, &lp, amount, minted, BTC_PRICE);
    assert!(admitted.filled, "a bound at the minted shares admits");
    assert_eq!(
        fixture.share_balance(&lp),
        minted,
        "shares minted at the bound"
    );
}

/// A depositor that cannot receive the refund, for example after dropping
/// its trustline once the escrow emptied its balance, cannot hold its order
/// open by making the rejection revert. The refund parks as claimable credit,
/// the order is removed and the keeper is paid. The depositor collects once
/// it can receive again.
#[test]
fn deposit_rejection_parks_the_refund_when_the_depositor_cannot_receive() {
    let mut fixture = TestFixture::create();
    let lp = fixture.new_user(200 * SCALAR_7);

    let amount = 100 * SCALAR_7;
    let minted = (amount - vault_fill_fee(amount)) * 1_000_000;
    let id = fixture.market.create_vault_order(
        &lp,
        &(VaultOrderKind::Deposit as u32),
        &amount,
        &(minted + 1),
    );
    fixture.token.set_authorized(&lp, &false);
    fixture.jump(5);
    let keeper_before = fixture.balance(&fixture.keeper);
    let payout =
        fixture
            .market
            .execute_vault_order(&fixture.keeper, &lp, &id, &fixture.price(BTC_PRICE));
    assert!(fixture.vault_order_rejected(&lp, id), "the rejection lands");
    assert_eq!(payout, EXEC_FEE);
    assert_eq!(fixture.balance(&fixture.keeper), keeper_before + EXEC_FEE);
    assert!(
        fixture.market.try_get_vault_order(&lp, &id).is_err(),
        "the order is removed"
    );
    assert_eq!(
        fixture.market.get_claimable_credit(&lp),
        amount,
        "the refund parks"
    );
    assert_eq!(fixture.share_balance(&lp), 0, "nothing minted");

    fixture.token.set_authorized(&lp, &true);
    let wallet = fixture.balance(&lp);
    assert_eq!(fixture.market.claim_credit(&lp), amount);
    assert_eq!(fixture.balance(&lp), wallet + amount);
    assert_eq!(fixture.market.get_claimable_credit(&lp), 0);
}

// ---- per-order min_out slippage bound (redeem) ----

/// A mature redeem whose `min_out` exceeds the assets the fill would pay is
/// rejected on the first keeper attempt: the order is removed, the shares
/// return to the LP, the keeper earns the exec_fee and nothing burns. A
/// recreated order with a bound at the paid assets fills.
#[test]
fn redeem_bound_rejects_a_min_out_above_the_paid_assets() {
    let mut fixture = TestFixture::create();
    let lp = fixture.new_user(200 * SCALAR_7);

    let amount = 100 * SCALAR_7; // 1_000_000_000
    let deposit = fixture.vault_deposit(&fixture.keeper, &lp, amount, 0, BTC_PRICE);
    assert!(deposit.filled, "seed deposit fills");
    let shares = fixture.share_balance(&lp);

    // The deposit fill itself donated a fee share to the vault, so redeeming
    // every share back prices slightly above par: redeemed = floor(shares *
    // (assets + 1) / (supply + 1_000_000)) = 999_849_999 (mirrors the
    // full-cycle test).
    let redeemed = 999_849_999;
    let redeem_fee = vault_fill_fee(redeemed);
    let assets_back = redeemed - redeem_fee;

    let vault_before = fixture.balance(&fixture.vault.address);
    let keeper_before = fixture.balance(&fixture.keeper);
    let rejected = fixture.vault_redeem(&fixture.keeper, &lp, shares, assets_back + 1, BTC_PRICE);
    assert!(
        rejected.rejected && !rejected.filled,
        "a bound above the paid assets rejects"
    );
    assert_eq!(rejected.payout, EXEC_FEE, "the keeper earns the exec_fee");
    assert!(
        fixture
            .market
            .try_get_vault_order(&lp, &rejected.id)
            .is_err(),
        "the order is removed"
    );
    assert_eq!(fixture.share_balance(&lp), shares, "the shares return");
    assert_eq!(
        fixture.balance(&fixture.vault.address),
        vault_before,
        "nothing burned"
    );
    assert_eq!(fixture.balance(&fixture.keeper), keeper_before + EXEC_FEE);

    let admitted = fixture.vault_redeem(&fixture.keeper, &lp, shares, assets_back, BTC_PRICE);
    assert!(admitted.filled, "a bound at the paid assets admits");
    assert_eq!(admitted.error, 0, "no fill error at the bound");
}

// ---- auth: the fill authorizes its own grandchild escrow pull ----

/// The deposit fill hands the escrowed assets to the vault as
/// `vault.strategy_deposit(from = market)`, so the asset pull runs as
/// `market -> vault -> token.transfer(market, vault)`. In that grandchild
/// frame invoker auto-authorization does not apply. The fill must
/// pre-authorize the transfer with `authorize_as_current_contract`. The
/// fixture mocks all auths, which hides a missing entry, so this test
/// switches the env to enforcing mode with an empty auth set.
/// `execute_vault_order` is permissionless, and every interior `require_auth`
/// must be satisfied by invoker rules plus the contract's own authorization.
#[test]
fn deposit_fill_authorizes_grandchild_escrow_pull_under_enforced_auth() {
    let mut fixture = TestFixture::create();
    let lp = fixture.new_user(200 * SCALAR_7);

    let amount = 100 * SCALAR_7;
    let id = fixture
        .market
        .create_vault_order(&lp, &(VaultOrderKind::Deposit as u32), &amount, &0);

    // A ledger later clears the commit-then-execute price-time gate.
    fixture.jump(5);
    let price = fixture.price(BTC_PRICE);

    // From here on nothing is mocked: an empty enforced auth set.
    fixture.env.set_auths(&[]);

    fixture
        .market
        .execute_vault_order(&fixture.keeper, &lp, &id, &price);
    assert_eq!(
        fixture.share_balance(&lp),
        (amount - vault_fill_fee(amount)) * 1_000_000,
        "the fill mints the net deposit at par under enforced auth",
    );
}
