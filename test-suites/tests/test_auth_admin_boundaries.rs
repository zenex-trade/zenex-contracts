//! Explicit account authorization, ownership deadlines and administrative
//! constructor/withdrawal boundaries. Market code executes as optimized WASM.
//! Constructor probes also deploy the shipped governance and treasury WASMs.
//! MockAuth verifies the required invocation trees, not real signatures.

use governance::{GovernanceClient, GovernanceError};
use market::{OrderKind, Status};
use soroban_sdk::{
    contract, contractclient, contractimpl,
    testutils::{Address as _, Ledger, MockAuth, MockAuthInvoke},
    token::TokenClient,
    xdr::{self, ScErrorCode, ScErrorType},
    Address, BytesN, Env, Error, IntoVal, Val, Vec,
};
use test_suites::constants::{EXEC_FEE, MIN_ORDER_NOTIONAL};
use test_suites::dependencies::governance::GOVERNANCE_WASM;
use test_suites::dependencies::treasury::TREASURY_WASM;
use test_suites::test_fixture::TestFixture;
use test_suites::SCALAR_18;
use treasury::{TreasuryClient, TreasuryError};

const MAX_DELAY: u64 = 60 * 24 * 3600;

#[contractclient(name = "OwnableClient")]
pub trait Ownable {
    fn transfer_ownership(e: &Env, new_owner: Address, live_until_ledger: u32);
    fn accept_ownership(e: &Env);
    fn get_owner(e: &Env) -> Option<Address>;
}

#[contractclient(name = "UpgradeableClient")]
pub trait Upgradeable {
    fn upgrade(e: &Env, new_wasm_hash: BytesN<32>, operator: Address);
}

fn auth(e: &Env, who: &Address, target: &Address, method: &'static str, args: Vec<Val>) {
    e.mock_auths(&[MockAuth {
        address: who,
        invoke: &MockAuthInvoke {
            contract: target,
            fn_name: method,
            args,
            sub_invokes: &[],
        },
    }]);
}

/// The host may extend TTL on a failed invocation. Contract values must roll back.
fn unchanged(e: &Env, call: impl FnOnce()) {
    let before = e.to_ledger_snapshot();
    call();
    let after = e.to_ledger_snapshot();
    assert!(before
        .ledger_entries
        .iter()
        .map(|(key, (value, _))| (key, value))
        .eq(after
            .ledger_entries
            .iter()
            .map(|(key, (value, _))| (key, value))));
}

/// Authorization failures are wrapped at the invocation boundary. Require the
/// underlying host cause too. A missing/expired pending transfer is not enough.
fn authorization_rejected(e: &Env, call: impl FnOnce()) {
    unchanged(e, call);
    let expected = xdr::ScVal::Error(
        Error::from_type_and_code(ScErrorType::Auth, ScErrorCode::InvalidAction)
            .try_into()
            .unwrap(),
    );
    assert!(
        e.host().get_events().unwrap().0.iter().any(|event| {
            let xdr::ContractEventBody::V0(body) = &event.event.body;
            event.event.type_ == xdr::ContractEventType::Diagnostic
                && body.topics.contains(&expected)
        }),
        "rejection must identify the missing authority"
    );
}

fn ownable_targets(f: &TestFixture) -> [Address; 5] {
    [
        f.market.address.clone(),
        f.oracle.address.clone(),
        f.factory.address.clone(),
        f.treasury.address.clone(),
        f.env.register(GOVERNANCE_WASM, (&f.owner, 1_u64)),
    ]
}

#[test]
fn ownership_accepts_at_deadline_and_rejects_former_owner_on_every_ownable_contract() {
    let f = TestFixture::create_testnet();
    for target in ownable_targets(&f) {
        let client = OwnableClient::new(&f.env, &target);
        let successor = Address::generate(&f.env);
        let until = f.env.ledger().sequence() + 2;
        auth(
            &f.env,
            &f.owner,
            &target,
            "transfer_ownership",
            (&successor, until).into_val(&f.env),
        );
        client.transfer_ownership(&successor, &until);
        assert_eq!(client.get_owner(), Some(f.owner.clone()));
        // The outgoing owner cannot accept on the successor's behalf.
        auth(
            &f.env,
            &f.owner,
            &target,
            "accept_ownership",
            Vec::new(&f.env),
        );
        authorization_rejected(&f.env, || {
            assert_eq!(
                client.try_accept_ownership(),
                Err(Ok(Error::from_type_and_code(
                    ScErrorType::Context,
                    ScErrorCode::InvalidAction,
                )))
            )
        });
        f.env
            .ledger()
            .with_mut(|ledger| ledger.sequence_number = until);
        auth(
            &f.env,
            &successor,
            &target,
            "accept_ownership",
            Vec::new(&f.env),
        );
        client.accept_ownership();
        assert_eq!(client.get_owner(), Some(successor.clone()));
        auth(
            &f.env,
            &successor,
            &target,
            "accept_ownership",
            Vec::new(&f.env),
        );
        assert_eq!(
            client.try_accept_ownership(),
            Err(Ok(Error::from_contract_error(2200)))
        );

        let next_deadline = until + 10;
        auth(
            &f.env,
            &f.owner,
            &target,
            "transfer_ownership",
            (&f.owner, next_deadline).into_val(&f.env),
        );
        authorization_rejected(&f.env, || {
            assert_eq!(
                client.try_transfer_ownership(&f.owner, &next_deadline),
                Err(Ok(Error::from_type_and_code(
                    ScErrorType::Context,
                    ScErrorCode::InvalidAction,
                )))
            )
        });
        auth(
            &f.env,
            &successor,
            &target,
            "transfer_ownership",
            (&f.owner, next_deadline).into_val(&f.env),
        );
        client.transfer_ownership(&f.owner, &next_deadline);
        auth(
            &f.env,
            &successor,
            &target,
            "transfer_ownership",
            (&f.owner, 0_u32).into_val(&f.env),
        );
        client.transfer_ownership(&f.owner, &0);
        assert_eq!(client.get_owner(), Some(successor));
    }
}

#[test]
fn expired_ownership_deadline_is_enforced_even_while_temporary_entry_lives() {
    let f = TestFixture::create_testnet();
    for target in ownable_targets(&f) {
        let client = OwnableClient::new(&f.env, &target);
        let successor = Address::generate(&f.env);
        let until = f.env.ledger().sequence() + 2;
        // The fixture's minimum temporary TTL is 16, longer than this deadline.
        auth(
            &f.env,
            &f.owner,
            &target,
            "transfer_ownership",
            (&successor, until).into_val(&f.env),
        );
        client.transfer_ownership(&successor, &until);
        f.env
            .ledger()
            .with_mut(|ledger| ledger.sequence_number = until + 1);
        auth(
            &f.env,
            &successor,
            &target,
            "accept_ownership",
            Vec::new(&f.env),
        );
        unchanged(&f.env, || {
            assert_eq!(
                client.try_accept_ownership(),
                Err(Ok(Error::from_contract_error(2203)))
            )
        });
        assert_eq!(client.get_owner(), Some(f.owner.clone()));
        let renewed = until + 10;
        auth(
            &f.env,
            &f.owner,
            &target,
            "transfer_ownership",
            (&successor, renewed).into_val(&f.env),
        );
        client.transfer_ownership(&successor, &renewed);
        auth(
            &f.env,
            &successor,
            &target,
            "accept_ownership",
            Vec::new(&f.env),
        );
        client.accept_ownership();
        assert_eq!(client.get_owner(), Some(successor));
    }
}

#[test]
fn ownership_cancellation_requires_current_owner_and_matching_pending_account() {
    let f = TestFixture::create_testnet();
    for target in ownable_targets(&f) {
        let client = OwnableClient::new(&f.env, &target);
        let successor = Address::generate(&f.env);
        let stranger = Address::generate(&f.env);
        let until = f.far_expiration();
        auth(
            &f.env,
            &f.owner,
            &target,
            "transfer_ownership",
            (&successor, until).into_val(&f.env),
        );
        client.transfer_ownership(&successor, &until);
        auth(
            &f.env,
            &successor,
            &target,
            "transfer_ownership",
            (&successor, 0_u32).into_val(&f.env),
        );
        authorization_rejected(&f.env, || {
            assert_eq!(
                client.try_transfer_ownership(&successor, &0),
                Err(Ok(Error::from_type_and_code(
                    ScErrorType::Context,
                    ScErrorCode::InvalidAction,
                )))
            )
        });
        auth(
            &f.env,
            &f.owner,
            &target,
            "transfer_ownership",
            (&stranger, 0_u32).into_val(&f.env),
        );
        unchanged(&f.env, || {
            assert_eq!(
                client.try_transfer_ownership(&stranger, &0),
                Err(Ok(Error::from_contract_error(2202)))
            )
        });
        auth(
            &f.env,
            &f.owner,
            &target,
            "transfer_ownership",
            (&successor, 0_u32).into_val(&f.env),
        );
        client.transfer_ownership(&successor, &0);
        auth(
            &f.env,
            &successor,
            &target,
            "accept_ownership",
            Vec::new(&f.env),
        );
        assert_eq!(
            client.try_accept_ownership(),
            Err(Ok(Error::from_contract_error(2200)))
        );
        assert_eq!(client.get_owner(), Some(f.owner.clone()));
    }
}

#[contract]
struct ConstructorProbe;

#[contractimpl]
impl ConstructorProbe {
    pub fn deploy(e: Env, hash: BytesN<32>, salt: BytesN<32>, args: Vec<Val>) -> Address {
        e.deployer()
            .with_current_contract(salt)
            .deploy_v2(hash, args)
    }
}

fn constructor_rejects(
    e: &Env,
    probe: &ConstructorProbeClient,
    hash: &BytesN<32>,
    args: Vec<Val>,
    code: u32,
) {
    let salt = BytesN::from_array(e, &[7; 32]);
    unchanged(e, || {
        assert_eq!(
            probe.try_deploy(hash, &salt, &args),
            Err(Ok(Error::from_type_and_code(
                ScErrorType::Context,
                ScErrorCode::InvalidAction
            )))
        )
    });
    let expected = xdr::ScVal::Error(Error::from_contract_error(code).try_into().unwrap());
    assert!(
        e.host().get_events().unwrap().0.iter().any(|event| {
            let xdr::ContractEventBody::V0(body) = &event.event.body;
            event.event.type_ == xdr::ContractEventType::Diagnostic
                && body.topics.contains(&expected)
        }),
        "constructor diagnostic must identify error {code}"
    );
}

#[test]
fn governance_constructor_and_delay_updates_enforce_inclusive_bounds() {
    let f = TestFixture::create_testnet();
    let probe_id = f.env.register(ConstructorProbe, ());
    let probe = ConstructorProbeClient::new(&f.env, &probe_id);
    let hash = f.env.deployer().upload_contract_wasm(GOVERNANCE_WASM);
    for invalid in [0_u64, MAX_DELAY + 1, u64::MAX] {
        constructor_rejects(
            &f.env,
            &probe,
            &hash,
            (&f.owner, invalid).into_val(&f.env),
            GovernanceError::InvalidDelay as u32,
        );
    }
    for (i, delay) in [1_u64, MAX_DELAY].into_iter().enumerate() {
        let id = probe.deploy(
            &hash,
            &BytesN::from_array(&f.env, &[i as u8; 32]),
            &(&f.owner, delay).into_val(&f.env),
        );
        let gov = GovernanceClient::new(&f.env, &id);
        assert_eq!(gov.get_delay(), delay);
    }
    let id = probe.deploy(
        &hash,
        &BytesN::from_array(&f.env, &[7; 32]),
        &(&f.owner, 1_u64).into_val(&f.env),
    );
    let gov = GovernanceClient::new(&f.env, &id);
    for next in [MAX_DELAY, 1] {
        let previous = gov.get_delay();
        auth(&f.env, &f.owner, &id, "set_delay", (next,).into_val(&f.env));
        gov.set_delay(&next);
        for invalid in [0_u64, MAX_DELAY + 1, u64::MAX] {
            auth(
                &f.env,
                &f.owner,
                &id,
                "set_delay",
                (invalid,).into_val(&f.env),
            );
            unchanged(&f.env, || {
                assert_eq!(
                    gov.try_set_delay(&invalid),
                    Err(Ok(GovernanceError::InvalidDelay.into()))
                )
            });
        }
        // An invalid overwrite cannot erase or postpone the valid pending update.
        f.env.mock_auths(&[]);
        f.jump(previous - 1);
        assert_eq!(
            gov.try_apply_delay(),
            Err(Ok(GovernanceError::NotUnlocked.into()))
        );
        f.jump(1);
        gov.apply_delay();
        assert_eq!(gov.get_delay(), next);
        assert_eq!(
            gov.try_apply_delay(),
            Err(Ok(GovernanceError::NotQueued.into()))
        );
    }
}

#[test]
fn treasury_constructor_zero_rate_and_withdrawal_edges_preserve_balances() {
    let mut f = TestFixture::create_testnet();
    let probe_id = f.env.register(ConstructorProbe, ());
    let probe = ConstructorProbeClient::new(&f.env, &probe_id);
    let hash = f.env.deployer().upload_contract_wasm(TREASURY_WASM);
    for invalid in [-1_i128, i128::MIN, SCALAR_18 / 2 + 1, i128::MAX] {
        constructor_rejects(
            &f.env,
            &probe,
            &hash,
            (&f.owner, invalid).into_val(&f.env),
            TreasuryError::InvalidRate as u32,
        );
    }
    for (i, rate) in [0_i128, SCALAR_18 / 2].into_iter().enumerate() {
        let id = probe.deploy(
            &hash,
            &BytesN::from_array(&f.env, &[i as u8; 32]),
            &(&f.owner, rate).into_val(&f.env),
        );
        assert_eq!(TreasuryClient::new(&f.env, &id).get_rate(), rate);
    }
    let id = probe.deploy(
        &hash,
        &BytesN::from_array(&f.env, &[7; 32]),
        &(&f.owner, SCALAR_18 / 2).into_val(&f.env),
    );
    let treasury = TreasuryClient::new(&f.env, &id);
    let receiver = f.new_user(0);
    f.token.mint(&id, &1_000);
    auth(
        &f.env,
        &f.owner,
        &id,
        "set_rate",
        (0_i128,).into_val(&f.env),
    );
    treasury.set_rate(&0);
    assert_eq!(treasury.get_rate(), 0);
    let token = TokenClient::new(&f.env, &f.token_address);
    for amount in [-1_i128, i128::MIN, 1_001, i128::MAX] {
        auth(
            &f.env,
            &f.owner,
            &id,
            "withdraw",
            (&f.token_address, &receiver, amount).into_val(&f.env),
        );
        unchanged(&f.env, || {
            assert!(treasury
                .try_withdraw(&f.token_address, &receiver, &amount)
                .is_err())
        });
    }
    auth(
        &f.env,
        &f.owner,
        &id,
        "withdraw",
        (&f.token_address, &receiver, 0_i128).into_val(&f.env),
    );
    treasury.withdraw(&f.token_address, &receiver, &0);
    assert_eq!((token.balance(&id), token.balance(&receiver)), (1_000, 0));
    auth(
        &f.env,
        &f.owner,
        &f.token_address,
        "set_authorized",
        (&receiver, false).into_val(&f.env),
    );
    f.token.set_authorized(&receiver, &false);
    auth(
        &f.env,
        &f.owner,
        &id,
        "withdraw",
        (&f.token_address, &receiver, 1_000_i128).into_val(&f.env),
    );
    unchanged(&f.env, || {
        assert!(treasury
            .try_withdraw(&f.token_address, &receiver, &1_000)
            .is_err())
    });
    auth(
        &f.env,
        &f.owner,
        &f.token_address,
        "set_authorized",
        (&receiver, true).into_val(&f.env),
    );
    f.token.set_authorized(&receiver, &true);
    auth(
        &f.env,
        &f.owner,
        &id,
        "withdraw",
        (&f.token_address, &receiver, 1_000_i128).into_val(&f.env),
    );
    treasury.withdraw(&f.token_address, &receiver, &1_000);
    assert_eq!((token.balance(&id), token.balance(&receiver)), (0, 1_000));
    assert_eq!(treasury.get_rate(), 0);
}

fn missing_authority() -> Error {
    Error::from_type_and_code(ScErrorType::Context, ScErrorCode::InvalidAction)
}

/// `#[only_owner]` is the only barrier on these entries. A stranger's call is
/// rejected for the owner's missing authorization, and no state changes.
#[test]
fn owner_only_entries_reject_a_stranger() {
    let f = TestFixture::create_testnet();
    let stranger = Address::generate(&f.env);

    let status = Status::OnIce as u32;
    let before = f.market.get_status();
    auth(
        &f.env,
        &stranger,
        &f.market.address,
        "set_status",
        (status,).into_val(&f.env),
    );
    authorization_rejected(&f.env, || {
        assert_eq!(
            f.market.try_set_status(&status),
            Err(Ok(missing_authority()))
        )
    });
    assert_eq!(f.market.get_status(), before);

    let price = 100 * SCALAR_18;
    auth(
        &f.env,
        &stranger,
        &f.market.address,
        "set_terminal_price",
        (price,).into_val(&f.env),
    );
    authorization_rejected(&f.env, || {
        assert_eq!(
            f.market.try_set_terminal_price(&price),
            Err(Ok(missing_authority()))
        )
    });

    let governance =
        GovernanceClient::new(&f.env, &f.env.register(GOVERNANCE_WASM, (&f.owner, 1_u64)));
    auth(
        &f.env,
        &stranger,
        &governance.address,
        "set_delay",
        (60_u64,).into_val(&f.env),
    );
    authorization_rejected(&f.env, || {
        assert_eq!(governance.try_set_delay(&60), Err(Ok(missing_authority())))
    });

    let hash = BytesN::from_array(&f.env, &[7; 32]);
    let factory = UpgradeableClient::new(&f.env, &f.factory.address);
    auth(
        &f.env,
        &stranger,
        &f.factory.address,
        "upgrade",
        (hash.clone(), stranger.clone()).into_val(&f.env),
    );
    authorization_rejected(&f.env, || {
        assert_eq!(
            factory.try_upgrade(&hash, &stranger),
            Err(Ok(missing_authority()))
        )
    });
}

/// Only the order's user can cancel it. A stranger's cancel is rejected for
/// the user's missing authorization, and the order keeps resting.
#[test]
fn cancel_order_rejects_a_stranger() {
    let mut f = TestFixture::create();
    let user = f.new_user(EXEC_FEE);
    let id = f.submit_order(
        &user,
        true,
        OrderKind::MarketIncrease,
        MIN_ORDER_NOTIONAL,
        0,
        0,
        0,
        f.far_expiration(),
    );
    let stranger = Address::generate(&f.env);
    auth(
        &f.env,
        &stranger,
        &f.market.address,
        "cancel_order",
        (user.clone(), id).into_val(&f.env),
    );
    authorization_rejected(&f.env, || {
        assert_eq!(
            f.market.try_cancel_order(&user, &id),
            Err(Ok(missing_authority()))
        )
    });
    assert!(
        f.market.try_get_order(&user, &id).is_ok(),
        "the order still rests"
    );
}
