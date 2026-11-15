use soroban_sdk::xdr::{ScErrorCode, ScErrorType};
use soroban_sdk::Error;

/// The host error a failed `require_auth` surfaces as at the top of a `try_`
/// call. An auth failure is not a contract error, so no `contracterror` enum
/// names it: the frame that demanded the signature traps and the host reports
/// `Error(Context, InvalidAction)`.
pub fn auth_error() -> Error {
    Error::from_type_and_code(ScErrorType::Context, ScErrorCode::InvalidAction)
}

pub fn assert_approx_eq_abs(a: i128, b: i128, delta: i128) {
    assert!(
        a > b - delta && a < b + delta,
        "assertion failed: `(left != right)` \
         (left: `{:?}`, right: `{:?}`, epsilon: `{:?}`)",
        a,
        b,
        delta
    );
}

/// All persistent and live temporary values must roll back. The host may prune
/// already-expired temporary entries or retain TTL extensions on rejection.
pub fn unchanged(env: &soroban_sdk::Env, call: impl FnOnce()) {
    use soroban_sdk::xdr::{ContractDataDurability, LedgerEntryData};
    let values = || {
        let mut entries = env
            .to_ledger_snapshot()
            .ledger_entries
            .into_iter()
            .filter(|(_, (entry, ttl))| {
                !matches!(&entry.data,
                LedgerEntryData::ContractData(data)
                    if data.durability == ContractDataDurability::Temporary
                        && ttl.is_some_and(|until| until < env.ledger().sequence()))
            })
            .map(|(key, (entry, _))| (key, entry))
            .collect::<std::vec::Vec<_>>();
        entries.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        entries
    };
    let before = values();
    call();
    assert_eq!(before, values(), "rejected call changed live ledger values");
}
