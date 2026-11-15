//! Factory contract events.

use soroban_sdk::{contractevent, Address};

use crate::FactoryInitMeta;

/// A market and vault pair was deployed through `deploy`. The `trading`
/// topic keeps its historical name: the indexer decodes past `Deploy` events
/// by topic name.
#[contractevent]
#[derive(Clone)]
pub struct Deploy {
    #[topic]
    pub trading: Address,
    #[topic]
    pub vault: Address,
}

/// The owner replaced the WASM hashes and treasury that future deploys use
/// through `set_init_meta`.
#[contractevent]
#[derive(Clone)]
pub struct InitMetaUpdate {
    /// The replacement metadata.
    pub init_meta: FactoryInitMeta,
}
