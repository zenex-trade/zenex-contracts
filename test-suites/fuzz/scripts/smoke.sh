#!/usr/bin/env bash
# Run from test-suites/fuzz; all builds/corpora remain in this workspace.
set -euo pipefail
cd "$(dirname "$0")/.."
# cargo-fuzz has no --locked option; validate its dependency graph before it builds.
cargo metadata --locked --format-version 1 > /dev/null
seconds="${FUZZ_SECONDS:-120}"
seed="${FUZZ_SEED:-109}"
rss="${FUZZ_RSS_MB:-2048}"
run_dir="${FUZZ_LOG_DIR:-campaigns/smoke}"
mkdir -p "$run_dir"
run_dir="$(cd "$run_dir" && pwd)"
{
  git rev-parse HEAD
  rustc --version
  cargo --version
  cargo fuzz --version
  printf 'seconds_per_target=%s seed=%s rss_mb=%s\n' "$seconds" "$seed" "$rss"
} > "$run_dir/versions.txt"
for target in fuzz_market_general fuzz_liquidation fuzz_vault_orders fuzz_oracle fuzz_economic_acceptance; do
  corpus="$run_dir/corpus/$target"
  mkdir -p "$corpus" "$run_dir/artifacts/$target"
  cp seeds/"$target"/* "$corpus/"
  export ZENEX_FUZZ_STATS="$run_dir/$target.stats"
  : > "$ZENEX_FUZZ_STATS" # A rerun cannot reuse old reachability evidence.
  cargo fuzz run "$target" "$corpus" --sanitizer none -- \
    -max_total_time="$seconds" -seed="$seed" -rss_limit_mb="$rss" \
    -timeout=30 -max_len=4096 -print_final_stats=1 \
    -artifact_prefix="$run_dir/artifacts/$target/" 2>&1 | tee "$run_dir/$target.log"
  test -s "$ZENEX_FUZZ_STATS"
  case "$target" in
    fuzz_oracle)
      required="oracle_success oracle_rejection" ;;
    fuzz_economic_acceptance)
      required="economic_terminal flat_assertion_reached lp_terminal_success \
open_success exit_success carry_checked claim_success bad_debt_checked \
liquidation_success adl_success admin_success blocked_frozen rollback_checked \
terminal_price_success lp_mid_redeem lp_mid_deposit lp_mid_deposit_refused" ;;
    *)
      required="stress_trace stress_operation_success stress_rollback_checked" ;;
  esac
  awk -F= -v required="$required" '
    NF != 2 || $1 == "" || $2 !~ /^[0-9]+$/ {
      print FILENAME ": malformed counter: " $0 > "/dev/stderr"
      failed = 1
      next
    }
    { counts[$1] = $2 }
    END {
      split(required, keys, " ")
      for (i in keys) {
        if ((counts[keys[i]] + 0) <= 0) {
          print FILENAME ": counter not reached: " keys[i] > "/dev/stderr"
          failed = 1
        }
      }
      if ("economic_terminal" in counts &&
          (counts["economic_attempt"] != counts["economic_terminal"] ||
           counts["exit_attempt"] != counts["exit_success"] ||
           counts["economic_terminal"] != counts["lp_terminal_success"])) {
        print FILENAME ": incomplete economic trace" > "/dev/stderr"
        failed = 1
      }
      exit failed
    }
  ' "$ZENEX_FUZZ_STATS"
done
echo "All smoke property reachability and mandatory completion counters passed."
