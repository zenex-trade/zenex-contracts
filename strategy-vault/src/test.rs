use soroban_sdk::{
    testutils::Address as _,
    token::{StellarAssetClient, TokenClient},
    Address, Env, String,
};

use crate::{StrategyVaultContract, StrategyVaultContractClient};

const SCALAR_7: i128 = 10_000_000;
/// The share units per asset unit at par: the vault's `10^6` virtual shares.
const SHARE_SCALE: i128 = 1_000_000;

/// Register a vault; the strategy holds 100,000 tokens to deposit from,
/// mirroring the market's escrow-then-fill flow.
fn setup_test<'a>() -> (
    Env,
    StrategyVaultContractClient<'a>,
    Address,
    Address,
    Address,
) {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let token = env.register_stellar_asset_contract_v2(admin.clone());
    let user = Address::generate(&env);
    let strategy = Address::generate(&env);

    StellarAssetClient::new(&env, &token.address()).mint(&strategy, &(100_000 * SCALAR_7));

    let vault_address = env.register(
        StrategyVaultContract,
        (
            String::from_str(&env, "Vault Shares"),
            String::from_str(&env, "vTKN"),
            token.address(),
            strategy.clone(),
        ),
    );

    let vault = StrategyVaultContractClient::new(&env, &vault_address);
    (env, vault, token.address(), user, strategy)
}

// ==================== uPnL share pricing ====================
//
// `mock_all_auths()` satisfies `require_strategy` in unit tests. On-chain,
// only the registered market contract may call the mutating entry points.
// Deposits pull assets from the strategy and redeems burn strategy-held
// shares, matching the market's escrow-then-fill flow.

// The bootstrap deposit on an empty vault mints at par: shares = assets *
// (0 supply + 1e6 virtual) / (0 backing + 1) = assets * 1e6.
#[test]
fn bootstrap_deposit_mints_at_par() {
    let (_env, vault, _token, user, strategy) = setup_test();
    let assets = 1000 * SCALAR_7;

    let shares = vault.strategy_deposit(&assets, &user, &strategy, &0);

    assert_eq!(shares, assets * SHARE_SCALE);
    assert_eq!(vault.balance(&user), assets * SHARE_SCALE);
    assert_eq!(vault.total_assets(), assets);
}

// Pending trader profit halves the marked backing, so 10 tokens buy roughly
// 20 tokens of shares. Exactly: supply 1e16 and backing 1e10 after the
// bootstrap; net_pnl 5e9 leaves effective backing 5e9; shares =
// floor(1e8 * (1e16 + 1e6) / (5e9 + 1)) = 199_999_999_980_000.
#[test]
fn deposit_with_pending_profit_mints_more_shares() {
    let (_env, vault, _token, user, strategy) = setup_test();
    vault.strategy_deposit(&(1000 * SCALAR_7), &user, &strategy, &0);

    let assets = 10 * SCALAR_7;
    let net_pnl = 500 * SCALAR_7;
    assert_eq!(
        vault.preview_deposit(&assets, &net_pnl),
        199_999_999_980_000
    );

    let shares = vault.strategy_deposit(&assets, &user, &strategy, &net_pnl);
    assert_eq!(shares, 199_999_999_980_000);
}

// Pending trader loss doubles the marked backing, so 10 tokens buy 5 tokens
// of shares. Exactly: net_pnl -1e10 lifts effective backing to 2e10; shares =
// floor(1e8 * (1e16 + 1e6) / (2e10 + 1)) = 50_000_000_002_499.
#[test]
fn deposit_with_pending_loss_mints_fewer_shares() {
    let (_env, vault, _token, user, strategy) = setup_test();
    vault.strategy_deposit(&(1000 * SCALAR_7), &user, &strategy, &0);

    let assets = 10 * SCALAR_7;
    let net_pnl = -1000 * SCALAR_7;
    assert_eq!(vault.preview_deposit(&assets, &net_pnl), 50_000_000_002_499);

    let shares = vault.strategy_deposit(&assets, &user, &strategy, &net_pnl);
    assert_eq!(shares, 50_000_000_002_499);
}

// A raw-price redeem (net_pnl 0) pays back pro rata: 5e15 of the 1e16
// supply returns floor(5e15 * (1e10 + 1) / (1e16 + 1e6)) = 5e9 assets.
#[test]
fn redeem_at_raw_price_pays_pro_rata() {
    let (_env, vault, token, user, strategy) = setup_test();
    vault.strategy_deposit(&(1000 * SCALAR_7), &strategy, &strategy, &0);

    let shares = 500 * SCALAR_7 * SHARE_SCALE;
    assert_eq!(vault.preview_redeem(&shares, &0), 500 * SCALAR_7);

    let assets = vault.strategy_redeem(&shares, &user, &strategy, &0);
    assert_eq!(assets, 500 * SCALAR_7);
    assert_eq!(TokenClient::new(&vault.env, &token).balance(&user), assets);
    assert_eq!(vault.total_assets(), 500 * SCALAR_7);
}

// Pending trader profit lowers the redemption price: net_pnl 2e9 leaves
// effective backing 8e9, so 5e15 shares of the 1e16 supply pay
// floor(5e15 * (8e9 + 1) / (1e16 + 1e6)) = 4_000_000_000 assets.
#[test]
fn redeem_with_pending_profit_pays_fewer_assets() {
    let (_env, vault, _token, user, strategy) = setup_test();
    vault.strategy_deposit(&(1000 * SCALAR_7), &strategy, &strategy, &0);

    let shares = 500 * SCALAR_7 * SHARE_SCALE;
    let net_pnl = 200 * SCALAR_7;
    assert_eq!(vault.preview_redeem(&shares, &net_pnl), 4_000_000_000);

    let assets = vault.strategy_redeem(&shares, &user, &strategy, &net_pnl);
    assert_eq!(assets, 4_000_000_000);
}

// Pending trader loss raises the redemption price: net_pnl -2e9 lifts
// effective backing to 1.2e10, so 5e15 shares of the 1e16 supply pay
// floor(5e15 * (1.2e10 + 1) / (1e16 + 1e6)) = 5_999_999_999 assets.
#[test]
fn redeem_with_pending_loss_pays_more_assets() {
    let (_env, vault, _token, user, strategy) = setup_test();
    vault.strategy_deposit(&(1000 * SCALAR_7), &strategy, &strategy, &0);

    let shares = 500 * SCALAR_7 * SHARE_SCALE;
    let net_pnl = -200 * SCALAR_7;
    assert_eq!(vault.preview_redeem(&shares, &net_pnl), 5_999_999_999);

    let assets = vault.strategy_redeem(&shares, &user, &strategy, &net_pnl);
    assert_eq!(assets, 5_999_999_999);
}

// The fixed offset of 6 gives the 7-decimal asset 13-decimal shares, and
// the bootstrap quote is assets * (0 supply + 1e6 virtual) / (0 backing + 1).
#[test]
fn fixed_offset_sets_share_decimals_and_scale() {
    let (_env, vault, _token, _user, _strategy) = setup_test();

    assert_eq!(vault.decimals(), 13);
    assert_eq!(
        vault.preview_deposit(&(10 * SCALAR_7), &0),
        10 * SCALAR_7 * SHARE_SCALE
    );
}

// ==================== pricing guards ====================

// A profit mark past the asset balance leaves shareholders nothing to price
// against.
#[test]
#[should_panic(expected = "Error(Contract, #801)")] // PnlExceedsAssets
fn deposit_with_profit_beyond_assets_rejects() {
    let (_env, vault, _token, user, strategy) = setup_test();
    vault.strategy_deposit(&(1000 * SCALAR_7), &user, &strategy, &0);

    vault.strategy_deposit(&(10 * SCALAR_7), &user, &strategy, &(1000 * SCALAR_7 + 1));
}

// A profit mark equal to the asset balance still prices, on the +1 floor.
#[test]
fn deposit_with_profit_matching_assets_prices_on_the_floor() {
    let (_env, vault, _token, user, strategy) = setup_test();
    let assets = 1000 * SCALAR_7;
    vault.strategy_deposit(&assets, &user, &strategy, &0);

    // shares = floor(1e8 * (1e16 + 1e6) / (0 + 1)).
    assert_eq!(
        vault.preview_deposit(&(10 * SCALAR_7), &assets),
        100_000_000 * (10_000_000_000_000_000 + SHARE_SCALE)
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #800)")] // InvalidAmount
fn zero_deposit_rejects() {
    let (_env, vault, _token, user, strategy) = setup_test();
    vault.strategy_deposit(&0, &user, &strategy, &0);
}

#[test]
#[should_panic(expected = "Error(Contract, #800)")] // InvalidAmount
fn zero_redeem_rejects() {
    let (_env, vault, _token, user, strategy) = setup_test();
    vault.strategy_deposit(&(1000 * SCALAR_7), &strategy, &strategy, &0);
    vault.strategy_redeem(&0, &user, &strategy, &0);
}

#[test]
#[should_panic(expected = "Error(Contract, #800)")] // InvalidAmount
fn negative_preview_rejects() {
    let (_env, vault, _token, _user, _strategy) = setup_test();
    vault.preview_deposit(&-1, &0);
}

#[test]
#[should_panic(expected = "Error(Contract, #800)")] // InvalidAmount
fn negative_redeem_preview_rejects() {
    let (_env, vault, _token, _user, _strategy) = setup_test();
    vault.preview_redeem(&-1, &0);
}

// The profit-beyond-assets backstop guards the redeem path the same way.
#[test]
#[should_panic(expected = "Error(Contract, #801)")] // PnlExceedsAssets
fn redeem_with_profit_beyond_assets_rejects() {
    let (_env, vault, _token, user, strategy) = setup_test();
    vault.strategy_deposit(&(1000 * SCALAR_7), &strategy, &strategy, &0);

    vault.strategy_redeem(&(500 * SCALAR_7), &user, &strategy, &(1000 * SCALAR_7 + 1));
}

// Redeeming more shares than the owner holds fails in the share token's
// balance update.
#[test]
#[should_panic(expected = "Error(Contract, #100)")] // InsufficientBalance
fn redeem_beyond_share_balance_rejects() {
    let (_env, vault, _token, user, strategy) = setup_test();
    vault.strategy_deposit(&(1000 * SCALAR_7), &strategy, &strategy, &0);

    vault.strategy_redeem(&(1000 * SCALAR_7 * SHARE_SCALE + 1), &user, &strategy, &0);
}

// ==================== supply bound ====================

/// The largest share supply the vault admits: pricing adds the virtual
/// shares to it, and the sum must fit `i128`.
const SUPPLY_BOUND: i128 = i128::MAX - SHARE_SCALE;

/// A pending loss of `10^6 - 1` units on the empty vault prices one share per
/// unit: shares = assets * (0 + 1e6) / ((0 + 1e6 - 1) + 1).
const ONE_SHARE_PER_UNIT: i128 = -(SHARE_SCALE - 1);

// A deposit that lands the supply exactly on the bound mints, and the vault
// still prices: one share redeems for one unit.
#[test]
fn deposit_filling_the_supply_to_its_bound_mints() {
    let (env, vault, token, user, strategy) = setup_test();
    StellarAssetClient::new(&env, &token).mint(&strategy, &(SUPPLY_BOUND - 100_000 * SCALAR_7));

    let shares = vault.strategy_deposit(&SUPPLY_BOUND, &user, &strategy, &ONE_SHARE_PER_UNIT);

    assert_eq!(shares, SUPPLY_BOUND);
    assert_eq!(vault.total_supply(), SUPPLY_BOUND);
    assert_eq!(vault.preview_redeem(&1, &ONE_SHARE_PER_UNIT), 1);
}

// One share past the bound is refused before anything moves.
#[test]
#[should_panic(expected = "Error(Contract, #802)")] // SupplyLimitExceeded
fn deposit_past_the_supply_bound_rejects() {
    let (env, vault, token, user, strategy) = setup_test();
    StellarAssetClient::new(&env, &token).mint(&strategy, &(SUPPLY_BOUND + 1 - 100_000 * SCALAR_7));

    vault.strategy_deposit(&(SUPPLY_BOUND + 1), &user, &strategy, &ONE_SHARE_PER_UNIT);
}

// ==================== payout bound ====================

// A pending loss prices the backing above the tokens held. One unit of loss
// lifts the full exit to exactly the balance: the 1e16 supply pays
// floor(1e16 * (1e10 + 1 + 1) / (1e16 + 1e6)) = 1e10.
#[test]
fn full_exit_paying_exactly_the_balance_redeems() {
    let (_env, vault, token, user, strategy) = setup_test();
    vault.strategy_deposit(&(1000 * SCALAR_7), &strategy, &strategy, &0);
    let shares = vault.total_supply();

    let assets = vault.strategy_redeem(&shares, &user, &strategy, &-1);
    assert_eq!(assets, 1000 * SCALAR_7);
    assert_eq!(TokenClient::new(&vault.env, &token).balance(&user), assets);
    assert_eq!(vault.total_assets(), 0);
}

// Two units of loss price the same exit one unit past the balance. The quote
// still reports it, and the redeem is refused before anything moves.
#[test]
#[should_panic(expected = "Error(Contract, #803)")] // PayoutExceedsAssets
fn full_exit_paying_past_the_balance_rejects() {
    let (_env, vault, _token, user, strategy) = setup_test();
    vault.strategy_deposit(&(1000 * SCALAR_7), &strategy, &strategy, &0);
    let shares = vault.total_supply();
    assert_eq!(vault.preview_redeem(&shares, &-2), 1000 * SCALAR_7 + 1);

    vault.strategy_redeem(&shares, &user, &strategy, &-2);
}

// ==================== strategy gate ====================

// Without the strategy's authorization the mutating entry points abort in
// `require_strategy`.
#[test]
#[should_panic(expected = "Error(Auth, InvalidAction)")]
fn deposit_without_strategy_auth_rejects() {
    let (env, vault, _token, user, strategy) = setup_test();

    env.set_auths(&[]);
    vault.strategy_deposit(&(1000 * SCALAR_7), &user, &strategy, &0);
}

#[test]
#[should_panic(expected = "Error(Auth, InvalidAction)")]
fn redeem_without_strategy_auth_rejects() {
    let (env, vault, _token, user, strategy) = setup_test();
    vault.strategy_deposit(&(1000 * SCALAR_7), &strategy, &strategy, &0);

    env.set_auths(&[]);
    vault.strategy_redeem(&(500 * SCALAR_7), &user, &strategy, &0);
}

#[test]
#[should_panic(expected = "Error(Auth, InvalidAction)")]
fn withdraw_without_strategy_auth_rejects() {
    let (env, vault, _token, user, strategy) = setup_test();
    vault.strategy_deposit(&(1000 * SCALAR_7), &user, &strategy, &0);

    env.set_auths(&[]);
    vault.strategy_withdraw(&(100 * SCALAR_7));
}

// ==================== strategy withdraw ====================

#[test]
fn strategy_withdraw_moves_assets_to_the_strategy() {
    let (_env, vault, token, user, strategy) = setup_test();
    vault.strategy_deposit(&(10_000 * SCALAR_7), &user, &strategy, &0);
    let token_client = TokenClient::new(&vault.env, &token);
    let strategy_before = token_client.balance(&strategy);

    vault.strategy_withdraw(&(2000 * SCALAR_7));

    assert_eq!(vault.total_assets(), 8000 * SCALAR_7);
    assert_eq!(
        token_client.balance(&strategy),
        strategy_before + 2000 * SCALAR_7
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #800)")] // InvalidAmount
fn zero_strategy_withdraw_rejects() {
    let (_env, vault, _token, user, strategy) = setup_test();
    vault.strategy_deposit(&(10_000 * SCALAR_7), &user, &strategy, &0);
    vault.strategy_withdraw(&0);
}

// ==================== share token ====================

#[test]
fn shares_transfer_freely() {
    let (env, vault, _token, user, strategy) = setup_test();
    let recipient = Address::generate(&env);

    vault.strategy_deposit(&(1000 * SCALAR_7), &user, &strategy, &0);
    vault.transfer(&user, &recipient, &(500 * SCALAR_7 * SHARE_SCALE));

    assert_eq!(vault.balance(&recipient), 500 * SCALAR_7 * SHARE_SCALE);
    assert_eq!(vault.balance(&user), 500 * SCALAR_7 * SHARE_SCALE);
}

#[test]
fn shares_transfer_from_spends_allowance() {
    let (env, vault, _token, user, strategy) = setup_test();
    let spender = Address::generate(&env);
    let recipient = Address::generate(&env);

    vault.strategy_deposit(&(1000 * SCALAR_7), &user, &strategy, &0);
    let half = 500 * SCALAR_7 * SHARE_SCALE;
    vault.approve(&user, &spender, &half, &1000);
    vault.transfer_from(&spender, &user, &recipient, &half);

    assert_eq!(vault.balance(&recipient), half);
    assert_eq!(vault.balance(&user), half);
    assert_eq!(vault.allowance(&user, &spender), 0);
}

// ==================== views ====================

// `get_strategy` reports the market contract registered at construction.
#[test]
fn get_strategy_reports_the_constructor_strategy() {
    let (_env, vault, _token, _user, strategy) = setup_test();

    assert_eq!(vault.get_strategy(), strategy);
}
