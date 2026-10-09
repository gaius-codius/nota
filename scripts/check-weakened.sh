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
