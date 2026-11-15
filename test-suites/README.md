# test-suites

Integration tests and shared fixtures for the Zenex contracts. This crate is a
`rlib` (not deployable); it exists so all contracts (`market`,
`strategy-vault`, `factory`, `oracle`, `treasury`, `governance`) can be
exercised together against a Soroban test environment.

## Layout

| Path | Purpose |
|------|---------|
| `src/test_fixture.rs` | Shared WASM stack: factory, market, vault, oracle, verifier, and treasury; signed prices and automatic resource limits |
| `src/lib.rs` | `default_config()`, the broad `deployed_config()` stress config and `to_factory_config()` |
| `src/setup.rs` | `create_fixture_with_data()` preset: `TestFixture::create()` plus $10M vault liquidity seeded as a donation |
| `fixtures/testnet-2026-09-05.json`, `src/testnet_profile.rs` | Recorded testnet Config, XLM feed and stack dials; `test_config()` multiplies only the three capacity caps by 1000 |
| `src/mock_chainlink.rs` | Explicit echo verifier for native fuzzing; integration tests use the real verifier |
| `src/chainlink_helper.rs` | Data Streams body encoder and deterministic public test signer |
| `src/resources.rs` | One resource-limit profile, enforced per top-level invocation |
| `fixtures/report.json` | One independently checked signed report, its signer configuration, and all nine decoded fields |
| `src/dependencies/` | Compiled Zenex WASMs and a `contractimport!` of the root `verifier.wasm` |
| `src/assertions.rs` | Assertion helpers: `auth_error()`, `assert_approx_eq_abs()` and the `unchanged()` rollback check |
| `src/carry.rs` | Hand-derived borrowing and funding indices and charges, independent of contract output |
| `src/constants.rs` | Shared test constants: `SCALAR_7`, `SCALAR_18`, `PRICE_SCALAR`, `feed_btc()`, `BTC_PRICE`, time units and mirrored `default_config()` values |
| `src/token.rs` | SAC token wrapper used as the settlement token |
| `fixtures/upgrade-market/` | Test-only changed-code market that `make build-upgrade-fixture` builds into `target/upgrade-fixture/` |
| `test_snapshots/` | Ledger snapshots that each `cargo test` run writes (gitignored) |
| `fuzz/` | `cargo-fuzz` targets (separate cargo workspace; see `fuzz/README.md`) |

Integration test files, all under `tests/`, grouped by what they cover:

| Area | Files |
|------|-------|
| Orders and fills | `test_orders.rs` (creation, validation, cancel, escrow refunds), `test_order_fills.rs` (keeper fills, triggers, locks, failed-fill isolation, the netted-side payout), `test_decrease_dust_clamp.rs` (a decrease into the dust band closes in full), `test_entry_rounding.rs` (a round trip at an unchanged quote never pays the trader), `test_entry_rounding_bounds.rs` (an entry covers its notional to within one base unit, whole or sliced), `test_utilization_sides.rs` (an increase consumes capacity only on its own side, and redemptions check both sides) |
| Prices | `test_price_timing.rs` (every priced route against every clock: `created_at`, the price cache, `priced_at`), `test_price_gates.rs` (staleness classes, protective versus strict fills, oracle setters), `test_verifier_acceptance.rs` (signed reports through the pinned real verifier: the rejection matrix, memo policy and expiry, signer replacement, the `fixtures/report.json` reference) |
| Forced closure | `test_liquidation.rs` (underwater liquidation, keeper payouts, bad debt), `test_forfeit_escape.rs` (an eroded position cannot dodge the saturated liquidation fee), `test_adl.rs` (auto-deleveraging triggers and partial reductions), `test_failed_payout.rs` (a failed trader transfer parks a claimable credit) |
| Accrual | `test_funding.rs` (funding and borrowing accrual, credit claims), `test_frozen_config_waiver.rs` (borrowing changes on a frozen market skip the same-ledger accrual, and the first fill after unfreeze prices the frozen window) |
| Bounds | `test_overflow_boundaries.rs` (where the `i128` limits sit in the pending borrowing and the share supply: a fill past the borrowing bound traps without effect, and the vault refuses a mint into its virtual shares), `test_rounding_dust.rs` (vault round trips never pay, a donation attack costs about 10^6 times the victim's loss, borrowing income lands within the rounding bound of the share-price marks, frequent settlement never pays less) |
| Testnet economics | `test_testnet_economics.rs` (recorded 20x margin ladder, real LP shares, both sides, carry, partial/full closes, liquidation, exact 55%/40% ADL boundaries and LP exits), `test_economic_boundaries.rs` (exact per-side open-interest caps, liquidation strictly below health equality, a borrowing dominance flip, unavailable payout recipients), `test_lifecycle_acceptance.rs` (flat idle books that reopen on the other side, and a governed wind-down through a trader default, retirement and the last LP redemption), `test_deployed_config.rs` (separate 100x stress fixture) |
| Vault and LP | `test_vault_orders.rs` (creation, cancel, escrow), `test_vault_order_fills.rs` (deposit and redeem fills, gates, `min_out` rejections and the parked refund), `test_share_pricing.rs` (the netted `lp_net_pnl` mark at the vault-order boundary), `test_vault_auth.rs` (strategy gate on the deployed vault WASM) |
| Administration | `test_admin.rs` (config updates, status transitions, owner auth), `test_governance.rs` (timelock queue, execute, cancel, `set_status` bypass, delay changes), `test_retirement.rs` (delist, terminal price, retire, wind-down closes), `test_upgrade.rs` (owner gating, executable replacement, storage survival), `test_treasury.rs` (fee accrual, owner withdrawal, rate bounds), `test_auth_admin_boundaries.rs` (ownership transfer deadlines and cancellation on every `Ownable` contract, governance delay bounds, treasury constructor and withdrawal edges), `test_governed_upgrade_acceptance.rs` (a timelocked changed-code market upgrade that preserves live state, orders and claims) |
| Pins | `test_config_spec.rs` (the factory `Config` mirror matches the market's spec), `test_error_codes.rs` (error enums stay disjoint across crates), `test_resource_budget.rs` (worst-case keeper paths under the network limits), `test_storage_lifetime_acceptance.rs` (TTL refresh coverage of market reads, idle vault and verifier instances, the governance queue and the factory registry) |

Counts per file are not listed here. `grep -c '#\[test\]'` gives the source count for any file.

## Setup

### Required

Follow the [repository setup](../README.md#getting-started) for the pinned Rust
toolchain and Stellar CLI. Run the commands below from the repository root.

The factory deploys contracts from precompiled WASM
(`src/dependencies/` reads `target/wasm32v1-none/release/`), so a fresh WASM
build is required after any contract change. `test_governed_upgrade_acceptance.rs`
also embeds `target/upgrade-fixture/upgrade_market.wasm`, which
`fixtures/upgrade-market/` builds from the current market source. The
`make test` target rebuilds WASM automatically. A stale WASM can invalidate
results without failing: a test may pass while exercising old bytecode, so
rebuild before trusting a run that followed a manual build.

### Optional (per workflow)

```sh
# Coverage (used by `make coverage` / `make coverage-html`)
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --version 0.9.0 --locked

# Mutation testing (used by `make mutants`)
cargo install cargo-mutants --version 27.1.0 --locked

# Fuzzing (used from `test-suites/fuzz/`)
(cd test-suites/fuzz && rustup show active-toolchain)
cargo install cargo-fuzz --version 0.13.2 --locked
```

## Running tests

From the repo root:

```sh
make test
```

The `test` target depends on `build`, which runs `stellar contract build
--locked --optimize` to produce the WASM artifacts the factory deploys. It also
depends on `fuzz-check`, which type-checks the fuzz workspace and runs its
self-tests, and on `build-upgrade-fixture`. After that,
`cargo test --locked --all --lib --tests` runs unit tests in every crate plus the integration
tests in this crate.

Direct `cargo test` invocations (below) reuse whatever WASM is already in
`target/wasm32v1-none/release/` and `target/upgrade-fixture/`. Run
`make build build-upgrade-fixture` first if a contract changed since the last
build.

To run a single integration test:

```sh
cargo test --locked -p test-suites --test test_order_fills resting_trigger_order_rejects_until_price_crosses
```

To run all tests in one file:

```sh
cargo test --locked -p test-suites --test test_liquidation
```

Tests use a deterministic ledger and public signing keys. `price()`,
`price_bid_ask()` and `report_at()` sign reports automatically; `sign_report()`
handles a custom body. The default verifier configuration has four signers and
two signatures (`f=1`); threshold tests override it explicitly. The reference
report in `fixtures/report.json` was independently matched by the ops signer.
The pinned verifier source and license are linked in the [root README](../README.md).
To reproduce `verifier.wasm`, check out `c3627a4780eb3b570a1ac170b0d5752082e81990`
in the upstream repository and work from `contracts/verifier/`. Use Rust 1.97.1
with the `wasm32v1-none` target to match the checked-in binary; Rust 1.98.1
produces a different hash. Run
`cargo +1.97.1 update -p ethnum --precise 1.5.3`, then
`cargo +1.97.1 build --locked --target wasm32v1-none --release -p verifier`.
The build writes `target/wasm32v1-none/release/verifier.wasm` under the upstream
root. Its SHA-256 matches the hash that `src/dependencies/verifier.rs` pins.

Every invocation, including direct client calls, enforces `src/resources.rs`.
SDK 26.1.1/host 26.1.4 models protocol 26 against the frozen protocol 27 limits
from 2026-09-08. These are local execution checks; network receipts belong to ops.
Dedicated resource cases still require 20% headroom for maximum parked orders,
signatures, and cold/cached liquidation verification. Return/transaction sizes
are outside the host meter. Four liquidations is a tested keeper sequence, not a
protocol cap.

For native source coverage, `cargo llvm-cov` automatically selects the native
factory/oracle/treasury through its `cfg(coverage)` flag. To exercise that
same mode directly:

```sh
cargo test --locked -p test-suites --features native-contracts --lib --tests
```

The market, vault and verifier remain WASM in that mode. Explicit
`create_from_wasm()` / `create_testnet_from_wasm()` constructors keep resource
and code-lifetime tests on WASM even during coverage. Unit tests and fuzzing
retain native execution; their instruction counts omit native VM work.

## Coverage

```sh
make coverage          # all workspace tests, LLVM JSON/HTML and text summary
make coverage-html     # same run; target/coverage/html/index.html
```

The Makefile invokes `cargo-llvm-cov` directly after rebuilding release and
upgrade-fixture WASMs. All root-workspace library and integration tests run;
`cfg(coverage)` selects the native integration path described above. Only the
reports exclude the test harness and dedicated test/testutils files. Event
files are not filtered out, and inline test modules still count. This is **mixed
native-source coverage**: embedded WASM internals, standalone fuzz targets,
external dependencies and source without LLVM mappings are not measured.
There is no production-only, branch-coverage or assertion-coverage claim.

`target/coverage/` contains LLVM's `llvm.json`, `html/` and per-file
`summary.txt` with the native counters, plus `tests.log`, the source commit and
working-tree patch, source/WASM checksums and tool versions. These are the
tools' own reports; there is no custom aggregation or percentage threshold.
CI runs the coverage job on every push to `main` and on manual dispatch, and
uploads the directory. It is not a pull-request gate. Failed builds, tests or
exports fail the job; old reports are cleared before building.

## Mutation testing

```sh
make mutants                         # full market exploration; can take a long time
make mutants MUTANTS_PACKAGE=oracle
```

The Makefile invokes `cargo-mutants` directly with a 120-second timeout per
mutation and retains its output under `target/mutation/`. `MUTANTS_OUTPUT`
selects another output directory. Bash `pipefail` preserves failures through
logging, including a failing unmutated baseline. Cargo-mutants supplies its
own baseline checks, mutation selection, timeouts and result classification;
there is no separate mutation runner.

For a bounded native run, call the same tool directly. This selects two
parked-credit arithmetic mutations and the test that checks the backing pool:

```sh
cargo mutants --cargo-arg=--locked --package market --test-package market \
  --file market/src/engine/settlement.rs \
  --re 'replace \+= with -= in pay_trader' --timeout 120 \
  --output target/mutation-credit \
  -- --lib engine::keeper::tests::pay_trader_failure_parks_claimable_credit -- --exact
```

Inspect `mutants.out/outcomes.json` and `mutants.out/log/` in the output
directory: surviving mutations, build failures and timeouts do not establish
that an assertion caught a fault.
Mutation campaigns run manually.
Native tests must execute the changed code. To test a mutation through embedded
WASM, rebuild that WASM for **each mutation**; a stale original blob cannot
validate the changed source.

## Fuzzing

`fuzz/` is a separate cargo workspace (nightly + `cargo-fuzz`) and is **not**
run by the workspace Cargo command. `make test` checks its targets and runs
its self-tests separately; see `fuzz/README.md` for targets and invariants.

## Fixture details

`TestFixture::create()` deploys, in order:

1. SAC token (7-decimal settlement token)
2. The real verifier with public test signers, and the `oracle` pointed at it
3. `treasury` (5% protocol rate)
4. `factory` (holding the uploaded market + vault WASM hashes and the treasury address)
5. The market + vault pair, atomically through `factory.deploy`

The market (stream `feed_btc()`, fixture prices at `PRICE_SCALAR`, `10^18`) is
fixed at deploy: v2 is one market per contract. `create_fixture_with_data()` additionally mints
$10M straight to the vault address — a donation raising the backing without
minting shares; tests exercising share accounting route through
`TestFixture::vault_deposit` instead.

`TestFixture::create_testnet()` reads the September 5 testnet snapshot from
`fixtures/testnet-2026-09-05.json`. Its market rates, 5% initial margin, 2%
maintenance margin, 0.5% liquidation fee, 55%/40% ADL band, oracle windows and
spread reduction, 30% treasury rate, and 6-decimal share offset match that
record. Only maximum position, per-side OI and vault capacities increase by
1000 for testing ($25M, $50M and $100M). The JSON retains the original limits;
`test_config()` applies the explicit `capacityMultiplier` to those three fields.

This fixture uses the recorded XLM feed ID and synthetic non-round 18-decimal
XLM reports with a 7-decimal SAC. It starts with an empty vault; its lifecycle
tests deposit through real LP orders and redeem every issued share after
flattening the book. They account for retained funding surplus, the virtual
asset and the vault's redeem-fee cut. Reports carry real signatures under the
test configuration; provider and deployed collateral identities remain external.

`deployed_config()` and `create_deployed()` retain their historical names for
the separate broad stress fixture, including its 100x margin ladder and the
same raised capacity limits. These test overrides are not deployment parameters.
