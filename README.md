# Zenex Contracts

Smart contracts for Zenex, a perpetual futures exchange on
[Stellar](https://stellar.org) (Soroban).

## Documentation

To learn more about Zenex, visit the docs:

- [Zenex Docs](https://docs.zenex.trade)

## Contracts

| Crate | Description |
|-------|-------------|
| `market` | Core perps engine: positions, fees, funding, liquidations, ADL |
| `strategy-vault` | ERC-4626-style collateral vault for one market: PnL-marked share price, market-gated deposits and redeems |
| `factory` | Atomic vault + market deployment |
| `oracle` | Chainlink Data Streams report verification and price policy |
| `treasury` | Protocol fee collection |
| `governance` | Timelock for admin calls, with an immediate `set_status` for emergency halts |
| `test-suites` | Integration tests, shared fixtures, and fuzz targets |

## Getting Started

Install Rust through [rustup](https://rustup.rs/) and the
[Stellar CLI 27.0.0 release binary](https://github.com/stellar/stellar-cli/releases/tag/v27.0.0)
on `PATH`.

Run commands from the repository root. Rustup installs Rust 1.98.1 and the
`wasm32v1-none` target from `rust-toolchain.toml`. Make targets use the committed
lockfiles; dependency changes must update them explicitly. Fuzz campaigns use
the dated nightly in `test-suites/fuzz/rust-toolchain.toml`.

Build the contracts with:

```
make
```

Run the unit tests, the fuzz-target type check and self-tests, and the
integration test suite with:

```
make test
```

See [test-suites/README.md](test-suites/README.md) for coverage, mutation
testing, and fuzzing.

Tests use [Chainlink’s verifier](https://github.com/smartcontractkit/data-streams-stellar/tree/c3627a4780eb3b570a1ac170b0d5752082e81990/contracts/verifier) ([MIT](https://github.com/smartcontractkit/data-streams-stellar/blob/c3627a4780eb3b570a1ac170b0d5752082e81990/LICENSE)), checked in as [verifier.wasm](verifier.wasm).

<details>
<summary>Verifier license notice</summary>

The MIT License (MIT)

Copyright (c) 2026 SmartContract ChainLink Limited SEZC

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.

</details>

## Deployment

`make` writes optimized WASM to `target/wasm32v1-none/release/`.

Release WASMs are built by the [release workflow](.github/workflows/release.yml) when a version tag is
pushed. For each contract it publishes a GitHub release with the WASM and its SHA-256, attests the
build provenance, and submits the hash to [stellar.expert](https://stellar.expert) for source
validation. Deploy the release files unchanged. The network stores contract code under the SHA-256 of
its bytes, so a deployed contract's WASM hash equals the SHA-256 of its release file.

Each release WASM embeds the `source_repo` and `home_domain` contract meta. The build is reproducible
with the pinned Rust toolchain and Stellar CLI. To check a deployed contract, fetch its WASM, find the
release whose file has the same hash, and rebuild that release's tag:

```
stellar contract fetch --id <contract_id> --network <network> --out-file deployed.wasm
stellar contract info meta --wasm deployed.wasm
git checkout <release tag>
make release
sha256sum deployed.wasm wasm/*.wasm
```

`gh attestation verify <file>.wasm --repo zenex-trade/zenex-contracts` checks the provenance of a
downloaded release file.

`factory.deploy` is permissionless: anyone can deploy a market and vault pair, with itself as the
pair's admin. `is_deployed` proves only that this factory deployed the address, not that the pair is
official or safely configured.

The settlement token's issuer must not be able to claw back protocol balances: its clawback flag must
be clear. Issuer authorization control is accepted. A payout the issuer blocks parks as claimable
credit.

## Security

See [SECURITY.md](SECURITY.md) for how to report a vulnerability and for the bounty scope.

## Contributing

Notes for contributors:

- Under no circumstances should the `overflow-checks` flag be removed,
  otherwise contract math will become unsafe.
