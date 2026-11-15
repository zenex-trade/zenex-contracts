//! Cross-contract client interfaces.

mod oracle;
mod treasury;
mod vault;

pub use oracle::{OracleClient, PriceData};
pub use treasury::TreasuryClient;
pub use vault::VaultClient;
