//! Optimized oracle contract WASM, for fixtures that deploy the shipped bytecode.

pub const ORACLE_WASM: &[u8] = include_bytes!("../../../target/wasm32v1-none/release/oracle.wasm");
