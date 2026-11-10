#!/usr/bin/env bash
# Rejects files that must never be committed (AGENTS.md rule 2): recordings and
# other media, and anything over 1 MiB, which is almost always a recording, a
# model or a build output. The files listed in check-files.allow, beside this
# script, may be larger: each is an exact path with the sha256 of its bytes.
#
# Usage: check-files.sh --staged          files staged for commit (pre-commit hook)
#        check-files.sh BASE HEAD         files added or changed in BASE..HEAD (CI)
#
# Requires: bash (3.2 or later), git, and sha256sum or shasum.
set -euo pipefail

max_bytes=$((1024 * 1024))
media='\.(wav|flac|mp3|m4a|aac|ogg|oga|opus|webm|weba|mp4|mkv|mov|caf|aiff?|wma|pcm|raw)$'
allow_file=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/check-files.allow

usage() {
  echo "usage: check-files.sh --staged | check-files.sh BASE HEAD" >&2
  exit 2
}

# Paths are listed unquoted, so a non-ASCII name is checked by its real name.
# Renames are listed as the file they add, so moving a file in can't skip it,
# and a type change (a symlink made a file) is checked like a modification.
diff_files() {
  git -c core.quotePath=false diff --no-renames --name-only --diff-filter=AMT "$@"
}
if [[ $# -eq 1 && $1 == --staged ]]; then
  files=$(diff_files --cached)
  blob() { git cat-file blob ":$1"; }
  size() { git cat-file -s ":$1"; }
elif [[ $# -eq 2 && $1 != -* ]]; then
  base=$1 head=$2
  files=$(diff_files "$base" "$head")
  blob() { git cat-file blob "$head:$1"; }
  size() { git cat-file -s "$head:$1"; }
else
  usage
fi

sha256() {
  if command -v sha256sum >/dev/null; then
    sha256sum | cut -d' ' -f1
  else
    shasum -a 256 | cut -d' ' -f1
  fi
}

# The allow list, as "sha256  path" lines, kept as the same lines in $allowed.
# A missing or malformed list, or one naming a path twice, stops the check
# rather than passing or refusing everything quietly. Plain strings, not an
# associative array: macOS ships bash 3.2.
if [[ ! -f $allow_file ]]; then
  echo "check-files.sh: allow list missing: $allow_file" >&2
  exit 2
fi
allowed=
line_no=0
while IFS= read -r line || [[ -n $line ]]; do
  line_no=$((line_no + 1))
  [[ -z $line || $line == \#* ]] && continue
  if [[ ! $line =~ ^[0-9a-f]{64}\ \ ([^[:space:]].*)$ ]]; then
    echo "check-files.sh: $allow_file:$line_no: expected \"<sha256>  <path>\": $line" >&2
    exit 2
  fi
  path=${BASH_REMATCH[1]}
  while IFS= read -r seen; do
    if [[ -n $seen && ${seen#*  } == "$path" ]]; then
      echo "check-files.sh: $allow_file:$line_no: path listed twice: $path" >&2
      exit 2
    fi
  done <<<"$allowed"
  allowed+=$line$'\n'
done <"$allow_file"

# True when the file's bytes, as staged or committed, are the ones the allow
# list names for its path.
is_allowed() {
  local entry
  entry="$(blob "$1" | sha256)  $1"
  while IFS= read -r line; do
    [[ -n $line && $line == "$entry" ]] && return 0
  done <<<"$allowed"
  return 1
}

status=0
while IFS= read -r f; do
  [[ -z $f ]] && continue
  if grep -qiE "$media" <<<"$f"; then
    echo "media file (recordings are never committed): $f" >&2
    status=1
  elif ! bytes=$(size "$f"); then
    # A path git still quotes (a control character in its name) can't be
    # read back; refuse it rather than let it pass unchecked.
    echo "cannot read the size of: $f" >&2
    status=1
  elif ((bytes > max_bytes)) && ! is_allowed "$f"; then
    echo "file over 1 MiB: $f ($bytes bytes)" >&2
    status=1
  fi
done <<<"$files"
exit "$status"
