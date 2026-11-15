//! Optimized market contract WASM, uploaded to the factory for deployment.

pub const MARKET_WASM: &[u8] = include_bytes!("../../../target/wasm32v1-none/release/market.wasm");
