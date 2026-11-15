//! Overflow boundaries: where the `i128` limits sit in LP share pricing, and
//! how a call fails past them. Contracts run as optimized WASM with
//! `overflow-checks`, so an overflow traps the call and never wraps.
//!
//! - The pending borrowing sums `notional * borrowing_idx` over both sides.
//!   At `MAX_OPEN_INTEREST` on both sides it fits until the summed index
//!   reaches about 170, the lifetime budget `MAX_OPEN_INTEREST` documents.
//! - The share supply. Pricing adds the `10^6` virtual shares to it, so the
//!   vault refuses a mint that would lift it above `i128::MAX - 10^6`. At par
//!   a mint is the deposit times `10^6`, so the bound sits just under a
//!   deposit of `i128::MAX / 10^6` units. A lower share price mints more
//!   shares per unit and moves the bound to a proportionally smaller deposit.
//!
//! Collateral balances held by contracts are `i128` in a Stellar asset
//! contract, so these tests fund their users with amounts no real asset's
//! supply reaches.

use market::engine::constants::{MAX_BORROW_RATE, MAX_OPEN_INTEREST, MAX_UTIL};
use market::Config;
use soroban_sdk::testutils::Ledger;
use soroban_sdk::{contractclient, Env};
use strategy_vault::strategy::StrategyVaultError;
use test_suites::carry::borrowing_index;
use test_suites::constants::{BTC_PRICE, EXEC_FEE, PRICE_SCALAR};
use test_suites::default_config;
use test_suites::test_fixture::{TestFixture, TREASURY_RATE};
use test_suites::{SCALAR_18, SCALAR_7};

#[contractclient(name = "SupplyClient")]
pub trait Supply {
    fn total_supply(e: Env) -> i128;
}

/// The vault's virtual shares, `10^DECIMALS_OFFSET`.
const VIRTUAL_SHARES: i128 = 1_000_000;

/// The largest first deposit (token-dec) whose par mint, `assets * 10^6`,
/// fits `i128`. Its mint would leave the supply 105_727 shares below
/// `i128::MAX`, inside the virtual shares.
const PAR_DEPOSIT_LIMIT: i128 = i128::MAX / VIRTUAL_SHARES;

/// Advances the clock by `seconds` and the sequence by one ledger. Entry
/// lifetimes count ledgers, so years pass on the clock without archiving the
/// deployed stack.
fn advance_clock(f: &TestFixture, seconds: u64) {
    f.env.ledger().with_mut(|ledger| {
        ledger.timestamp += seconds;
        ledger.sequence_number += 1;
    });
}

// ---- pending borrowing at the open-interest bound ----

/// A book as large as the bounds allow:
///
/// - positions and open interest at `MAX_OPEN_INTEREST`,
/// - a `MAX_UTIL` ceiling, so a vault of a fifth of the open interest backs
///   each side,
/// - the borrowing curve at `MAX_BORROW_RATE`,
/// - no vault fees, a one-unit impact fee and no funding.
fn max_book_config() -> Config {
    Config {
        max_position_notional: MAX_OPEN_INTEREST,
        max_open_interest: MAX_OPEN_INTEREST,
        max_util_open: MAX_UTIL,
        max_util_withdraw: MAX_UTIL,
        impact_scalar: i128::MAX,
        borrow_rate: MAX_BORROW_RATE,
        increased_borrow_rate: MAX_BORROW_RATE,
        funding_increase: 0,
        funding_decrease: 0,
        threshold_stable_funding: 0,
        funding_min: 0,
        funding_max: 0,
        deposit_fee: 0,
        redeem_fee: 0,
        max_vault_balance: i128::MAX,
        ..default_config()
    }
}

/// Both sides hold `MAX_OPEN_INTEREST` in equal base tokens, so both accrue,
/// each near the maximum rate. A deposit prices the pending borrowing until
/// the summed index passes 170. Past it the deposit traps on the overflowing
/// sum and changes nothing, while the keeper's accrual and each side's own
/// product still fit.
///
/// The positions opened at index 0, so the weights are 0 and the pending
/// borrowing is `MAX_OPEN_INTEREST * (idx_long + idx_short) / 10^18`, the
/// summed index in units.
#[test]
fn pending_borrowing_prices_at_the_lifetime_bound_and_traps_past_it() {
    let config = max_book_config();
    let mut f = TestFixture::create_with(config.clone());
    let one = PRICE_SCALAR;

    // A $2e10 vault: `MAX_UTIL` of each half is exactly the open interest.
    let vault = MAX_OPEN_INTEREST / 5;
    let lp = f.new_user(vault + EXEC_FEE);
    assert!(f.vault_deposit(&f.keeper, &lp, vault, 0, one).filled);

    let margin = MAX_OPEN_INTEREST / 5;
    let long = f.new_user(margin + EXEC_FEE);
    let short = f.new_user(margin + EXEC_FEE);
    let price = f.price(one);
    f.open(&long, true, MAX_OPEN_INTEREST, margin, &price);
    f.open(&short, false, MAX_OPEN_INTEREST, margin, &price);
    let data = f.market.get_market_data();
    assert_eq!(
        (data.notional.long, data.notional.short),
        (MAX_OPEN_INTEREST, MAX_OPEN_INTEREST)
    );
    assert_eq!(
        data.tokens.long, data.tokens.short,
        "a token tie: both sides accrue"
    );

    // Each side accrues at the same per-second rate against the vault.
    let per_second = borrowing_index(&config, MAX_OPEN_INTEREST, f.vault.total_assets(), 1);
    let t1 = 85 * SCALAR_18 / per_second;
    advance_clock(&f, t1 as u64);
    let data = f.market.accrue(&f.price(one));
    assert_eq!(data.borrowing_idx.long, per_second * t1);
    assert_eq!(data.borrowing_idx.short, per_second * t1);

    // At the bound: the deposit fill (five seconds later) prices it all. The
    // vault's share is the pending borrowing net of the treasury cut,
    // rounded up for a deposit.
    let depositor = f.new_user(2 * (1_000 * SCALAR_7 + EXEC_FEE));
    let at_bound = f.vault_deposit(&f.keeper, &depositor, 1_000 * SCALAR_7, 0, one);
    assert!(at_bound.filled, "the deposit prices the summed index");
    let summed = 2 * per_second * (t1 + 5);
    assert!(summed <= 170 * SCALAR_18 + 10 * per_second);
    let income = (summed * (SCALAR_18 - TREASURY_RATE) + SCALAR_18 - 1) / SCALAR_18;
    assert_eq!(f.last_vault_fill_net_pnl(), -income);

    // Past the bound: the sum overflows once each side's index passes
    // `i128::MAX / (2 * MAX_OPEN_INTEREST)`, about 85.07. At 85.1 per side
    // the fill traps and the vault is untouched.
    let index = per_second * (t1 + 5);
    advance_clock(&f, ((851 * SCALAR_18 / 10 - index) / per_second) as u64);
    let assets = f.vault.total_assets();
    let supply = SupplyClient::new(&f.env, &f.vault.address).total_supply();
    let past_bound = f.vault_deposit(&f.keeper, &depositor, 1_000 * SCALAR_7, 0, one);
    assert!(!past_bound.filled, "the overflowing sum traps the fill");
    assert_eq!(
        past_bound.error,
        u32::MAX,
        "a host trap, not a contract error"
    );
    assert_eq!(f.vault.total_assets(), assets);
    assert_eq!(
        SupplyClient::new(&f.env, &f.vault.address).total_supply(),
        supply
    );

    // The keeper's accrual never sums the sides, and each side's own product
    // still fits. Only the sum is past the limit.
    let data = f.market.accrue(&f.price(one));
    let long_product = MAX_OPEN_INTEREST.checked_mul(data.borrowing_idx.long);
    let short_product = MAX_OPEN_INTEREST.checked_mul(data.borrowing_idx.short);
    assert!(long_product.is_some() && short_product.is_some());
    assert!(long_product
        .unwrap()
        .checked_add(short_product.unwrap())
        .is_none());
}

// ---- the share supply ----

/// No vault fees and an unbounded vault, so a first deposit mints exactly
/// its assets times the virtual shares.
fn share_limit_config() -> Config {
    Config {
        deposit_fee: 0,
        redeem_fee: 0,
        max_vault_balance: i128::MAX,
        ..default_config()
    }
}

/// One unit past `PAR_DEPOSIT_LIMIT` gives a share count past `i128::MAX`,
/// so the fill traps in the share maths and the order rests with nothing
/// minted.
#[test]
fn a_deposit_whose_par_mint_overflows_i128_traps() {
    let mut f = TestFixture::create_with(share_limit_config());
    let lp = f.new_user(PAR_DEPOSIT_LIMIT + 1 + EXEC_FEE);
    let supply = SupplyClient::new(&f.env, &f.vault.address);

    let over = f.vault_deposit(&f.keeper, &lp, PAR_DEPOSIT_LIMIT + 1, 0, BTC_PRICE);
    assert!(!over.filled, "the share count overflows");
    assert_eq!(over.error, u32::MAX, "a host trap, not a contract error");
    assert_eq!(supply.total_supply(), 0);
    assert_eq!(f.vault.total_assets(), 0);
    assert!(
        f.market.try_get_vault_order(&lp, &over.id).is_ok(),
        "the order rests"
    );
}

/// A deposit of `PAR_DEPOSIT_LIMIT` would leave the supply 105_727 shares
/// below `i128::MAX`, inside the virtual shares, so the vault refuses it. One
/// unit less fills and leaves the largest par supply under the bound. The
/// vault keeps pricing at that edge: a further `min_deposit` is refused, a
/// redeem of the shares it would mint fills, and the same deposit then fits.
#[test]
fn the_vault_refuses_a_mint_into_the_virtual_shares_and_stays_live() {
    let config = share_limit_config();
    let mut f = TestFixture::create_with(config.clone());
    let lp = f.new_user(2 * PAR_DEPOSIT_LIMIT + 2 * config.min_deposit + 5 * EXEC_FEE);
    let supply = SupplyClient::new(&f.env, &f.vault.address);
    let refused = StrategyVaultError::SupplyLimitExceeded as u32;

    let at_limit = f.vault_deposit(&f.keeper, &lp, PAR_DEPOSIT_LIMIT, 0, BTC_PRICE);
    assert!(!at_limit.filled);
    assert_eq!(at_limit.error, refused);
    assert_eq!(supply.total_supply(), 0);
    assert_eq!(f.vault.total_assets(), 0);

    let under = f.vault_deposit(&f.keeper, &lp, PAR_DEPOSIT_LIMIT - 1, 0, BTC_PRICE);
    assert!(under.filled);
    let edge = i128::MAX - 105_727 - VIRTUAL_SHARES;
    assert_eq!(supply.total_supply(), edge);

    let more = f.vault_deposit(&f.keeper, &lp, config.min_deposit, 0, BTC_PRICE);
    assert!(!more.filled, "its shares exceed the 105_727 left");
    assert_eq!(more.error, refused);

    let room = f.vault_redeem(
        &f.keeper,
        &lp,
        config.min_deposit * VIRTUAL_SHARES,
        0,
        BTC_PRICE,
    );
    assert!(room.filled, "the conversions still fit");
    let refill = f.vault_deposit(&f.keeper, &lp, config.min_deposit, 0, BTC_PRICE);
    assert!(refill.filled);
    assert_eq!(supply.total_supply(), edge);
}
