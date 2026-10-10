#!/usr/bin/env bash
# Tests check-weakened.sh in a throwaway repo: a branch that is behind its
# base, and changes nothing, reports nothing; a test the branch itself
# deletes is reported; a changed, deleted or untagged `// check-bound` line
# is reported, as are two files swapping values, and one moved unchanged
# isn't; a base with no merge base is
# an error; and a call with no base prints its usage. Also checks that the
# crash harnesses' bounds in this repo carry the tag.
#
# Requires: bash, git.
set -euo pipefail

scripts=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
script=$scripts/check-weakened.sh
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

# Bounds tagged `// check-bound`.
git checkout -q main
printf 'fn d() {}\nconst LIMIT: u64 = 850; // check-bound\nconst OTHER: u64 = 1; // check-bound\n' >bound.rs
git add bound.rs
git commit -qm bounds
bound_case() { # name, new contents of bound.rs
  git checkout -q -b "$1" main
  printf '%b' "$2" >bound.rs
  git commit -qam "$1"
  out=$("$script" main "$1" 2>&1) || true
}
bound_case loosened 'fn d() {}\nconst LIMIT: u64 = 2_000; // check-bound\nconst OTHER: u64 = 1; // check-bound\n'
grep -qx '  - bound set: bound.rs: const LIMIT: u64 = 2_000; // check-bound' <<<"$out" ||
  fail "missed a loosened bound's new value: $out"
grep -qx '  - bound was: bound.rs: const LIMIT: u64 = 850; // check-bound' <<<"$out" ||
  fail "missed a loosened bound's old value: $out"
grep -q 'OTHER' <<<"$out" && fail "reported an unchanged bound: $out"
if PR_BODY=nothing "$script" main loosened >/dev/null 2>&1; then
  fail "passed an unexplained loosened bound"
fi
bound_case untagged 'fn d() {}\nconst LIMIT: u64 = 850;\nconst OTHER: u64 = 1; // check-bound\n'
grep -qx '  - bound was: bound.rs: const LIMIT: u64 = 850; // check-bound' <<<"$out" ||
  fail "missed a bound whose tag was dropped: $out"
bound_case deleted 'fn d() {}\nconst OTHER: u64 = 1; // check-bound\n'
grep -qx '  - bound was: bound.rs: const LIMIT: u64 = 850; // check-bound' <<<"$out" ||
  fail "missed a deleted bound: $out"
bound_case moved 'const OTHER: u64 = 1; // check-bound\nfn d() {}\nconst LIMIT: u64 = 850; // check-bound\n'
[[ $out == "check-weakened: nothing found" ]] ||
  fail "reported bounds moved unchanged: $out"
# Two files swapping equal-looking bounds: one is loosened.
git checkout -q main
printf 'const LIMIT: u64 = 2_000; // check-bound\n' >other.rs
git add other.rs
git commit -qm other
git checkout -q -b swapped
printf 'const LIMIT: u64 = 2_000; // check-bound\n' >bound.rs
printf 'const LIMIT: u64 = 850; // check-bound\n' >other.rs
git commit -qam swapped
out=$("$script" main swapped 2>&1) || true
grep -qx '  - bound set: bound.rs: const LIMIT: u64 = 2_000; // check-bound' <<<"$out" ||
  fail "missed bounds two files swapped: $out"

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

# The bounds the crash harnesses check carry the tag, so changing one is
# reported.
bounds=$(grep -rE --include='*.rs' 'const (MAX_LAG|LAG_LIMIT|LOSS_LIMIT):' "$scripts/../crates" || true)
[[ -n $bounds ]] || fail "found none of the crash harnesses' bounds"
while IFS= read -r line; do
  [[ -z $line ]] || grep -qE '//[[:space:]]*check-bound([^A-Za-z0-9_-]|$)' <<<"$line" ||
    fail "bound without its tag: $line"
done <<<"$bounds"
(( $(grep -c . <<<"$bounds") >= 4 )) || fail "expected at least 4 tagged bounds: $bounds"

exit "$status"
