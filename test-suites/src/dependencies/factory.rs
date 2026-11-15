//! Optimized factory WASM for the default integration stack.

pub const FACTORY_WASM: &[u8] =
    include_bytes!("../../../target/wasm32v1-none/release/factory.wasm");
