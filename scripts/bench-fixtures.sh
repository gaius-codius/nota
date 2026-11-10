#!/usr/bin/env bash
# Runs the code checks' tests that need the research bench's full fixtures.
#
# The bench (its scripts, transcripts and logs) stays on the machine that ran
# it and is never committed; only short term pairs are, in
# crates/nota-checks/tests/fixtures/. Tests that read the bench's other
# fixtures are #[ignore = "needs the local bench fixtures"], so CI lists them
# as ignored rather than passing them. This script:
#   1. exports the fixtures from the bench twice, into target/, and fails if
#      the two exports differ (the export must be repeatable);
#   2. fails if a committed fixture differs from its fresh export, so the
#      committed copy can't drift from the bench or be edited by hand;
#   3. runs every ignored test in nota-checks against the export.
# A clean run is part of the acceptance of each code checks change.
#
# Usage: scripts/bench-fixtures.sh
#   NOTA_BENCH names the bench folder (default:
#   working-docs/reference/2026-10-01-research-bench in this checkout).
#
# Requires: bash, python3 (standard library only), cargo and cargo-nextest.
set -euo pipefail

repo=$(git rev-parse --show-toplevel)
bench=${NOTA_BENCH:-$repo/working-docs/reference/2026-10-01-research-bench}
exporter=$bench/fixtures/export.py
if [[ ! -r $exporter ]]; then
  echo "bench-fixtures: no exporter at $exporter; this needs the local research bench (set NOTA_BENCH)" >&2
  exit 2
fi

# The exports go in target/, not /tmp, which is a small shared tmpfs here.
out=$repo/target/bench-fixtures
rm -rf "$out"
mkdir -p "$out"
python3 -I "$exporter" "$out/first" --committed "$out/first/c1_committed.json"
python3 -I "$exporter" "$out/second" --committed "$out/second/c1_committed.json"
if ! diff -r "$out/first" "$out/second" >/dev/null; then
  echo "bench-fixtures: two exports differ; the export isn't repeatable" >&2
  diff -r "$out/first" "$out/second" | head -20 >&2
  exit 1
fi

committed=$repo/crates/nota-checks/tests/fixtures/c1_pairs.json
if ! cmp -s "$out/first/c1_committed.json" "$committed"; then
  echo "bench-fixtures: $committed differs from the bench's export; re-export it, or find the hand edit" >&2
  exit 1
fi

NOTA_BENCH_FIXTURES=$out/first cargo nextest run -p nota-checks --all-targets --locked \
  --run-ignored only --no-tests=fail
