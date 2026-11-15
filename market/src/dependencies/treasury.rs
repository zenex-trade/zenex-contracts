//! Treasury client. The market reads the protocol fee rate at settlement.

use soroban_sdk::{contractclient, Env};

/// The treasury contract interface.
#[allow(dead_code)] // consumed by the generated `TreasuryClient`
#[contractclient(name = "TreasuryClient")]
pub trait TreasuryInterface {
    /// Returns the treasury's share of every protocol fee (`SCALAR_18`).
    fn get_rate(e: Env) -> i128;
}
