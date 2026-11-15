//! Integration coverage for the `Upgradeable` surface on the market contract
//! and the oracle: owner gating, the `operator`-must-be-owner pin, that the
//! executable is genuinely replaced, that storage survives the swap, and that
//! a governance-owned contract upgrades only through the timelock.
//!
//! The vault and treasury are deliberately not upgradeable, so there is
//! nothing to cover for them here. The factory's own upgrade surface and its
//! `set_init_meta` hash refresh live in `factory/src/test.rs`, next to the
//! deploy path they affect.
//!
//! The fixture starts with `mock_all_auths_allowing_non_root_auth`. The
//! auth-rejection tests clear that so the `#[only_owner]` check has nothing to
//! satisfy it.

use soroban_sdk::testutils::Address as _;
use soroban_sdk::{contractclient, Address, BytesN, Env};
use test_suites::dependencies::governance::GOVERNANCE_WASM;

use governance::{GovernanceClient, GovernanceError};
use market::MarketError;
use oracle::OracleError;
use soroban_sdk::{IntoVal, Symbol, Val, Vec as SdkVec};
use test_suites::constants::BTC_PRICE;
use test_suites::dependencies::market::MARKET_WASM;
use test_suites::dependencies::vault::VAULT_WASM;
use test_suites::setup::create_fixture_with_data;
use test_suites::test_fixture::TestFixture;
use test_suites::SCALAR_7;

/// The `stellar-contract-utils` upgrade entry point. Bound locally rather than
/// pulled in as a dependency, matching how `test_governance.rs` binds the
/// stellar-access `Ownable` surface.
#[contractclient(name = "UpgradeableClient")]
pub trait UpgradeableInterface {
    fn upgrade(e: &Env, new_wasm_hash: BytesN<32>, operator: Address);
}

/// The stellar-access two-step ownership handshake, as in `test_governance.rs`.
#[contractclient(name = "OwnableClient")]
pub trait OwnableInterface {
    fn transfer_ownership(e: &Env, new_owner: Address, live_until_ledger: u32);
    fn accept_ownership(e: &Env);
}

/// Governance timelock in seconds, inside the contract's valid delay range and
/// past in a single fixture `jump`.
const DELAY: u64 = 3600;

// ---- market: the happy path ----

/// An owner upgrade lands, and the market's storage is untouched by the swap.
/// Covers all three TTL tiers: instance (config), shared (market data) and
/// user (an open position).
///
/// Upgrading to the same WASM is the honest shape for this assertion. It
/// isolates "does the upgrade call preserve state" from "did the code change",
/// which the next test covers separately.
#[test]
fn owner_upgrade_preserves_market_storage() {
    let mut fixture = create_fixture_with_data();
    let user = fixture.new_user(1_000 * SCALAR_7);
    let price = fixture.price(BTC_PRICE);
    fixture.open(&user, true, 1_000 * SCALAR_7, 200 * SCALAR_7, &price);

    let config_before = fixture.market.get_config();
    let data_before = fixture.market.get_market_data();
    let position_before = fixture.position(&user, true);
    assert!(
        position_before.notional > 0,
        "the fixture opened a position to carry across the upgrade"
    );

    let same_wasm = fixture.env.deployer().upload_contract_wasm(MARKET_WASM);
    let upgradeable = UpgradeableClient::new(&fixture.env, &fixture.market.address);
    upgradeable.upgrade(&same_wasm, &fixture.owner);

    assert_eq!(
        fixture.market.get_config().min_order_margin,
        config_before.min_order_margin,
        "instance config survives the upgrade"
    );
    assert_eq!(
        fixture.market.get_market_data().notional.long,
        data_before.notional.long,
        "the shared-tier market data survives the upgrade"
    );
    assert_eq!(
        fixture.position(&user, true).notional,
        position_before.notional,
        "the user-tier position survives the upgrade"
    );
}

/// The upgrade genuinely rewrites the executable, rather than merely returning
/// successfully: after pointing the market at the vault's WASM, a `Market`
/// entry point no longer exists to call.
#[test]
fn upgrade_replaces_the_executable() {
    let fixture = TestFixture::create();

    let foreign_wasm = fixture.env.deployer().upload_contract_wasm(VAULT_WASM);
    let upgradeable = UpgradeableClient::new(&fixture.env, &fixture.market.address);
    upgradeable.upgrade(&foreign_wasm, &fixture.owner);

    assert!(
        fixture.market.try_get_config().is_err(),
        "the market interface is gone once the executable is replaced"
    );
}

// ---- market: authorization ----

/// `upgrade` is owner-gated: with the mocked auths cleared, the `#[only_owner]`
/// check has nothing to satisfy it and the call traps on authorization.
#[test]
fn upgrade_requires_owner_auth() {
    let fixture = TestFixture::create();
    let same_wasm = fixture.env.deployer().upload_contract_wasm(MARKET_WASM);
    let upgradeable = UpgradeableClient::new(&fixture.env, &fixture.market.address);

    fixture.env.set_auths(&[]);
    assert!(
        upgradeable.try_upgrade(&same_wasm, &fixture.owner).is_err(),
        "upgrade without owner auth is rejected"
    );
    assert!(
        fixture.market.try_get_config().is_ok(),
        "the contract still answers its own interface after the rejected upgrade"
    );
}

/// `operator` is pinned to the owner. The blanket auth mock satisfies the
/// owner check, so a stranger passed as `operator` is caught by the explicit
/// comparison and not by `require_auth`. That is the whole point of the pin.
#[test]
fn upgrade_rejects_an_operator_that_is_not_the_owner() {
    let fixture = TestFixture::create();
    let stranger = Address::generate(&fixture.env);
    let same_wasm = fixture.env.deployer().upload_contract_wasm(MARKET_WASM);
    let upgradeable = UpgradeableClient::new(&fixture.env, &fixture.market.address);

    assert_eq!(
        upgradeable.try_upgrade(&same_wasm, &stranger),
        Err(Ok(MarketError::UpgradeNotOwner.into())),
        "an operator that is not the owner is rejected"
    );
    assert!(
        fixture.market.try_get_config().is_ok(),
        "the contract is untouched after the rejected upgrade"
    );
}

// ---- market under governance ----

/// With governance owning the market, an upgrade is only reachable through the
/// timelock: queued, refused before the delay, and applied after it.
///
/// This is the whole reason no bespoke upgrade-delay machinery exists. The
/// generic `queue`/`execute` pair already gates every owner-only entry point
/// other than the emergency `set_status`.
#[test]
fn governance_gates_an_upgrade_behind_the_timelock() {
    let fixture = TestFixture::create();
    let gov_id = fixture
        .env
        .register(GOVERNANCE_WASM, (&fixture.owner, DELAY));

    let ownable = OwnableClient::new(&fixture.env, &fixture.market.address);
    let live_until = fixture.far_expiration();
    ownable.transfer_ownership(&gov_id, &live_until);
    ownable.accept_ownership();

    let same_wasm = fixture.env.deployer().upload_contract_wasm(MARKET_WASM);
    let governance = GovernanceClient::new(&fixture.env, &gov_id);
    // The operator is governance itself: it is the owner now, so the pin holds.
    let args: SdkVec<Val> = SdkVec::from_array(
        &fixture.env,
        [
            same_wasm.into_val(&fixture.env),
            gov_id.clone().into_val(&fixture.env),
        ],
    );
    let nonce = governance.queue(
        &fixture.market.address,
        &Symbol::new(&fixture.env, "upgrade"),
        &args,
    );

    assert_eq!(
        governance.try_execute(&nonce),
        Err(Ok(GovernanceError::NotUnlocked.into())),
        "the queued upgrade is refused before the delay elapses"
    );

    fixture.jump(DELAY + 1);
    governance.execute(&nonce);

    assert!(
        fixture.market.try_get_config().is_ok(),
        "the market still answers after the timelocked upgrade to the same WASM"
    );
}

// ---- oracle ----

/// The oracle carries the same surface with the same gating.
#[test]
fn oracle_upgrade_is_owner_gated_and_operator_pinned() {
    let fixture = TestFixture::create();
    let stranger = Address::generate(&fixture.env);
    // Any uploaded blob proves the auth path. These cases never reach the swap.
    let some_wasm = fixture.env.deployer().upload_contract_wasm(VAULT_WASM);
    let upgradeable = UpgradeableClient::new(&fixture.env, &fixture.oracle.address);

    assert_eq!(
        upgradeable.try_upgrade(&some_wasm, &stranger),
        Err(Ok(OracleError::UpgradeNotOwner.into())),
        "an operator that is not the owner is rejected"
    );

    fixture.env.set_auths(&[]);
    assert!(
        upgradeable.try_upgrade(&some_wasm, &fixture.owner).is_err(),
        "upgrade without owner auth is rejected"
    );
}
