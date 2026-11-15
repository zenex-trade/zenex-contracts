//! Strategy-gate integration tests for the factory-deployed vault WASM: only
//! the registered strategy (the market contract) may move vault assets or
//! mint and burn shares.
//!
//! Every `strategy_*` entry point runs `StrategyVault::require_strategy`, which
//! is `storage::get_strategy(e).require_auth()`. That is a `require_auth` on the
//! *market contract address*, not on the caller. The fixture's blanket
//! `mock_all_auths_allowing_non_root_auth` satisfies that for anybody, so each
//! test first narrows the mock (as `test_governance.rs` does) to a single
//! address: a freshly generated stranger. A stranger's signature can never
//! stand in for the strategy contract's own authorization, so the gate fires.
//!
//! # Error surface
//! A failed `require_auth` is not a contract error. No `contracterror` enum
//! names it, so nothing propagates a `StrategyVaultError`. The vault WASM frame
//! traps on the missing authorization and the `try_` client surfaces the host's
//! `Error(Context, InvalidAction)`, which the tests pin via [`auth_error`]. Each
//! rejection is paired with a state read (total assets, share supply, balances)
//! proving the call reverted whole. The withdraw test adds the positive
//! control: the same call with the market contract's auth mocked succeeds.
//!
//! The vault client is declared locally rather than reused from
//! `test_suites::dependencies::vault`, whose `VaultInterface` is deliberately
//! read-only (`query_asset`/`total_assets`/`balance`). The strategy-gated
//! mutations are bound here, where the auth tests need them.

use soroban_sdk::testutils::{Address as _, MockAuth, MockAuthInvoke};
use soroban_sdk::{contractclient, Address, Env, IntoVal, Val, Vec as SdkVec};

use test_suites::assertions::auth_error;
use test_suites::constants::{BTC_PRICE, SCALAR_7};
use test_suites::setup::create_fixture_with_data;
use test_suites::test_fixture::TestFixture;

/// The deployed strategy-vault surface under test, mirroring
/// `strategy-vault/src/contract.rs` (the mutations plus the reads the
/// assertions need). `total_supply` comes from the vault's `FungibleToken`
/// implementation.
#[allow(dead_code)] // consumed by the generated `StrategyVaultClient`
#[contractclient(name = "StrategyVaultClient")]
pub trait StrategyVaultInterface {
    fn strategy_deposit(
        e: Env,
        assets: i128,
        receiver: Address,
        from: Address,
        net_pnl: i128,
    ) -> i128;
    fn strategy_redeem(
        e: Env,
        shares: i128,
        receiver: Address,
        owner: Address,
        net_pnl: i128,
    ) -> i128;
    fn strategy_withdraw(e: Env, amount: i128);
    fn total_assets(e: Env) -> i128;
    fn total_supply(e: Env) -> i128;
    fn balance(e: Env, account: Address) -> i128;
    fn get_strategy(e: Env) -> Address;
    fn query_asset(e: Env) -> Address;
}

/// Assets a stranger tries to move out of the vault (token-dec).
const ATTEMPT_AMOUNT: i128 = 1_000 * SCALAR_7;

/// Narrow the fixture's blanket auth mock to `address` authorizing exactly
/// `fn_name(args)` on the vault, so every other `require_auth` in the call
/// must fail on its own merits. That includes the vault's own strategy gate.
fn mock_only(fixture: &TestFixture, address: &Address, fn_name: &'static str, args: SdkVec<Val>) {
    fixture.env.mock_auths(&[MockAuth {
        address,
        invoke: &MockAuthInvoke {
            contract: &fixture.vault.address,
            fn_name,
            args,
            sub_invokes: &[],
        },
    }]);
}

/// A stranger cannot pull assets out of the vault. `strategy_withdraw` demands
/// the market contract's authorization, so the call fails and the vault's
/// balance, share supply, and the stranger's wallet are all untouched. Mocking
/// the market contract's auth instead lets the identical call through, which
/// pins the gate to the strategy address rather than to the mock narrowing.
#[test]
fn stranger_cannot_strategy_withdraw() {
    let fixture = create_fixture_with_data();
    let vault = StrategyVaultClient::new(&fixture.env, &fixture.vault.address);
    let stranger = Address::generate(&fixture.env);

    let assets_before = vault.total_assets();
    let supply_before = vault.total_supply();
    let args = (ATTEMPT_AMOUNT,).into_val(&fixture.env);
    mock_only(&fixture, &stranger, "strategy_withdraw", args);

    assert_eq!(
        vault.try_strategy_withdraw(&ATTEMPT_AMOUNT),
        Err(Ok(auth_error())),
        "a non-strategy caller cannot withdraw vault assets",
    );
    assert_eq!(
        vault.total_assets(),
        assets_before,
        "vault assets unchanged"
    );
    assert_eq!(
        vault.total_supply(),
        supply_before,
        "share supply unchanged"
    );
    assert_eq!(
        fixture.balance(&stranger),
        0,
        "the stranger received nothing"
    );

    // Positive control: the same call authorized by the registered strategy
    // (the market contract) lands and moves the assets to it.
    let market = fixture.market.address.clone();
    let args = (ATTEMPT_AMOUNT,).into_val(&fixture.env);
    mock_only(&fixture, &market, "strategy_withdraw", args);
    vault.strategy_withdraw(&ATTEMPT_AMOUNT);
    assert_eq!(
        vault.total_assets(),
        assets_before - ATTEMPT_AMOUNT,
        "the strategy's own withdraw moves the assets",
    );
    assert_eq!(
        fixture.balance(&market),
        ATTEMPT_AMOUNT,
        "the market holds the withdrawn assets"
    );
}

/// A stranger cannot mint itself vault shares: `strategy_deposit` demands the
/// market contract's authorization, so no shares are minted and no assets are
/// pulled from the stranger's wallet.
#[test]
fn stranger_cannot_strategy_deposit() {
    let mut fixture = create_fixture_with_data();
    let stranger = fixture.new_user(ATTEMPT_AMOUNT);
    let vault = StrategyVaultClient::new(&fixture.env, &fixture.vault.address);

    let assets_before = vault.total_assets();
    let supply_before = vault.total_supply();
    let args = (ATTEMPT_AMOUNT, stranger.clone(), stranger.clone(), 0_i128).into_val(&fixture.env);
    mock_only(&fixture, &stranger, "strategy_deposit", args);

    assert_eq!(
        vault.try_strategy_deposit(&ATTEMPT_AMOUNT, &stranger, &stranger, &0),
        Err(Ok(auth_error())),
        "a non-strategy caller cannot mint vault shares",
    );
    assert_eq!(
        vault.total_assets(),
        assets_before,
        "vault assets unchanged"
    );
    assert_eq!(
        vault.total_supply(),
        supply_before,
        "share supply unchanged"
    );
    assert_eq!(
        vault.balance(&stranger),
        0,
        "no shares minted to the stranger"
    );
    assert_eq!(
        fixture.balance(&stranger),
        ATTEMPT_AMOUNT,
        "the stranger's assets were not pulled"
    );
}

/// A stranger cannot burn an LP's shares. With real shares outstanding from a
/// keeper-filled vault deposit, `strategy_redeem` still demands the market
/// contract's authorization. The LP keeps every share and the vault keeps
/// every asset.
#[test]
fn stranger_cannot_strategy_redeem_an_lp_position() {
    // A bare fixture, not the pre-seeded one: the seeded $10M donation exceeds
    // the config's `max_vault_balance`, so the LP deposit fill would trap.
    let mut fixture = TestFixture::create();
    let lp = fixture.new_user(200 * SCALAR_7);
    let vault = StrategyVaultClient::new(&fixture.env, &fixture.vault.address);

    // Mint real shares the way LPs do: a vault deposit order filled by a keeper
    // a ledger later (the commit-then-execute price-time gate).
    let deposit = fixture.vault_deposit(&fixture.keeper, &lp, 100 * SCALAR_7, 0, BTC_PRICE);
    assert!(deposit.filled, "the seed deposit filled");
    let lp_shares = vault.balance(&lp);
    assert!(lp_shares > 0, "the LP holds shares to steal");

    let assets_before = vault.total_assets();
    let supply_before = vault.total_supply();
    let stranger = Address::generate(&fixture.env);
    let args = (lp_shares, stranger.clone(), lp.clone(), 0_i128).into_val(&fixture.env);
    mock_only(&fixture, &stranger, "strategy_redeem", args);

    assert_eq!(
        vault.try_strategy_redeem(&lp_shares, &stranger, &lp, &0),
        Err(Ok(auth_error())),
        "a non-strategy caller cannot redeem another address's shares",
    );
    assert_eq!(
        vault.balance(&lp),
        lp_shares,
        "the LP still holds its shares"
    );
    assert_eq!(
        vault.total_supply(),
        supply_before,
        "share supply unchanged"
    );
    assert_eq!(
        vault.total_assets(),
        assets_before,
        "vault assets unchanged"
    );
    assert_eq!(
        fixture.balance(&stranger),
        0,
        "the stranger received nothing"
    );
}

/// The vault's immutable wiring is what the gate keys on: `get_strategy` is the
/// market contract deployed alongside it by the factory, and `query_asset` is
/// the market's settlement token.
#[test]
fn vault_reports_its_strategy_and_settlement_asset() {
    let fixture = create_fixture_with_data();
    let vault = StrategyVaultClient::new(&fixture.env, &fixture.vault.address);

    assert_eq!(
        vault.get_strategy(),
        fixture.market.address,
        "the registered strategy is the paired market contract",
    );
    assert_eq!(
        vault.query_asset(),
        fixture.token_address,
        "the vault's underlying asset is the settlement token",
    );
}
