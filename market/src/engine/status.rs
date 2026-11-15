//! Operational status: the five-state lifecycle enum [`Status`], its
//! transition rules, and the status entry bodies [`set_status`] and
//! [`set_terminal_price`].

use crate::engine::constants::{DELIST_DEADLINE, DELIST_GRACE};
use crate::errors::MarketError;
use crate::events::{StatusUpdate, TerminalPriceUpdate};
use crate::storage;
use soroban_sdk::{panic_with_error, token, Env};

/// The contract's operational status. It is an instance storage singleton,
/// stored as its `u32` discriminant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Status {
    /// Normal trading.
    Active = 0,
    /// Opens are blocked. Everything else keeps running.
    OnIce = 1,
    /// Every price-bearing and fund-moving path is halted.
    Frozen = 2,
    /// The wind-down. Opens are blocked. The delist is revertible within
    /// [`DELIST_GRACE`]. After that, the owner can maintain a flat settlement
    /// price. Once [`DELIST_DEADLINE`] passes, positions can be
    /// force-liquidated.
    Delisted = 3,
    /// Defunct. Only funding claims, direct vault redeems, and cancels remain
    /// live.
    Retired = 4,
}

impl Status {
    /// Decodes a raw `u32` status discriminant.
    ///
    /// # Errors
    /// - [`MarketError::InvalidStatus`] if `raw` is not a status discriminant.
    pub(crate) fn from_u32(e: &Env, raw: u32) -> Status {
        match raw {
            0 => Status::Active,
            1 => Status::OnIce,
            2 => Status::Frozen,
            3 => Status::Delisted,
            4 => Status::Retired,
            _ => panic_with_error!(e, MarketError::InvalidStatus),
        }
    }

    /// Returns whether the status accepts size-growing increase fills. Only
    /// [`Status::Active`] does.
    pub fn allows_open(&self) -> bool {
        matches!(self, Status::Active)
    }

    /// Checks an admin [`set_status`] transition. It is the pure transition
    /// core and touches no state.
    ///
    /// # Arguments
    /// - `to`: the target status.
    /// - `delisted_at`: the stored timestamp (seconds) of the current
    ///   wind-down's first delist, or `None` outside a wind-down.
    /// - `now`: the ledger timestamp (seconds).
    ///
    /// # Errors
    /// - [`MarketError::InvalidStatus`] on a same-status set, any transition
    ///   out of [`Status::Retired`], or a trading status after the grace
    ///   window expired.
    fn check_transition(
        &self,
        to: &Status,
        delisted_at: Option<u64>,
        now: u64,
    ) -> Result<(), MarketError> {
        match (self, to) {
            // Retired is final. A same-status set is an operator error.
            (Status::Retired, _) => Err(MarketError::InvalidStatus),
            (from, to) if from == to => Err(MarketError::InvalidStatus),
            // The emergency freeze, the wind-down entry, and retirement are
            // always reachable. The `set_status` body, not this matrix, gates
            // retirement on the empty book.
            (_, Status::Frozen | Status::Delisted | Status::Retired) => Ok(()),
            // The trading statuses are reachable only while un-delisted or
            // within the grace window.
            (_, Status::Active | Status::OnIce) => match delisted_at {
                Some(at) if now >= at.saturating_add(DELIST_GRACE) => {
                    Err(MarketError::InvalidStatus)
                }
                _ => Ok(()),
            },
        }
    }
}

/// Returns `true` once the delist deadline has passed, which unlocks forced
/// liquidation.
pub(crate) fn deadline_passed(e: &Env) -> bool {
    storage::get_delisted_at(e)
        .is_some_and(|at| e.ledger().timestamp() >= at.saturating_add(DELIST_DEADLINE))
}

/// Returns `true` once the delist grace window has expired, which unlocks
/// the flat settlement price.
fn grace_expired(e: &Env) -> bool {
    storage::get_delisted_at(e)
        .is_some_and(|at| e.ledger().timestamp() >= at.saturating_add(DELIST_GRACE))
}

/// Applies an admin status transition. Refer to
/// [`Market::set_status`](crate::Market::set_status).
pub fn set_status(e: &Env, status: &Status) {
    let delisted_at = storage::get_delisted_at(e);
    let now = e.ledger().timestamp();
    storage::get_status(e)
        .check_transition(status, delisted_at, now)
        .unwrap_or_else(|err| panic_with_error!(e, err));

    match status {
        // The grace window anchors at the first delist only. A freeze and
        // unfreeze during the wind-down does not restart it.
        Status::Delisted => {
            if delisted_at.is_none() {
                storage::set_delisted_at(e, now);
            }
        }
        // Reverting to a trading status ends the wind-down. The grace anchor
        // is dropped, so a later delist starts a fresh window.
        Status::Active | Status::OnIce => {
            if delisted_at.is_some() {
                storage::remove_delisted_at(e);
            }
        }
        Status::Retired => retire(e),
        Status::Frozen => {}
    }

    storage::set_status(e, status);
    StatusUpdate {
        status: *status as u32,
    }
    .publish(e);
}

/// Runs the retirement clear-out. It requires an empty book and sweeps the
/// credit-pool surplus to the vault.
///
/// After the sweep the pool exactly backs `credit_owed`. A pool ledger
/// beyond the actual token balance traps the transfer. Adding tokens to
/// the contract balance unblocks retirement.
///
/// # Errors
/// - [`MarketError::MarketNotCleared`] if any position remains open.
fn retire(e: &Env) {
    let mut data = storage::get_market_data(e);
    if data.notional.total() != 0 || data.tokens.total() != 0 || data.margin.total() != 0 {
        panic_with_error!(e, MarketError::MarketNotCleared);
    }

    let surplus = data.credit_pool - data.credit_owed;
    if surplus > 0 {
        let token = token::Client::new(e, &storage::get_token(e));
        token.transfer(
            &e.current_contract_address(),
            storage::get_vault(e),
            &surplus,
        );
        data.credit_pool = data.credit_owed;
        storage::set_market_data(e, &data);
    }
}

/// Sets or refreshes the flat settlement price of a delisted market. Refer
/// to [`Market::set_terminal_price`](crate::Market::set_terminal_price).
pub fn set_terminal_price(e: &Env, price: i128) {
    if storage::get_status(e) != Status::Delisted {
        panic_with_error!(e, MarketError::InvalidStatus);
    }
    if !grace_expired(e) {
        panic_with_error!(e, MarketError::InvalidStatus);
    }
    if price <= 0 {
        panic_with_error!(e, MarketError::InvalidPrice);
    }
    storage::set_terminal_price(e, price);
    TerminalPriceUpdate { price }.publish(e);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::constants::PRICE_SCALAR;

    const DELISTED_AT: Option<u64> = Some(1_000);
    const IN_GRACE: u64 = 1_000 + DELIST_GRACE - 1;
    const PAST_GRACE: u64 = 1_000 + DELIST_GRACE;

    // The admin set {Active, OnIce, Frozen} interchanges freely on a live market.
    #[test]
    fn admin_statuses_interchange() {
        assert_eq!(
            Status::Active.check_transition(&Status::OnIce, None, 0),
            Ok(())
        );
        assert_eq!(
            Status::OnIce.check_transition(&Status::Frozen, None, 0),
            Ok(())
        );
        assert_eq!(
            Status::Frozen.check_transition(&Status::Active, None, 0),
            Ok(())
        );
        assert_eq!(
            Status::Active.check_transition(&Status::Frozen, None, 0),
            Ok(())
        );
    }

    // A same-status set is an operator error.
    #[test]
    fn same_status_rejected() {
        assert_eq!(
            Status::Active.check_transition(&Status::Active, None, 0),
            Err(MarketError::InvalidStatus)
        );
        assert_eq!(
            Status::Delisted.check_transition(&Status::Delisted, DELISTED_AT, IN_GRACE),
            Err(MarketError::InvalidStatus)
        );
    }

    // Delisting and freezing stay available throughout the wind-down.
    #[test]
    fn delist_and_freeze_reachable_from_any_live_status() {
        assert_eq!(
            Status::Active.check_transition(&Status::Delisted, None, 0),
            Ok(())
        );
        assert_eq!(
            Status::Frozen.check_transition(&Status::Delisted, DELISTED_AT, PAST_GRACE),
            Ok(())
        );
        assert_eq!(
            Status::Delisted.check_transition(&Status::Frozen, DELISTED_AT, PAST_GRACE),
            Ok(())
        );
        assert_eq!(
            Status::OnIce.check_transition(&Status::Frozen, None, 0),
            Ok(())
        );
    }

    // Within the grace window a delist can still be reverted.
    #[test]
    fn undelist_allowed_within_grace() {
        assert_eq!(
            Status::Delisted.check_transition(&Status::Active, DELISTED_AT, IN_GRACE),
            Ok(())
        );
        assert_eq!(
            Status::Frozen.check_transition(&Status::OnIce, DELISTED_AT, IN_GRACE),
            Ok(())
        );
    }

    // Once the grace window expires the trading statuses are gone for good.
    #[test]
    fn trading_statuses_unreachable_after_grace() {
        assert_eq!(
            Status::Delisted.check_transition(&Status::Active, DELISTED_AT, PAST_GRACE),
            Err(MarketError::InvalidStatus)
        );
        assert_eq!(
            Status::Frozen.check_transition(&Status::OnIce, DELISTED_AT, PAST_GRACE),
            Err(MarketError::InvalidStatus)
        );
    }

    // Retirement is reachable from any status. The set_status body enforces
    // the empty-book clear-out.
    #[test]
    fn retired_reachable_from_any_status() {
        assert_eq!(
            Status::Delisted.check_transition(&Status::Retired, DELISTED_AT, IN_GRACE),
            Ok(())
        );
        assert_eq!(
            Status::Frozen.check_transition(&Status::Retired, DELISTED_AT, PAST_GRACE),
            Ok(())
        );
        assert_eq!(
            Status::Active.check_transition(&Status::Retired, None, 0),
            Ok(())
        );
    }

    // Retired is final.
    #[test]
    fn retired_is_final() {
        assert_eq!(
            Status::Retired.check_transition(&Status::Active, DELISTED_AT, PAST_GRACE),
            Err(MarketError::InvalidStatus)
        );
        assert_eq!(
            Status::Retired.check_transition(&Status::Frozen, DELISTED_AT, PAST_GRACE),
            Err(MarketError::InvalidStatus)
        );
    }

    // Only Active accepts opens.
    #[test]
    fn only_active_allows_open() {
        assert!(Status::Active.allows_open());
        assert!(!Status::OnIce.allows_open());
        assert!(!Status::Frozen.allows_open());
        assert!(!Status::Delisted.allows_open());
        assert!(!Status::Retired.allows_open());
    }

    use crate::engine::constants::BASE_UNIT;
    use crate::engine::market_data::{MarketData, SidePair};
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::{contract, Address};

    #[contract]
    struct Harness;

    // The first delist anchors the grace window. A freeze and unfreeze during
    // the wind-down keeps the original anchor.
    #[test]
    fn delist_anchors_grace_once() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            storage::set_status(&e, &Status::Active);
            e.ledger().with_mut(|ledger| ledger.timestamp = 500);
            set_status(&e, &Status::Delisted);
            assert_eq!(storage::get_delisted_at(&e), Some(500));

            e.ledger().with_mut(|ledger| ledger.timestamp = 600);
            set_status(&e, &Status::Frozen);
            set_status(&e, &Status::Delisted);
            assert_eq!(storage::get_delisted_at(&e), Some(500));
        });
    }

    // An in-grace revert clears the anchor. A later delist starts fresh.
    #[test]
    fn undelist_clears_grace_anchor() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            storage::set_status(&e, &Status::Active);
            e.ledger().with_mut(|ledger| ledger.timestamp = 500);
            set_status(&e, &Status::Delisted);
            set_status(&e, &Status::Active);
            assert_eq!(storage::get_delisted_at(&e), None);

            e.ledger().with_mut(|ledger| ledger.timestamp = 900);
            set_status(&e, &Status::Delisted);
            assert_eq!(storage::get_delisted_at(&e), Some(900));
        });
    }

    // With the book cleared, retiring sweeps the funding-pool surplus
    // (pool 500 - owed 300 = 200) to the vault. The rest of the contract
    // balance keeps backing the owed funding and any escrowed orders.
    #[test]
    fn retire_sweeps_surplus_and_marks_retired() {
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let vault = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &1_000);

        e.as_contract(&contract_id, || {
            storage::set_token(&e, &token.address());
            storage::set_vault(&e, &vault);
            storage::set_status(&e, &Status::Active);
            storage::set_market_data(
                &e,
                &MarketData {
                    credit_pool: 500,
                    credit_owed: 300,
                    ..Default::default()
                },
            );
            set_status(&e, &Status::Retired);

            assert_eq!(storage::get_status(&e), Status::Retired);
            let data = storage::get_market_data(&e);
            assert_eq!(data.credit_pool, 300);
            assert_eq!(data.credit_owed, 300);
        });
        assert_eq!(
            token::Client::new(&e, &token.address()).balance(&vault),
            200
        );
    }

    #[test]
    #[should_panic(expected = "#706")]
    fn retire_rejects_open_positions() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            storage::set_status(&e, &Status::Active);
            let mut data = MarketData::default();
            data.notional.long = 1;
            storage::set_market_data(&e, &data);
            set_status(&e, &Status::Retired);
        });
    }

    // The emptiness check sums both sides, so a balanced book is still open:
    // 50.0 long against 50.0 short, with equal tokens and margin, blocks
    // retirement.
    #[test]
    #[should_panic(expected = "#706")]
    fn retire_rejects_a_balanced_book() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            storage::set_status(&e, &Status::Active);
            storage::set_market_data(
                &e,
                &MarketData {
                    notional: SidePair {
                        long: 50_0000000,
                        short: 50_0000000,
                    },
                    tokens: SidePair {
                        long: 5 * BASE_UNIT,
                        short: 5 * BASE_UNIT,
                    },
                    margin: SidePair {
                        long: 10_0000000,
                        short: 10_0000000,
                    },
                    ..Default::default()
                },
            );
            set_status(&e, &Status::Retired);
        });
    }

    // Margin left on the book blocks retirement even with no size behind it.
    // The book clears only when notional, tokens, and margin are all zero.
    #[test]
    #[should_panic(expected = "#706")]
    fn retire_rejects_residual_margin() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            storage::set_status(&e, &Status::Active);
            let mut data = MarketData::default();
            data.margin.short = 1;
            storage::set_market_data(&e, &data);
            set_status(&e, &Status::Retired);
        });
    }

    // A pool that exactly backs what it owes has no surplus to sweep, so
    // retirement moves no tokens and the token contract emits nothing.
    #[test]
    fn retire_without_surplus_moves_no_tokens() {
        use soroban_sdk::testutils::Events as _;
        let e = Env::default();
        e.mock_all_auths();
        let contract_id = e.register(Harness, ());
        let token_admin = Address::generate(&e);
        let vault = Address::generate(&e);
        let token = e.register_stellar_asset_contract_v2(token_admin);
        token::StellarAssetClient::new(&e, &token.address()).mint(&contract_id, &300);

        e.as_contract(&contract_id, || {
            storage::set_token(&e, &token.address());
            storage::set_vault(&e, &vault);
            storage::set_status(&e, &Status::Active);
            storage::set_market_data(
                &e,
                &MarketData {
                    credit_pool: 300,
                    credit_owed: 300,
                    ..Default::default()
                },
            );
            set_status(&e, &Status::Retired);
            assert_eq!(storage::get_status(&e), Status::Retired);
        });
        assert_eq!(
            e.events()
                .all()
                .filter_by_contract(&token.address())
                .events()
                .len(),
            0,
            "no transfer, not even a zero one"
        );
        assert_eq!(
            token::Client::new(&e, &token.address()).balance(&contract_id),
            300
        );
    }

    // Once the grace window expires the flat settlement price can be set
    // and refreshed.
    #[test]
    fn set_terminal_price_sets_and_refreshes() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            storage::set_status(&e, &Status::Delisted);
            storage::set_delisted_at(&e, 0);
            e.ledger()
                .with_mut(|ledger| ledger.timestamp = DELIST_GRACE);
            set_terminal_price(&e, 95 * PRICE_SCALAR / 10);
            assert_eq!(storage::get_terminal_price(&e), 95 * PRICE_SCALAR / 10);
            set_terminal_price(&e, 92 * PRICE_SCALAR / 10);
            assert_eq!(storage::get_terminal_price(&e), 92 * PRICE_SCALAR / 10);
        });
    }

    #[test]
    #[should_panic(expected = "#702")]
    fn set_terminal_price_rejects_within_grace() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            storage::set_status(&e, &Status::Delisted);
            storage::set_delisted_at(&e, 0);
            e.ledger()
                .with_mut(|ledger| ledger.timestamp = DELIST_GRACE - 1);
            set_terminal_price(&e, 95 * PRICE_SCALAR / 10);
        });
    }

    #[test]
    #[should_panic(expected = "#702")]
    fn set_terminal_price_rejects_outside_delisted() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            storage::set_status(&e, &Status::Active);
            set_terminal_price(&e, 95 * PRICE_SCALAR / 10);
        });
    }

    #[test]
    #[should_panic(expected = "#701")]
    fn set_terminal_price_rejects_non_positive_price() {
        let e = Env::default();
        let contract_id = e.register(Harness, ());
        e.as_contract(&contract_id, || {
            storage::set_status(&e, &Status::Delisted);
            storage::set_delisted_at(&e, 0);
            e.ledger()
                .with_mut(|ledger| ledger.timestamp = DELIST_DEADLINE);
            set_terminal_price(&e, 0);
        });
    }
}
