#!/usr/bin/env bash
# Rejects files that must never be committed (AGENTS.md rule 2): recordings and
# other media, and anything over 1 MiB, which is almost always a recording, a
# model or a build output. The files listed in check-files.allow, beside this
# script, may be larger: each is an exact path with the sha256 of its bytes.
#
# Usage: check-files.sh --staged          files staged for commit (pre-commit hook)
#        check-files.sh BASE HEAD         files added or changed in BASE..HEAD (CI)
set -euo pipefail

max_bytes=$((1024 * 1024))
media='\.(wav|flac|mp3|m4a|aac|ogg|oga|opus|webm|weba|mp4|mkv|mov|caf|aiff?|wma|pcm|raw)$'
allow_file=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-files.allow

usage() {
  echo "usage: check-files.sh --staged | check-files.sh BASE HEAD" >&2
  exit 2
}

# Renames are listed as the file they add, so moving a file in can't skip it.
if [[ $# -eq 1 && $1 == --staged ]]; then
  files=$(git diff --cached --no-renames --name-only --diff-filter=AM)
  blob() { git cat-file blob ":$1"; }
  size() { git cat-file -s ":$1"; }
elif [[ $# -eq 2 && $1 != -* ]]; then
  base=$1 head=$2
  files=$(git diff --no-renames --name-only --diff-filter=AM "$base" "$head")
  blob() { git cat-file blob "$head:$1"; }
  size() { git cat-file -s "$head:$1"; }
else
  usage
fi

# The allow list, as "sha256  path" lines. A missing or malformed list stops
# the check rather than passing or refusing everything quietly.
declare -A allowed=()
if [[ ! -f $allow_file ]]; then
  echo "check-files.sh: allow list missing: $allow_file" >&2
  exit 2
fi
line_no=0
while IFS= read -r line || [[ -n $line ]]; do
  line_no=$((line_no + 1))
  [[ -z $line || $line == \#* ]] && continue
  if [[ ! $line =~ ^([0-9a-f]{64})\ \ ([^[:space:]].*)$ ]]; then
    echo "check-files.sh: $allow_file:$line_no: expected \"<sha256>  <path>\": $line" >&2
    exit 2
  fi
  allowed[${BASH_REMATCH[2]}]=${BASH_REMATCH[1]}
done <"$allow_file"

# True when the file's bytes are the ones the allow list names for its path.
is_allowed() {
  local want=${allowed[$1]-} got
  [[ -n $want ]] || return 1
  got=$(blob "$1" | sha256sum)
  [[ ${got%% *} == "$want" ]]
}

status=0
while IFS= read -r f; do
  [[ -z $f ]] && continue
  if grep -qiE "$media" <<<"$f"; then
    echo "media file (recordings are never committed): $f" >&2
    status=1
  elif (( $(size "$f") > max_bytes )) && ! is_allowed "$f"; then
    echo "file over 1 MiB: $f ($(size "$f") bytes)" >&2
    status=1
  fi
done <<<"$files"
exit "$status"
