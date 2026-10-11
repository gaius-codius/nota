#!/usr/bin/env bash
# Tests check-test-targets.sh on small fixtures: a workspace whose every
# target is listed passes, and one with a target left out (an example, a test
# behind a feature) fails and names it.
#
# Requires: bash, jq.
set -euo pipefail

check=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-test-targets.sh
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT

status=0
# expect pass|fail|usage NAME [TEXT...] -- ARGS...: runs the check on ARGS
# (usage: exit status 2); each TEXT must be in what it printed.
expect() {
  local want=$1 name=$2 got out rc=0
  shift 2
  local texts=()
  while [[ $1 != -- ]]; do
    texts+=("$1")
    shift
  done
  shift
  out=$("$check" "$@" 2>&1) || rc=$?
  case $rc in
    0) got=pass ;;
    1) got=fail ;;
    2) got=usage ;;
    *) got="status $rc" ;;
  esac
  if [[ $got != "$want" ]]; then
    echo "check-test-targets.sh on $name: expected $want, got $got:" >&2
    echo "$out" >&2
    status=1
    return
  fi
  for t in ${texts[@]+"${texts[@]}"}; do
    if ! grep -qF -- "$t" <<<"$out"; then
      echo "check-test-targets.sh on $name: output doesn't name '$t':" >&2
      echo "$out" >&2
      status=1
    fi
  done
}

# Two members: `app` (a library, a binary, an example and a build script) and
# `tool-kit`, whose directory isn't its name (a cdylib library and an
# integration test). `outside` isn't a member, so its targets don't count.
app='path+file:///w/crates/app#0.1.0'
kit='path+file:///w/crates/kit#tool-kit@0.2.0'
cat >"$dir/metadata.json" <<EOF
{
  "workspace_members": ["$app", "$kit"],
  "packages": [
    {"id": "$app", "targets": [
      {"kind": ["lib"], "name": "app"},
      {"kind": ["bin"], "name": "app"},
      {"kind": ["example"], "name": "bench_tool"},
      {"kind": ["custom-build"], "name": "build-script-build"}
    ]},
    {"id": "$kit", "targets": [
      {"kind": ["cdylib", "rlib"], "name": "tool_kit"},
      {"kind": ["test"], "name": "faked", "required-features": ["fake"]}
    ]},
    {"id": "registry+https://example.invalid#outside@1.0.0", "targets": [
      {"kind": ["lib"], "name": "outside"}
    ]}
  ]
}
EOF
# nextest's list: `listed TARGET...` writes one with those targets, each as
# "id kind name".
listed() {
  local out=$dir/$1 sep=
  shift
  {
    echo '{"rust-binaries": {'
    for t in "$@"; do
      read -r id kind name <<<"$t"
      printf '%s"%s/%s": {"package-id": "%s", "kind": "%s", "binary-name": "%s"}\n' \
        "$sep" "$kind" "$name" "$id" "$kind" "$name"
      sep=,
    done
    echo '}}'
  } >"$out"
}

listed all.json "$app lib app" "$app bin app" "$app example bench_tool" \
  "$kit lib tool_kit" "$kit test faked"
expect pass every-target 'all 5 workspace targets' -- "$dir/metadata.json" "$dir/all.json"

# Without --all-targets: the example isn't built.
listed no-examples.json "$app lib app" "$app bin app" "$kit lib tool_kit" "$kit test faked"
expect fail no-examples "app's example bench_tool isn't in the test gate" '1 targets left out' -- \
  "$dir/metadata.json" "$dir/no-examples.json"

# Without the feature: the test is skipped, and the package is named from an
# id of the form #name@version.
listed no-feature.json "$app lib app" "$app bin app" "$app example bench_tool" "$kit lib tool_kit"
expect fail no-feature "tool-kit's test faked isn't in the test gate" -- \
  "$dir/metadata.json" "$dir/no-feature.json"

# A target listed under another package doesn't count for this one.
listed wrong-package.json "$app lib app" "$app bin app" "$app example bench_tool" \
  "$kit lib tool_kit" "$app test faked"
expect fail wrong-package "tool-kit's test faked" -- "$dir/metadata.json" "$dir/wrong-package.json"

# Bad arguments and input.
expect usage one-argument 'usage: check-test-targets.sh' -- "$dir/metadata.json"
expect usage missing-file "can't read $dir/none.json" -- "$dir/metadata.json" "$dir/none.json"
echo 'not json' >"$dir/bad.json"
expect usage bad-metadata "isn't cargo metadata's JSON" -- "$dir/bad.json" "$dir/all.json"
expect usage bad-list "isn't nextest's JSON binaries list" -- "$dir/metadata.json" "$dir/bad.json"
echo '{"workspace_members": [], "packages": []}' >"$dir/empty.json"
expect usage no-members 'no workspace targets' -- "$dir/empty.json" "$dir/all.json"

exit "$status"
