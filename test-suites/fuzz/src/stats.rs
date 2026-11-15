//! Process-local property reachability, periodically persisted outside the corpus.
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
static COUNTS: OnceLock<Mutex<BTreeMap<&'static str, u64>>> = OnceLock::new();
static OUTPUT_PATH: OnceLock<Option<String>> = OnceLock::new();

// Campaign runners configure this once before launching the target process.
fn output_path() -> Option<&'static str> {
    OUTPUT_PATH
        .get_or_init(|| std::env::var("ZENEX_FUZZ_STATS").ok())
        .as_deref()
}

pub fn record(name: &'static str, n: u64) {
    let mut counts = COUNTS.get_or_init(Default::default).lock().unwrap();
    *counts.entry(name).or_default() += n;
    if let Some(path) = output_path() {
        let total: u64 = counts.values().sum();
        // First reaches are always recorded. Subsequent snapshots are bounded
        // to avoid making filesystem I/O dominate an oracle campaign.
        if counts[name] == n || total % 64 == 0 {
            let body = counts
                .iter()
                .map(|(k, v)| format!("{k}={v}\n"))
                .collect::<String>();
            drop(counts);
            persist(path, body);
        }
    }
}
/// Persist the completed-input counters, including the last callback of a run.
pub fn flush() {
    if let Some(path) = output_path() {
        let counts = COUNTS.get_or_init(Default::default).lock().unwrap();
        let body = counts
            .iter()
            .map(|(k, v)| format!("{k}={v}\n"))
            .collect::<String>();
        drop(counts);
        persist(path, body);
    }
}

fn persist(path: &str, body: String) {
    if let Err(error) = std::fs::write(path, body) {
        // Missing evidence must fail the campaign. Exit as infrastructure
        // failure after releasing COUNTS, not as an accounting assertion or
        // mutex-poisoning panic. libFuzzer may map this exit to its own code.
        eprintln!("cannot persist fuzz campaign statistics at {path}: {error}");
        std::process::exit(2);
    }
}
