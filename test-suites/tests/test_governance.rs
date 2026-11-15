//! Integration coverage for the governance contract owning the market contract:
//! transferring market ownership to governance, queueing a timelocked
//! `set_config`, the timelock gate on execution, cancellation, the owner-gated
//! auth model for queue and cancel, the delay-bypassing emergency
//! `set_status`, the two-phase `set_delay` / `apply_delay` change that
//! cannot shorten an already-queued action's wait, and the persistent queue
//! entries that outlive a week-long wait.
//!
//! The fixture does not wire governance, so each test registers
//! the governance WASM, hands it market ownership through the stellar-access
//! two-step transfer, and drives `set_config` (the canonical governed action)
//! end to end. The fixture starts with `mock_all_auths_allowing_non_root_auth`.
//! The auth-rejection tests narrow that to a single mocked authorizer.

use soroban_sdk::testutils::{Address as _, MockAuth, MockAuthInvoke};
use soroban_sdk::{contractclient, Address, Env, IntoVal, Symbol, Val, Vec as SdkVec};
use test_suites::dependencies::governance::GOVERNANCE_WASM;

use governance::{GovernanceClient, GovernanceError};
use market::{Config, MarketError, OrderKind, Status};
use test_suites::test_fixture::TestFixture;
use test_suites::{default_config, SCALAR_7};

/// The stellar-access `Ownable` surface the market contract exposes. The
/// fixture's `MarketClient` binds only the `Market` trait, so this local
/// client dispatches the ownership calls to the same contract address by name.
#[contractclient(name = "OwnableClient")]
pub trait OwnableInterface {
    fn transfer_ownership(e: &Env, new_owner: Address, live_until_ledger: u32);
    fn accept_ownership(e: &Env);
    fn get_owner(e: &Env) -> Option<Address>;
}

/// Governance timelock in seconds, well inside the contract's valid delay range
/// and past in a single fixture `jump`.
const DELAY: u64 = 3600;

/// A wait far longer than a day and inside the delay ceiling: the queue entry
/// outlives it and the action still executes.
const ONE_WEEK: u64 = 7 * 24 * 3600;

/// The reduced delay the `set_delay` tests install: short enough that an
/// action queued under it matures inside a single fixture `jump`, and far
/// below [`DELAY`] so a shortening is unmistakable.
const SHORT_DELAY: u64 = 60;

/// The market `Status` discriminants this file drives, derived from the
/// enum (`Frozen`, not `Delisted`, which starts the wind-down).
const STATUS_ACTIVE: u32 = Status::Active as u32;
const STATUS_FROZEN: u32 = Status::Frozen as u32;

/// The `min_order_margin` the governed `set_config` installs, distinct from
/// the baseline `default_config` value (1_000_000) so the change is observable.
/// `min_order_margin` is neither a funding nor a borrowing parameter, so
/// applying it needs no same-ledger accrual.
const NEW_MIN_ORDER_MARGIN: i128 = 2_000_000;

/// Register a governance contract owned by the fixture owner and transfer
/// market ownership to it via the two-step Ownable handshake. Returns the
/// fixture and the governance contract address.
fn setup() -> (TestFixture<'static>, Address) {
    let fixture = TestFixture::create();
    let gov_id = fixture
        .env
        .register(GOVERNANCE_WASM, (&fixture.owner, DELAY));

    // Two-step transfer: the current owner proposes, the governance contract
    // accepts. Both auths are covered by the fixture's blanket mock.
    let ownable = OwnableClient::new(&fixture.env, &fixture.market.address);
    let live_until = fixture.far_expiration();
    ownable.transfer_ownership(&gov_id, &live_until);
    ownable.accept_ownership();

    (fixture, gov_id)
}

/// A `default_config` clone with `min_order_margin` bumped to [`NEW_MIN_ORDER_MARGIN`].
fn config_with_new_min_order_margin() -> Config {
    let mut config = default_config();
    config.min_order_margin = NEW_MIN_ORDER_MARGIN;
    config
}

/// The single-arg `Vec<Val>` payload for a `set_config(config)` call.
fn set_config_args(fixture: &TestFixture, config: &Config) -> SdkVec<Val> {
    SdkVec::from_array(&fixture.env, [config.clone().into_val(&fixture.env)])
}

/// After the two-step Ownable handshake governance is the market owner, and
/// the former owner's directly-authorized `set_config` no longer authorizes.
#[test]
fn ownership_transfers_to_governance_and_old_owner_loses_control() {
    let (fixture, gov_id) = setup();

    let ownable = OwnableClient::new(&fixture.env, &fixture.market.address);
    assert_eq!(
        ownable.get_owner(),
        Some(gov_id.clone()),
        "governance is the market owner after the handshake",
    );

    // The former owner tries a direct config change. With only the old owner's
    // auth mocked, the market's `#[only_owner]` still demands the new owner
    // (governance), so the call cannot authorize.
    let new_config = config_with_new_min_order_margin();
    fixture.env.mock_auths(&[MockAuth {
        address: &fixture.owner,
        invoke: &MockAuthInvoke {
            contract: &fixture.market.address,
            fn_name: "set_config",
            args: (new_config.clone(),).into_val(&fixture.env),
            sub_invokes: &[],
        },
    }]);

    let result = fixture.market.try_set_config(&new_config);
    assert!(
        result.is_err(),
        "old owner can no longer set market config directly",
    );
    assert_eq!(
        fixture.market.get_config().min_order_margin,
        default_config().min_order_margin,
        "config unchanged after the rejected direct call",
    );
}

/// A queued action rejects `NotUnlocked` while the delay runs, and the target
/// config stays untouched.
#[test]
fn execute_before_timelock_elapses_rejects() {
    let (fixture, gov_id) = setup();
    let gov = GovernanceClient::new(&fixture.env, &gov_id);

    let new_config = config_with_new_min_order_margin();
    let args = set_config_args(&fixture, &new_config);
    let nonce = gov.queue(
        &fixture.market.address,
        &Symbol::new(&fixture.env, "set_config"),
        &args,
    );

    // No time has passed, so the timelock has not opened.
    let result = gov.try_execute(&nonce);
    assert_eq!(
        result,
        Err(Ok(GovernanceError::NotUnlocked.into())),
        "execution before the delay elapses is rejected",
    );

    // The config is untouched.
    assert_eq!(
        fixture.market.get_config().min_order_margin,
        default_config().min_order_margin,
        "config unchanged while the action is still locked",
    );
}

/// Past the delay the queued `set_config` lands on the market, and the consumed
/// entry cannot be replayed (`NotQueued`).
#[test]
fn execute_after_timelock_applies_config() {
    let (fixture, gov_id) = setup();
    let gov = GovernanceClient::new(&fixture.env, &gov_id);

    let new_config = config_with_new_min_order_margin();
    let args = set_config_args(&fixture, &new_config);
    let nonce = gov.queue(
        &fixture.market.address,
        &Symbol::new(&fixture.env, "set_config"),
        &args,
    );

    // Advance one second past the delay so `unlock_time <= now`.
    fixture.jump(DELAY + 1);
    gov.execute(&nonce);

    assert_eq!(
        fixture.market.get_config().min_order_margin,
        NEW_MIN_ORDER_MARGIN,
        "market config reflects the executed governance action",
    );

    // The queue entry is consumed. A second execute finds nothing.
    let replay = gov.try_execute(&nonce);
    assert_eq!(
        replay,
        Err(Ok(GovernanceError::NotQueued.into())),
        "an executed action cannot be replayed",
    );
}

/// A cancelled entry is gone for good: execution rejects `NotQueued` even
/// after the delay, and the config never changes.
#[test]
fn cancelled_action_cannot_execute() {
    let (fixture, gov_id) = setup();
    let gov = GovernanceClient::new(&fixture.env, &gov_id);

    let new_config = config_with_new_min_order_margin();
    let args = set_config_args(&fixture, &new_config);
    let nonce = gov.queue(
        &fixture.market.address,
        &Symbol::new(&fixture.env, "set_config"),
        &args,
    );

    gov.cancel(&nonce);

    // Even past the delay the cancelled entry is gone.
    fixture.jump(DELAY + 1);
    let result = gov.try_execute(&nonce);
    assert_eq!(
        result,
        Err(Ok(GovernanceError::NotQueued.into())),
        "a cancelled action is no longer executable",
    );
    assert_eq!(
        fixture.market.get_config().min_order_margin,
        default_config().min_order_margin,
        "config unchanged after a cancelled action",
    );
}

/// `queue` is owner-gated: with only a stranger's auth mocked the call fails,
/// and no entry lands in the queue (nonce 0 stays vacant).
#[test]
fn queue_by_non_owner_rejects() {
    let (fixture, gov_id) = setup();
    let gov = GovernanceClient::new(&fixture.env, &gov_id);

    let new_config = config_with_new_min_order_margin();
    let args = set_config_args(&fixture, &new_config);
    let non_owner = Address::generate(&fixture.env);

    // Mock only the non-owner's auth. The `#[only_owner]` on `queue` wants the
    // governance owner, which is absent.
    fixture.env.mock_auths(&[MockAuth {
        address: &non_owner,
        invoke: &MockAuthInvoke {
            contract: &gov_id,
            fn_name: "queue",
            args: (
                fixture.market.address.clone(),
                Symbol::new(&fixture.env, "set_config"),
                args.clone(),
            )
                .into_val(&fixture.env),
            sub_invokes: &[],
        },
    }]);

    let result = gov.try_queue(
        &fixture.market.address,
        &Symbol::new(&fixture.env, "set_config"),
        &args,
    );
    assert!(result.is_err(), "a non-owner cannot queue an action");

    // Nothing was queued: the first nonce a successful queue would have
    // allocated (0) resolves to no entry.
    fixture.env.mock_all_auths_allowing_non_root_auth();
    assert_eq!(
        gov.try_get_queued(&0),
        Err(Ok(GovernanceError::NotQueued.into())),
        "the rejected queue left no entry behind",
    );
}

/// `cancel` is owner-gated: a stranger's cancel fails and the queued action
/// remains executable after the delay.
#[test]
fn cancel_by_non_owner_rejects() {
    let (fixture, gov_id) = setup();
    let gov = GovernanceClient::new(&fixture.env, &gov_id);

    // Queue under the fixture's blanket owner auth.
    let new_config = config_with_new_min_order_margin();
    let args = set_config_args(&fixture, &new_config);
    let nonce = gov.queue(
        &fixture.market.address,
        &Symbol::new(&fixture.env, "set_config"),
        &args,
    );

    // A non-owner then attempts to cancel with only its own auth mocked.
    let non_owner = Address::generate(&fixture.env);
    fixture.env.mock_auths(&[MockAuth {
        address: &non_owner,
        invoke: &MockAuthInvoke {
            contract: &gov_id,
            fn_name: "cancel",
            args: (nonce,).into_val(&fixture.env),
            sub_invokes: &[],
        },
    }]);

    let result = gov.try_cancel(&nonce);
    assert!(result.is_err(), "a non-owner cannot cancel a queued action");

    // The action survives the rejected cancel: it still executes after the delay.
    fixture.jump(DELAY + 1);
    fixture.env.mock_all_auths_allowing_non_root_auth();
    gov.execute(&nonce);
    assert_eq!(
        fixture.market.get_config().min_order_margin,
        NEW_MIN_ORDER_MARGIN,
        "the queued action remains executable after a rejected cancel",
    );
}

/// Queued actions are independent rows: distinct nonces, out-of-order
/// execution, each applying its own payload.
#[test]
fn second_queued_action_is_independent_of_the_first() {
    let (fixture, gov_id) = setup();
    let gov = GovernanceClient::new(&fixture.env, &gov_id);

    // Two distinct set_config actions, each carrying its own sentinel margin
    // floor (both above exec_fee, which the floor must clear).
    let mut first_config = default_config();
    first_config.min_order_margin = 1_111_111;
    let mut second_config = default_config();
    second_config.min_order_margin = 2_222_222;

    let first_nonce = gov.queue(
        &fixture.market.address,
        &Symbol::new(&fixture.env, "set_config"),
        &set_config_args(&fixture, &first_config),
    );
    let second_nonce = gov.queue(
        &fixture.market.address,
        &Symbol::new(&fixture.env, "set_config"),
        &set_config_args(&fixture, &second_config),
    );
    assert_ne!(
        first_nonce, second_nonce,
        "each queued action gets its own nonce"
    );

    fixture.jump(DELAY + 1);

    // Execute out of order: the second action lands first.
    gov.execute(&second_nonce);
    assert_eq!(
        fixture.market.get_config().min_order_margin,
        2_222_222,
        "the second action applies independently",
    );

    // The first action is still queued and applies afterwards.
    gov.execute(&first_nonce);
    assert_eq!(
        fixture.market.get_config().min_order_margin,
        1_111_111,
        "the first action remains queued and applies on its own",
    );
}

/// `execute` carries no `require_auth`: a matured action lands with only an
/// unrelated signer mocked, since the market authorizes governance as its direct
/// invoker.
#[test]
fn execute_is_permissionless() {
    let (fixture, gov_id) = setup();
    let gov = GovernanceClient::new(&fixture.env, &gov_id);

    let new_config = config_with_new_min_order_margin();
    let args = set_config_args(&fixture, &new_config);
    let nonce = gov.queue(
        &fixture.market.address,
        &Symbol::new(&fixture.env, "set_config"),
        &args,
    );

    // Drop the fixture's blanket mock down to a lone unrelated signer: execute
    // carries no `require_auth`. The market authorizes the nested `set_config`
    // for governance as its direct invoker, so no owner signature is needed.
    fixture.jump(DELAY + 1);
    let stranger = Address::generate(&fixture.env);
    fixture.env.mock_auths(&[MockAuth {
        address: &stranger,
        invoke: &MockAuthInvoke {
            contract: &gov_id,
            fn_name: "get_delay",
            args: ().into_val(&fixture.env),
            sub_invokes: &[],
        },
    }]);

    gov.execute(&nonce);
    assert_eq!(
        fixture.market.get_config().min_order_margin,
        NEW_MIN_ORDER_MARGIN,
        "anyone can execute a matured action without owner authorization",
    );
}

/// A small market-increase order the status tests use to probe whether the market
/// still accepts order creation. Returns the raw `try_` result so callers can
/// pin the rejection.
fn try_create_probe_order(
    fixture: &TestFixture,
    user: &Address,
) -> Result<
    Result<u32, soroban_sdk::ConversionError>,
    Result<soroban_sdk::Error, soroban_sdk::InvokeError>,
> {
    fixture.market.try_create_order(
        user,
        &true,
        &(OrderKind::MarketIncrease as u32),
        &(1_000 * SCALAR_7), // $1,000 notional, above the dust floor
        &(200 * SCALAR_7),   // $200 margin (5x)
        &0,
        &0,
        &fixture.far_expiration(),
    )
}

/// The emergency `set_status` bypasses the timelock entirely: the freeze lands
/// in the same call, order creation immediately rejects `MarketFrozen`, and
/// unfreezing back to `Active` restores it. No step needs a queue or a jump.
#[test]
fn set_status_freezes_and_unfreezes_the_market_without_delay() {
    let (mut fixture, gov_id) = setup();
    let gov = GovernanceClient::new(&fixture.env, &gov_id);
    let user = fixture.new_user(1_000 * SCALAR_7);

    // Baseline: the market is Active and takes orders.
    assert_eq!(
        fixture.market.get_status(),
        STATUS_ACTIVE,
        "market starts Active"
    );
    let id = try_create_probe_order(&fixture, &user)
        .expect("an Active market accepts order creation")
        .expect("the created order id decodes");
    assert!(
        fixture.market.try_get_order(&user, &id).is_ok(),
        "the order rests on the Active market",
    );

    // No queue, no jump: the freeze is visible on the very next read.
    gov.set_status(&fixture.market.address, &STATUS_FROZEN);
    assert_eq!(
        fixture.market.get_status(),
        STATUS_FROZEN,
        "the emergency freeze applies immediately",
    );

    let frozen = try_create_probe_order(&fixture, &user);
    assert_eq!(
        frozen,
        Err(Ok(MarketError::MarketFrozen.into())),
        "a frozen market rejects order creation",
    );

    // Frozen -> Active is a valid transition on an un-delisted market, and it
    // is likewise immediate.
    gov.set_status(&fixture.market.address, &STATUS_ACTIVE);
    assert_eq!(
        fixture.market.get_status(),
        STATUS_ACTIVE,
        "the market is live again"
    );
    let resumed_id = try_create_probe_order(&fixture, &user)
        .expect("order creation resumes after the unfreeze")
        .expect("the created order id decodes");
    assert!(
        fixture.market.try_get_order(&user, &resumed_id).is_ok(),
        "the post-unfreeze order rests",
    );
}

/// `set_status` is owner-gated on governance: with only a stranger's auth
/// mocked the emergency path fails and the market stays `Active`.
#[test]
fn set_status_by_non_owner_rejects() {
    let (fixture, gov_id) = setup();
    let gov = GovernanceClient::new(&fixture.env, &gov_id);
    let non_owner = Address::generate(&fixture.env);

    fixture.env.mock_auths(&[MockAuth {
        address: &non_owner,
        invoke: &MockAuthInvoke {
            contract: &gov_id,
            fn_name: "set_status",
            args: (fixture.market.address.clone(), STATUS_FROZEN).into_val(&fixture.env),
            sub_invokes: &[],
        },
    }]);

    let result = gov.try_set_status(&fixture.market.address, &STATUS_FROZEN);
    assert!(
        result.is_err(),
        "a non-owner cannot trigger the emergency freeze"
    );
    assert_eq!(
        fixture.market.get_status(),
        STATUS_ACTIVE,
        "market status unchanged after the rejected freeze",
    );
}

/// A delay change is itself timelocked at the *current* delay: `set_delay`
/// only records the pending value, `apply_delay` rejects `NotUnlocked` until
/// [`DELAY`] has passed, and the pending change is consumed once applied.
#[test]
fn set_delay_applies_only_after_the_current_delay() {
    let (fixture, gov_id) = setup();
    let gov = GovernanceClient::new(&fixture.env, &gov_id);

    gov.set_delay(&SHORT_DELAY);
    assert_eq!(gov.get_delay(), DELAY, "set_delay only queues the change");

    // Immediately, and one second short of the current delay, the change is
    // still locked (unlock_time = queued_at + DELAY).
    assert_eq!(
        gov.try_apply_delay(),
        Err(Ok(GovernanceError::NotUnlocked.into())),
        "the pending delay cannot be applied at once",
    );
    fixture.jump(DELAY - 1);
    assert_eq!(
        gov.try_apply_delay(),
        Err(Ok(GovernanceError::NotUnlocked.into())),
        "one second short of the current delay is still locked",
    );
    assert_eq!(
        gov.get_delay(),
        DELAY,
        "the delay is untouched while locked"
    );

    // One second past it the change lands and is consumed.
    fixture.jump(2);
    gov.apply_delay();
    assert_eq!(gov.get_delay(), SHORT_DELAY, "the new delay is in force");
    assert_eq!(
        gov.try_apply_delay(),
        Err(Ok(GovernanceError::NotQueued.into())),
        "the pending delay change cannot be applied twice",
    );
}

/// Shortening the delay never shortens a wait that is already running: a
/// queued action keeps the `unlock_time` stamped at queue time.
///
/// Timeline (t relative to the start, [`DELAY`] = 3600s, [`SHORT_DELAY`] = 60s):
/// - t=0: `set_delay(60)` queues, unlocking at t=3600 (the *current* delay).
/// - t=1800: action A is queued under the old delay, so it unlocks at t=5400.
/// - t=3601: `apply_delay` lands and `get_delay` is 60. A has now been queued
///   for 1801s, thirty times the new delay. It is still locked, because
///   its stored unlock_time (5400) is what gates it.
/// - t=3661: action B, queued at t=3601 under the new 60s delay, executes.
///   A is still locked. The new delay governs new actions only.
/// - t=5401: A finally executes, having served its full original 3600s wait.
#[test]
fn shortened_delay_does_not_shorten_an_already_queued_action() {
    let (fixture, gov_id) = setup();
    let gov = GovernanceClient::new(&fixture.env, &gov_id);
    let set_config = Symbol::new(&fixture.env, "set_config");

    // t=0: queue the delay reduction.
    gov.set_delay(&SHORT_DELAY);

    // t=1800: action A, queued while the 3600s delay is still in force.
    fixture.jump(1_800);
    let config_a = config_with_new_min_order_margin();
    let nonce_a = gov.queue(
        &fixture.market.address,
        &set_config,
        &set_config_args(&fixture, &config_a),
    );

    // t=3601: the delay reduction applies.
    fixture.jump(1_801);
    gov.apply_delay();
    assert_eq!(
        gov.get_delay(),
        SHORT_DELAY,
        "the shortened delay is in force"
    );
    assert_eq!(
        gov.try_execute(&nonce_a),
        Err(Ok(GovernanceError::NotUnlocked.into())),
        "A keeps its original unlock time despite the shortened delay",
    );
    assert_eq!(
        gov.get_queued(&nonce_a).unlock_time,
        fixture.env.ledger().timestamp() + 1_799,
        "A still has 1,799s of its original wait left",
    );

    // t=3601: action B is queued under the new delay and matures in 60s.
    let mut config_b = default_config();
    config_b.min_order_margin = 3_000_000;
    let nonce_b = gov.queue(
        &fixture.market.address,
        &set_config,
        &set_config_args(&fixture, &config_b),
    );

    // t=3661: B executes. A is untouched by B's shorter clock.
    fixture.jump(60);
    gov.execute(&nonce_b);
    assert_eq!(
        fixture.market.get_config().min_order_margin,
        3_000_000,
        "the action queued under the new delay applies after 60s",
    );
    assert_eq!(
        gov.try_execute(&nonce_a),
        Err(Ok(GovernanceError::NotUnlocked.into())),
        "A is still locked while a later-queued action has already executed",
    );

    // t=5401: A's full original wait is served and it executes.
    fixture.jump(1_740);
    gov.execute(&nonce_a);
    assert_eq!(
        fixture.market.get_config().min_order_margin,
        NEW_MIN_ORDER_MARGIN,
        "A executes only after its full original delay",
    );
}

/// A queued action outlives a week-long wait: the entry still reads back with
/// its original unlock time and executes.
#[test]
fn queued_action_survives_a_week_long_wait() {
    let (fixture, gov_id) = setup();
    let gov = GovernanceClient::new(&fixture.env, &gov_id);

    let new_config = config_with_new_min_order_margin();
    let args = set_config_args(&fixture, &new_config);
    let queued_at = fixture.env.ledger().timestamp();
    let nonce = gov.queue(
        &fixture.market.address,
        &Symbol::new(&fixture.env, "set_config"),
        &args,
    );

    fixture.jump(ONE_WEEK);
    assert_eq!(
        gov.get_queued(&nonce).unlock_time,
        queued_at + DELAY,
        "the entry is still queued after a week",
    );
    gov.execute(&nonce);
    assert_eq!(
        fixture.market.get_config().min_order_margin,
        NEW_MIN_ORDER_MARGIN,
        "the action executes after a week-long wait",
    );
}

/// A pending delay change outlives a week-long wait and still applies.
#[test]
fn pending_delay_change_survives_a_week_long_wait() {
    let (fixture, gov_id) = setup();
    let gov = GovernanceClient::new(&fixture.env, &gov_id);

    gov.set_delay(&SHORT_DELAY);
    fixture.jump(ONE_WEEK);
    gov.apply_delay();
    assert_eq!(
        gov.get_delay(),
        SHORT_DELAY,
        "the delay change applies after a week-long wait",
    );
}
