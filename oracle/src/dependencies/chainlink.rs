//! The Chainlink Data Streams verifier client.

use soroban_sdk::{contractclient, Address, Bytes, Env};

/// Interface to the deployed Chainlink Data Streams verifier contract. It
/// checks the report's DON signatures and config activity only, so every
/// protocol gate stays on our side. It is not a view: it `require_auth`s
/// `sender` and bumps its own TTLs.
#[allow(dead_code)]
#[contractclient(name = "VerifierClient")]
pub trait ChainlinkVerifier {
    /// Verifies `signed_report` on behalf of `sender` and returns the raw
    /// report body. Traps if the signatures or the DON config do not verify.
    fn verify(env: Env, signed_report: Bytes, sender: Address) -> Bytes;
}
