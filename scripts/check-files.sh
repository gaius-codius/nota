#!/usr/bin/env bash
# Rejects files that must never be committed (AGENTS.md rule 2): recordings and
# other media, and anything over 1 MiB, which is almost always a recording, a
# model or a build output.
#
# Usage: check-files.sh --staged          files staged for commit (pre-commit hook)
#        check-files.sh BASE HEAD         files added or changed in BASE..HEAD (CI)
set -euo pipefail

max_bytes=$((1024 * 1024))
media='\.(wav|flac|mp3|m4a|aac|ogg|oga|opus|webm|weba|mp4|mkv|mov|caf|aiff?|wma|pcm|raw)$'

usage() {
  echo "usage: check-files.sh --staged | check-files.sh BASE HEAD" >&2
  exit 2
}

if [[ $# -eq 1 && $1 == --staged ]]; then
  files=$(git diff --cached --name-only --diff-filter=AM)
  size() { git cat-file -s ":$1"; }
elif [[ $# -eq 2 && $1 != -* ]]; then
  base=$1 head=$2
  files=$(git diff --name-only --diff-filter=AM "$base" "$head")
  size() { git cat-file -s "$head:$1"; }
else
  usage
fi

status=0
while IFS= read -r f; do
  [[ -z $f ]] && continue
  if grep -qiE "$media" <<<"$f"; then
    echo "media file (recordings are never committed): $f" >&2
    status=1
  elif (( $(size "$f") > max_bytes )); then
    echo "file over 1 MiB: $f ($(size "$f") bytes)" >&2
    status=1
  fi
done <<<"$files"
exit "$status"
