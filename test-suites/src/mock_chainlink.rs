//! Echo verifier used explicitly by the native fuzz harness.
//! Integration tests use the real WASM verifier and shared report signer.
//! Fuzz targets isolate oracle decoding/policy from expensive signature work.

use soroban_sdk::{contract, contractimpl, symbol_short, Address, Bytes, Env};

#[contract]
pub struct MockChainlinkVerifier;

#[contractimpl]
impl MockChainlinkVerifier {
    /// Echo the report bytes back as the "verified" body, counting the call.
    pub fn verify(e: Env, signed_report: Bytes, _sender: Address) -> Bytes {
        let calls = Self::call_count(e.clone()) + 1;
        e.storage().instance().set(&symbol_short!("calls"), &calls);
        signed_report
    }

    /// The number of `verify` calls that landed.
    pub fn call_count(e: Env) -> u32 {
        e.storage()
            .instance()
            .get(&symbol_short!("calls"))
            .unwrap_or(0)
    }
}
