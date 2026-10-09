#!/usr/bin/env bash
# Tests check-files.sh's arguments: a call without the ones it needs prints
# its usage instead of failing on an unbound variable, and a media file in a
# commit range is refused.
#
# Requires: bash, git.
set -euo pipefail

script=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-files.sh
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT

status=0
fail() {
  echo "check-files.test.sh: $*" >&2
  status=1
}

cd "$dir"
git init -q -b main
git config user.name test
git config user.email test@example.invalid
git config commit.gpgsign false
echo text >a.txt
git add a.txt
git commit -qm one
touch talk.wav
git add talk.wav
git commit -qm two

for args in "" "HEAD~1" "--bogus" "--staged HEAD"; do
  # shellcheck disable=SC2086 # split on purpose: each case is a word list
  if out=$("$script" $args 2>&1); then
    fail "accepted arguments \"$args\""
  fi
  grep -q '^usage: check-files.sh' <<<"$out" || fail "no usage for arguments \"$args\": $out"
done

if out=$("$script" HEAD~1 HEAD 2>&1); then
  fail "accepted a media file"
fi
grep -q 'media file (recordings are never committed): talk.wav' <<<"$out" ||
  fail "expected talk.wav refused, got: $out"
"$script" HEAD~1 HEAD~1 >/dev/null 2>&1 || fail "refused an empty range"

exit "$status"
