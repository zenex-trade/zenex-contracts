//! Factory failure/retry matrix through optimized factory, vault and market
//! WASM. Each rejected invocation must leave the entire ledger unchanged,
//! including an existing funded pair, and a corrected retry reuses its salt.
//! Auth entries bind the admin to the factory call and both constructor host
//! calls. Only signature verification uses the SDK's mock account contract.

extern crate std;

use super::*;
use soroban_sdk::{
    contract, contractimpl,
    testutils::MockAuthContract,
    token::{StellarAssetClient, TokenClient},
    xdr::{self, ScErrorCode, ScErrorType, ToXdr},
    Bytes, Error, Executable, Vec as SdkVec,
};

// Constructor error codes from market::MarketError. Kept numeric here to pin
// the deployed WASM error boundary.
const INVALID_CONFIG: u32 = 700;
const NEGATIVE_VALUE: u32 = 710;

/// A well-typed metadata boundary, including values the SAC cannot expose.
#[contract]
struct MetadataToken;

#[contractimpl]
impl MetadataToken {
    pub fn __constructor(e: Env, decimals: u32) {
        e.storage()
            .instance()
            .set(&Symbol::new(&e, "decimals"), &decimals);
    }

    pub fn decimals(e: Env) -> u32 {
        e.storage()
            .instance()
            .get(&Symbol::new(&e, "decimals"))
            .unwrap()
    }
}

/// The method exists, but its return value cannot decode as SEP-41 `u32`.
#[contract]
struct MalformedMetadataToken;

#[contractimpl]
impl MalformedMetadataToken {
    pub fn decimals() -> i128 {
        -1
    }
}

#[derive(Clone)]
struct Inputs {
    admin: Address,
    salt: BytesN<32>,
    token: Address,
    oracle: Address,
    feed: BytesN<32>,
    config: Config,
    name: String,
    symbol: String,
}

impl Inputs {
    fn args(&self, e: &Env) -> SdkVec<Val> {
        (
            &self.admin,
            &self.salt,
            &self.token,
            &self.oracle,
            &self.feed,
            &self.config,
            &self.name,
            &self.symbol,
        )
            .into_val(e)
    }

    fn vault_salt(&self, e: &Env) -> BytesN<32> {
        let mut preimage = Bytes::from_array(e, &self.salt.to_array());
        preimage.extend_from_slice(b"vault");
        e.crypto().sha256(&preimage).to_bytes()
    }

    fn addresses(&self, e: &Env) -> (Address, Address) {
        (
            e.deployer()
                .with_address(self.admin.clone(), self.salt.clone())
                .deployed_address(),
            e.deployer()
                .with_address(self.admin.clone(), self.vault_salt(e))
                .deployed_address(),
        )
    }
}

struct Fixture {
    e: Env,
    factory: Address,
    owner: Address,
    meta: FactoryInitMeta,
    original: Inputs,
}

impl Fixture {
    fn new() -> Self {
        let e = test_env();
        e.set_auths(&[]);
        let owner = e.register(MockAuthContract, ());
        let admin = e.register(MockAuthContract, ());
        let meta = FactoryInitMeta {
            market_hash: e.deployer().upload_contract_wasm(MARKET_WASM),
            vault_hash: e.deployer().upload_contract_wasm(VAULT_WASM),
            treasury: Address::generate(&e),
        };
        let factory = e.register(FACTORY_WASM, (owner.clone(), meta.clone()));
        let original = Inputs {
            token: create_token(&e, &admin),
            admin,
            salt: BytesN::random(&e),
            oracle: Address::generate(&e),
            feed: feed(&e),
            config: default_config(),
            name: String::from_str(&e, "Factory rollback LP"),
            symbol: String::from_str(&e, "frLP"),
        };
        let fixture = Self {
            e,
            factory,
            owner,
            meta,
            original,
        };
        fixture.deploy_ok(&fixture.original);
        let (_, vault) = fixture.original.addresses(&fixture.e);
        let mint = MockAuthInvoke {
            contract: &fixture.original.token,
            fn_name: "mint",
            args: (&vault, 1_234_0000000_i128).into_val(&fixture.e),
            sub_invokes: &[],
        };
        fixture.e.set_auths(&[MockAuth {
            address: &fixture.original.admin,
            invoke: &mint,
        }
        .into()]);
        StellarAssetClient::new(&fixture.e, &fixture.original.token).mint(&vault, &1_234_0000000);
        fixture.e.set_auths(&[]);
        fixture
    }

    fn fresh(&self) -> Inputs {
        Inputs {
            salt: BytesN::random(&self.e),
            ..self.original.clone()
        }
    }

    /// Authorize exact inputs independently of what the factory requests.
    /// Omitting either child is useful for checking partial-deployment rollback.
    fn authorize(&self, input: &Inputs, signer: &Address, children: usize) {
        let e = &self.e;
        let meta = FactoryClient::new(e, &self.factory).get_init_meta();
        let root = MockAuthInvoke {
            contract: &self.factory,
            fn_name: "deploy",
            args: input.args(e),
            sub_invokes: &[],
        };
        let mut entry: xdr::SorobanAuthorizationEntry = MockAuth {
            address: signer,
            invoke: &root,
        }
        .into();
        let (market, vault) = input.addresses(e);
        let create = |salt: BytesN<32>, hash: BytesN<32>, args: SdkVec<Val>| {
            xdr::SorobanAuthorizedInvocation {
                function: xdr::SorobanAuthorizedFunction::CreateContractV2HostFn(
                    xdr::CreateContractArgsV2 {
                        contract_id_preimage: xdr::ContractIdPreimage::Address(
                            xdr::ContractIdPreimageFromAddress {
                                address: (&input.admin).try_into().unwrap(),
                                salt: xdr::Uint256(salt.to_array()),
                            },
                        ),
                        executable: xdr::ContractExecutable::Wasm(xdr::Hash(hash.to_array())),
                        constructor_args: args.try_into().unwrap(),
                    },
                ),
                sub_invocations: Default::default(),
            }
        };
        let invocations = [
            create(
                input.vault_salt(e),
                meta.vault_hash,
                (&input.name, &input.symbol, &input.token, &market).into_val(e),
            ),
            create(
                input.salt.clone(),
                meta.market_hash,
                (
                    &input.admin,
                    &input.token,
                    &vault,
                    &input.oracle,
                    &meta.treasury,
                    &input.feed,
                    &input.config,
                )
                    .into_val(e),
            ),
        ];
        entry.root_invocation.sub_invocations =
            invocations[..children].to_vec().try_into().unwrap();
        e.set_auths(&[entry]);
    }

    fn set_meta(&self, meta: &FactoryInitMeta) {
        let invoke = MockAuthInvoke {
            contract: &self.factory,
            fn_name: "set_init_meta",
            args: (meta,).into_val(&self.e),
            sub_invokes: &[],
        };
        self.e.set_auths(&[MockAuth {
            address: &self.owner,
            invoke: &invoke,
        }
        .into()]);
        FactoryClient::new(&self.e, &self.factory).set_init_meta(meta);
    }

    fn assert_pair(&self, input: &Inputs, pair: &(Address, Address)) {
        let e = &self.e;
        assert_eq!(pair, &input.addresses(e));
        assert_eq!(
            pair.0.executable(),
            Some(Executable::Wasm(self.meta.market_hash.clone()))
        );
        assert_eq!(
            pair.1.executable(),
            Some(Executable::Wasm(self.meta.vault_hash.clone()))
        );
        let factory = FactoryClient::new(e, &self.factory);
        assert!(factory.is_deployed(&pair.0));
        assert!(
            !factory.is_deployed(&pair.1),
            "vaults are not registry markets"
        );
        for (method, expected) in [
            ("get_token", &input.token),
            ("get_vault", &pair.1),
            ("get_oracle", &input.oracle),
            ("get_treasury", &self.meta.treasury),
        ] {
            assert_eq!(
                e.invoke_contract::<Address>(&pair.0, &Symbol::new(e, method), vec![e]),
                *expected
            );
        }
        assert_eq!(
            e.invoke_contract::<Option<Address>>(&pair.0, &Symbol::new(e, "get_owner"), vec![e]),
            Some(input.admin.clone())
        );
        assert_eq!(
            e.invoke_contract::<BytesN<32>>(&pair.0, &Symbol::new(e, "get_feed"), vec![e]),
            input.feed
        );
        let stored: Config = e.invoke_contract(&pair.0, &Symbol::new(e, "get_config"), vec![e]);
        assert_eq!(
            stored.to_xdr(e),
            input.config.clone().to_xdr(e),
            "all config fields survive deployment"
        );
        assert_eq!(
            e.invoke_contract::<Address>(&pair.1, &Symbol::new(e, "query_asset"), vec![e]),
            input.token
        );
        assert_eq!(
            e.invoke_contract::<Address>(&pair.1, &Symbol::new(e, "get_strategy"), vec![e]),
            pair.0
        );
        let shares = TokenClient::new(e, &pair.1);
        // The 7-decimal token plus the vault's fixed offset of 6.
        assert_eq!(shares.decimals(), 13);
        assert_eq!(shares.name(), input.name);
        assert_eq!(shares.symbol(), input.symbol);
    }

    fn deploy_ok(&self, input: &Inputs) {
        self.authorize(input, &input.admin, 2);
        let pair: (Address, Address) = self.e.invoke_contract(
            &self.factory,
            &Symbol::new(&self.e, "deploy"),
            input.args(&self.e),
        );
        self.assert_pair(input, &pair);
    }

    /// Capture after setting auth/metadata: the failed call itself may not
    /// change a single ledger entry or emit a successful contract event.
    fn reject(&self, input: &Inputs, error: Error) {
        let before = self.e.to_ledger_snapshot();
        assert_eq!(
            self.e.try_invoke_contract::<(Address, Address), Error>(
                &self.factory,
                &Symbol::new(&self.e, "deploy"),
                input.args(&self.e),
            ),
            Err(Ok(host_failure()))
        );
        let errors: std::vec::Vec<xdr::ScVal> = self
            .e
            .host()
            .get_events()
            .unwrap()
            .0
            .iter()
            .filter(|event| event.event.type_ == xdr::ContractEventType::Diagnostic)
            .flat_map(|event| {
                let xdr::ContractEventBody::V0(body) = &event.event.body;
                body.topics
                    .iter()
                    .filter(|value| matches!(value, xdr::ScVal::Error(_)))
                    .cloned()
            })
            .collect();
        assert!(
            errors.contains(&xdr::ScVal::Error(error.try_into().unwrap())),
            "missing expected cause: {error:?}; observed {errors:?}"
        );
        assert!(
            before == self.e.to_ledger_snapshot(),
            "failed deployment changed the ledger"
        );
        assert!(
            self.e.events().all().events().is_empty(),
            "failed deployment published an event"
        );
        self.assert_pair(&self.original, &self.original.addresses(&self.e));
        assert_eq!(
            TokenClient::new(&self.e, &self.original.token)
                .balance(&self.original.addresses(&self.e).1),
            1_234_0000000
        );
    }

    fn assert_absent(&self, input: &Inputs) {
        let (market, vault) = input.addresses(&self.e);
        assert!(!market.exists(), "failed deployment left a market instance");
        assert!(!vault.exists(), "failed deployment left a vault instance");
        assert!(!FactoryClient::new(&self.e, &self.factory).is_deployed(&market));
    }
}

/// The constructor host boundary wraps recoverable errors in this category;
/// `reject` also pins the underlying cause in structured diagnostic events.
fn host_failure() -> Error {
    Error::from_type_and_code(ScErrorType::Context, ScErrorCode::InvalidAction)
}

#[test]
fn duplicate_admin_and_salt_preserves_the_original_pair() {
    let f = Fixture::new();
    f.authorize(&f.original, &f.original.admin, 2);
    f.reject(
        &f.original,
        Error::from_type_and_code(ScErrorType::Storage, ScErrorCode::ExistingValue),
    );
    f.deploy_ok(&f.fresh());
}

#[test]
fn missing_admin_auth_leaves_the_salt_reusable() {
    let f = Fixture::new();
    let input = f.fresh();
    f.e.set_auths(&[]);
    f.reject(
        &input,
        Error::from_type_and_code(ScErrorType::Auth, ScErrorCode::InvalidAction),
    );
    f.assert_absent(&input);
    f.deploy_ok(&input);
}

#[test]
fn another_signer_cannot_authorize_the_admins_deployment() {
    let f = Fixture::new();
    let input = f.fresh();
    let stranger = f.e.register(MockAuthContract, ());
    f.authorize(&input, &stranger, 2);
    f.reject(
        &input,
        Error::from_type_and_code(ScErrorType::Auth, ScErrorCode::InvalidAction),
    );
    f.assert_absent(&input);
    f.deploy_ok(&input);
}

#[test]
fn missing_constructor_auth_rolls_back_and_retries() {
    // With one child authorized the vault constructor succeeds first; the
    // missing market authorization must still roll back that vault.
    for children in [0, 1] {
        let f = Fixture::new();
        let input = f.fresh();
        f.authorize(&input, &input.admin, children);
        f.reject(
            &input,
            Error::from_type_and_code(ScErrorType::Auth, ScErrorCode::InvalidAction),
        );
        f.assert_absent(&input);
        f.deploy_ok(&input);
    }
}

#[test]
fn invalid_config_rolls_back_the_vault_and_allows_corrected_retry() {
    // Range, cross-field capacity and negative-value categories. Individual
    // Config bounds are covered in market::engine::config's unit matrix.
    for case in 0..3 {
        let f = Fixture::new();
        let corrected = f.fresh();
        let mut invalid = corrected.clone();
        let code = match case {
            0 => {
                invalid.config.init_margin = invalid.config.maintenance_margin;
                INVALID_CONFIG
            }
            1 => {
                invalid.config.max_open_interest = invalid.config.max_position_notional - 1;
                INVALID_CONFIG
            }
            _ => {
                invalid.config.fee_dom = -1;
                NEGATIVE_VALUE
            }
        };
        f.authorize(&invalid, &invalid.admin, 2);
        f.reject(&invalid, Error::from_contract_error(code));
        f.assert_absent(&invalid);
        f.deploy_ok(&corrected);
    }
}

#[test]
fn invalid_feed_prefix_rolls_back_and_allows_corrected_retry() {
    for prefix in [[0, 2], [1, 3]] {
        let f = Fixture::new();
        let corrected = f.fresh();
        let mut invalid = corrected.clone();
        let mut id = invalid.feed.to_array();
        id[..2].copy_from_slice(&prefix);
        invalid.feed = BytesN::from_array(&f.e, &id);
        f.authorize(&invalid, &invalid.admin, 2);
        f.reject(&invalid, Error::from_contract_error(INVALID_CONFIG));
        f.assert_absent(&invalid);
        f.deploy_ok(&corrected);
    }
}

#[test]
fn missing_wasm_hashes_roll_back_and_retry_after_metadata_repair() {
    for market_missing in [false, true] {
        let f = Fixture::new();
        let input = f.fresh();
        let mut invalid = f.meta.clone();
        let missing = BytesN::from_array(&f.e, &[0xff; 32]);
        if market_missing {
            invalid.market_hash = missing;
        } else {
            invalid.vault_hash = missing;
        }
        f.set_meta(&invalid);
        f.authorize(&input, &input.admin, 2);
        f.reject(
            &input,
            Error::from_type_and_code(ScErrorType::Storage, ScErrorCode::MissingValue),
        );
        f.assert_absent(&input);
        f.set_meta(&f.meta);
        f.deploy_ok(&input);
    }
}

#[test]
fn incompatible_wasm_rolls_back_and_retries_after_metadata_repair() {
    for market_wrong in [false, true] {
        let f = Fixture::new();
        let input = f.fresh();
        let mut invalid = f.meta.clone();
        if market_wrong {
            invalid.market_hash = invalid.vault_hash.clone();
        } else {
            invalid.vault_hash = invalid.market_hash.clone();
        }
        f.set_meta(&invalid);
        f.authorize(&input, &input.admin, 2);
        f.reject(
            &input,
            Error::from_type_and_code(ScErrorType::WasmVm, ScErrorCode::UnexpectedSize),
        );
        f.assert_absent(&input);
        f.set_meta(&f.meta);
        f.deploy_ok(&input);
    }
}

#[test]
fn incompatible_token_metadata_rolls_back_and_allows_token_correction() {
    for case in 0..3 {
        let f = Fixture::new();
        let corrected = f.fresh();
        let mut invalid = corrected.clone();
        invalid.token = match case {
            0 => Address::generate(&f.e),
            // A different deployed contract with no decimals method. Using
            // this factory itself would instead fail the reentrancy gate.
            1 => {
                f.e.register(FACTORY_WASM, (f.owner.clone(), f.meta.clone()))
            }
            _ => f.e.register(MalformedMetadataToken, ()),
        };
        f.authorize(&invalid, &invalid.admin, 2);
        let cause = match case {
            0 => Error::from_type_and_code(ScErrorType::Storage, ScErrorCode::MissingValue),
            1 => Error::from_type_and_code(ScErrorType::WasmVm, ScErrorCode::MissingValue),
            _ => Error::from_type_and_code(ScErrorType::WasmVm, ScErrorCode::InvalidAction),
        };
        f.reject(&invalid, cause);
        f.assert_absent(&invalid);
        f.deploy_ok(&corrected);
    }
}

#[test]
fn token_decimals_other_than_seven_roll_back_and_retry() {
    for decimals in [0_u32, 6, 8, 18] {
        let f = Fixture::new();
        let mut corrected = f.fresh();
        corrected.token = f.e.register(MetadataToken, (7_u32,));
        let mut invalid = corrected.clone();
        invalid.token = f.e.register(MetadataToken, (decimals,));
        f.authorize(&invalid, &invalid.admin, 2);
        f.reject(&invalid, Error::from_contract_error(INVALID_CONFIG));
        f.assert_absent(&invalid);
        f.deploy_ok(&corrected);
    }
}
