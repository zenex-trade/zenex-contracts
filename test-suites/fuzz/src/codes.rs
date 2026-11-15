//! Per-entry error tables.
//!
//! The protocol's contract is that every rejected call traps with a documented
//! error code. Anything else is a bug: a host error, an arithmetic overflow, an
//! out-of-budget trap, or a code belonging to a different entry point. Each
//! entry therefore gets its own allowlist rather than one union: a
//! `NotLiquidatable` out of `create_order` is a finding, not a pass.
//!
//! The lists were built from the `# Errors` sections of the `Market` trait in
//! `market/src/contract.rs` and the `panic_with_error!` sites reachable from
//! each entry.

use soroban_sdk::xdr::ScErrorType;
use soroban_sdk::{Error, InvokeError};

// --- MarketError (market/src/errors.rs) ---
const INVALID_CONFIG: u32 = 700;
const INVALID_PRICE: u32 = 701;
const INVALID_STATUS: u32 = 702;
const BORROWING_NOT_ACCRUED: u32 = 703;
const MARKET_FROZEN: u32 = 704;
const INCREASE_HALTED: u32 = 705;
const MARKET_NOT_CLEARED: u32 = 706;
const NEGATIVE_VALUE: u32 = 710;
const NOTIONAL_BELOW_MIN: u32 = 711;
const NOTIONAL_ABOVE_MAX: u32 = 712;
const INSUFFICIENT_MARGIN: u32 = 713;
const UTILIZATION_EXCEEDED: u32 = 714;
const OPEN_INTEREST_EXCEEDED: u32 = 715;
const SIZE_ROUNDS_TO_ZERO: u32 = 716;
const POSITION_NOT_FOUND: u32 = 720;
const NOTIONAL_LOCKED: u32 = 721;
const NOT_LIQUIDATABLE: u32 = 722;
const POSITION_LIQUIDATABLE: u32 = 723;
const ORDER_NOT_FOUND: u32 = 730;
const ORDER_EXPIRED: u32 = 731;
const INVALID_ORDER: u32 = 732;
const TOO_MANY_ORDERS: u32 = 733;
const UNKNOWN_KIND: u32 = 734;
const STALE_PRICE: u32 = 740;
const PRICE_BOUND_EXCEEDED: u32 = 741;
const TRIGGER_NOT_MET: u32 = 742;
const VAULT_ORDER_NOT_FOUND: u32 = 750;
const VAULT_ORDER_LOCKED: u32 = 751;
const VAULT_BALANCE_EXCEEDED: u32 = 753;
const PENDING_PNL_EXCEEDED: u32 = 754;
const NOTHING_TO_CLAIM: u32 = 760;
const ADL_NOT_TRIGGERED: u32 = 770;
const ADL_OVERSHOOT: u32 = 771;
const ADL_NOT_ELIGIBLE: u32 = 772;

/// `OracleError` codes reachable through `verify_price`, which every
/// price-bearing keeper entry calls inside `Market::load`.
pub const ORACLE: &[u32] = &[
    780, // InvalidData    — malformed report body
    781, // InvalidPrice   — non-positive or crossed book, int192 overflow
    782, // PriceStale
    784, // ReportExpired
    790, // FeedMismatch
    793, // PriceAhead
];

/// `StrategyVaultError` codes, raised by the vault on the share-pricing paths.
///
/// OpenZeppelin's `VaultTokenError` codes are absent: the vault prices shares
/// itself and calls only OpenZeppelin's internal deposit and withdraw, which
/// raise none of them.
const VAULT_INVALID_AMOUNT: u32 = 800;
const VAULT_PNL_EXCEEDS_ASSETS: u32 = 801;
const VAULT_PAYOUT_EXCEEDS_ASSETS: u32 = 803;

/// OpenZeppelin `FungibleTokenError::InsufficientBalance` — a redeem order for
/// more shares than the LP holds.
const OZ_INSUFFICIENT_SHARES: u32 = 100;

/// Stellar-asset-contract `BalanceError` — a trader escrowing more settlement
/// token than they hold.
///
/// Allowlisted only on the *creation* entries, where the trader funds the
/// escrow. A fill never pulls from the trader (the margin was escrowed at
/// creation), so a balance error out of `execute_*` means the contract tried
/// to pay out money it does not have: a solvency bug, and it must fail the run.
const SAC_BALANCE: u32 = 10;

/// `create_order` — trader-side validation plus the escrow transfer.
pub const CREATE_ORDER: &[u32] = &[
    MARKET_FROZEN,
    UNKNOWN_KIND,
    NEGATIVE_VALUE,
    INVALID_ORDER,
    TOO_MANY_ORDERS,
    NOTIONAL_ABOVE_MAX,
    ORDER_EXPIRED,
    SAC_BALANCE,
];

/// `cancel_order`.
pub const CANCEL_ORDER: &[u32] = &[MARKET_FROZEN, ORDER_NOT_FOUND];

/// `execute_order` — eligibility, then the increase or decrease body.
pub const EXECUTE_ORDER: &[u32] = &[
    MARKET_FROZEN,
    ORDER_NOT_FOUND,
    INCREASE_HALTED,
    ORDER_EXPIRED,
    STALE_PRICE,
    TRIGGER_NOT_MET,
    PRICE_BOUND_EXCEEDED,
    POSITION_NOT_FOUND,
    NOTIONAL_BELOW_MIN,
    NOTIONAL_ABOVE_MAX,
    OPEN_INTEREST_EXCEEDED,
    SIZE_ROUNDS_TO_ZERO,
    UTILIZATION_EXCEEDED,
    INSUFFICIENT_MARGIN,
    NOTIONAL_LOCKED,
    // A decrease fill of a liquidatable position rejects (the keeper
    // liquidation is the only exit). The teardown's close-then-liquidate
    // fallback anticipates exactly this path.
    POSITION_LIQUIDATABLE,
];

/// `execute_liquidation`.
pub const EXECUTE_LIQUIDATION: &[u32] = &[
    MARKET_FROZEN,
    POSITION_NOT_FOUND,
    STALE_PRICE,
    NOT_LIQUIDATABLE,
];

/// `update_adl_state`.
pub const UPDATE_ADL_STATE: &[u32] = &[MARKET_FROZEN, STALE_PRICE];

/// `execute_adl` — the ADL gates plus the forced-decrease body.
pub const EXECUTE_ADL: &[u32] = &[
    MARKET_FROZEN,
    STALE_PRICE,
    INVALID_ORDER,
    ADL_NOT_TRIGGERED,
    ADL_OVERSHOOT,
    ADL_NOT_ELIGIBLE,
    POSITION_NOT_FOUND,
    POSITION_LIQUIDATABLE,
    NOTIONAL_LOCKED,
    NOTIONAL_BELOW_MIN,
];

/// `create_vault_order` — validation plus the asset or share escrow.
pub const CREATE_VAULT_ORDER: &[u32] = &[
    MARKET_FROZEN,
    INVALID_STATUS,
    UNKNOWN_KIND,
    NEGATIVE_VALUE,
    INVALID_ORDER,
    SAC_BALANCE,
    OZ_INSUFFICIENT_SHARES,
    // The `Retired`-market direct redeem prices shares inside this entry.
    VAULT_INVALID_AMOUNT,
    VAULT_PNL_EXCEEDS_ASSETS,
];

/// `cancel_vault_order`.
pub const CANCEL_VAULT_ORDER: &[u32] = &[MARKET_FROZEN, VAULT_ORDER_NOT_FOUND];

/// `execute_vault_order` — the fill gates plus the vault's share maths.
/// `VAULT_PNL_EXCEEDS_ASSETS` (801) is deliberately absent: fills hand the
/// vault `lp_net_pnl`, structurally below the realizable ceiling, so a
/// landed 801 means the cap does not hold. `VAULT_PAYOUT_EXCEEDS_ASSETS`
/// (803) is a redeem priced above the vault's tokens while positions owe it.
pub const EXECUTE_VAULT_ORDER: &[u32] = &[
    MARKET_FROZEN,
    VAULT_ORDER_NOT_FOUND,
    STALE_PRICE,
    VAULT_ORDER_LOCKED,
    VAULT_BALANCE_EXCEEDED,
    UTILIZATION_EXCEEDED,
    PENDING_PNL_EXCEEDED,
    VAULT_INVALID_AMOUNT,
    VAULT_PAYOUT_EXCEEDS_ASSETS,
];

/// `claim_credit`.
pub const CLAIM_CREDIT: &[u32] = &[MARKET_FROZEN, NOTHING_TO_CLAIM];

/// `accrue` (price-bearing).
pub const ACCRUE: &[u32] = &[MARKET_FROZEN];

/// `set_status` (owner).
pub const SET_STATUS: &[u32] = &[INVALID_STATUS, MARKET_NOT_CLEARED];

/// `set_terminal_price` (owner).
pub const SET_TERMINAL_PRICE: &[u32] = &[INVALID_STATUS, INVALID_PRICE];

/// `set_config` (owner).
pub const SET_CONFIG: &[u32] = &[INVALID_CONFIG, NEGATIVE_VALUE, BORROWING_NOT_ACCRUED];

/// Assert that a `try_*` outcome either succeeded or trapped with a code the
/// entry is allowed to raise.
///
/// `price_bearing` adds the price-verifier's codes, which reach the caller
/// through `Market::load`.
pub fn check<T, E: core::fmt::Debug>(
    outcome: &Result<Result<T, E>, Result<Error, InvokeError>>,
    allowed: &[u32],
    price_bearing: bool,
    context: &str,
) {
    crate::stats::record("stress_operation_attempt", 1);
    crate::stats::record(
        if succeeded(outcome) {
            "stress_operation_success"
        } else {
            "stress_operation_rejection"
        },
        1,
    );
    match outcome {
        Ok(Ok(_)) => {}
        // A successful invocation whose return value would not decode is a
        // harness/ABI mismatch, not a contract finding, but it must not pass
        // silently either.
        Ok(Err(e)) => panic!("[{context}] return value did not decode: {e:?}"),
        Err(Ok(error)) if error.is_type(ScErrorType::Contract) => {
            let code = error.get_code();
            let known = allowed.contains(&code) || (price_bearing && ORACLE.contains(&code));
            assert!(
                known,
                "[{context}] contract error {code} is not one this entry may raise \
                 (allowed: {allowed:?}{})",
                if price_bearing {
                    ", plus oracle codes"
                } else {
                    ""
                }
            );
        }
        Err(Ok(error)) => panic!("[{context}] host error: {error:?}"),
        Err(Err(e)) => panic!("[{context}] invoke error: {e:?}"),
    }
}

/// Whether a `try_*` outcome landed.
pub fn succeeded<T, E>(outcome: &Result<Result<T, E>, Result<Error, InvokeError>>) -> bool {
    matches!(outcome, Ok(Ok(_)))
}
