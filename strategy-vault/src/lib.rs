#![no_std]

//! Zenex strategy-vault contract.
//!
//! ERC-4626-style collateral vault for a single market. Share pricing marks
//! the backing with the strategy-supplied net pending trader PnL. Deposits,
//! redeems, and strategy withdrawals require the registered strategy, the
//! market contract, to authorize the call.

mod contract;
mod storage;
pub mod strategy;
pub use contract::{StrategyVaultContract, StrategyVaultContractClient};

#[cfg(test)]
mod test;
