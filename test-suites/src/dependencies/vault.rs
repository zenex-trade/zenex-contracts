use soroban_sdk::{contractclient, Address, Env};

/// Vault WASM bytes
pub const VAULT_WASM: &[u8] =
    include_bytes!("../../../target/wasm32v1-none/release/strategy_vault.wasm");

/// The slice of the deployed vault's interface the fixture and tests read.
/// Manually defined (rather than reusing OpenZeppelin's stellar-tokens types)
/// to avoid duplicate type conflicts. The strategy-gated mutations are bound
/// locally where a test needs them (see `test_vault_auth.rs`).
#[allow(dead_code)] // Trait is used by the generated VaultClient via #[contractclient]
#[contractclient(name = "VaultClient")]
pub trait VaultInterface {
    /// Returns the address of the underlying asset that the vault manages
    fn query_asset(e: Env) -> Address;

    /// Returns the total amount of underlying assets held by the vault
    fn total_assets(e: Env) -> i128;

    /// Returns the balance of shares for a given address
    fn balance(e: Env, account: Address) -> i128;
}
