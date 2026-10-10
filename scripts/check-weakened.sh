#!/usr/bin/env bash
# Lists changes that weaken or bypass a check, so they're seen and explained
# rather than slipped in. Coding agents reach for these when a check is in the
# way; sometimes they're right, but never silently.
#
# Usage: check-weakened.sh BASE [HEAD]
# Compares HEAD with its merge base with BASE (`git diff BASE...HEAD`), so on
# a branch behind BASE, what BASE gained since isn't counted as removed.
# Prints each finding. Exits 1 if there are findings and no explanation was
# given: in CI, a line starting "Check changes:" in the PR body (passed in the
# PR_BODY environment variable). Locally, with no PR_BODY, it only reports.
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "usage: check-weakened.sh BASE [HEAD]" >&2
  exit 2
fi
base=$1 head=${2:-HEAD}
# Every diff below runs where `set -e` can't see git fail, so a missing merge
# base (a shallow clone, unrelated histories) must stop the script here, or
# it would report nothing found.
if ! mb=$(git merge-base "$base" "$head"); then
  echo "check-weakened: no merge base of $base and $head (a shallow clone?)" >&2
  exit 2
fi
findings=()

# Config that defines the checks.
while IFS= read -r f; do
  [[ -n $f ]] && findings+=("check config changed: $f")
done < <(git diff --name-only "$mb" "$head" -- \
  rust-toolchain.toml rustfmt.toml clippy.toml clippy-crates.txt typos.toml deny.toml \
  .config/nextest.toml .cargo/config.toml .cargo/mutants.toml lefthook.yml \
  .github/workflows 'scripts/check-*' \
  | sort -u)

# Lint table edits in any Cargo.toml.
if git diff -U0 "$mb" "$head" -- '*Cargo.toml' \
  | grep -E '^[-+][^-+]' \
  | grep -qE '\[(workspace\.)?lints|"(allow|warn|deny|forbid)"'; then
  findings+=("lint levels changed in a Cargo.toml")
fi

# Added lines in Rust code that silence a lint or skip a test.
while IFS= read -r line; do
  [[ -n $line ]] && findings+=("added: $line")
done < <(git diff -U0 "$mb" "$head" -- '*.rs' \
  | grep -E '^\+[^+]' \
  | grep -E '#!?\[(allow|expect)\(|#\[ignore|cfg_attr\([^)]*(allow|expect|ignore)|no_mangle|\bunsafe\b' \
  | sed 's/^+[[:space:]]*//' || true)

# Bounds a test or harness checks (a crash test's loss limit, a capture
# harness's lag bound, a sweep's floor on the crash points it reached) carry
# a check-bound comment on the line that sets them, so a change to that line
# is reported: a new value, a deleted constant or a dropped tag. In Rust the
# tag is a `//` comment anywhere on the line; in a shell script it's a `#`
# comment that ends a line of code, so a tag quoted in a string or mentioned
# in a comment isn't one. A bound loosened where it's compared, not where it's set, is left to
# review. Lines are compared with their file's path, so a tagged line moved
# unchanged within its file isn't counted, and two files swapping values are.
bound_lines() { # - for the base's side, + for the branch's
  git diff -U0 "$mb" "$head" -- '*.rs' '*.sh' \
    | awk -v side="$1" '
        /^diff --git / { header = 1; shell = /\.sh$/; next }
        header && /^--- / { old = substr($0, 5); sub(/^a\//, "", old); next }
        header && /^\+\+\+ / { new = substr($0, 5); sub(/^b\//, "", new); next }
        /^@@/ { header = 0; next }
        header { next }
        substr($0, 1, 1) != side { next }
        shell ? /^.[ \t]*[^# \t].*[ \t]#[ \t]*check-bound[ \t]*$/ : /\/\/[ \t]*check-bound([^A-Za-z0-9_-]|$)/ {
          line = substr($0, 2); sub(/^[ \t]+/, "", line)
          print (side == "-" ? old : new) ": " line
        }' \
    | LC_ALL=C sort || true
}
while IFS= read -r line; do
  [[ -n $line ]] && findings+=("bound set: $line")
done < <(LC_ALL=C comm -13 <(bound_lines -) <(bound_lines +))
while IFS= read -r line; do
  [[ -n $line ]] && findings+=("bound was: $line")
done < <(LC_ALL=C comm -23 <(bound_lines -) <(bound_lines +))

# Tests removed: more #[test] attributes deleted than added.
removed=$(git diff -U0 "$mb" "$head" -- '*.rs' | grep -cE '^-[[:space:]]*#\[(test|proptest|rstest)' || true)
added=$(git diff -U0 "$mb" "$head" -- '*.rs' | grep -cE '^\+[[:space:]]*#\[(test|proptest|rstest)' || true)
if (( removed > added )); then
  findings+=("tests removed: $removed test attributes deleted, $added added")
fi

# Snapshot files changed (accepted with cargo insta).
while IFS= read -r f; do
  [[ -n $f ]] && findings+=("snapshot changed: $f")
done < <(git diff --name-only --diff-filter=MD "$mb" "$head" -- '*.snap')

if (( ${#findings[@]} == 0 )); then
  echo "check-weakened: nothing found"
  exit 0
fi

echo "Changes that weaken or bypass a check:"
printf '  - %s\n' "${findings[@]}"

if [[ -z ${PR_BODY+x} ]]; then
  echo "(report only; in CI the PR body must explain these in a \"Check changes:\" line)"
  exit 0
fi
if grep -qiE '^[[:space:]]*check changes:[[:space:]]*[^[:space:]]' <<<"$PR_BODY"; then
  echo "Explained in the PR body."
  exit 0
fi
echo "Add a line starting \"Check changes:\" to the PR body explaining why." >&2
exit 1
