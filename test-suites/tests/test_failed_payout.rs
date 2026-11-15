//! Integration coverage for the failed-payout fallback in keeper-driven
//! settlements: a trader payout whose direct transfer fails is parked as a
//! claimable credit instead of aborting the fill. The trader collects it
//! through `claim_credit` once able to receive again.
//!
//! The settlement asset is a Stellar Asset Contract, so
//! `set_authorized(user, false)` fails the incoming transfer exactly as a
//! deauthorized trustline does on-chain. Every expected amount is derived
//! from the config in the per-test comments, never from contract output.

use soroban_sdk::token::TokenClient;
use soroban_sdk::Address;

use market::{OrderKind, Status};
use test_suites::carry::{lone_side_carry, Carry};
use test_suites::constants::BTC_PRICE;
use test_suites::setup::{create_fixture_with_data, DEFAULT_VAULT_LIQUIDITY};
use test_suites::test_fixture::TestFixture;
use test_suites::{default_config, SCALAR_18, SCALAR_7};

/// Notional opened by every position under test: `$1,000` (token-dec).
const NOTIONAL: i128 = 1_000 * SCALAR_7;
/// Margin posted by the 5x positions: `$200` (token-dec).
const MARGIN: i128 = 200 * SCALAR_7;
/// The full BTC price at 18 decimals ($100k) used for opens and flat closes.
const PRICE_OPEN: i128 = BTC_PRICE;

/// Seconds the parked close waits, past the 30 s default notional lock.
const HOLD: i128 = 35;
/// Trade fee an open of [`NOTIONAL`] pays (token-dec): base `fee_dom` 0.5% of
/// `1e10` = `5e7` plus the size-quadratic impact `(1e10)^2 / 1e14` = `1e6`.
const OPEN_FEE: i128 = 51_000_000;
/// Trade fee a flat full close of [`NOTIONAL`] pays (token-dec): base
/// `fee_non_dom` 0.3% = `3e7` plus the same `1e6` impact.
const CLOSE_FEE: i128 = 31_000_000;
/// Keeper cut of the close's trade fee (token-dec): `keeper_rate(10%) * 3.1e7`.
const KEEPER_TRADE_CUT: i128 = 3_100_000;

/// The carry a lone [`NOTIONAL`] long accrues over the [`HOLD`] (token-dec).
///
/// The long is the whole book, so the funding it pays has no receiver and
/// banks as pool surplus. At the flat `$100k` mark it reserves its own
/// notional, against the seeded liquidity plus the open's vault leg (the
/// trade fee less the keeper's 10% and the treasury's 5%).
fn hold_carry() -> Carry {
    let backing = DEFAULT_VAULT_LIQUIDITY + OPEN_FEE - OPEN_FEE / 10 - OPEN_FEE / 20;
    lone_side_carry(&default_config(), NOTIONAL, NOTIONAL, backing, HOLD)
}

/// Margin remaining after a flat full close settles its own fees and the
/// carry the hold accrued (token-dec): the open leaves `2e9 - 5.1e7`, then the
/// close debits `3.1e7` and the carry.
fn settled_margin() -> i128 {
    MARGIN - OPEN_FEE - CLOSE_FEE - hold_carry().total()
}

/// Open a 5x long, rest a full-close decrease, deauthorize the trader, and
/// fill the close with a third-party keeper, so the trader payout parks.
/// Returns the trader and its wallet balance at fill time (token-dec).
fn park_close_payout(fixture: &mut TestFixture) -> (Address, i128) {
    let user = fixture.new_user(1_000 * SCALAR_7);
    let open_price = fixture.price(PRICE_OPEN);
    fixture.open(&user, true, NOTIONAL, MARGIN, &open_price);

    let id = fixture.submit_order(
        &user,
        true,
        OrderKind::MarketDecrease,
        NOTIONAL,
        0,
        0,
        0,
        fixture.far_expiration(),
    );
    // Past the 30 s default notional lock, so the full close is fillable.
    fixture.jump(HOLD as u64);
    fixture.token.set_authorized(&user, &false);
    let wallet = fixture.balance(&user);

    let price = fixture.price(PRICE_OPEN);
    fixture.execute_order(&fixture.keeper, &user, id, &price);
    (user, wallet)
}

/// A close fill whose trader payout cannot be received completes anyway and
/// parks the payout: the claimable balance rises by the settled remainder,
/// owed and pool bank it together, and no token allowance is granted.
#[test]
fn close_fill_to_deauthorized_receiver_parks_the_payout() {
    let mut fixture = create_fixture_with_data();
    let data_before = fixture.market.get_market_data();
    let (user, wallet) = park_close_payout(&mut fixture);

    // The flat close realizes no PnL, so the parked payout is the settled
    // margin.
    let parked = settled_margin();
    // The lone long's funding had no receiver, so it banks into the pool on
    // the same close: the pool rises by both, the owed ledger only by the
    // parked payout.
    let surplus = hold_carry().funding;
    assert_eq!(
        fixture.market.get_claimable_credit(&user),
        parked,
        "the failed payout is credited claimable",
    );
    let data = fixture.market.get_market_data();
    assert_eq!(
        data.credit_owed - data_before.credit_owed,
        parked,
        "owed rose by the parked amount",
    );
    assert_eq!(
        data.credit_pool - data_before.credit_pool,
        parked + surplus,
        "pool rose by the parked amount and the funding surplus",
    );
    // The fill completed: the position row is cleared.
    assert_eq!(fixture.position(&user, true).notional, 0, "position closed");
    assert_eq!(fixture.balance(&user), wallet, "the wallet is untouched");
    // The parked tokens stayed with the contract, and the failure path
    // granted no pull allowance.
    let token = TokenClient::new(&fixture.env, &fixture.token_address);
    assert_eq!(
        token.allowance(&fixture.market.address, &user),
        0,
        "no token allowance on the failure path",
    );
    assert_eq!(
        fixture.balance(&fixture.market.address),
        parked + surplus,
        "the contract retains the parked payout and the funding surplus",
    );
}

/// Re-authorized, the trader claims the parked payout in full: the wallet
/// receives it, the claimable entry clears, and the pool ledger drains on
/// both sides.
#[test]
fn reauthorized_trader_claims_the_parked_payout() {
    let mut fixture = create_fixture_with_data();
    let (user, _) = park_close_payout(&mut fixture);

    fixture.token.set_authorized(&user, &true);
    let before = fixture.balance(&user);
    let paid = fixture.market.claim_credit(&user);

    let parked = settled_margin();
    let surplus = hold_carry().funding;
    assert_eq!(paid, parked, "the full parked payout is claimed");
    assert_eq!(
        fixture.balance(&user) - before,
        parked,
        "the tokens landed in the trader's wallet",
    );
    assert_eq!(
        fixture.market.get_claimable_credit(&user),
        0,
        "the claimable entry is cleared",
    );
    let data = fixture.market.get_market_data();
    // The claim drains what was owed. The funding surplus is owed to nobody,
    // so it stays in the pool for the retirement sweep.
    assert_eq!(data.credit_pool, surplus, "the pool drained to the surplus");
    assert_eq!(data.credit_owed, 0, "nothing remains owed");
    assert_eq!(
        fixture.balance(&fixture.market.address),
        surplus,
        "the contract paid the parked tokens out and kept the surplus",
    );
}

/// A liquidation whose net remainder cannot be received completes, pays the
/// keeper normally, and parks the remainder as a claimable credit.
#[test]
fn liquidation_to_deauthorized_receiver_parks_the_remainder() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(1_000 * SCALAR_7);
    let open_price = fixture.price(PRICE_OPEN);
    fixture.open(&user, true, NOTIONAL, MARGIN, &open_price);

    fixture.token.set_authorized(&user, &false);
    let wallet = fixture.balance(&user);

    // pnl = floor(tokens 1e5 * bid / 1e18) - notional
    //     = 8_400_000_000 - 10_000_000_000 = -1_600_000_000, so equity =
    // 1_918_000_000 - 1_600_000_000 = 318_000_000, above the 1e8 fee cap: the
    // full liquidation fee is charged and 218_000_000 remains for the trader.
    let remainder = 218_000_000_i128;
    let price = fixture.price_usd(84_000);
    let reward = fixture
        .market
        .execute_liquidation(&fixture.keeper, &user, &true, &price);

    // The keeper is paid its trade-fee cut plus 10% of the 1e8 liquidation fee.
    assert_eq!(reward, KEEPER_TRADE_CUT + 10_000_000, "keeper reward");
    assert_eq!(fixture.balance(&user), wallet, "the wallet is untouched");
    assert_eq!(
        fixture.market.get_claimable_credit(&user),
        remainder,
        "the net remainder is parked claimable",
    );
    let data = fixture.market.get_market_data();
    assert_eq!(data.credit_owed, remainder, "owed banks the remainder");
    assert_eq!(data.credit_pool, remainder, "pool banks the remainder");
    assert_eq!(
        fixture.position(&user, true).notional,
        0,
        "position cleared"
    );
}

/// A parked payout leaves the retirement sweep unchanged: owed and pool rise
/// together, so the swept surplus equals the funding surplus accrued before
/// the parking. The contract retains exactly the owed credit.
#[test]
fn retirement_sweep_is_unchanged_by_a_parked_payout() {
    // Funding velocity on (acceleration 1e6/s^2, hold band 2%-5% skew, cap
    // 1e11) with a lone long, so the paid funding has no receiver and the
    // pool accrues a pure surplus.
    let mut config = default_config();
    config.funding_increase = 1_000_000;
    config.funding_decrease = 500_000;
    config.threshold_stable_funding = 5 * SCALAR_18 / 100;
    config.threshold_decrease_funding = 2 * SCALAR_18 / 100;
    config.funding_max = 100_000_000_000;
    let mut fixture = TestFixture::create_with(config.clone());
    fixture.seed_vault(DEFAULT_VAULT_LIQUIDITY);

    let user = fixture.new_user(1_000 * SCALAR_7);
    let open_price = fixture.price(PRICE_OPEN);
    // 2x leverage, so the hour of carry stays well inside the margin.
    let margin = 500 * SCALAR_7;
    fixture.open(&user, true, NOTIONAL, margin, &open_price);
    let id = fixture.submit_order(
        &user,
        true,
        OrderKind::MarketDecrease,
        NOTIONAL,
        0,
        0,
        0,
        fixture.far_expiration(),
    );

    fixture.jump(3_600);
    fixture.token.set_authorized(&user, &false);
    let price = fixture.price(PRICE_OPEN);
    fixture.execute_order(&fixture.keeper, &user, id, &price);

    // The lone long paid an hour of carry. Funding has no receiver, so its
    // charge is the pool surplus: the rate ramps to 1e6 * skew(1) * 3600 =
    // 3.6e9/s, the index advances 3.6e9 * 3600 = 1.296e13, and the charge is
    // ceil(1e10 * 1.296e13 / 1e18) = 129_600. The borrowing leg settles to
    // the vault instead, against the seeded liquidity plus the open's vault
    // leg, with the lone long reserving its own notional at the flat mark.
    let backing = DEFAULT_VAULT_LIQUIDITY + OPEN_FEE - OPEN_FEE / 10 - OPEN_FEE / 20;
    let carry = lone_side_carry(&config, NOTIONAL, NOTIONAL, backing, 3_600);
    let surplus = carry.funding;
    assert_eq!(surplus, 129_600, "the hour of funding at skew 1");
    // The close parks the margin (5e9 - 5.1e7 open fees) less the 3.1e7 close
    // fees and the carry.
    let parked = margin - OPEN_FEE - CLOSE_FEE - carry.total();
    let data = fixture.market.get_market_data();
    assert_eq!(
        fixture.market.get_claimable_credit(&user),
        parked,
        "the payout is parked claimable",
    );
    assert_eq!(data.credit_owed, parked, "owed equals the parked credit");
    assert_eq!(
        data.credit_pool,
        parked + surplus,
        "pool holds the credit plus the funding surplus",
    );
    assert_eq!(
        data.credit_pool - data.credit_owed,
        surplus,
        "the surplus is unchanged by the parking",
    );

    // Retirement sweeps exactly the pre-parking surplus and leaves the pool
    // ledger backing the owed credit.
    let vault_before = fixture.vault.total_assets();
    fixture.market.set_status(&(Status::Retired as u32));
    assert_eq!(
        fixture.vault.total_assets() - vault_before,
        surplus,
        "the sweep moves only the funding surplus",
    );
    let data = fixture.market.get_market_data();
    assert_eq!(data.credit_pool, parked, "pool ledger backs the credit");
    assert_eq!(
        fixture.balance(&fixture.market.address),
        parked,
        "the contract holds exactly the credit",
    );

    // The parked credit survives retirement and pays in full.
    fixture.token.set_authorized(&user, &true);
    assert_eq!(
        fixture.market.claim_credit(&user),
        parked,
        "the claim pays after retirement",
    );
}
