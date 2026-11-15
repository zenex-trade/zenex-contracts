//! Compiled-WASM artifacts and the vault client used by the integration fixture.
//!
//! The market and strategy-vault contracts are deployed from their optimized
//! WASM (via the factory), so the fixture exercises the same bytecode that ships.
//! The default integration stack runs every contract as WASM. The explicit
//! `native-contracts` feature retains native factory/oracle/treasury
//! execution for source coverage. That mode cannot measure their VM costs.

pub mod factory;
pub mod governance;
pub mod market;
pub mod oracle;
pub mod treasury;
pub mod vault;
pub mod verifier;
