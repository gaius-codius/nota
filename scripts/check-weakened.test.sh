#!/usr/bin/env bash
# Tests check-weakened.sh in a throwaway repo: a branch that is behind its
# base, and changes nothing, reports nothing; a test the branch itself
# deletes is reported; a base with no merge base is an error; and a call
# with no base prints its usage.
#
# Requires: bash, git.
set -euo pipefail

script=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-weakened.sh
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT

status=0
fail() {
  echo "check-weakened.test.sh: $*" >&2
  status=1
}

cd "$dir"
git init -q -b main
git config user.name test
git config user.email test@example.invalid
git config commit.gpgsign false
printf '#[test]\nfn a() {}\n' >a.rs
git add a.rs
git commit -qm one
git branch topic
# main moves on: a new test, and a lint exception.
printf '#[test]\nfn b() {}\n#[expect(clippy::x, reason = "y")]\nfn c() {}\n' >b.rs
git add b.rs
git commit -qm two

if ! out=$("$script" main topic 2>&1); then
  fail "failed on a branch behind its base: $out"
fi
[[ $out == "check-weakened: nothing found" ]] ||
  fail "reported what the base gained as the branch's: $out"

git checkout -q topic
git rm -q a.rs
git commit -qm three
out=$("$script" main topic 2>&1) || true
grep -q 'tests removed: 1 test attributes deleted, 0 added' <<<"$out" ||
  fail "missed a test the branch deleted: $out"
if PR_BODY=nothing "$script" main topic >/dev/null 2>&1; then
  fail "passed an unexplained deleted test"
fi

# Unrelated histories have no merge base: an error, never "nothing found".
git checkout -q --orphan unrelated
git commit -q --allow-empty -m four
if out=$(PR_BODY=nothing "$script" main unrelated 2>&1); then
  fail "passed with no merge base: $out"
fi
grep -q '^check-weakened: no merge base of main and unrelated' <<<"$out" ||
  fail "no merge base, but no error saying so: $out"

if out=$("$script" 2>&1); then
  fail "accepted no arguments"
fi
grep -q '^usage: check-weakened.sh BASE \[HEAD\]' <<<"$out" || fail "no usage without arguments: $out"

exit "$status"
