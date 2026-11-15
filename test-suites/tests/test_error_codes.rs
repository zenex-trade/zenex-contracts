//! Workspace error-code registry: every `#[contracterror]` discriminant
//! defined by a workspace contract crate must be globally unique, except the
//! codes in `SHARED_CODES`, which several contracts deliberately raise with
//! an identical variant name and meaning.
//!
//! The contracts call each other inside one keeper transaction (the market calls
//! the vault, the oracle, and the treasury). A trapped typed error surfaces as a
//! bare `Error(Contract, #n)` with no originating contract. A shared
//! discriminant is therefore ambiguous to every SDK, keeper, and indexer, unless
//! every owner of the code means exactly the same thing by it.
//!
//! The registry is read from the crate sources rather than a hand-kept list,
//! so a newly added variant is covered without touching this test. Vendored
//! dependencies (OpenZeppelin) are out of scope: only workspace crates are
//! scanned.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Codes deliberately shared across contracts: the variant name and the
/// condition are identical in every crate that raises the code, so a bare
/// `Error(Contract, #n)` still names exactly one condition. A collision on
/// one of these codes passes only when every colliding variant carries the
/// listed name. Any other variant landing on the code still fails.
const SHARED_CODES: &[(u32, &str)] = &[
    // `upgrade` called with an `operator` that is not the contract owner.
    // Market, oracle and factory raise it identically.
    (600, "UpgradeNotOwner"),
];

/// Workspace crates that ship a deployable contract spec with a
/// `#[contracterror]` enum.
const CONTRACT_CRATES: &[&str] = &[
    "market",
    "strategy-vault",
    "factory",
    "oracle",
    "treasury",
    "governance",
];

/// One parsed error variant: crate, enum, variant, discriminant.
#[derive(Debug)]
struct ErrorCode {
    crate_name: String,
    enum_name: String,
    variant: String,
    code: u32,
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("test-suites sits one level under the workspace root")
        .to_path_buf()
}

/// Recursively collect the `.rs` sources of a crate's `src` tree, skipping
/// test files (a `#[contracterror]` in a test module is a mock, not ABI).
fn source_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("crate src directory is readable") {
        let path = entry.expect("directory entry is readable").path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if path.is_dir() {
            source_files(&path, out);
        } else if name.ends_with(".rs") && !name.contains("test") {
            out.push(path);
        }
    }
}

/// Parse every `Variant = N` discriminant inside `#[contracterror]` enums of
/// one source file into `out`.
fn parse_contract_errors(crate_name: &str, path: &Path, out: &mut Vec<ErrorCode>) {
    let source = fs::read_to_string(path).expect("source file is readable");
    let mut armed = false; // saw #[contracterror], waiting for the enum
    let mut enum_name = String::new();
    let mut in_enum = false;
    for line in source.lines() {
        let line = line.trim();
        if line.starts_with("#[contracterror]") {
            armed = true;
            continue;
        }
        if armed && !in_enum {
            if let Some(rest) = line
                .strip_prefix("pub enum ")
                .or_else(|| line.strip_prefix("enum "))
            {
                enum_name = rest
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                in_enum = true;
            }
            continue;
        }
        if in_enum {
            if line.starts_with('}') {
                armed = false;
                in_enum = false;
                continue;
            }
            // Skip docs, comments, and attributes. A variant line reads
            // `Name = 123,` with an optional trailing comment.
            if line.starts_with("//") || line.starts_with('#') {
                continue;
            }
            let line = line.split("//").next().unwrap_or_default().trim();
            if let Some((variant, code)) = line.split_once('=') {
                let code = code
                    .trim()
                    .trim_end_matches(',')
                    .parse::<u32>()
                    .expect("contracterror discriminant is a u32 literal");
                out.push(ErrorCode {
                    crate_name: crate_name.to_string(),
                    enum_name: enum_name.clone(),
                    variant: variant.trim().to_string(),
                    code,
                });
            }
        }
    }
}

fn collect_workspace_error_codes() -> Vec<ErrorCode> {
    let root = workspace_root();
    let mut codes = Vec::new();
    for crate_name in CONTRACT_CRATES {
        let mut files = Vec::new();
        source_files(&root.join(crate_name).join("src"), &mut files);
        for file in files {
            parse_contract_errors(crate_name, &file, &mut codes);
        }
    }
    codes
}

/// Every workspace `#[contracterror]` discriminant outside `SHARED_CODES` is
/// unique across all contract crates, so a surfaced `Error(Contract, #n)`
/// names one variant.
#[test]
fn workspace_error_discriminants_are_globally_disjoint() {
    let codes = collect_workspace_error_codes();

    // Parser sanity: the scan found the known enums and a plausible variant
    // count, so an empty or misparsed registry cannot pass vacuously.
    assert!(
        codes.len() >= 40,
        "expected at least 40 workspace error variants, found {}",
        codes.len()
    );
    for (crate_name, variant, code) in [
        ("market", "InvalidConfig", 700),
        ("market", "UpgradeNotOwner", 600),
        ("factory", "UpgradeNotOwner", 600),
        ("oracle", "UpgradeNotOwner", 600),
        ("oracle", "FeedMismatch", 790),
        ("strategy-vault", "InvalidAmount", 800),
        ("governance", "NotQueued", 810),
        ("treasury", "InvalidRate", 900),
    ] {
        assert!(
            codes.iter().any(|entry| entry.crate_name == crate_name
                && entry.variant == variant
                && entry.code == code),
            "expected {crate_name}::{variant} = {code} in the parsed registry"
        );
    }

    let mut by_code: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for entry in &codes {
        by_code.entry(entry.code).or_default().push(format!(
            "{}::{}::{}",
            entry.crate_name, entry.enum_name, entry.variant
        ));
    }
    let collisions: Vec<String> = by_code
        .iter()
        .filter(|(code, owners)| {
            if owners.len() < 2 {
                return false;
            }
            !SHARED_CODES.iter().any(|(shared, variant)| {
                *code == shared
                    && owners
                        .iter()
                        .all(|owner| owner.ends_with(&format!("::{variant}")))
            })
        })
        .map(|(code, owners)| format!("#{code}: {}", owners.join(", ")))
        .collect();
    assert!(
        collisions.is_empty(),
        "error-code collisions across workspace contracts:\n{}",
        collisions.join("\n")
    );
}
