#!/usr/bin/env bash
# Builds every filesystem component in components/ that has a test app
# (not KV/SQLite, which have their own scripts) into target/components.
set -euo pipefail
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
for c in queue docs timeseries templates; do
    bash "$root/components/$c/build.sh"
done
