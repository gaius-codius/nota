#!/usr/bin/env bash
# Runs the code checks' tests that need the research bench's full fixtures.
#
# The bench (its scripts, transcripts and logs) stays on the machine that ran
# it and is never committed; only short spans are (C1's term pairs, and C2's
# fixes, each gated alone), in crates/nota-checks/tests/fixtures/. Tests that
# read the bench's other fixtures are #[ignore = "needs the local bench
# fixtures"], so CI lists them as ignored rather than passing them. This
# script:
#   1. exports the fixtures from the bench twice, into target/, and fails if
#      the two exports differ (the export must be repeatable);
#   2. fails if a committed fixture differs from its fresh export, so the
#      committed copy can't drift from the bench or be edited by hand (C4's
#      invented fixture is written by the bench's own Python packages, in
#      its laya venv);
#   3. runs every ignored test in nota-checks against the export.
# A clean run is part of the acceptance of each code checks change.
#
# Usage: scripts/bench-fixtures.sh
#   NOTA_BENCH names the bench folder (default:
#   working-docs/reference/2026-10-01-research-bench in this checkout).
#
# Requires: bash, python3 (standard library only), the bench's laya venv
# (wordfreq, metaphone and jellyfish), cargo and cargo-nextest.
set -euo pipefail

# The checkout this script is in, wherever it's run from.
cd "$(dirname "${BASH_SOURCE[0]}")/.."
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
# Each export writes the committed subsets into its own committed/ folder,
# named as their copies in crates/nota-checks/tests/fixtures/ are.
for run in first second; do
  mkdir -p "$out/$run/committed"
  python3 -I "$exporter" "$out/$run" --committed "$out/$run/committed/c1_pairs.json" \
    --committed-c2 "$out/$run/committed/c2_gate.json"
done
if ! diff -r "$out/first" "$out/second" >/dev/null; then
  echo "bench-fixtures: two exports differ; the export isn't repeatable" >&2
  diff -r "$out/first" "$out/second" | head -20 >&2
  exit 1
fi

# Every committed copy is compared, so each one that differs is named.
stale=0
for name in c1_pairs.json c2_gate.json; do
  committed=$repo/crates/nota-checks/tests/fixtures/$name
  if ! cmp -s "$out/first/committed/$name" "$committed"; then
    echo "bench-fixtures: $committed differs from the bench's export; re-export it, or find the hand edit" >&2
    stale=1
  fi
done
if ((stale)); then
  exit 1
fi

venv_python=$bench/laya/.venv/bin/python
if [[ ! -x $venv_python ]]; then
  echo "bench-fixtures: no venv at $venv_python; C4's fixture needs the bench's wordfreq, metaphone and jellyfish" >&2
  exit 2
fi
"$venv_python" -I "$bench/fixtures/c4_invented.py" "$out/first/c4_invented.json"
"$venv_python" -I "$bench/fixtures/c4_invented.py" "$out/second/c4_invented.json"
if ! cmp -s "$out/first/c4_invented.json" "$out/second/c4_invented.json"; then
  echo "bench-fixtures: two exports of C4's invented fixture differ; it isn't repeatable" >&2
  exit 1
fi
committed=$repo/crates/nota-checks/tests/fixtures/c4_invented.json
if ! cmp -s "$out/first/c4_invented.json" "$committed"; then
  echo "bench-fixtures: $committed differs from the bench's export; re-export it, or find the hand edit" >&2
  exit 1
fi

NOTA_BENCH_FIXTURES=$out/first cargo nextest run -p nota-checks --all-targets --locked \
  --run-ignored only --no-tests=fail
