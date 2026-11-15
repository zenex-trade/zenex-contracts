//! Shared per-invocation limits: protocol 27 snapshot from 2026-09-08.
//! SDK 26 models protocol 26 execution. Applied network costs belong to ops.

use soroban_env_host::InvocationResourceLimits;
use soroban_sdk::{testutils::cost_estimate::NetworkInvocationResourceLimits, Env};

/// Pin every integration invocation to the same limits, including direct clients.
pub fn enforce(env: &Env) {
    let limits = limits();
    env.cost_estimate()
        .budget()
        .reset_limits(limits.instructions as u64, u64::MAX);
    env.cost_estimate().enforce_resource_limits(limits);
}

/// The chosen network snapshot, independent of the SDK's older defaults.
pub fn limits() -> InvocationResourceLimits {
    InvocationResourceLimits {
        instructions: 400_000_000,
        ledger_entries: 400,
        write_entries: 200,
        disk_read_entries: 200,
        disk_read_bytes: 200_000,
        write_bytes: 132_096,
        ..InvocationResourceLimits::mainnet()
    }
}
