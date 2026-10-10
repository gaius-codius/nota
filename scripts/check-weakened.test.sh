#!/usr/bin/env bash
# Tests check-weakened.sh in a throwaway repo: a branch that is behind its
# base, and changes nothing, reports nothing; a test the branch itself
# deletes is reported; a changed, deleted or untagged `// check-bound` line
# is reported, as are two files swapping values, and one moved unchanged
# isn't; a lowered coverage floor and a crash script's `#`-tagged bound are
# reported, and a tag quoted or mentioned in a shell comment isn't one; a
# base with no merge base is an error; and a call with no base prints its
# usage. Also checks that the crash harnesses' bounds in this repo carry the
# tag.
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
# A sweep's coverage floor, tagged at the end of its assert.
git checkout -q main
printf 'fn e(ops: usize) {\n    assert!(ops > 150, "{ops}"); // check-bound\n}\n' >floor.rs
git add floor.rs
git commit -qm floor
git checkout -q -b floor
printf 'fn e(ops: usize) {\n    assert!(ops > 15, "{ops}"); // check-bound\n}\n' >floor.rs
git commit -qam floor
out=$("$script" main floor 2>&1) || true
grep -qx '  - bound set: floor.rs: assert!(ops > 15, "{ops}"); // check-bound' <<<"$out" ||
  fail "missed a lowered coverage floor: $out"
# A crash script's bound: a `#` comment that ends the line. One quoted in a
# string isn't a tag.
git checkout -q main
printf 'min_rows=2 # check-bound\necho "x # check-bound" y\nmin_ops=3 # check-bound\r\n' >bound.sh
git add bound.sh
git commit -qm shell
git checkout -q -b shell
printf 'min_rows=1 # check-bound\necho "x # check-bound" z\nmin_ops=2 # check-bound\r\n' >bound.sh
git commit -qam shell
out=$("$script" main shell 2>&1) || true
grep -qx '  - bound set: bound.sh: min_rows=1 # check-bound' <<<"$out" ||
  fail "missed a shell bound's new value: $out"
grep -qx '  - bound was: bound.sh: min_rows=2 # check-bound' <<<"$out" ||
  fail "missed a shell bound's old value: $out"
grep -q '  - bound set: bound.sh: min_ops=2 # check-bound' <<<"$out" ||
  fail "missed a shell bound on a CRLF line: $out"
grep -q 'echo' <<<"$out" && fail "took a quoted tag for a bound: $out"
git checkout -q -b shell-prose main
printf 'min_rows=2 # check-bound\necho "x # check-bound" y\nmin_ops=3 # check-bound\r\n# A bound carries # check-bound\n' >bound.sh
git commit -qam shell-prose
out=$("$script" main shell-prose 2>&1) || true
[[ $out == "check-weakened: nothing found" ]] ||
  fail "took a comment's mention of the tag for a bound: $out"

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
# reported: the loss and lag limits, the capture harnesses' floors for
# audio playing, and the real-capture script's baseline floors. The crash
# sweeps' floors are too many to name one by one, so the guard checks each
# file still has at least as many tags as it was given, and that the floors
# of the common shapes (the crash summary's counts, the worst lag reached)
# have theirs, wherever they are.
repo=$scripts/..
recorder=$repo/crates/nota-recorder
tagged() { # what was searched for, the lines found
  [[ -n $2 ]] || fail "found none of: $1"
  while IFS= read -r line; do
    [[ -z $line ]] || grep -qE '//[[:space:]]*check-bound([^A-Za-z0-9_-]|$)' <<<"$line" ||
      fail "bound without its tag: $line"
  done <<<"$2"
}
bounds=$(grep -rE --include='*.rs' 'const (MAX_LAG|LAG_LIMIT|LOSS_LIMIT):' "$repo/crates" || true)
bounds+=$'\n'$(grep -E 'const MIN_PEAK:' "$recorder/examples/real_capture.rs" \
  "$recorder/examples/capture_wall_time.rs" || true)
tagged "the crash harnesses' limits" "$bounds"
(( $(grep -c . <<<"$bounds") >= 6 )) || fail "expected at least 6 tagged bounds: $bounds"
tagged "the crash sweeps' floors" "$(grep -rhE --include='*.rs' \
  'summary\.(scenario_ops|recovery_crashed|rerun_crashed) >|worst(\.get\(\))? >=' \
  "$repo/crates" || true)"
for want in nota-recorder/src/segment/tests.rs:35 nota-recorder/src/capture/stop_tests.rs:6 \
  nota-recorder/src/journal/tests.rs:6 nota-recorder/src/segment/tests/epochs.rs:3 \
  nota-recorder/src/segment/tests/disk_full.rs:5 nota-recorder/src/capture/tests.rs:7 \
  nota/src/library/kept.rs:1 nota-recorder/examples/capture_wall_time.rs:3; do
  file=$repo/crates/${want%:*}
  got=$(grep -cE '//[[:space:]]*check-bound([^A-Za-z0-9_-]|$)' "$file" || true)
  (( got >= ${want#*:} )) || fail "${want%:*} has $got tagged bounds, expected at least ${want#*:}"
done
floors=$(grep -E '^min_[a-z_]+=' "$scripts/real-capture-crash.sh" || true)
[[ $(grep -c . <<<"$floors") -eq 2 ]] || fail "expected the script's 2 baseline floors: $floors"
while IFS= read -r line; do
  grep -qE '[[:space:]]#[[:space:]]*check-bound[[:space:]]*$' <<<"$line" ||
    fail "bound without its tag: $line"
done <<<"$floors"

exit "$status"
