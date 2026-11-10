#!/usr/bin/env bash
# Tests check-files.sh: a call without the arguments it needs prints its usage
# instead of failing on an unbound variable, a media file is refused, a file
# over 1 MiB is refused unless check-files.allow names its exact path and
# the sha256 of its staged or committed bytes, a rename, a type change and a
# non-ASCII name are checked like any other file, and the shipped allow list
# holds exactly the wordfreq entry. The allow-list cases run a copy of the
# script beside a list of the test's own, so the script itself has no override.
#
# Requires: bash, git, sha256sum.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
script=$here/check-files.sh
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT

status=0
fail() {
  echo "check-files.test.sh: $*" >&2
  status=1
}

mkdir "$dir/repo"
cd "$dir/repo"
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

# The shipped list: exactly the wordfreq file from #148, nothing else.
want='dffae8066b78dce0a6667cf5f58e567054f902674667090a7ac8a8a44628b05c  crates/nota-checks/data/wordfreq/large_en.msgpack.gz'
got=$(grep -v -e '^#' -e '^$' "$here/check-files.allow")
[[ $got == "$want" ]] || fail "shipped allow list is not the wordfreq entry alone: $got"

# A copy of the script beside an allow list that names big/data.bin, filled
# with 1,100,000 "a" bytes, and big/rec.wav, filled with "w".
mkdir "$dir/tool"
cp "$script" "$dir/tool/check-files.sh"
copy=$dir/tool/check-files.sh
fill() { mkdir -p "$(dirname "$2")"; head -c 1100000 /dev/zero | tr '\0' "$1" >"$2"; }
fill a "$dir/a.bin"
fill w "$dir/w.bin"
sum() { sha256sum "$1" | cut -d' ' -f1; }
{
  echo "# test list"
  echo
  echo "$(sum "$dir/a.bin")  big/data.bin"
  echo "$(sum "$dir/w.bin")  big/rec.wav"
} >"$dir/tool/check-files.allow"

# commit_case NAME: commits what's staged, then checks HEAD~1..HEAD.
# expect_pass / expect_refused WHAT [MESSAGE] run the copy on that range.
commit_case() { git commit -qm "$1"; }
expect_pass() {
  local out
  out=$("$copy" HEAD~1 HEAD 2>&1) || fail "refused $1: $out"
}
expect_refused() {
  local out
  if out=$("$copy" HEAD~1 HEAD 2>&1); then
    fail "accepted $1"
  fi
  grep -qF -- "$2" <<<"$out" || fail "$1: expected \"$2\", got: $out"
}

# The listed path with the listed bytes passes, staged and in a range.
mkdir big
cp "$dir/a.bin" big/data.bin
git add big/data.bin
out=$("$copy" --staged 2>&1) || fail "refused the allow-listed file when staged: $out"
commit_case allowed
expect_pass "the allow-listed file"

# The listed path with other bytes is refused as over 1 MiB, staged and in a
# range, even with the listed bytes back on disk: the check reads what is
# staged or committed, not the working tree.
fill b big/data.bin
git add big/data.bin
cp "$dir/a.bin" big/data.bin
if out=$("$copy" --staged 2>&1); then
  fail "accepted other bytes at the allow-listed path when staged"
fi
grep -qF 'file over 1 MiB: big/data.bin (1100000 bytes)' <<<"$out" ||
  fail "staged other bytes: got: $out"
commit_case changed
expect_refused "other bytes at the allow-listed path" 'file over 1 MiB: big/data.bin (1100000 bytes)'

# The listed bytes at another path are refused.
cp "$dir/a.bin" big/copy.bin
git add big/copy.bin
commit_case elsewhere
expect_refused "the allow-listed bytes at another path" 'file over 1 MiB: big/copy.bin (1100000 bytes)'

# Any other file over 1 MiB is refused.
fill c other.bin
git add other.bin
commit_case other
expect_refused "an unlisted large file" 'file over 1 MiB: other.bin (1100000 bytes)'

# Renaming the allowed file to another path is checked at its new path, and a
# rename to a media name is refused as media.
git rm -q big/copy.bin other.bin
cp "$dir/a.bin" big/data.bin
git add big/data.bin
commit_case reset
git mv big/data.bin moved.bin
commit_case moved
expect_refused "the allow-listed file renamed" 'file over 1 MiB: moved.bin (1100000 bytes)'
git mv a.txt lecture.wav
commit_case renamed-media
expect_refused "a rename to a media name" 'media file (recordings are never committed): lecture.wav'

# A large file with a non-ASCII name is checked by its real name, and a symlink
# replaced by a large file is checked like any change.
fill d "café.bin"
git add "café.bin"
commit_case non-ascii
expect_refused "a large file with a non-ASCII name" 'file over 1 MiB: café.bin (1100000 bytes)'
ln -s a.bin link.bin
git add link.bin
commit_case symlink
rm link.bin
fill e link.bin
git add link.bin
commit_case typechange
expect_refused "a symlink replaced by a large file" 'file over 1 MiB: link.bin (1100000 bytes)'

# A name git still quotes (a tab in it) can't be read back, so it is refused
# rather than passed unchecked.
echo small >"$dir/repo/tab"$'\t'"name.txt"
git add "tab"$'\t'"name.txt"
commit_case tab
expect_refused "a name with a tab in it" 'cannot read the size of: "tab\tname.txt"'

# The media rule wins over the allow list.
cp "$dir/w.bin" big/rec.wav
git add big/rec.wav
commit_case media
expect_refused "an allow-listed media file" 'media file (recordings are never committed): big/rec.wav'

# A missing or malformed allow list, or one naming a path twice, stops the
# check.
mv "$dir/tool/check-files.allow" "$dir/tool/saved.allow"
if out=$("$copy" HEAD~1 HEAD~1 2>&1); then
  fail "ran without an allow list"
fi
grep -qF 'allow list missing' <<<"$out" || fail "missing list: got: $out"
for bad in "abc  big/data.bin" "$(sum "$dir/a.bin") big/data.bin" "$(sum "$dir/a.bin")  "; do
  printf '%s\n' "$bad" >"$dir/tool/check-files.allow"
  if out=$("$copy" HEAD~1 HEAD~1 2>&1); then
    fail "ran with a malformed allow list line: $bad"
  fi
  grep -qF 'expected "<sha256>  <path>"' <<<"$out" || fail "malformed line \"$bad\": got: $out"
done
printf '%s  big/data.bin\n' "$(sum "$dir/a.bin")" "$(sum "$dir/w.bin")" >"$dir/tool/check-files.allow"
if out=$("$copy" HEAD~1 HEAD~1 2>&1); then
  fail "ran with a path listed twice"
fi
grep -qF 'check-files.allow:2: path listed twice: big/data.bin' <<<"$out" || fail "duplicate path: got: $out"

exit "$status"
