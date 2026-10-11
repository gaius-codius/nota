#!/usr/bin/env bash
# Fails when a crate in the tree isn't classified in clippy-crates.txt, or when
# a function or type the map lists under [ban] isn't in clippy.toml's
# disallowed-methods or disallowed-types. So a dependency that brings a new way
# to read the clock or write a file (as jiff brought `Timestamp::now`) fails
# here until someone has looked, instead of passing clippy because nobody
# banned it. nota's own crates are the ones named nota or nota-*; every other
# package in the lock files counts as a dependency.
#
# Also fails when the lock files move a classified crate past the semver line
# it was reviewed at (`jiff@0.2`: 0.3 fails, 0.2.9 passes), since the version
# that can add a clock read or a file write is the one that can add API. And
# on a map entry for a crate no longer in the tree, a crate listed twice or
# under contradictory sections, and an [indirect] crate that a nota crate now
# depends on directly. clippy-crates.txt explains the sections.
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
# The semver-compatible line of a version: 1.4.2 is on line 1, 0.4.2 on 0.4,
# 0.0.3 on 0.0.3. A pre-release or build suffix leaves it as it is.
function line_of(v,   p) {
  sub(/[-+].*/, "", v)
  split(v, p, ".")
  if (p[1] + 0 > 0) return p[1] + 0
  if (p[2] + 0 > 0) return "0." (p[2] + 0)
  return "0.0." (p[3] + 0)
}
# Records the lines in LIST (comma-separated) as the lines of crate C, from map line N.
function reviewed(c, list, w, n,   k, ls, i) {
  if (c in line_at) { problem(map ":" n ": " w " already has its reviewed line on line " line_at[c]); return }
  line_at[c] = n
  k = split(list, ls, ",")
  for (i = 1; i <= k; i++) {
    if (ls[i] !~ /^([1-9][0-9]*|0\.[1-9][0-9]*|0\.0\.(0|[1-9][0-9]*))$/) {
      problem(map ":" n ": not a semver line (1, 0.4 or 0.0.3): " w); continue
    }
    if ((c, ls[i]) in has_line) { problem(map ":" n ": line " ls[i] " twice: " w); continue }
    has_line[c, ls[i]] = 1
    lines_of[c] = lines_of[c] (lines_of[c] == "" ? "" : ",") ls[i]
  }
}
function flush(   i) {
  if (pkg != "") {
    # The crates of nota are named nota or nota-*; any other package,
    # including a path dependency outside the workspace, needs classifying.
    if (!src && pkg ~ /^nota(-|$)/) {
      for (i = 1; i <= ndeps; i++) dep_of_member[deps[i]] = 1
    } else {
      external[pkg] = 1
      in_tree[ident(pkg)] = pkg
      if (!((ident(pkg), line_of(ver)) in tree_ver))
        tree_ver[ident(pkg), line_of(ver)] = ver " in " pkg_lock
    }
  }
  pkg = ""; ver = ""; src = 0; ndeps = 0; in_deps = 0
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
    if (kind != "ban" && kind != "allow" && kind != "none" && kind != "indirect" && kind != "reviewed") {
      problem(map ":" FNR ": unknown section [" kind "]"); kind = "?"; next
    }
    if ((kind == "ban" || kind == "reviewed") && reason != "") problem(map ":" FNR ": [" kind "] takes no reason")
    if (kind != "ban" && kind != "reviewed" && reason == "") problem(map ":" FNR ": [" kind "] needs a reason: [" kind ": why]")
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
      if (index(w, "@")) { problem(map ":" FNR ": a path takes no line; give its crate one under [reviewed]: " w); continue }
      if (w !~ /^[A-Za-z_][A-Za-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)+$/) { problem(map ":" FNR ": not a path: " w); continue }
      c = substr(w, 1, index(w, "::") - 1)
      if (w in path_line) { problem(map ":" FNR ": " w " is already listed on line " path_line[w]); continue }
      path_line[w] = FNR
      has_path[c] = FNR
      if (kind == "ban") banned_in_map[w] = FNR
    } else {
      if (kind == "ban") { problem(map ":" FNR ": [ban] takes paths, not crate names: " w); continue }
      name = w; list = ""
      if (index(w, "@")) { name = substr(w, 1, index(w, "@") - 1); list = substr(w, index(w, "@") + 1) }
      if (name !~ /^[A-Za-z0-9_-]+$/) { problem(map ":" FNR ": not a crate name: " w); continue }
      c = ident(name)
      if (kind == "indirect") {
        if (list != "") problem(map ":" FNR ": [indirect] takes no line, as nota code never names the crate: " w)
      } else if (list == "") {
        problem(map ":" FNR ": " w " needs the semver line it was reviewed at: " w "@1, or " w "@0.4")
      } else {
        reviewed(c, list, w, FNR)
      }
      if (kind == "reviewed") { reviewed_as[c] = name; continue }
      if (c in named_line) { problem(map ":" FNR ": " name " is already listed on line " named_line[c]); continue }
      named_line[c] = FNR
      named_kind[c] = kind
      named_as[c] = name
    }
  }
  next
}

# clippy.toml: the paths in disallowed-methods, and in disallowed-types (a
# banned type takes all its functions with it). Comments are dropped first,
# so a ban commented out does not count; a path always comes before any "#"
# in a reason.
file == 2 {
  line = $0
  sub(/#.*/, "", line)
  if (line ~ /^[a-z-]+[ \t]*=/) in_bans = (line ~ /^disallowed-(methods|types)[ \t]*=/)
  if (in_bans && match(line, /path[ \t]*=[ \t]*"[^"]*"/))
    in_clippy[quoted(substr(line, RSTART, RLENGTH))] = 1
  next
}

# A lock: the crates of nota have no source; the crates they depend on are
# direct.
/^\[\[package\]\]/ { flush(); next }
/^name = / { pkg = quoted($0); pkg_lock = FILENAME; next }
/^version = / { ver = quoted($0); next }
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
    if (!(w in in_clippy)) problem(map ":" banned_in_map[w] ": " w " is under [ban], but clippy.toml'"'"'s disallowed-methods and disallowed-types don'"'"'t name it")
  # Every crate nota can name has the line it was reviewed at, and the lock
  # files have it on that line only.
  for (c in in_tree) {
    if (!(c in has_path) && !((c in named_kind) && named_kind[c] != "indirect")) continue
    if (!(c in line_at)) {
      if (!(c in named_line)) problem(in_tree[c] ": no reviewed line; add " in_tree[c] "@<line> under [reviewed]")
      continue
    }
  }
  for (k in tree_ver) {
    split(k, kl, SUBSEP)
    c = kl[1]
    if (!(c in line_at) || (k in has_line)) continue
    problem(map ":" line_at[c] ": " in_tree[c] " " tree_ver[k] " is past line " lines_of[c] ", where it was reviewed; look for what the new version adds that reads the clock or writes files, as for a new crate, then record line " kl[2])
  }
  for (k in has_line) {
    split(k, kl, SUBSEP)
    if ((kl[1] in in_tree) && !(k in tree_ver))
      problem(map ":" line_at[kl[1]] ": " in_tree[kl[1]] " was reviewed at line " kl[2] ", which the lock files no longer have; remove it")
  }
  for (c in reviewed_as) {
    if (!(c in in_tree)) problem(map ":" line_at[c] ": no crate " reviewed_as[c] " in the lock files; remove it")
    else if (c in named_line) problem(map ":" line_at[c] ": " reviewed_as[c] " is under [" named_kind[c] "] on line " named_line[c] "; give it its line there, not under [reviewed]")
    else if (!(c in has_path)) problem(map ":" line_at[c] ": " reviewed_as[c] " is under [reviewed] but has no functions listed; classify it under [none] or [allow]")
  }
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
echo "check-clippy-bans: every crate in the tree is classified at the line it's on, and every ban is in clippy.toml"
