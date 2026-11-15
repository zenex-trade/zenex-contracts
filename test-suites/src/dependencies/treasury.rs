//! Optimized treasury contract WASM, for fixtures that deploy the shipped bytecode.

pub const TREASURY_WASM: &[u8] =
    include_bytes!("../../../target/wasm32v1-none/release/treasury.wasm");
