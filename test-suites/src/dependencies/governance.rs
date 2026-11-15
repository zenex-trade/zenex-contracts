//! Optimized governance WASM for integration fixtures that inspect its runtime.

pub const GOVERNANCE_WASM: &[u8] =
    include_bytes!("../../../target/wasm32v1-none/release/governance.wasm");
