# Fuzz targets

The fuzz workspace runs the contracts natively through Soroban testutils, with
mocked authorization and an echo verifier. It is its own cargo workspace:
`make test` type-checks every target and runs the harness self-tests, and CI
runs a short fuzz smoke on every pull request.

| Target | What it exercises |
|---|---|
| `fuzz_economic_acceptance` | Complete trader and LP lifecycles on the recorded testnet config (`test-suites/fixtures/testnet-2026-09-05.json`, with the position, open-interest and vault caps raised 1000×): closes, liquidation with bad debt, ADL, a frozen market, LP fills priced against the live book, and delisting through retirement. An independent model predicts every balance, receipt, borrowing checkpoint and share price. |
| `fuzz_oracle` | Report decoding and the oracle's checks, compared with an independent decoder: the exact error on a rejection, the exact prices on success. |
| `fuzz_market_general` | Random order, accrual, claim and price sequences under stress settings. |
| `fuzz_liquidation` | Random liquidation, ADL and wind-down sequences. |
| `fuzz_vault_orders` | Random deposit and redeem sequences, with share prices marked against open positions, through retirement. |

After every step the harness checks the market's invariants: balances never go
negative, the market holds what it owes, positions match the aggregates, the
borrowing checkpoints and the pending borrowing they imply agree with the
positions, and a rejected call leaves every stored value unchanged. The self-tests inject faults
to prove those checks fire.

## Running

The workspace pins its own nightly toolchain (`rust-toolchain.toml`) and uses
`cargo-fuzz` 0.13.2. From the repository root:

```bash
(cd test-suites/fuzz && rustup show active-toolchain)
cargo install cargo-fuzz --version 0.13.2 --locked
FUZZ_SECONDS=15 FUZZ_SEED=109 make fuzz-smoke
FUZZ_SECONDS=300 FUZZ_SEED=20260911 FUZZ_LOG_DIR=campaigns/long make fuzz-smoke
```

`make fuzz-smoke` runs each target for `FUZZ_SECONDS` in turn. It starts from
the committed inputs in `seeds/` and writes each run's corpus, crash inputs and
logs under `campaigns/`. `FUZZ_RSS_MB` sets the memory ceiling (2048 MiB by
default). A campaign fails if a target never reaches its required checks.

To replay one input, from this directory:

```bash
cargo fuzz run fuzz_economic_acceptance campaigns/long/corpus/fuzz_economic_acceptance/INPUT --sanitizer none -- -runs=1
```

## Scope

These are native logic campaigns. They do not cover real verifier signatures,
network resource limits, authorization, governance queues, factory deployment,
or collateral behavior. The integration tests in `test-suites/tests`
cover those. Deployed state and storage archival need a live network.
