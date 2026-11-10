#!/usr/bin/env bash
# Soak test (D-51): churn and mixed traffic on two in-process nodes for
# SECONDS (default 1800); the process's RSS, open fds and memory mappings
# must stay flat once warm. Samples go to CSV (default soak.csv) as it runs.
#   scripts/soak.sh [seconds] [csv]
set -euo pipefail
cd "$(dirname "$0")/.."
secs=${1:-1800}
csv=$(realpath -m "${2:-soak.csv}")
ulimit -n "$(ulimit -Hn)"
SOAK_SECS=$secs SOAK_CSV=$csv env -u RUST_BACKTRACE \
  cargo test --locked -p nanovm --test soak -- --ignored --nocapture
