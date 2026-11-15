//! Harness self-tests.
//!
//! The failure mode that quietly kills a fuzz harness is not a crash: it is
//! every call bouncing off a gate, so the fuzzer burns CPU on the rejection
//! paths and the invariants are never exercised against real state. These
//! tests pin the reachability the stateful targets depend on: a fill lands, a
//! close settles, funding accrues and is claimable, and an LP can deposit.
//! They double as the check that `Ledger` accepts a legitimate protocol run.

use crate::codes;
use crate::fixture::{Fixture, SCALAR_7};
use crate::invariants::{effective_book, position, Ledger, Snapshot};
use crate::price::Oracle;

/// Bootstrap must seed each feed's own mark, including on the first protective
/// load when a valid submitted report predates the seed deposit's cached price.
#[test]
fn bootstrap_cache_matches_the_selected_feed_on_protective_load() {
    use test_suites::chainlink_helper::{build_report, ReportTimes};

    for (f, expected) in [
        (Fixture::create(), 100_000_000_000_000_000_000_000i128),
        (Fixture::create_testnet(), 180_000_000_000_000_000),
    ] {
        let now = f.env.ledger().timestamp();
        let submitted = expected / 2;
        let older = build_report(
            &f.env,
            &f.feed,
            ReportTimes::at(now - 1),
            submitted,
            submitted,
            submitted,
        );
        f.env.as_contract(&f.market_address, || {
            let effective = market::engine::market::Market::load(&f.env, &older, true, true).price;
            assert_eq!(
                (effective.bid, effective.ask, effective.publish_time),
                (expected, expected, now),
                "protective load uses the bootstrap feed's mark"
            );
            let cached = market::storage::get_price_cache(&f.env).unwrap();
            assert_eq!(
                (cached.bid, cached.ask, cached.publish_time),
                (expected, expected, now),
                "the older report preserves the bootstrap cache"
            );
        });
    }
}

/// The u16 stress grammar covers $1 through $65,536. Base atoms retain the
/// settlement token's seven decimals. Normal sizes and one-atom edges both fill.
#[test]
fn stress_notional_bounds_fill_at_production_price_precision() {
    use crate::price::{MAX_PRICE, MIN_PRICE, PRICE_SCALAR, START_PRICE};

    for price in [
        MIN_PRICE,
        START_PRICE,
        START_PRICE + PRICE_SCALAR,
        MAX_PRICE,
    ] {
        for units in [0u16, 999, u16::MAX] {
            let f = Fixture::create();
            let oracle = Oracle {
                mid: price,
                ..Oracle::default()
            };
            let notional = (i128::from(units) + 1) * SCALAR_7;
            for (idx, is_long) in [(0, true), (1, false)] {
                open(&f, &oracle, idx, is_long, notional, notional / 2);
                let p = position(&f, &f.user(idx), is_long);
                let numerator = notional * PRICE_SCALAR;
                let expected = if is_long {
                    numerator / price
                } else {
                    (numerator + price - 1) / price
                };
                assert_eq!(p.notional, notional);
                assert_eq!(
                    p.tokens, expected,
                    "base atoms: {units}, {price}, {is_long}"
                );
                assert!(p.tokens > 0);
                if price == START_PRICE {
                    assert_eq!(p.tokens, (i128::from(units) + 1) * 100);
                }
                // The same-dollar mark expressed at 8 or 18 decimals yields
                // identical base atoms when its matching scalar is used.
                let old_price = price / 10_000_000_000;
                let old_numerator = notional * 100_000_000;
                let old_tokens = if is_long {
                    old_numerator / old_price
                } else {
                    (old_numerator + old_price - 1) / old_price
                };
                assert_eq!(p.tokens, old_tokens);
            }
        }
    }
}

/// Open a market increase and fill it in the same ledger.
fn open(f: &Fixture, oracle: &Oracle, idx: usize, is_long: bool, notional: i128, margin: i128) {
    let user = f.user(idx);
    let id = f.market.create_order(
        &user,
        &is_long,
        &0u32,
        &notional,
        &margin,
        &0,
        &0,
        &f.far_expiration(),
    );
    f.market
        .execute_order(&f.keeper, &user, &id, &oracle.fresh_bytes(&f.env));
}

/// A fill lands, posts margin net of fees, and leaves the settlement identity
/// intact.
#[test]
fn a_market_increase_fills_and_posts_margin() {
    let f = Fixture::create();
    let oracle = Oracle::default();
    let mut ledger = Ledger::new(f.users.len());

    let notional = 1_000 * SCALAR_7;
    let margin = 200 * SCALAR_7;
    open(&f, &oracle, 0, true, notional, margin);

    let p = position(&f, &f.user(0), true);
    assert_eq!(p.notional, notional, "position notional");
    assert!(p.tokens > 0, "position holds base size");
    assert!(
        p.margin > 0 && p.margin < margin,
        "margin {} is posted net of fees, under the {margin} escrowed",
        p.margin
    );

    ledger.check(&f, "after fill");
}

/// A full close settles, returns the book to flat, and leaves the contract
/// holding nothing but the funding pool.
#[test]
fn a_full_close_returns_the_book_to_flat() {
    let f = Fixture::create();
    let oracle = Oracle::default();
    let mut ledger = Ledger::new(f.users.len());

    open(&f, &oracle, 0, true, 1_000 * SCALAR_7, 200 * SCALAR_7);
    ledger.check(&f, "after fill");

    // Clear the decrease lock, then close the whole position.
    f.jump(3_600);
    let user = f.user(0);
    close(&f, &oracle, 0, true);

    assert_eq!(position(&f, &user, true).notional, 0, "position closed");
    ledger.check(&f, "after close");

    let snapshot = Snapshot::take(&f);
    assert_eq!(
        f.contract_balance(),
        snapshot.market.credit_pool,
        "a flat book with no live orders holds only the funding pool"
    );
}

/// A one-sided book charges the open side and banks the payments as pool
/// surplus, since there is no counter-side to credit. This is the state the
/// `credit_pool >= credit_owed` invariant exists to police, so the harness
/// must actually be able to reach it.
#[test]
fn a_one_sided_book_accrues_funding_into_pool_surplus() {
    let f = Fixture::create();
    let oracle = Oracle::default();
    let mut ledger = Ledger::new(f.users.len());

    open(&f, &oracle, 0, true, 10_000 * SCALAR_7, 2_000 * SCALAR_7);

    // A day of one-sided book: the funding rate ramps at full skew.
    f.jump(86_400);
    f.market.accrue(&oracle.bytes(&f.env));
    let market = Snapshot::take(&f).market;
    assert!(
        market.funding_rate > 0,
        "longs alone should ramp the rate positive, got {}",
        market.funding_rate
    );
    assert!(
        market.funding_idx.long > 0,
        "the paying side's index should have moved, got {}",
        market.funding_idx.long
    );
    assert_eq!(
        market.funding_idx.short, 0,
        "an empty counter-side is credited nothing"
    );

    // Realize the accrual by touching the position, then confirm the payment
    // landed in the pool with nothing owed against it.
    f.jump(3_600);
    let user = f.user(0);
    let id = f.market.create_order(
        &user,
        &true,
        &3u32,
        &i128::MAX,
        &0,
        &0,
        &0,
        &f.far_expiration(),
    );
    f.market
        .execute_order(&f.keeper, &user, &id, &oracle.fresh_bytes(&f.env));

    let market = Snapshot::take(&f).market;
    assert!(
        market.credit_pool > 0,
        "the payer's funding should have banked into the pool"
    );
    assert_eq!(
        market.credit_owed, 0,
        "nothing was credited, so nothing is owed"
    );
    ledger.check(&f, "after one-sided funding");
}

/// Funding flows from the dominant side to the thin side and lands as a
/// claimable balance, so the claim path and the `credit_owed == sum of
/// claimable` invariant are both reachable.
#[test]
fn funding_credits_the_thin_side_and_can_be_claimed() {
    let f = Fixture::create();
    let oracle = Oracle::default();
    let mut ledger = Ledger::new(f.users.len());

    open(&f, &oracle, 0, true, 20_000 * SCALAR_7, 4_000 * SCALAR_7);
    open(&f, &oracle, 1, false, 2_000 * SCALAR_7, 400 * SCALAR_7);

    f.jump(86_400);
    f.market.accrue(&oracle.bytes(&f.env));

    // Touch both positions so their accruals realize: the long banks its
    // payment, the short's credit becomes claimable.
    f.jump(3_600);
    for (idx, is_long) in [(0usize, true), (1usize, false)] {
        let user = f.user(idx);
        let id = f.market.create_order(
            &user,
            &is_long,
            &3u32,
            &i128::MAX,
            &0,
            &0,
            &0,
            &f.far_expiration(),
        );
        let filled = f
            .market
            .try_execute_order(&f.keeper, &user, &id, &oracle.fresh_bytes(&f.env));
        codes::check(&filled, codes::EXECUTE_ORDER, true, "close");
    }

    let short = f.user(1);
    let claimable = Snapshot::take(&f)
        .users
        .iter()
        .find(|u| u.address == short)
        .map(|u| u.claimable)
        .expect("the short trader is in the snapshot");
    assert!(
        claimable > 0,
        "the receiving side should have accrued a claimable balance"
    );

    let paid = f.market.claim_credit(&short);
    assert_eq!(paid, claimable, "the claim pays the whole balance");
    ledger.check(&f, "after claim");
}

/// An LP can deposit and be minted shares, which is what `fuzz_vault_orders`
/// spends its run on.
#[test]
fn a_vault_deposit_fills_and_mints_shares() {
    let f = Fixture::create();
    let oracle = Oracle::default();
    let mut ledger = Ledger::new(f.users.len());

    let lp = f.user(0);
    let id = f
        .market
        .create_vault_order(&lp, &0u32, &(1_000 * SCALAR_7), &0);
    assert!(id > 0, "a live market queues the deposit");
    ledger.check(&f, "after deposit escrow");

    // The fill must strictly postdate the order's creation.
    f.jump(10);
    f.market
        .execute_vault_order(&f.keeper, &lp, &id, &oracle.fresh_bytes(&f.env));

    assert!(f.vault.balance(&lp) > 0, "the depositor was minted shares");
    assert_eq!(
        f.vault.total_supply(),
        f.vault.balance(&lp) + f.vault.balance(&f.seed_lp) + f.vault.balance(&f.market_address),
        "share supply matches the holders"
    );
    ledger.check(&f, "after deposit fill");
}

/// A position pushed under maintenance margin is liquidatable, so
/// `fuzz_liquidation` can reach the forced-closure paths rather than bouncing
/// off `NotLiquidatable` for its whole run.
#[test]
fn a_price_move_makes_a_leveraged_long_liquidatable() {
    let f = Fixture::create();
    let mut oracle = Oracle::default();
    let mut ledger = Ledger::new(f.users.len());

    // 15% margin, above the 10% initial-margin floor, so the 20% drop below
    // wipes the equity.
    open(&f, &oracle, 0, true, 10_000 * SCALAR_7, 1_500 * SCALAR_7);

    oracle.mid = oracle.mid * 80 / 100;
    let user = f.user(0);
    f.market
        .execute_liquidation(&f.keeper, &user, &true, &oracle.fresh_bytes(&f.env));

    assert_eq!(
        position(&f, &user, true).notional,
        0,
        "the liquidation closed the position"
    );
    ledger.check(&f, "after liquidation");
}

/// A winning side arms ADL and can then be deleveraged.
///
/// This is the whole point of `fuzz_liquidation`: if the harness cannot get a
/// side over the trigger, `execute_adl` never gets past `AdlNotTriggered` and
/// the ADL gates go unfuzzed. Arming needs the side's pending PnL above
/// `adl_max_pnl` (50%) of half the vault, so it takes a large position and a
/// large move. That is exactly the combination the target has to be able to
/// reach.
#[test]
fn a_winning_side_arms_adl_and_can_be_deleveraged() {
    let f = Fixture::create();
    let mut oracle = Oracle::default();
    let mut ledger = Ledger::new(f.users.len());

    // $65k of long at ~7x, just inside the utilization cap on a $200k vault.
    open(&f, &oracle, 0, true, 65_000 * SCALAR_7, 9_000 * SCALAR_7);

    // Double the mark and clear the decrease lock.
    oracle.mid *= 2;
    f.jump(30);

    let state = f.market.update_adl_state(&oracle.fresh_bytes(&f.env));
    assert!(state.long, "a doubled long should arm the long side");
    assert!(!state.short, "the empty short side stays disarmed");
    ledger.check_adl_armed(
        &f,
        oracle.bid(),
        oracle.ask(),
        oracle.publish_time(&f.env),
        "after arming",
    );

    let user = f.user(0);
    let before = position(&f, &user, true).notional;
    let slice = before / 5;
    let payout = f
        .market
        .execute_adl(&f.keeper, &user, &true, &slice, &oracle.fresh_bytes(&f.env));
    assert!(payout >= 0, "the keeper reward is non-negative");

    let after = position(&f, &user, true).notional;
    assert!(
        after < before && after > 0,
        "the slice deleveraged part of the position: {before} -> {after}"
    );
    ledger.check(&f, "after ADL");
}

/// Arms ADL on `is_long` with a fresh doubled (long) or halved (short) mark,
/// then submits an older report at the opening mark. The contract prices the
/// older report at its newer cache, so the model must accept the armed side.
fn arm_then_submit_older(is_long: bool) {
    let f = Fixture::create();
    let mut oracle = Oracle::default();
    let ledger = Ledger::new(f.users.len());
    let start = oracle.mid;
    open(&f, &oracle, 0, is_long, 65_000 * SCALAR_7, 9_000 * SCALAR_7);

    oracle.mid = if is_long { start * 2 } else { start / 2 };
    f.jump(30);
    f.market.update_adl_state(&oracle.fresh_bytes(&f.env));
    let fresh = (oracle.bid(), oracle.ask());
    ledger.check_adl_armed(
        &f,
        fresh.0,
        fresh.1,
        oracle.publish_time(&f.env),
        "fresh mark",
    );

    // One second older than the cache, still inside the close window.
    oracle.mid = start;
    oracle.stale_secs = 1;
    f.market.update_adl_state(&oracle.bytes(&f.env));
    let submitted = (oracle.bid(), oracle.ask());
    let publish_time = oracle.publish_time(&f.env);
    assert_eq!(
        effective_book(&f, submitted.0, submitted.1, publish_time),
        fresh,
        "the older report is upgraded to the cached mark"
    );
    ledger.check_adl_armed(&f, submitted.0, submitted.1, publish_time, "cached mark");
}

/// An older in-window report on a substitution route models at the cached
/// mark, so a correct contract does not fail [`Ledger::check_adl_armed`]
/// (long side).
#[test]
fn cached_adl_mark_agrees_with_the_contract() {
    arm_then_submit_older(true);
}

/// The same trace on the short side.
#[test]
fn cached_adl_mark_agrees_with_the_contract_for_a_short() {
    arm_then_submit_older(false);
}

/// A report newer than the cache is priced as submitted: the cache advances
/// to it and the model must not reach back to the older mark.
#[test]
fn a_newer_report_is_priced_as_submitted() {
    let f = Fixture::create();
    let mut oracle = Oracle::default();
    let ledger = Ledger::new(f.users.len());
    open(&f, &oracle, 0, true, 65_000 * SCALAR_7, 9_000 * SCALAR_7);

    oracle.mid *= 2;
    f.jump(30);
    f.market.update_adl_state(&oracle.fresh_bytes(&f.env));

    // A later, fresh report at a different mark supersedes the cache.
    f.jump(30);
    oracle.mid = oracle.mid * 3 / 2;
    f.market.update_adl_state(&oracle.fresh_bytes(&f.env));
    let submitted = (oracle.bid(), oracle.ask());
    let publish_time = oracle.publish_time(&f.env);
    assert_eq!(
        effective_book(&f, submitted.0, submitted.1, publish_time),
        submitted,
        "the newer report is the mark"
    );
    ledger.check_adl_armed(&f, submitted.0, submitted.1, publish_time, "newer mark");
}

/// Fill a full market decrease of `idx`'s `is_long` position.
fn close(f: &Fixture, oracle: &Oracle, idx: usize, is_long: bool) {
    let user = f.user(idx);
    let id = f.market.create_order(
        &user,
        &is_long,
        &3u32, // MarketDecrease
        &i128::MAX,
        &0,
        &0,
        &0,
        &f.far_expiration(),
    );
    f.market
        .execute_order(&f.keeper, &user, &id, &oracle.fresh_bytes(&f.env));
}

/// Opens staggered positions so the borrowing weight holds checkpoints at
/// different indices with borrowing pending, then closes them all.
fn staggered_book(f: &Fixture, oracle: &Oracle, ledger: &mut Ledger) {
    open(f, oracle, 0, true, 1_000 * SCALAR_7, 200 * SCALAR_7);
    f.jump(3_600);
    open(f, oracle, 1, true, 2_000 * SCALAR_7, 400 * SCALAR_7);
    open(f, oracle, 2, false, 500 * SCALAR_7, 100 * SCALAR_7);
    ledger.check(f, "staggered book");
    f.jump(3_600);
}

/// The weight tracks staggered checkpoints mid-flight, leaves borrowing
/// pending, and returns to zero once every position closes.
#[test]
fn borrowing_weight_tracks_checkpoints_and_clears_at_flat() {
    let f = Fixture::create();
    let oracle = Oracle::default();
    let mut ledger = Ledger::new(f.users.len());
    staggered_book(&f, &oracle, &mut ledger);

    let market = Snapshot::take(&f).market;
    assert!(
        market.borrowing_weight.long > 0,
        "the long side holds checkpoints"
    );
    let pending_long =
        market.notional.long * market.borrowing_idx.long - market.borrowing_weight.long;
    assert!(
        pending_long > 0,
        "the first long's checkpoint trails the index, so borrowing is pending"
    );

    close(&f, &oracle, 0, true);
    ledger.check(&f, "after first close");
    close(&f, &oracle, 1, true);
    close(&f, &oracle, 2, false);
    ledger.check(&f, "after last close");

    let market = Snapshot::take(&f).market;
    assert_eq!(market.notional.total(), 0, "the book is flat");
    assert_eq!(
        (market.borrowing_weight.long, market.borrowing_weight.short),
        (0, 0)
    );
}

/// Writes `tamper` over the stored market data.
fn tamper_market(f: &Fixture, tamper: impl FnOnce(&mut market::MarketData)) {
    f.env.as_contract(&f.market_address, || {
        let mut data = market::storage::get_market_data(&f.env);
        tamper(&mut data);
        market::storage::set_market_data(&f.env, &data);
    });
}

#[test]
#[should_panic(expected = "borrowing weight drifted from the sum of position checkpoints")]
fn ledger_catches_borrowing_weight_drift() {
    let f = Fixture::create();
    let oracle = Oracle::default();
    let mut ledger = Ledger::new(f.users.len());
    staggered_book(&f, &oracle, &mut ledger);
    // One unit below the index keeps pending non-negative, so only the
    // aggregate check can catch it.
    tamper_market(&f, |m| m.borrowing_weight.long -= 1);
    ledger.check(&f, "tampered");
}

#[test]
#[should_panic(expected = "pending borrowing went negative")]
fn ledger_catches_a_checkpoint_ahead_of_the_index() {
    let f = Fixture::create();
    let oracle = Oracle::default();
    let mut ledger = Ledger::new(f.users.len());
    staggered_book(&f, &oracle, &mut ledger);
    tamper_market(&f, |m| {
        m.borrowing_weight.short = m.notional.short * m.borrowing_idx.short + 1
    });
    ledger.check(&f, "tampered");
}

/// Any weight left at a flat book is caught at once: with no notional,
/// pending borrowing is `0 - weight`, so a positive leftover trips the
/// per-step non-negativity check.
#[test]
#[should_panic(expected = "long pending borrowing went negative")]
fn ledger_catches_borrowing_weight_left_at_flat() {
    let f = Fixture::create();
    let oracle = Oracle::default();
    let mut ledger = Ledger::new(f.users.len());
    open(&f, &oracle, 0, true, 1_000 * SCALAR_7, 200 * SCALAR_7);
    f.jump(3_600);
    close(&f, &oracle, 0, true);
    ledger.check(&f, "after close");
    tamper_market(&f, |m| m.borrowing_weight.long = 1);
    ledger.check(&f, "tampered");
}

/// Moves `idx`'s `is_long` checkpoint by `delta` and the side's weight with
/// it, so the weight still matches the rows and only the per-position checks
/// can see the change.
fn shift_checkpoint(f: &Fixture, idx: usize, is_long: bool, delta: i128) {
    let user = f.user(idx);
    f.env.as_contract(&f.market_address, || {
        let key = market::storage::DataKey::Position(user.clone(), is_long);
        let mut p: market::Position = f.env.storage().persistent().get(&key).unwrap();
        p.borrowing_idx += delta;
        f.env.storage().persistent().set(&key, &p);
        let mut data = market::storage::get_market_data(&f.env);
        data.borrowing_weight.add(is_long, p.notional * delta);
        market::storage::set_market_data(&f.env, &data);
    });
}

#[test]
#[should_panic(expected = "is ahead of the side index")]
fn ledger_catches_a_position_checkpoint_ahead_of_the_index() {
    let f = Fixture::create();
    let oracle = Oracle::default();
    let mut ledger = Ledger::new(f.users.len());
    staggered_book(&f, &oracle, &mut ledger);
    // User 1's long sits one unit past the index. User 0's older long keeps
    // the side's sum positive, so only the per-position check sees it.
    let index = Snapshot::take(&f).market.borrowing_idx.long;
    let ahead = index + 1 - position(&f, &f.user(1), true).borrowing_idx;
    shift_checkpoint(&f, 1, true, ahead);
    ledger.check(&f, "tampered");
}

#[test]
#[should_panic(expected = "checkpoint moved backward")]
fn ledger_catches_an_open_checkpoint_moving_backward() {
    let f = Fixture::create();
    let oracle = Oracle::default();
    let mut ledger = Ledger::new(f.users.len());
    staggered_book(&f, &oracle, &mut ledger);
    shift_checkpoint(&f, 1, true, -1);
    ledger.check(&f, "tampered");
}
