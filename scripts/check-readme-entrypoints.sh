#!/usr/bin/env bash
# scripts/check-readme-entrypoints.sh
#
# Fails if any contract entrypoint (`pub fn` in the contract impl) is missing
# from the "Contract Entrypoints" table in README.md, or if the entrypoint
# count stated above the table is out of date.
#
#   ./scripts/check-readme-entrypoints.sh

set -euo pipefail

cd "$(dirname "$0")/.."

LIB=contracts/trellis_core/src/lib.rs
README=README.md

fns=$(grep -E '^    pub fn ' "$LIB" | sed -E 's/^    pub fn ([a-z0-9_]+).*/\1/')
count=$(printf '%s\n' "$fns" | wc -l | tr -d ' ')

missing=0
for fn in $fns; do
  if ! grep -qE "^\| \`$fn\` \|" "$README"; then
    echo "ERROR: entrypoint '$fn' is missing from the README Contract Entrypoints table."
    missing=1
  fi
done

if ! grep -q "exposes $count public entrypoints" "$README"; then
  echo "ERROR: README must state 'exposes $count public entrypoints' ($LIB has $count)."
  missing=1
fi

if [ "$missing" -ne 0 ]; then
  exit 1
fi

echo "README documents all $count contract entrypoints."
