//! Chainlink verifier used by the shared test stack. Its source and license are
//! in README.md.

soroban_sdk::contractimport!(
    file = "../verifier.wasm",
    sha256 = "703e435b781b6ca46ad4e11fa8537866fb60c8d58ef481be0157ad2b909a6abe"
);

pub const SHA256: &str = "703e435b781b6ca46ad4e11fa8537866fb60c8d58ef481be0157ad2b909a6abe";
