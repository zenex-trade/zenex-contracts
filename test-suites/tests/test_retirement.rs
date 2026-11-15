//! Integration coverage for the delist-to-retirement wind-down lifecycle.
//!
//! The plain Active/OnIce/Frozen status matrix lives in `test_admin`. This suite
//! starts at the delist and follows the wind-down through the grace window, the
//! delist deadline, the flat terminal price, the forced wind-down close, and the
//! final retirement clear-out (the surplus sweep, the deposit-escrow and
//! claimable-funding carve-outs, and the Retired-market entry policy).
//!
//! Every expected amount is derived from the default config by hand. The
//! derivations live in the per-test comments.
//!
//! Shared anchors (default config, 7-dec token, BTC feed at 18 decimals):
//! - Position under test: notional `$1,000` on `$200` margin (5x), tokens `1e5`.
//! - Every fill pays the size-quadratic impact `notional^2 / impact_scalar`
//!   = `(1e10)^2 / 1e14` = `1e6` on top of its base fee.
//! - Opening into an empty book is dominant: base `fee_dom` 0.5% = `5e7` plus
//!   the `1e6` impact, leaving margin `1_949_000_000`.
//! - A full close is improving (it shrinks the lone side): base `fee_non_dom`
//!   0.3% = `3e7` plus the `1e6` impact, so the settled margin before PnL
//!   is `1_949_000_000 - 31_000_000 = 1_918_000_000`.
//! - Keeper reward on a close: `keeper_rate` 10% of the `3.1e7` trade fee
//!   = `3_100_000`.

use soroban_sdk::Address;

use market::{MarketError, OrderKind, Status, VaultOrderKind};
use test_suites::carry::{lone_side_carry, Carry};
use test_suites::constants::{BTC_PRICE, EXEC_FEE, PRICE_SCALAR, SCALAR_7};
use test_suites::setup::{create_fixture_with_data, DEFAULT_VAULT_LIQUIDITY};
use test_suites::test_fixture::TestFixture;
use test_suites::{default_config, SCALAR_18};

/// Notional opened by the position under test: `$1,000` (token-dec).
const NOTIONAL: i128 = 1_000 * SCALAR_7;
/// Margin posted by the position under test: `$200` (token-dec), i.e. 5x.
const MARGIN: i128 = 200 * SCALAR_7;
/// The full BTC price at 18 decimals ($100k) used for opens.
const PRICE_OPEN: i128 = BTC_PRICE;

/// Margin remaining after a full close settles its own fees (token-dec):
/// open leaves `2e9 - base_dom(5e7) - impact(1e6) = 1_949_000_000`, then the
/// close debits `base_non_dom(3e7) + impact(1e6)`.
const SETTLED_MARGIN: i128 = 1_918_000_000;
/// Keeper reward on a full close (token-dec): `keeper_rate(10%) * trade_fee(3.1e7)`
const KEEPER_CLOSE_PAYOUT: i128 = 3_100_000;

/// Delist grace window (seconds), mirroring `DELIST_GRACE` in
/// `market/src/engine/constants.rs` (1 day).
const DELIST_GRACE: u64 = 86_400;
/// Delist deadline (seconds), mirroring `DELIST_DEADLINE` in
/// `market/src/engine/constants.rs` (7 days).
const DELIST_DEADLINE: u64 = 7 * 86_400;

/// A comfortable deposit that clears every dust floor: 100 tokens (7-dec).
const DEPOSIT_AMOUNT: i128 = 100 * SCALAR_7;

/// Trade fee the open pays (token-dec): base `fee_dom` 0.5% of `1e10` = `5e7`
/// plus the size-quadratic impact `1e6`.
const OPEN_FEE: i128 = 51_000_000;

/// The carry the position under test owes over the whole delist deadline at a
/// `price_usd` settlement mark (token-dec).
///
/// The lone long reserves its ask-marked value, `1e5 tokens * price_usd` at
/// the 18-dec feed, against the seeded liquidity plus the open's vault leg
/// (the trade fee less the keeper's 10% and the treasury's 5%). The book is
/// one-sided for the whole week, so the funding rate ramps at full skew and
/// saturates at `funding_max`.
fn deadline_carry(price_usd: i128) -> Carry {
    lone_side_carry(
        &default_config(),
        NOTIONAL,
        100_000 * price_usd,
        DEFAULT_VAULT_LIQUIDITY + OPEN_FEE - OPEN_FEE / 10 - OPEN_FEE / 20,
        DELIST_DEADLINE as i128,
    )
}

/// Open a fresh 5x long at $100k and return its user.
fn open_long_5x(fixture: &mut TestFixture) -> Address {
    let user = fixture.new_user(1_000 * SCALAR_7);
    let open_price = fixture.price(PRICE_OPEN);
    fixture.open(&user, true, NOTIONAL, MARGIN, &open_price);
    user
}

// --- delist grace window ---

/// Inside the grace window a delist is still revertible: reverting to `Active`
/// clears the wind-down and reopens the market.
#[test]
fn delist_within_grace_window_reverts_to_active() {
    let fixture = create_fixture_with_data();

    fixture.market.set_status(&(Status::Delisted as u32));
    assert_eq!(fixture.market.get_status(), Status::Delisted as u32);

    // Advance to just short of the grace deadline, still inside the window.
    fixture.jump(DELIST_GRACE - 5);
    fixture.market.set_status(&(Status::Active as u32));

    assert_eq!(fixture.market.get_status(), Status::Active as u32);
    // Reverting clears the wind-down anchor, so the market is fully live again.
    assert_eq!(
        fixture.market.get_retirement(),
        None,
        "delist anchor cleared"
    );
}

/// Once the grace window has elapsed the delist is permanent: a revert to
/// `Active` rejects with `InvalidStatus`.
#[test]
fn delist_after_grace_window_rejects_revert_to_active() {
    let fixture = create_fixture_with_data();

    fixture.market.set_status(&(Status::Delisted as u32));

    // At exactly `delisted_at + DELIST_GRACE` the trading statuses are gone for good.
    fixture.jump(DELIST_GRACE);
    let result = fixture.market.try_set_status(&(Status::Active as u32));

    assert_eq!(result, Err(Ok(MarketError::InvalidStatus.into())));
    // The rejected call traps: the market is still delisted.
    assert_eq!(fixture.market.get_status(), Status::Delisted as u32);
}

// --- terminal (flat settlement) price ---

/// Within the delist grace window the flat settlement price cannot be set:
/// while the delist is still revertible the price feed stays authoritative.
#[test]
fn set_terminal_price_within_grace_rejects_invalid_status() {
    let fixture = create_fixture_with_data();

    fixture.market.set_status(&(Status::Delisted as u32));

    // No jump: the grace window is still open, so the set is refused.
    let result = fixture
        .market
        .try_set_terminal_price(&(90_000 * PRICE_SCALAR));

    assert_eq!(result, Err(Ok(MarketError::InvalidStatus.into())));
}

/// Once the grace window expires the flat settlement price lands and is
/// repeatable: with the price feed dead, the owner keeps the stored value
/// tracking fair value.
#[test]
fn set_terminal_price_after_grace_lands_and_is_repeatable() {
    let fixture = create_fixture_with_data();

    let delisted_at = fixture.env.ledger().timestamp();
    fixture.market.set_status(&(Status::Delisted as u32));

    // Cross the grace window exactly, unlocking the flat settlement price.
    fixture.jump(DELIST_GRACE);

    let first = 90_000 * PRICE_SCALAR; // $90k in price_scalar units
    fixture.market.set_terminal_price(&first);
    assert_eq!(
        fixture.market.get_retirement(),
        Some((first, delisted_at)),
        "terminal price and delist anchor reported",
    );

    // A second set overwrites the first: the owner tracks fair value down.
    let second = 85_000 * PRICE_SCALAR; // $85k
    fixture.market.set_terminal_price(&second);
    assert_eq!(
        fixture.market.get_retirement(),
        Some((second, delisted_at)),
        "terminal price updated in place",
    );
}

/// With a terminal price stored, a fill settles flat at that price and ignores
/// the submitted payload entirely.
#[test]
fn terminal_price_settles_close_flat_regardless_of_payload() {
    let mut fixture = create_fixture_with_data();
    let user = open_long_5x(&mut fixture);

    // Delist and cross the deadline, then anchor the flat settlement at $90k.
    fixture.market.set_status(&(Status::Delisted as u32));
    fixture.jump(DELIST_DEADLINE);
    let terminal = 90_000 * PRICE_SCALAR; // $90k
    fixture.market.set_terminal_price(&terminal);

    // A full close carrying a $100k payload. Under the terminal price the payload
    // is never verified, so the fill prices at $90k:
    //   pnl = floor(tokens 1e5 * 9e22 / 1e18) - notional
    //       = 9_000_000_000 - 10_000_000_000 = -1_000_000_000.
    // The week of carry the position ran settles on the same close, marked at
    // the terminal $90k. returned = settled margin + pnl - carry.
    // Were the $100k payload honored, pnl would be 0 and the payout nearly
    // 1e9 larger, so the smaller figure is proof the flat terminal price drove
    // the settlement.
    let flat_pnl = -1_000_000_000_i128;
    let expected_payout = SETTLED_MARGIN + flat_pnl - deadline_carry(90_000).total();

    let close_id = fixture.submit_order(
        &user,
        true,
        OrderKind::MarketDecrease,
        NOTIONAL,
        0,
        0,
        0,
        fixture.far_expiration(),
    );
    let ignored_payload = fixture.price_usd(100_000);
    let trader_before = fixture.balance(&user);
    let keeper_before = fixture.balance(&fixture.keeper);

    let reward = fixture.execute_order(&fixture.keeper, &user, close_id, &ignored_payload);

    // The keeper collects its fee cut plus the exec_fee escrowed at order creation.
    assert_eq!(
        reward,
        KEEPER_CLOSE_PAYOUT + EXEC_FEE,
        "keeper reward on the flat close"
    );
    assert_eq!(
        fixture.balance(&user) - trader_before,
        expected_payout,
        "trader payout settled at the terminal $90k, not the $100k payload",
    );
    assert_eq!(
        fixture.balance(&fixture.keeper) - keeper_before,
        KEEPER_CLOSE_PAYOUT + EXEC_FEE,
        "keeper credited",
    );
    assert_eq!(fixture.position(&user, true).notional, 0, "position closed");
    fixture.assert_borrowing_books(&fixture.users);
}

// --- wind-down forced close ---

/// Past the delist deadline the wind-down waiver force-closes a healthy
/// position and returns the net remainder to the trader. It still charges the
/// liquidation fee (the notice window was the free exit).
#[test]
fn wind_down_force_close_charges_fee_and_returns_net_remainder() {
    let mut fixture = create_fixture_with_data();
    let user = open_long_5x(&mut fixture);

    // Delist and cross the deadline. No terminal price is set, so the
    // submitted live price is verified. Closing at the $100k mark leaves
    // pnl = 0, so equity is the 1_918_000_000 settled margin less the week of
    // carry, far above the 5e8 maintenance margin: healthy. The wind-down
    // waiver closes it anyway, charging the full 1e8 liquidation fee out of
    // the remainder.
    fixture.market.set_status(&(Status::Delisted as u32));
    fixture.jump(DELIST_DEADLINE);

    let trader_before = fixture.balance(&user);
    let keeper_before = fixture.balance(&fixture.keeper);
    let vault_before = fixture.vault.total_assets();

    let price = fixture.price_usd(100_000);
    let reward = fixture
        .market
        .execute_liquidation(&fixture.keeper, &user, &true, &price);

    // keeper_rate 10% of the 3.1e7 close fee plus 10% of the 1e8 liquidation
    // fee.
    assert_eq!(reward, KEEPER_CLOSE_PAYOUT + 10_000_000, "keeper reward");
    // The trader receives the settled remainder net of the 1e8 liquidation fee
    // and the week of carry, marked at the live $100k.
    let carry = deadline_carry(100_000);
    assert_eq!(
        fixture.balance(&user) - trader_before,
        SETTLED_MARGIN - 100_000_000 - carry.total(),
        "net remainder returned to the trader",
    );
    assert_eq!(
        fixture.balance(&fixture.keeper) - keeper_before,
        KEEPER_CLOSE_PAYOUT + 10_000_000,
        "keeper credited",
    );
    // No PnL: the vault banks the close-fee remainder after the keeper
    // (3.1e6) and treasury (1.55e6) cuts, the liquidation fee net of the
    // keeper (1e7) and treasury (5e6) cuts = 85_000_000, and the borrowing
    // fee net of the treasury's 5% (the keeper takes no cut of borrowing).
    // The funding leg banks into the market's credit pool instead.
    assert_eq!(
        fixture.vault.total_assets() - vault_before,
        111_350_000 + carry.borrowing - carry.borrowing / 20,
        "vault banks the fee remainders and the borrowing fee",
    );
    assert_eq!(
        fixture.position(&user, true).notional,
        0,
        "position cleared"
    );
    fixture.assert_borrowing_books(&fixture.users);
}

// --- wind-down still accepts liquidity ---

/// A delisted market keeps accepting deposits: the vault must keep taking
/// liquidity to fund the remaining profit payouts.
#[test]
fn deposits_allowed_while_delisted() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(200 * SCALAR_7);

    fixture.market.set_status(&(Status::Delisted as u32));

    let id = fixture.market.create_vault_order(
        &user,
        &(VaultOrderKind::Deposit as u32),
        &DEPOSIT_AMOUNT,
        &0,
    );

    assert_eq!(id, 1, "deposit order created on a delisted market");
    assert_eq!(
        fixture.balance(&fixture.market.address),
        DEPOSIT_AMOUNT + EXEC_FEE,
        "market contract holds the deposit and exec_fee escrow",
    );
}

/// A delisted market halts opens: a size-growing `Increase` rejects with
/// `IncreaseHalted` at fill even though the order rests fine.
#[test]
fn market_increase_blocked_while_delisted() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(1_000 * SCALAR_7);

    fixture.market.set_status(&(Status::Delisted as u32));

    // The order creation is price-free and allowed on a delisted market. The halt
    // fires at the keeper fill.
    let id = fixture.submit_order(
        &user,
        true,
        OrderKind::MarketIncrease,
        NOTIONAL,
        MARGIN,
        0,
        0,
        fixture.far_expiration(),
    );
    let price = fixture.price(PRICE_OPEN);
    let result = fixture
        .market
        .try_execute_order(&fixture.keeper, &user, &id, &price);

    assert_eq!(result, Err(Ok(MarketError::IncreaseHalted.into())));
    // The rejected fill traps: no position opened.
    assert_eq!(fixture.position(&user, true).notional, 0);
}

// --- retirement clear-out ---

/// Retirement requires an empty book: an open position blocks the transition
/// with `MarketNotCleared`.
#[test]
fn retire_with_open_book_rejects_market_not_cleared() {
    let mut fixture = create_fixture_with_data();
    let _user = open_long_5x(&mut fixture);

    fixture.market.set_status(&(Status::Delisted as u32));
    fixture.jump(DELIST_DEADLINE);

    let result = fixture.market.try_set_status(&(Status::Retired as u32));

    assert_eq!(result, Err(Ok(MarketError::MarketNotCleared.into())));
    // The rejected transition traps: still delisted, not retired.
    assert_eq!(fixture.market.get_status(), Status::Delisted as u32);
}

/// Retirement sweeps only the credit-pool surplus: with no position ever
/// opened nothing accrues, so the vault receives nothing, a raw balance
/// surplus stays with the contract, and an escrowed deposit order survives and
/// remains refundable.
#[test]
fn retire_sweeps_only_the_credit_pool_surplus() {
    let mut fixture = create_fixture_with_data();
    let depositor = fixture.new_user(200 * SCALAR_7);

    // A pending (unfilled) deposit escrows DEPOSIT_AMOUNT in the contract. The
    // sweep must leave it behind.
    let deposit_id = fixture.market.create_vault_order(
        &depositor,
        &(VaultOrderKind::Deposit as u32),
        &DEPOSIT_AMOUNT,
        &0,
    );

    // A raw balance surplus minted straight to the contract: the sweep reads
    // the credit-pool ledger, not the balance, so this stays put.
    let surplus = 500 * SCALAR_7;
    fixture.mint(&fixture.market.address, surplus);

    fixture.market.set_status(&(Status::Delisted as u32));

    let contract_before = fixture.balance(&fixture.market.address);
    let vault_before = fixture.vault.total_assets();
    assert_eq!(
        contract_before,
        surplus + DEPOSIT_AMOUNT + EXEC_FEE,
        "balance = surplus + escrow",
    );
    // No position was ever opened, so no funding accrued: the pool ledger is
    // empty.
    assert_eq!(
        fixture.market.get_market_data().credit_pool,
        0,
        "no funding pool"
    );

    fixture.market.set_status(&(Status::Retired as u32));

    assert_eq!(
        fixture.vault.total_assets(),
        vault_before,
        "vault receives nothing without a pool surplus",
    );
    assert_eq!(
        fixture.balance(&fixture.market.address),
        contract_before,
        "the contract balance is untouched",
    );

    // The escrow survives retirement and can be cancelled back to the
    // depositor. The raw surplus stays with the contract.
    // The return value reports the primary escrow. The transfer also refunds
    // the exec_fee, which the contract-balance assert below accounts for.
    let refunded = fixture.market.cancel_vault_order(&depositor, &deposit_id);
    assert_eq!(
        refunded, DEPOSIT_AMOUNT,
        "escrow refundable after retirement"
    );
    assert_eq!(
        fixture.balance(&fixture.market.address),
        surplus,
        "only the unswept surplus remains once the escrow is returned",
    );
}

/// Retirement preserves a claimable funding balance: the sweep carves out
/// `credit_owed`, and the earning trader can still claim its full balance
/// after the market is retired.
#[test]
fn retire_preserves_claimable_funding_through_the_sweep() {
    // A funding config that ramps hard on any skew so an imbalanced book accrues
    // a claimable funding balance to the minority side within an hour.
    let mut config = default_config();
    let max_funding_rate = 10 * SCALAR_18 / 31_536_000; // 1000% APR, per second
    config.funding_increase = max_funding_rate;
    config.funding_max = max_funding_rate;
    config.threshold_stable_funding = 0; // any skew ramps
    config.threshold_decrease_funding = 0;
    config.funding_min = 0;
    config.funding_decrease = 0;
    let mut fixture = TestFixture::create_with(config);
    fixture.seed_vault(10_000_000 * SCALAR_7);

    // A long-dominant book (long tokens 2e5 > short tokens 1e5): longs pay,
    // shorts earn. Both are low-leverage so the funding charge stays well within
    // the paying long's margin.
    let long_user = fixture.new_user(3_000 * SCALAR_7);
    let short_user = fixture.new_user(3_000 * SCALAR_7);
    let open_price = fixture.price(PRICE_OPEN);
    fixture.open(
        &long_user,
        true,
        2_000 * SCALAR_7,
        1_000 * SCALAR_7,
        &open_price,
    );
    fixture.open(
        &short_user,
        false,
        1_000 * SCALAR_7,
        1_000 * SCALAR_7,
        &open_price,
    );

    // Accrue funding over an hour with the book imbalanced, then close both sides
    // so the book is empty for retirement. The short's earned funding persists as
    // a claimable balance backed by the internal pool.
    fixture.jump(3_600);
    fixture.market.accrue(&fixture.price(PRICE_OPEN));
    let close_price = fixture.price(PRICE_OPEN);
    fixture.assert_borrowing_books(&fixture.users);
    fixture.close(&short_user, false, 1_000 * SCALAR_7, 0, &close_price);
    fixture.close(&long_user, true, 2_000 * SCALAR_7, 0, &close_price);
    fixture.assert_borrowing_books(&fixture.users);

    let owed = fixture.market.get_claimable_credit(&short_user);
    assert!(owed > 0, "the short earned a claimable funding balance");
    assert_eq!(
        fixture.market.get_market_data().credit_owed,
        owed,
        "the sole earner accounts for all funding owed",
    );

    // A deposit-escrow carve-out rides along to show it survives the sweep.
    let depositor = fixture.new_user(200 * SCALAR_7);
    fixture.market.create_vault_order(
        &depositor,
        &(VaultOrderKind::Deposit as u32),
        &DEPOSIT_AMOUNT,
        &0,
    );

    let pool_before = fixture.market.get_market_data().credit_pool;
    let vault_before = fixture.vault.total_assets();

    fixture.market.set_status(&(Status::Retired as u32));

    // The sweep moves the pool surplus (paid minus owed funding) to the vault.
    // Afterwards the pool ledger exactly backs the owed funding.
    assert_eq!(
        fixture.vault.total_assets() - vault_before,
        pool_before - owed,
        "vault receives the funding-pool surplus",
    );
    assert_eq!(
        fixture.market.get_market_data().credit_pool,
        owed,
        "pool ledger backs exactly the owed funding",
    );
    assert_eq!(
        fixture.balance(&fixture.market.address),
        owed + DEPOSIT_AMOUNT + EXEC_FEE,
        "contract retains the owed funding plus the deposit escrow",
    );

    // The carved-out funding is genuinely payable: the earner claims its full
    // balance after retirement (claims stay live on a Retired market).
    let short_before = fixture.balance(&short_user);
    let claimed = fixture.market.claim_credit(&short_user);
    assert_eq!(claimed, owed, "the full owed funding is claimed");
    assert_eq!(
        fixture.balance(&short_user) - short_before,
        owed,
        "the earner is paid its owed funding",
    );
    assert_eq!(
        fixture.balance(&fixture.market.address),
        DEPOSIT_AMOUNT + EXEC_FEE,
        "only the deposit escrow remains after the claim",
    );
}

// --- retired-market entry policy ---

/// A retired market ends every trading and deposit path but keeps redemptions
/// and claims live: `create_order` and fills reject `MarketFrozen`, a deposit
/// rejects `InvalidStatus`, and a redeem executes instantly with id `0`.
#[test]
fn retired_market_rejects_trading_and_deposits_but_allows_instant_redeem() {
    // An unseeded vault so the seed deposit fits under `max_vault_balance` (the
    // default liquidity donation would already sit above the cap).
    let mut fixture = TestFixture::create();
    let user = fixture.new_user(2_000 * SCALAR_7);
    let price = fixture.price(PRICE_OPEN);

    // Seed the user with vault shares via a deposit fill while still live.
    let deposit = fixture.vault_deposit(&fixture.keeper, &user, DEPOSIT_AMOUNT, 0, PRICE_OPEN);
    assert!(deposit.filled, "seed deposit fills immediately");
    let shares = fixture.vault.balance(&user);
    assert!(shares > 0, "deposit fill minted shares");

    // A resting order created while live, to attempt filling after retirement.
    let resting = fixture.submit_order(
        &user,
        true,
        OrderKind::MarketIncrease,
        NOTIONAL,
        MARGIN,
        0,
        0,
        fixture.far_expiration(),
    );

    // Retire directly from a cleared (position-free) book.
    fixture.market.set_status(&(Status::Retired as u32));

    // Trading paths are dead: creating an order and filling one both reject with
    // MarketFrozen (704).
    assert_eq!(
        fixture.market.try_create_order(
            &user,
            &true,
            &(OrderKind::MarketIncrease as u32),
            &NOTIONAL,
            &MARGIN,
            &0,
            &0,
            &fixture.far_expiration(),
        ),
        Err(Ok(MarketError::MarketFrozen.into())),
        "create_order dead on a retired market",
    );
    assert_eq!(
        fixture
            .market
            .try_execute_order(&fixture.keeper, &user, &resting, &price),
        Err(Ok(MarketError::MarketFrozen.into())),
        "fills dead on a retired market",
    );

    // A deposit is rejected outright (there is no future fill to honor it).
    assert_eq!(
        fixture.market.try_create_vault_order(
            &user,
            &(VaultOrderKind::Deposit as u32),
            &DEPOSIT_AMOUNT,
            &0
        ),
        Err(Ok(MarketError::InvalidStatus.into())),
        "deposits rejected on a retired market",
    );

    // A redeem forwards straight to the vault: it executes instantly, returns the
    // reserved id 0, burns the shares, and pays the assets out.
    let user_before = fixture.balance(&user);
    let redeem_id =
        fixture
            .market
            .create_vault_order(&user, &(VaultOrderKind::Redeem as u32), &shares, &0);
    assert_eq!(redeem_id, 0, "instant redeem returns the reserved id 0");
    assert_eq!(fixture.vault.balance(&user), 0, "shares burned");
    assert!(
        fixture.balance(&user) > user_before,
        "assets paid straight out on the instant redeem",
    );
}

// --- retirement reporting ---

/// `get_retirement` reports `None` until the first delist, then the wind-down
/// anchor with a zero terminal price until a flat price is set.
#[test]
fn get_retirement_reports_none_then_delist_anchor() {
    let fixture = create_fixture_with_data();

    // A live market has no wind-down anchor.
    assert_eq!(
        fixture.market.get_retirement(),
        None,
        "no anchor before delist"
    );

    let delisted_at = fixture.env.ledger().timestamp();
    fixture.market.set_status(&(Status::Delisted as u32));

    // Delisting anchors the window. The terminal price reads 0 until one is set.
    assert_eq!(
        fixture.market.get_retirement(),
        Some((0, delisted_at)),
        "delist anchors the window with a zero terminal price",
    );
}
