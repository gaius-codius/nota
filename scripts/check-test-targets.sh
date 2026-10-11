#!/usr/bin/env bash
# Fails when a test target in the workspace isn't in the test gate: a library,
# binary, example, integration test or bench that `cargo nextest run
# --workspace --all-targets` doesn't build and run. Cargo leaves a target out
# without a word when the gate's arguments don't select it (examples, before
# --all-targets) or when its required-features aren't enabled, so a target's
# unit tests could stop running and every check would still pass.
#
# The list is made with the gate's arguments, and the gate's own commands (the
# pre-push `test` in lefthook.yml, and CI's test and coverage runs) are checked
# to pass the same ones, so dropping --all-targets from one fails here too.
#
# Usage: check-test-targets.sh [METADATA_JSON NEXTEST_JSON]
#        check-test-targets.sh --gates FILE...
#   With no arguments, checks lefthook.yml and .github/workflows/ci.yml as
#   --gates does, then runs `cargo metadata` and `cargo nextest list` on the
#   repo (which builds the test binaries). With two files, reads that output
#   from them: `cargo metadata --format-version 1 --no-deps` and `cargo
#   nextest list --list-type binaries-only --message-format json`.
#   --gates: every `cargo nextest run` (or `cargo llvm-cov nextest`) in each
#   FILE passes the gate's arguments, and each FILE has one.
#
# Requires: bash, jq; cargo and cargo-nextest without arguments.
set -euo pipefail

gate_args="--workspace --all-targets"

# gates FILE...: prints a problem for each nextest run in FILE that doesn't
# pass the gate's arguments, and for a FILE with none.
gates() {
  local f runs line bad=0
  for f in "$@"; do
    if [[ ! -r $f ]]; then
      echo "check-test-targets: can't read $f" >&2
      return 2
    fi
    runs=$(grep -nE 'cargo +(llvm-cov +)?nextest( +run)? ' "$f" || true)
    if [[ -z $runs ]]; then
      echo "check-test-targets: $f runs no tests with cargo nextest" >&2
      bad=1
    fi
    while IFS= read -r line; do
      [[ -z $line || $line == *"nextest run $gate_args "* || $line == *"nextest $gate_args "* ]] && continue
      echo "check-test-targets: $f:${line%%:*}: the tests don't run with $gate_args: ${line#*:}" >&2
      bad=1
    done <<<"$runs"
  done
  return "$bad"
}

if [[ ${1-} == --gates ]]; then
  shift
  if [[ $# -eq 0 ]]; then
    echo "usage: check-test-targets.sh --gates FILE..." >&2
    exit 2
  fi
  gates "$@" && rc=0 || rc=$?
  [[ $rc -eq 0 ]] && echo "check-test-targets: every test run passes $gate_args"
  exit "$rc"
fi

if [[ $# -eq 0 ]]; then
  cd "$(dirname "${BASH_SOURCE[0]}")/.."
  gates lefthook.yml .github/workflows/ci.yml
  dir=$(mktemp -d)
  trap 'rm -rf "$dir"' EXIT
  cargo metadata --format-version 1 --no-deps --locked >"$dir/metadata.json"
  # shellcheck disable=SC2086 # the gate's arguments, split on purpose
  cargo nextest list $gate_args --locked \
    --list-type binaries-only --message-format json >"$dir/nextest.json"
  set -- "$dir/metadata.json" "$dir/nextest.json"
fi
if [[ $# -ne 2 ]]; then
  echo "usage: check-test-targets.sh [METADATA_JSON NEXTEST_JSON]" >&2
  exit 2
fi
for f in "$@"; do
  if [[ ! -r $f ]]; then
    echo "check-test-targets: can't read $f" >&2
    exit 2
  fi
done

# One line per target, "package-id kind name": cargo's library kinds (lib,
# rlib, cdylib, ...) are nextest's `lib`, and build scripts have no tests.
if ! targets=$(jq -r '
    .workspace_members as $members
    | .packages[] | select(.id as $id | $members | index($id))
    | .id as $id | .targets[]
    | (if (.kind | index("proc-macro")) then "proc-macro"
       elif (.kind | any(. == "lib" or . == "rlib" or . == "dylib"
                         or . == "cdylib" or . == "staticlib")) then "lib"
       else .kind[0] end) as $kind
    | select($kind != "custom-build")
    | "\($id) \($kind) \(.name)"' "$1" | sort); then
  echo "check-test-targets: $1 isn't cargo metadata's JSON" >&2
  exit 2
fi
if ! listed=$(jq -r '."rust-binaries"[] | "\(."package-id") \(.kind) \(."binary-name")"' "$2" | sort); then
  echo "check-test-targets: $2 isn't nextest's JSON binaries list" >&2
  exit 2
fi
if [[ -z $targets ]]; then
  echo "check-test-targets: no workspace targets in $1" >&2
  exit 2
fi

missing=$(comm -23 <(echo "$targets") <(echo "$listed"))
if [[ -n $missing ]]; then
  # The package's name, from its id: `...#name@version` when the name isn't
  # the directory's, else `.../name#version`.
  while read -r id kind name; do
    pkg=${id##*#}
    if [[ $pkg == *@* ]]; then
      pkg=${pkg%%@*}
    else
      pkg=${id%#*}
      pkg=${pkg##*/}
    fi
    echo "check-test-targets: $pkg's $kind $name isn't in the test gate; check its required-features, test = false, or the gate's arguments" >&2
  done <<<"$missing"
  echo "check-test-targets: $(wc -l <<<"$missing") targets left out" >&2
  exit 1
fi
echo "check-test-targets: all $(wc -l <<<"$targets") workspace targets are in the test gate"
