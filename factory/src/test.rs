use crate::storage::FactoryInitMeta;
use crate::Config;
use crate::{FactoryClient, FactoryContract, FactoryError};

use soroban_sdk::{
    map,
    testutils::{Address as _, BytesN as _, Events, MockAuth, MockAuthInvoke},
    vec, Address, BytesN, Env, IntoVal, Map, String, Symbol, Val,
};
use stellar_contract_utils::upgradeable::UpgradeableClient;

mod deployment_failures;

const MARKET_WASM: &[u8] = include_bytes!("../../target/wasm32v1-none/release/market.wasm");
const VAULT_WASM: &[u8] = include_bytes!("../../target/wasm32v1-none/release/strategy_vault.wasm");
/// The factory's own optimized WASM, for upgrading it to itself.
const FACTORY_WASM: &[u8] = include_bytes!("../../target/wasm32v1-none/release/factory.wasm");

const SCALAR_18: i128 = 1_000_000_000_000_000_000;

/// A V3-prefixed (`0x0003…`) stream id for the deployed market.
fn feed(e: &Env) -> BytesN<32> {
    let mut id = [0u8; 32];
    id[1] = 0x03;
    id[31] = 6;
    BytesN::from_array(e, &id)
}

fn default_config() -> Config {
    Config {
        keeper_rate: SCALAR_18 / 10, // 10%
        min_position_notional: 10_000_000,
        max_position_notional: 1_000_000_000_000,
        max_open_interest: 10_000_000_000_000,
        min_order_notional: 1_000_000,
        min_order_margin: 1_000_000,
        exec_fee: 100_000,
        fee_dom: SCALAR_18 / 200,          // 0.5%
        fee_non_dom: 3 * SCALAR_18 / 1000, // 0.3%
        impact_scalar: 100_000_000_000_000,
        max_util_open: 8 * SCALAR_18 / 10,     // 80%
        max_util_withdraw: 9 * SCALAR_18 / 10, // 90%
        init_margin: SCALAR_18 / 10,           // 10%
        maintenance_margin: SCALAR_18 / 20,    // 5%
        liq_fee: SCALAR_18 / 100,              // 1%
        notional_lock: 30,
        target_util: SCALAR_18 / 2, // 50%
        borrow_rate: 0,
        increased_borrow_rate: 0,
        funding_increase: 0,
        funding_decrease: 0,
        threshold_stable_funding: 0,
        threshold_decrease_funding: 0,
        funding_min: 0,
        funding_max: 0,
        adl_max_pnl: SCALAR_18 / 2,           // 50%
        adl_clear_target: 4 * SCALAR_18 / 10, // 40%
        max_pnl_trader: 9 * SCALAR_18 / 10,
        max_pnl_withdraw: 15 * SCALAR_18 / 100, // 15%
        redeem_lock: 0,
        deposit_fee: SCALAR_18 / 1000, // 10 bps
        redeem_fee: SCALAR_18 / 1000,  // 10 bps
        min_deposit: 1_000_000,
        max_vault_balance: 10_000_000_000_000,
    }
}

fn setup_factory(e: &Env) -> (Address, FactoryClient<'_>) {
    setup_factory_owned_by(e, &Address::generate(e))
}

fn setup_factory_owned_by<'a>(e: &'a Env, owner: &Address) -> (Address, FactoryClient<'a>) {
    let market_hash = e.deployer().upload_contract_wasm(MARKET_WASM);
    let vault_hash = e.deployer().upload_contract_wasm(VAULT_WASM);
    let treasury = Address::generate(e);
    let init_meta = FactoryInitMeta {
        market_hash,
        vault_hash,
        treasury,
    };
    let address = e.register(FactoryContract {}, (owner.clone(), init_meta));
    let client = FactoryClient::new(e, &address);
    (address, client)
}

fn create_token(e: &Env, admin: &Address) -> Address {
    e.register_stellar_asset_contract_v2(admin.clone())
        .address()
}

/// A test environment under the live network's instruction ceiling.
fn test_env() -> Env {
    let e = Env::default();
    // The live network caps a transaction at 400M instructions, the one limit
    // `Env::default()` does not already enforce. Memory stays unpinned here:
    // the invocation limits enforce the network's 40 MiB, and the budget's
    // ceiling would also bound the test harness's own shadow work.
    e.cost_estimate()
        .budget()
        .reset_limits(400_000_000, u64::MAX);
    e.mock_all_auths_allowing_non_root_auth();
    e
}

#[test]
fn test_factory_deploy() {
    let e = test_env();

    let (_factory_address, factory) = setup_factory(&e);
    let admin = Address::generate(&e);
    let token = create_token(&e, &admin);
    let oracle = Address::generate(&e);
    let salt = BytesN::<32>::random(&e);

    let (market_address, vault_address) = factory.deploy(
        &admin,
        &salt,
        &token,
        &oracle,
        &feed(&e),
        &default_config(),
        &String::from_str(&e, "Zenex LP"),
        &String::from_str(&e, "zLP"),
    );

    assert!(factory.is_deployed(&market_address));
    assert!(!factory.is_deployed(&Address::generate(&e)));
    // The returned vault is the one wired into the market contract.
    let wired_vault: Address = e.invoke_contract(
        &market_address,
        &soroban_sdk::Symbol::new(&e, "get_vault"),
        soroban_sdk::vec![&e],
    );
    assert_eq!(wired_vault, vault_address);

    let salt2 = BytesN::<32>::random(&e);
    let (market_2, vault_2) = factory.deploy(
        &admin,
        &salt2,
        &token,
        &oracle,
        &feed(&e),
        &default_config(),
        &String::from_str(&e, "Zenex LP 2"),
        &String::from_str(&e, "zLP2"),
    );
    assert_ne!(market_address, market_2);
    assert_ne!(vault_address, vault_2);
    assert!(factory.is_deployed(&market_2));
}

#[test]
fn test_salt_pair_low_bit_flip_both_deployable() {
    // The same admin deploying with salt S and then S^1 (low bit of byte 31
    // flipped) must succeed for both. Under the old bit-flip vault-salt
    // derivation the second deploy computed vault salt S — the address the
    // first deploy's market contract already occupies — and trapped.
    let e = test_env();

    let (_factory_address, factory) = setup_factory(&e);
    let admin = Address::generate(&e);
    let token = create_token(&e, &admin);
    let oracle = Address::generate(&e);
    let salt = BytesN::<32>::random(&e);
    let mut flipped_bytes = salt.to_array();
    flipped_bytes[31] ^= 1;
    let flipped_salt = BytesN::from_array(&e, &flipped_bytes);

    let (market_a, vault_a) = factory.deploy(
        &admin,
        &salt,
        &token,
        &oracle,
        &feed(&e),
        &default_config(),
        &String::from_str(&e, "Zenex LP A"),
        &String::from_str(&e, "zLPA"),
    );

    let (market_b, vault_b) = factory.deploy(
        &admin,
        &flipped_salt,
        &token,
        &oracle,
        &feed(&e),
        &default_config(),
        &String::from_str(&e, "Zenex LP B"),
        &String::from_str(&e, "zLPB"),
    );

    assert_ne!(market_a, market_b);
    assert_ne!(vault_a, vault_b);
    assert!(factory.is_deployed(&market_a));
    assert!(factory.is_deployed(&market_b));
}

#[test]
fn test_deploy_address_binds_to_admin() {
    // Two admins reusing the SAME salt derive different addresses, so neither
    // can occupy the other's. Under factory-namespaced salts this would have
    // collided and the second deploy would revert.
    let e = test_env();

    let (_factory_address, factory) = setup_factory(&e);
    let admin_a = Address::generate(&e);
    let admin_b = Address::generate(&e);
    let token = create_token(&e, &admin_a);
    let oracle = Address::generate(&e);
    let salt = BytesN::<32>::random(&e);

    let (market_a, _vault_a) = factory.deploy(
        &admin_a,
        &salt,
        &token,
        &oracle,
        &feed(&e),
        &default_config(),
        &String::from_str(&e, "Zenex LP A"),
        &String::from_str(&e, "zLPA"),
    );

    let (market_b, _vault_b) = factory.deploy(
        &admin_b,
        &salt,
        &token,
        &oracle,
        &feed(&e),
        &default_config(),
        &String::from_str(&e, "Zenex LP B"),
        &String::from_str(&e, "zLPB"),
    );

    assert_ne!(market_a, market_b);
    assert!(factory.is_deployed(&market_a));
    assert!(factory.is_deployed(&market_b));
}

/// The market constructor pins the feed to the V3 (`0x0003…`) schema: a
/// deploy carrying any other stream id fails at deployment, not on the
/// first fill.
#[test]
#[should_panic(expected = "#700")]
fn deploy_rejects_a_non_v3_feed() {
    let e = test_env();

    let (_factory_address, factory) = setup_factory(&e);
    let admin = Address::generate(&e);
    let token = create_token(&e, &admin);
    let oracle = Address::generate(&e);
    let salt = BytesN::<32>::random(&e);

    let mut id = [0u8; 32];
    id[1] = 0x02; // a V2 stream id
    factory.deploy(
        &admin,
        &salt,
        &token,
        &oracle,
        &BytesN::from_array(&e, &id),
        &default_config(),
        &String::from_str(&e, "Zenex LP"),
        &String::from_str(&e, "zLP"),
    );
}

// ---- owner surface: hash refresh and upgrade ----

/// `set_init_meta` is what keeps new markets on current code. The WASM hashes
/// live in instance storage, so upgrading the factory's own bytecode would not
/// refresh them; this is the entry that does.
#[test]
fn set_init_meta_replaces_the_hashes_future_deploys_use() {
    let e = test_env();

    let owner = Address::generate(&e);
    let (_factory_address, factory) = setup_factory_owned_by(&e, &owner);
    let before = factory.get_init_meta();

    // Any other uploaded blob stands in for "the next release of the market".
    let next_market_hash = e.deployer().upload_contract_wasm(VAULT_WASM);
    assert_ne!(
        before.market_hash, next_market_hash,
        "the stand-in hash differs from the installed one"
    );
    let replacement = FactoryInitMeta {
        market_hash: next_market_hash.clone(),
        vault_hash: before.vault_hash.clone(),
        treasury: before.treasury.clone(),
    };
    factory.set_init_meta(&replacement);
    // The replacement is attested in full: the hashes decide which code
    // every future market runs. Read before the next call, which resets the
    // event buffer.
    let data: Map<Symbol, Val> = map![&e, (Symbol::new(&e, "init_meta"), replacement.into_val(&e))];
    assert_eq!(
        e.events().all().filter_by_contract(&factory.address),
        vec![
            &e,
            (
                factory.address.clone(),
                (Symbol::new(&e, "init_meta_update"),).into_val(&e),
                data.into_val(&e),
            ),
        ],
    );

    let after = factory.get_init_meta();
    assert_eq!(
        after.market_hash, next_market_hash,
        "future deploys install the refreshed market hash"
    );
    assert_eq!(
        after.vault_hash, before.vault_hash,
        "the untouched vault hash is preserved"
    );
    assert_eq!(
        after.treasury, before.treasury,
        "the untouched treasury is preserved"
    );
}

/// `set_init_meta` demands the *owner* specifically, not merely some signature.
///
/// Clearing the auth mock only proves an authorization is required; this pins
/// the stronger property, because `set_init_meta` decides which WASM every
/// future market runs and where its fees go. A stranger who signs for himself
/// is rejected and the metadata is untouched.
#[test]
fn set_init_meta_rejects_a_non_owner_signature() {
    let e = test_env();

    let owner = Address::generate(&e);
    let stranger = Address::generate(&e);
    let (factory_address, factory) = setup_factory_owned_by(&e, &owner);
    let before = factory.get_init_meta();

    let hostile = FactoryInitMeta {
        market_hash: e.deployer().upload_contract_wasm(VAULT_WASM),
        vault_hash: before.vault_hash.clone(),
        treasury: stranger.clone(),
    };

    // The stranger authorizes his own call. Nothing else is mocked, so the
    // owner's signature is absent.
    e.mock_auths(&[MockAuth {
        address: &stranger,
        invoke: &MockAuthInvoke {
            contract: &factory_address,
            fn_name: "set_init_meta",
            args: (hostile.clone(),).into_val(&e),
            sub_invokes: &[],
        },
    }]);

    assert!(
        factory.try_set_init_meta(&hostile).is_err(),
        "a non-owner signature does not satisfy set_init_meta"
    );

    let after = factory.get_init_meta();
    assert_eq!(
        after.market_hash, before.market_hash,
        "the market WASM hash is unchanged after the rejected call"
    );
    assert_eq!(
        after.treasury, before.treasury,
        "the fee sink is unchanged after the rejected call"
    );
}

/// `set_init_meta` is owner-gated: with the mocked auths cleared, the
/// `#[only_owner]` check has nothing to satisfy it.
#[test]
fn set_init_meta_requires_owner_auth() {
    let e = test_env();

    let owner = Address::generate(&e);
    let (_factory_address, factory) = setup_factory_owned_by(&e, &owner);
    let before = factory.get_init_meta();

    e.set_auths(&[]);
    assert!(
        factory
            .try_set_init_meta(&FactoryInitMeta {
                market_hash: e.deployer().upload_contract_wasm(VAULT_WASM),
                vault_hash: before.vault_hash.clone(),
                treasury: before.treasury.clone(),
            })
            .is_err(),
        "set_init_meta without owner auth is rejected"
    );
    assert_eq!(
        factory.get_init_meta().market_hash,
        before.market_hash,
        "the hashes are unchanged after the rejected call"
    );
}

/// `operator` is pinned to the owner: the blanket auth mock satisfies the owner
/// check, so a stranger is caught by the explicit comparison instead.
#[test]
fn upgrade_rejects_an_operator_that_is_not_the_owner() {
    let e = test_env();

    let owner = Address::generate(&e);
    let stranger = Address::generate(&e);
    let (factory_address, _factory) = setup_factory_owned_by(&e, &owner);

    let some_wasm = e.deployer().upload_contract_wasm(VAULT_WASM);
    let upgradeable = UpgradeableClient::new(&e, &factory_address);
    assert_eq!(
        upgradeable.try_upgrade(&some_wasm, &stranger),
        Err(Ok(FactoryError::UpgradeNotOwner.into())),
        "an operator that is not the owner is rejected"
    );
}

/// The owner upgrade lands and the deploy registry survives the swap.
#[test]
fn owner_upgrade_preserves_the_deploy_registry() {
    let e = test_env();

    let owner = Address::generate(&e);
    let (factory_address, factory) = setup_factory_owned_by(&e, &owner);
    let admin = Address::generate(&e);
    let token = create_token(&e, &admin);
    let oracle = Address::generate(&e);
    let salt = BytesN::<32>::random(&e);
    let (market_address, _vault_address) = factory.deploy(
        &admin,
        &salt,
        &token,
        &oracle,
        &feed(&e),
        &default_config(),
        &String::from_str(&e, "Zenex LP"),
        &String::from_str(&e, "zLP"),
    );

    let same_wasm = e.deployer().upload_contract_wasm(FACTORY_WASM);
    let upgradeable = UpgradeableClient::new(&e, &factory_address);
    upgradeable.upgrade(&same_wasm, &owner);

    assert!(
        factory.is_deployed(&market_address),
        "the persistent deploy registry survives the upgrade"
    );
}
