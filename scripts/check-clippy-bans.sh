#!/usr/bin/env bash
# Fails when a crate in the tree isn't classified in clippy-crates.txt, or when
# a function the map lists under [ban] isn't in clippy.toml's
# disallowed-methods. So a dependency that brings a new way to read the clock
# or write a file (as jiff brought `Timestamp::now`) fails here until someone
# has looked, instead of passing clippy because nobody banned it.
#
# Also fails on a map entry for a crate no longer in the tree, a crate listed
# twice or under contradictory sections, and an [indirect] crate that a nota
# crate now depends on directly. clippy-crates.txt explains the sections.
#
# Usage: check-clippy-bans.sh [MAP CLIPPY_TOML LOCK...]
# With no arguments, checks the repo's clippy-crates.txt and clippy.toml
# against Cargo.lock and fuzz/Cargo.lock.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
if [[ $# -eq 0 ]]; then
  cd "$root"
  set -- clippy-crates.txt clippy.toml Cargo.lock fuzz/Cargo.lock
fi
if [[ $# -lt 3 ]]; then
  echo "usage: check-clippy-bans.sh [MAP CLIPPY_TOML LOCK...]" >&2
  exit 2
fi
for f in "$@"; do
  if [[ ! -r $f ]]; then
    echo "check-clippy-bans: can't read $f" >&2
    exit 2
  fi
done
map=$1

# POSIX awk only: CI's awk is mawk. Files in order: the map, clippy.toml,
# then each lock. Prints one problem per line.
problems=$(awk -v map="$map" '
function problem(s) { print s }
function ident(name) { gsub(/-/, "_", name); return name }
function quoted(s) { sub(/^[^"]*"/, "", s); sub(/".*$/, "", s); return s }
function flush(   i) {
  if (pkg != "") {
    if (src) {
      external[pkg] = 1
      in_tree[ident(pkg)] = pkg
    } else {
      for (i = 1; i <= ndeps; i++) dep_of_member[deps[i]] = 1
    }
  }
  pkg = ""; src = 0; ndeps = 0; in_deps = 0
}

FNR == 1 { flush(); file++ }

# The map.
file == 1 {
  line = $0
  sub(/#.*/, "", line)
  if (line ~ /^[ \t]*$/) next
  if (line ~ /^\[/) {
    if (line !~ /^\[[a-z]+(:[^]]*)?\][ \t]*$/) {
      problem(map ":" FNR ": not a section header: " line); kind = "?"; next
    }
    kind = line; sub(/^\[/, "", kind); sub(/\][ \t]*$/, "", kind)
    reason = ""
    if (index(kind, ":")) {
      reason = substr(kind, index(kind, ":") + 1); kind = substr(kind, 1, index(kind, ":") - 1)
      gsub(/^[ \t]+|[ \t]+$/, "", reason)
    }
    if (kind != "ban" && kind != "allow" && kind != "none" && kind != "indirect") {
      problem(map ":" FNR ": unknown section [" kind "]"); kind = "?"; next
    }
    if (kind == "ban" && reason != "") problem(map ":" FNR ": [ban] takes no reason; clippy.toml gives each ban its reason")
    if (kind != "ban" && reason == "") problem(map ":" FNR ": [" kind "] needs a reason: [" kind ": why]")
    next
  }
  if (kind == "") { problem(map ":" FNR ": entry before any section"); kind = "?" }
  if (kind == "?") next
  n = split(line, words, /[ \t]+/)
  for (i = 1; i <= n; i++) {
    w = words[i]
    if (w == "") continue
    if (index(w, "::")) {
      if (kind != "ban" && kind != "allow") { problem(map ":" FNR ": a path under [" kind "], which takes crate names: " w); continue }
      if (w !~ /^[A-Za-z_][A-Za-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)+$/) { problem(map ":" FNR ": not a path: " w); continue }
      c = substr(w, 1, index(w, "::") - 1)
      if (w in path_line) { problem(map ":" FNR ": " w " is already listed on line " path_line[w]); continue }
      path_line[w] = FNR
      has_path[c] = FNR
      if (kind == "ban") banned_in_map[w] = FNR
    } else {
      if (kind == "ban") { problem(map ":" FNR ": [ban] takes paths, not crate names: " w); continue }
      if (w !~ /^[A-Za-z0-9_-]+$/) { problem(map ":" FNR ": not a crate name: " w); continue }
      c = ident(w)
      if (c in named_line) { problem(map ":" FNR ": " w " is already listed on line " named_line[c]); continue }
      named_line[c] = FNR
      named_kind[c] = kind
      named_as[c] = w
    }
  }
  next
}

# clippy.toml: the paths in disallowed-methods.
file == 2 {
  if ($0 ~ /^[a-z-]+[ \t]*=/) in_methods = ($0 ~ /^disallowed-methods[ \t]*=/)
  if (in_methods && match($0, /path[ \t]*=[ \t]*"[^"]*"/))
    in_clippy[quoted(substr($0, RSTART, RLENGTH))] = 1
  next
}

# A lock: packages with a source are dependencies, those without are the
# workspace members, and the crates those depend on are direct.
/^\[\[package\]\]/ { flush(); next }
/^name = / { pkg = quoted($0); next }
/^source = / { src = 1; next }
/^dependencies = \[/ { in_deps = 1; next }
in_deps && /^\]/ { in_deps = 0; next }
in_deps { d = quoted($0); sub(/ .*/, "", d); deps[++ndeps] = d; next }

END {
  flush()
  for (p in external)
    if (!(ident(p) in has_path) && !(ident(p) in named_line))
      problem(p ": not in " map "; list what it has that reads the clock or writes files under [ban] (and in clippy.toml), or put it under [none] or [indirect] with a reason")
  for (c in has_path) {
    if (!(c in in_tree)) problem(map ":" has_path[c] ": no crate " c " in the lock files; remove its entries")
    if ((c in named_kind) && named_kind[c] != "allow")
      problem(map ":" named_line[c] ": " named_as[c] " is under [" named_kind[c] "] but has functions listed on line " has_path[c])
  }
  for (c in named_line)
    if (!(c in in_tree)) problem(map ":" named_line[c] ": no crate " named_as[c] " in the lock files; remove it")
  for (w in banned_in_map)
    if (!(w in in_clippy)) problem(map ":" banned_in_map[w] ": " w " is under [ban], but clippy.toml'"'"'s disallowed-methods doesn'"'"'t name it")
  for (p in dep_of_member)
    if ((ident(p) in named_kind) && named_kind[ident(p)] == "indirect")
      problem(map ":" named_line[ident(p)] ": " p " is under [indirect], but a nota crate now depends on it directly; classify its functions")
}
' "$@" | sort -V)

if [[ -n $problems ]]; then
  echo "$problems" >&2
  echo "check-clippy-bans: $(wc -l <<<"$problems") problems; see the top of clippy-crates.txt" >&2
  exit 1
fi
echo "check-clippy-bans: every crate in the tree is classified, and every ban is in clippy.toml"
