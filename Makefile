.PHONY: default test build release fuzz-check fuzz-smoke fmt clean coverage coverage-html loc mutants coverage-clean

# The CLI is pinned to the version CI installs (.github/workflows/ci.yml). The
# `stellar-cli` target checks that the `stellar` on PATH is that version, so a
# different install fails the build instead of emitting different artifacts.
STELLAR_CLI_VERSION ?= 27.0.0
# Exported, not spliced into the recipe, so the value never reaches the shell
# as text.
export STELLAR_CLI_VERSION

default: build

# Fails unless the `stellar` on PATH is the pinned version.
.PHONY: stellar-cli
stellar-cli:
	@stellar --version | grep -q "^stellar $$STELLAR_CLI_VERSION " || \
		{ echo "stellar-cli $$STELLAR_CLI_VERSION is required, found: $$(stellar --version | head -1)" >&2; exit 1; }

# `--tests` selects every target with `test = true`, the in-crate unit suites
# included. `--lib` is named alongside it so the intent reads explicitly.
test: build fuzz-check build-upgrade-fixture
	cargo test --locked --all --lib --tests

# The changed-code upgrade fixture is an isolated test executable. It reuses
# current market source and adds one revision entry; it is not a release WASM.
.PHONY: build-upgrade-fixture
build-upgrade-fixture: stellar-cli
	CARGO_TARGET_DIR="$(CURDIR)/target/upgrade-fixture-build" stellar contract build \
		--manifest-path "$(CURDIR)/test-suites/fixtures/upgrade-market/Cargo.toml" \
		--locked --optimize --out-dir "$(CURDIR)/target/upgrade-fixture"

build: stellar-cli
	stellar contract build --locked --optimize

# Rebuilds the release WASMs into `wasm/`, which git ignores, the way the
# release workflow builds them: the same Stellar CLI, the pinned Rust toolchain and the same contract
# meta. At a release tag the output must equal the published release assets
# byte for byte. A changed or untracked file could reach the build, so the
# target fails unless `git status` is clean. The two cargo variables hold the
# workspace's release profile against a strip or debug setting in a user-level
# cargo config, which would change every hash.
RELEASE_REPO ?= github:zenex-trade/zenex-contracts
RELEASE_HOME_DOMAIN ?= app.zenex.trade
release: stellar-cli
	@test -z "$$(git status --porcelain)" || { echo "release needs a clean checkout; commit, stash or remove changed and untracked files" >&2; exit 1; }
	rm -f wasm/*.wasm
	CARGO_PROFILE_RELEASE_STRIP=symbols CARGO_PROFILE_RELEASE_DEBUG=0 \
		stellar contract build --locked --optimize --out-dir wasm \
		--meta source_repo=$(RELEASE_REPO) --meta home_domain=$(RELEASE_HOME_DOMAIN)

# `test-suites/fuzz` is its own cargo workspace, so `cargo test --all` never
# compiles it. That is how the v1 targets rotted for a whole release without
# anyone noticing; type-checking them here is what keeps that from recurring.
# Stable is enough to compile the targets — only `cargo fuzz run` needs nightly.
# Its own target dir: sharing `target/` with the main workspace mixes two
# `soroban_sdk` builds and fails with "multiple different versions of crate".
fuzz-check: build
	cargo check --locked --manifest-path test-suites/fuzz/Cargo.toml --all-targets --target-dir target/fuzz-check
	cargo test --locked --manifest-path test-suites/fuzz/Cargo.toml --lib --target-dir target/fuzz-check

# Timeboxed fuzz smoke over all five targets (needs nightly + cargo-fuzz).
# `--sanitizer none` is deliberate: these targets are pure safe Rust, so ASAN
# costs ~11x and finds nothing the overflow checks and invariants do not.
FUZZ_SECONDS ?= 120
FUZZ_SEED ?= 109
FUZZ_RSS_MB ?= 2048
export FUZZ_SECONDS FUZZ_SEED FUZZ_RSS_MB
fuzz-smoke: build
	cd test-suites/fuzz && ./scripts/smoke.sh

# The fuzz workspace and the upgrade fixture are their own cargo workspaces,
# so format them explicitly. CI checks all three.
fmt:
	cargo fmt --all
	cargo fmt --manifest-path test-suites/fuzz/Cargo.toml --all
	cargo fmt --manifest-path test-suites/fixtures/upgrade-market/Cargo.toml

clean:
	cargo clean

# Bash pipefail preserves test/tool failures when tee retains their output.
coverage mutants: SHELL := /bin/bash
coverage mutants: .SHELLFLAGS := -eu -o pipefail -c

# Execute every workspace test; filter only the rendered native-source reports.
# Inline test regions remain included; embedded WASM internals are not measured.
COVERAGE_IGNORE := /test-suites/|/(testutils|test)(/|\.rs$$)|/test_[^/]+\.rs$$
coverage: export CARGO_LLVM_COV_TARGET_DIR = $(CURDIR)/target/coverage-build
coverage: export CARGO_INCREMENTAL = 0
coverage: export CARGO_PROFILE_DEV_DEBUG = 0
coverage: export CARGO_PROFILE_TEST_DEBUG = 0
coverage: coverage-clean build build-upgrade-fixture
	mkdir -p target/coverage
	git rev-parse HEAD > target/coverage/commit.txt
	git diff HEAD > target/coverage/worktree.patch
	git ls-files -z | xargs -0 sha256sum > target/coverage/sources.sha256
	sha256sum verifier.wasm target/wasm32v1-none/release/*.wasm target/upgrade-fixture/*.wasm > target/coverage/wasm.sha256
	cargo llvm-cov --version > target/coverage/toolchain.txt
	rustc --version --verbose >> target/coverage/toolchain.txt
	cargo --version >> target/coverage/toolchain.txt
	stellar --version >> target/coverage/toolchain.txt
	cargo llvm-cov --locked --workspace --lib --tests --no-report 2>&1 | tee target/coverage/tests.log
	cargo llvm-cov report --locked --ignore-filename-regex '$(COVERAGE_IGNORE)' --json --output-path target/coverage/llvm.json
	cargo llvm-cov report --locked --ignore-filename-regex '$(COVERAGE_IGNORE)' --html --output-dir target/coverage
	cargo llvm-cov report --locked --ignore-filename-regex '$(COVERAGE_IGNORE)' | tee target/coverage/summary.txt

# Clear reports even if a prerequisite build fails, so CI cannot upload stale success.
coverage-clean:
	rm -rf -- "$(CURDIR)/target/coverage"

coverage-html: coverage
	@echo "Coverage report generated at target/coverage/html/index.html"

loc:
	@for dir in market strategy-vault factory oracle treasury governance; do \
		echo "=== $$dir ==="; \
		cd $(CURDIR)/$$dir && cargo warloc 2>/dev/null; \
		echo ""; \
		cd $(CURDIR); \
	done

# cargo-mutants owns mutation selection, baselines, timeouts and result classification.
MUTANTS_PACKAGE ?= market
MUTANTS_OUTPUT ?= target/mutation
export MUTANTS_PACKAGE MUTANTS_OUTPUT
mutants:
	mkdir -p "$$MUTANTS_OUTPUT"
	cargo mutants --cargo-arg=--locked --package "$$MUTANTS_PACKAGE" --timeout 120 --output "$$MUTANTS_OUTPUT" 2>&1 | tee "$$MUTANTS_OUTPUT/runner.log"
