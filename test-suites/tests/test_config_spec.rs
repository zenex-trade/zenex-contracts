//! Cross-crate ABI guard: the factory hand-mirrors the market contract's
//! `Config` struct, and the two must stay field-identical for the factory's
//! `deploy` constructor invocation to decode on the market side. This test
//! compares the `Config` UDT entries of both compiled WASM specs so any drift
//! fails CI instead of a deploy.

use soroban_sdk::xdr::ScSpecEntry;
use soroban_spec::read::from_wasm;

const MARKET_WASM: &[u8] = include_bytes!("../../target/wasm32v1-none/release/market.wasm");
const FACTORY_WASM: &[u8] = include_bytes!("../../target/wasm32v1-none/release/factory.wasm");

/// The `Config` UDT field layout of a compiled contract spec, as
/// `(name, type)` pairs in spec order. Doc strings are excluded: the XDR
/// struct encoding is keyed by field name and type alone.
fn config_fields(wasm: &[u8]) -> Vec<(String, String)> {
    from_wasm(wasm)
        .expect("readable contract spec")
        .into_iter()
        .find_map(|entry| match entry {
            ScSpecEntry::UdtStructV0(udt) if udt.name.to_utf8_string_lossy() == "Config" => Some(
                udt.fields
                    .iter()
                    .map(|field| {
                        (
                            field.name.to_utf8_string_lossy(),
                            format!("{:?}", field.type_),
                        )
                    })
                    .collect(),
            ),
            _ => None,
        })
        .expect("a Config UDT entry in the spec")
}

/// The factory's mirrored `Config` must encode identically to the market's:
/// same field names and types.
#[test]
fn factory_config_spec_matches_market() {
    assert_eq!(
        config_fields(FACTORY_WASM),
        config_fields(MARKET_WASM),
        "the factory's Config mirror drifted from the market's Config",
    );
}
