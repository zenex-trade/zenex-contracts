//! Shared harness for the Zenex v2 stateful fuzz targets.
//!
//! The four stateful targets (`fuzz_market_general`, `fuzz_liquidation`,
//! `fuzz_vault_orders`, `fuzz_economic_acceptance`) all drive the same
//! protocol, so the deployment, the price grammar, the per-entry error
//! tables, and the invariant ledger live here once. A v2 API change breaks
//! this crate, not four copies of it.
//!
//! Read the crate's `README.md` for the targets and the invariant set.

pub mod codes;
pub mod fixture;
pub mod invariants;
pub mod price;

#[cfg(test)]
mod selftest;

pub mod economic;
pub mod observer;
pub mod oracle_model;
pub mod orders;
pub mod stats;
