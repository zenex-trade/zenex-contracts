//! Rejected invocations must preserve ledger values. Host TTL maintenance can
//! survive a failed invocation, so live-until metadata is deliberately excluded.
//! Expired temporary entries are excluded too. The network treats them as
//! deleted, but the test host keeps them until a read drops them. Its
//! resource metering reads every key a call touched after the rollback, so a
//! rejected call that read one removes it.
use soroban_sdk::{
    xdr::{ContractDataDurability, LedgerEntry, LedgerKey},
    Env,
};
#[derive(Debug, PartialEq, Eq)]
pub struct Values(Vec<(LedgerKey, LedgerEntry)>);
impl Values {
    pub fn take(env: &Env) -> Self {
        let sequence = env.ledger().sequence();
        let mut entries: Vec<_> = env
            .to_ledger_snapshot()
            .ledger_entries
            .into_iter()
            .filter(|(k, (_, live_until))| !expired_temporary(k, *live_until, sequence))
            .map(|(k, (v, _ttl))| (*k, *v))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Self(entries)
    }
    pub fn assert_unchanged(&self, env: &Env) {
        assert_eq!(
            *self,
            Self::take(env),
            "rejected call changed ledger values"
        );
    }
}

/// Mirrors the host's expiry test: a temporary entry is gone once its
/// live-until ledger is below the current one.
fn expired_temporary(key: &LedgerKey, live_until: Option<u32>, sequence: u32) -> bool {
    matches!(
        key,
        LedgerKey::ContractData(data) if data.durability == ContractDataDurability::Temporary
    ) && live_until.is_some_and(|ledger| ledger < sequence)
}

/// Check the per-entry error discipline plus exact rejection rollback.
pub fn check<T, E: std::fmt::Debug>(
    env: &Env,
    before: &Values,
    result: &Result<Result<T, E>, Result<soroban_sdk::Error, soroban_sdk::InvokeError>>,
    allowed: &[u32],
    price_bearing: bool,
    context: &str,
) {
    crate::codes::check(result, allowed, price_bearing, context);
    if !crate::codes::succeeded(result) {
        before.assert_unchanged(env);
        crate::stats::record("stress_rollback_checked", 1);
    }
}

/// Stress grammar never claims mandatory completion. Preserve its exact ending
/// categories separately from economic_terminal (which only the economic
/// acceptance target can increment), so rejected actions cannot masquerade as
/// successful exits.
pub fn finish_stress(f: &crate::fixture::Fixture) {
    let s = crate::invariants::Snapshot::take(f);
    let positions = s.market.notional.total();
    if positions > 0 {
        crate::stats::record("stress_nonterminal_open", 1);
        if f.market.get_status() == 2 {
            crate::stats::record("stress_nonterminal_frozen", 1);
        }
    } else if s
        .users
        .iter()
        .any(|u| !u.orders.is_empty() || !u.vault_orders.is_empty() || u.claimable > 0)
    {
        crate::stats::record("stress_flat_with_liabilities_or_escrows", 1);
    } else {
        crate::stats::record("stress_flat_no_user_liabilities", 1);
    }
    crate::stats::record("stress_trace", 1);
    crate::stats::flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ttl_maintenance_is_excluded_but_really_changes_metadata() {
        let f = crate::fixture::Fixture::create();
        let before = Values::take(&f.env);
        let metadata_before = f.env.to_ledger_snapshot().ledger_entries;
        f.env.as_contract(&f.market_address, || {
            f.env.storage().instance().extend_ttl(3_000_000, 3_000_000)
        });
        let metadata_after = f.env.to_ledger_snapshot().ledger_entries;
        assert_ne!(
            metadata_before, metadata_after,
            "test must actually extend a live-until ledger"
        );
        before.assert_unchanged(&f.env);
    }
    /// The price cache lives one day of ledgers. A rejected priced call after
    /// it lapses reads it, and the host's post-call metering then drops it.
    #[test]
    fn rejected_call_that_reads_an_expired_price_cache_passes() {
        let f = crate::fixture::Fixture::create();
        let oracle = crate::price::Oracle::default();
        f.market.update_adl_state(&oracle.bytes(&f.env));
        let cached = || {
            f.env.as_contract(&f.market_address, || {
                f.env
                    .storage()
                    .temporary()
                    .has(&market::storage::DataKey::PriceCache)
            })
        };
        assert!(cached(), "a priced call writes the cache");
        f.jump(2 * 86_400);
        let before = Values::take(&f.env);
        // No position: `Market::load` reads the cache, then the call traps.
        let outcome =
            f.market
                .try_execute_liquidation(&f.keeper, &f.user(0), &true, &oracle.bytes(&f.env));
        assert!(!crate::codes::succeeded(&outcome));
        before.assert_unchanged(&f.env);
    }
    #[test]
    #[should_panic(expected = "rejected call changed ledger values")]
    fn rejected_balance_mutation_is_detected() {
        let f = crate::fixture::Fixture::create();
        let before = Values::take(&f.env);
        f.token.transfer(&f.user(0), &f.keeper, &1);
        before.assert_unchanged(&f.env);
    }
}
