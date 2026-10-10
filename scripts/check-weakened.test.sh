#!/usr/bin/env bash
# Tests check-weakened.sh in a throwaway repo: a branch that is behind its
# base, and changes nothing, reports nothing; a test the branch itself
# deletes is reported; a changed, deleted or untagged `// check-bound` line
# is reported, as are two files swapping values, and one moved unchanged
# isn't; a lowered coverage floor and a crash script's `#`-tagged bound are
# reported, and a tag quoted or mentioned in a shell comment isn't one; a
# base with no merge base is an error; and a call with no base prints its
# usage. Also checks that the crash harnesses' bounds in this repo carry the
# tag, and that every crash or seed sweep in the crash test files (each
# file with tests that uses the fake filesystem) reports into the sweep
# helper, whose floor proves it wasn't vacuous.
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
for want in nota-recorder/src/segment/tests.rs:48 nota-recorder/src/capture/stop_tests.rs:6 \
  nota-recorder/src/journal/tests.rs:10 nota-recorder/src/segment/tests/epochs.rs:3 \
  nota-recorder/src/segment/tests/disk_full.rs:7 nota-recorder/src/capture/tests.rs:9 \
  nota/src/library/kept.rs:1 nota-recorder/examples/capture_wall_time.rs:3 \
  nota-recorder/src/disk/tests.rs:4 nota-recorder/src/segment/findings/tests.rs:5 \
  nota-recorder/src/segment/tests/repair.rs:3 nota-recorder/src/session/marks.rs:2 \
  nota-recorder/src/session/tests.rs:1 nota/src/record/start.rs:4 \
  nota-recorder/src/segment/publisher/tests.rs:2; do
  file=$repo/crates/${want%:*}
  # Only a tag after code counts: a comment of its own sets no bound.
  got=$(grep -cE '^[[:space:]]*[^/[:space:]].*//[[:space:]]*check-bound([^A-Za-z0-9_-]|$)' "$file" || true)
  (( got >= ${want#*:} )) || fail "${want%:*} has $got tagged bounds, expected at least ${want#*:}"
done

# Every crash or seed sweep in these files reports into the sweep helper
# (`nota-recorder/src/fs/sweep.rs`) and states a floor there, which fails
# a sweep that reached too little. A sweep is a loop (`for`, `while`,
# `loop` or `for_each`) that crashes, fails or stops the fake filesystem,
# at a point or with a seed it may vary, or that calls a function or a
# closure doing so at a point it's given without a floor of its own
# (followed through several calls); or a crash test run expected to pass. A function needs
# at least as many floors as the sweeps it runs, so a new sweep can't be
# added without one, even beside another; the helper fails at run time a
# sweep it never floors. A loop over a list of outcomes written out in
# full, crashing nothing else, isn't a sweep: nothing in it can go
# unreached. The awk is POSIX (CI's is mawk); where gawk is installed the
# guard runs it in POSIX mode, so a gawk-only construct fails here first.
#
# One awk program, two jobs: with pass=1 it prints the functions that
# crash or fail at a point they're given and floor nothing; with pass=2,
# each function running more sweeps than it floors. Comments in it would
# end the quoting at their first apostrophe, so they're here: strings,
# block comments and line comments are dropped before matching; floors
# count once per sweep they name (`sweep`, `summary.scenario()`), so two
# floors on one sweep don't cover another; a closure (`let run = |..|`)
# that crashes or fails at a point it's given counts as a helper within
# its function; a method call (`.run(`) is never a helper's; a loop
# ends when the body it opened closes (its head may span lines), and one
# that runs a crash test counts as that run; a run whose next line is
# `.unwrap_err()` expects a failure.
# shellcheck disable=SC2016 # the awk program's `$` aren't the shell's.
sweep_awk='
  function flush() {
    if (name == "") return
    if (pass == 1 && point && floors == 0) print family "|" name
    if (pass == 2 && sweeps > floors)
      printf "%s:%d: %s runs %d sweeps with %d floors\n", file, start, name, sweeps, floors
    name = ""
  }
  BEGIN { n = split(points, list, " "); for (i = 1; i <= n; i++) at_point[list[i]] = 1 }
  FNR == 1 {
    flush(); file = FILENAME; depth = 0
    family = file
    sub(/\/tests\/[^\/]*\.rs$/, "/tests.rs", family)
  }
  /^[[:space:]]*(pub(\([a-z]+\))? )?fn [a-z_0-9]+/ {
    flush()
    match($0, /fn [a-z_0-9]+/)
    name = substr($0, RSTART + 3, RLENGTH - 3)
    start = FNR; sweeps = 0; floors = 0; point = 0; loops = 0; marked = 0; pending = 0; ran = 0
    split("", floored); split("", local); closure = ""
  }
  {
    code = $0
    gsub(/"([^"\\]|\\.)*"/, "\"\"", code)
    gsub(/\/\*([^*]|\*[^\/])*\*\//, "", code)
    sub(/\/\/.*/, "", code)
    if (code ~ /^[[:space:]]*(for .* in |while |loop \{)|\.for_each\(/) {
      loops++; at[loops] = depth; opened[loops] = 0
    }
    if (closure == "" && match(code, /let (mut )?[a-z_][a-z_0-9]* = (move )?\|/)) {
      closure = substr(code, RSTART + 4, RLENGTH - 4)
      sub(/^mut /, "", closure); sub(/ .*/, "", closure)
      closure_at = depth; closure_opened = 0
    }
    raw = code ~ /(crash_after|fail_after)\(\*?[a-z_]/
    if (raw) point = 1
    if (raw && closure != "") local[closure] = 1
    hit = code ~ /crash_after\(|fail_after\(|(fail_at|stop_after_ops): Some\(\*?[a-z_]|Partial \{ seed(: [^0-9 }][^}]*)? \}/
    rest = " " code
    while (match(rest, /[^.A-Za-z_0-9][a-z_][a-z_0-9]*\(/)) {
      f = substr(rest, RSTART + 1, RLENGTH - 2)
      if ((family "|" f) in at_point || f in local) { hit = 1; point = 1 }
      rest = substr(rest, RSTART + RLENGTH)
    }
    if (loops && hit) marked = 1
    if (pending && code ~ /^[[:space:]]*\.unwrap_err\(\)/) sweeps--
    pending = 0
    if (code ~ /\.run\(\)[[:space:]]*($|[.?])/ && code !~ /\.run\(\)\.unwrap_err\(\)/) {
      sweeps++
      ran = loops > 0
      pending = code ~ /\.run\(\)[[:space:]]*$/
    }
    rest = code
    while (match(rest, /\.(interrupted_more_than|interrupted_at_least|saw_more_than|saw_each)\(/)) {
      key = substr(rest, 1, RSTART - 1)
      gsub(/[[:space:]]/, "", key)
      if (key == "") key = "line " FNR
      if (!(key in floored)) { floored[key] = 1; floors++ }
      rest = substr(rest, RSTART + RLENGTH)
    }
    depth += gsub(/\{/, "{", code) - gsub(/\}/, "}", code)
    if (closure != "") {
      if (depth > closure_at) closure_opened = 1
      if (closure_opened ? depth <= closure_at : code ~ /;[[:space:]]*$/) closure = ""
    }
    while (loops) {
      if (depth > at[loops]) opened[loops] = 1
      if (!opened[loops] || depth > at[loops]) break
      loops--
      if (loops == 0) { sweeps += marked && !ran; marked = 0; ran = 0 }
    }
  }
  END { flush() }
'
sweep_awk_cmd=(awk)
if command -v gawk >/dev/null; then sweep_awk_cmd=(gawk --posix); fi
bypassing() { # files; prints each function with more sweeps than floors
  local points="" next round
  # Each round finds the callers of the last round's functions, until a
  # round adds none. One that never settles is an error, not a pass with
  # some helpers unfollowed.
  for round in $(seq 1 50); do
    next=$("${sweep_awk_cmd[@]}" -v pass=1 -v points="$points" "$sweep_awk" "$@" | sort -u | tr '\n' ' ')
    [[ $next == "$points" ]] && break
    points=$next
  done
  if [[ $next != "$points" ]]; then
    echo "bypassing: helpers still unsettled after $round rounds"
    return
  fi
  "${sweep_awk_cmd[@]}" -v pass=2 -v points="$points" "$sweep_awk" "$@"
}
cat >"$dir/sweeps.rs" <<'RUST'
fn bypasses() {
    for at in 0..ops {
        let fs = FakeFs::new();
        fs.crash_after(at);
    }
}
fn seeds_without() {
    for seed in 0..9 {
        check(fs.crash(CrashOutcome::Partial { seed }), "{x}");
    }
}
fn runs_without() {
    let summary = CrashTest::new(a, b, c)
        .run()
        .unwrap();
    let done = job.run();
}
fn runs_a_job() {
    let done = job.run();
}
fn uses_it() {
    let mut sweep = Sweep::new();
    for at in 0..ops {
        fs.fail_after(at, kind);
        sweep.failure_point(&fs);
    }
    sweep.interrupted_at_least(3); // check-bound
}
fn floors_a_crash_test() {
    let summary = CrashTest::new(a, b, c).run().unwrap();
    summary.scenario().interrupted_more_than(40); // check-bound
}
fn expects_a_failure() {
    let failure = CrashTest::new(a, b, c)
        .run()
        .unwrap_err();
}
fn lists_its_outcomes() {
    for outcome in [
        CrashOutcome::Partial { seed: 1 },
        CrashOutcome::KeepAll,
    ] {
        check(fs.crash(outcome));
    }
    fs.crash_after(3);
}
fn crashes_at(fs: &FakeFs, after: usize) {
    fs.crash_after(after);
}
fn sweeps_through_a_helper() {
    for after in 0..=ops {
        crashes_at(&fs, after);
    }
}
fn sweeps_from(fs: &FakeFs, from: usize) {
    let mut sweep = Sweep::new();
    for after in from..9 {
        fs.crash_after(after);
        sweep.crash_point(fs);
    }
    sweep.interrupted_at_least(3); // check-bound
}
fn runs_whole_sweeps() {
    for from in [0, 3] {
        sweeps_from(&fs, from);
    }
}
fn a_second_sweep_beside_one_floored() {
    let mut sweep = Sweep::new();
    for at in 0..9 {
        fs.crash_after(at);
        sweep.crash_point(&fs);
    }
    sweep.interrupted_at_least(3); // check-bound
    /* Sweep::new() .interrupted_at_least(1) */
    for at in 0..9 {
        fs.fail_after(at, kind);
    }
}
fn while_and_for_each() {
    while at < ops {
        fs.crash_after(at);
    }
    (0..ops).for_each(|at| {
        fs.crash_after(at);
    });
}
fn seed_from_an_expression() {
    for i in 0..9 {
        check(fs.crash(CrashOutcome::Partial { seed: i * 7 }));
    }
}
fn fails_through_a_recording() {
    for at in &points {
        record(&fs, Recording { fail_at: Some(*at), ..plain });
    }
}
fn runs_one_to_pass_one_to_fail() {
    CrashTest::new(a, b, c).run()?;
    let failure = CrashTest::new(a, b, c).run().unwrap_err();
}
fn checks_at(fs: &FakeFs, at: usize) {
    crashes_at(fs, at);
}
fn sweeps_two_calls_deep() {
    for at in 0..ops {
        checks_at(&fs, at);
    }
}
fn two_floors_on_one_sweep_cover_one() {
    let mut sweep = Sweep::with_outcomes();
    for at in 0..9 {
        fs.crash_after(at);
        sweep.crash_point(&fs);
        sweep.saw(at);
    }
    sweep.interrupted_at_least(3); // check-bound
    sweep.saw_each([1, 2]); // check-bound
    for at in 0..9 {
        fs.crash_after(at);
    }
}
fn sweeps_through_a_closure() {
    let run = |fail_at: Option<usize>| {
        if let Some(at) = fail_at {
            disk.fail_after(at, kind);
        }
        salvage(&disk)
    };
    for at in 0..ops {
        run(Some(at));
    }
}
fn calls_a_method_named_like_a_helper() {
    for job in jobs {
        job.crashes_at(&fs, 3);
    }
}
RUST
cat >"$dir/tail.rs" <<'RUST'
fn the_last_function() {
    for at in 0..ops {
        fs.crash_after(at);
    }
}
RUST
# A helper is followed within its module's family (a parent's `tests.rs`
# and the files in its `tests/`), not into another module with a function
# of the same name.
mkdir -p "$dir/x/tests" "$dir/z"
cat >"$dir/x/tests.rs" <<'RUST'
fn crashes_here(fs: &FakeFs, after: usize) {
    fs.crash_after(after);
}
RUST
cat >"$dir/x/tests/y.rs" <<'RUST'
fn sweeps_through_a_parent_helper() {
    for after in 0..9 {
        crashes_here(&fs, after);
    }
}
RUST
cat >"$dir/z/tests.rs" <<'RUST'
fn calls_another_modules_name() {
    for after in 0..9 {
        crashes_here(&fs, after);
    }
}
RUST
got=$(cd "$dir" && bypassing x/tests.rs x/tests/y.rs z/tests.rs)
[[ $got == "x/tests/y.rs:1: sweeps_through_a_parent_helper runs 1 sweeps with 0 floors" ]] ||
  fail "the sweep guard followed helpers across the wrong modules: $got"
got=$(cd "$dir" && bypassing sweeps.rs tail.rs)
want='sweeps.rs:1: bypasses runs 1 sweeps with 0 floors
sweeps.rs:7: seeds_without runs 1 sweeps with 0 floors
sweeps.rs:12: runs_without runs 1 sweeps with 0 floors
sweeps.rs:50: sweeps_through_a_helper runs 1 sweeps with 0 floors
sweeps.rs:68: a_second_sweep_beside_one_floored runs 2 sweeps with 1 floors
sweeps.rs:80: while_and_for_each runs 2 sweeps with 0 floors
sweeps.rs:88: seed_from_an_expression runs 1 sweeps with 0 floors
sweeps.rs:93: fails_through_a_recording runs 1 sweeps with 0 floors
sweeps.rs:98: runs_one_to_pass_one_to_fail runs 1 sweeps with 0 floors
sweeps.rs:105: sweeps_two_calls_deep runs 1 sweeps with 0 floors
sweeps.rs:110: two_floors_on_one_sweep_cover_one runs 2 sweeps with 1 floors
sweeps.rs:123: sweeps_through_a_closure runs 1 sweeps with 0 floors
tail.rs:1: the_last_function runs 1 sweeps with 0 floors'
[[ $got == "$want" ]] || fail "the sweep guard found the wrong sweeps: $got"
# The files checked: every one with tests, so a new crash test file is
# covered without naming it here, even one that reaches the fake
# filesystem only through its parent's helpers. The fake, the crash test
# and the sweep helper are left out: their own tests check the
# instruments, crashing them on purpose to see them work.
swept=()
while IFS= read -r file; do
  swept+=("$file")
done < <(cd "$repo/crates" && grep -rl --include='*.rs' '#\[test\]' . | sed 's#^\./##' |
  grep -vxE 'nota-recorder/src/fs/(fake|crash|sweep)\.rs' | sort)
(( ${#swept[@]} >= 40 )) || fail "expected at least 40 test files: ${swept[*]-}"
found=$(cd "$repo/crates" && bypassing "${swept[@]}")
[[ -z $found ]] || fail "a sweep without the helper's floor:"$'\n'"$found"
# Each floor the helper checks is a bound, wherever it is.
tagged "the sweeps' floors" "$(grep -rhE --include='*.rs' \
  '\.(interrupted_more_than|interrupted_at_least|saw_more_than|saw_each)\(' \
  --exclude=sweep.rs "$repo/crates" || true)"

floors=$(grep -E '^min_[a-z_]+=' "$scripts/real-capture-crash.sh" || true)
[[ $(grep -c . <<<"$floors") -eq 2 ]] || fail "expected the script's 2 baseline floors: $floors"
while IFS= read -r line; do
  grep -qE '[[:space:]]#[[:space:]]*check-bound[[:space:]]*$' <<<"$line" ||
    fail "bound without its tag: $line"
done <<<"$floors"

exit "$status"
