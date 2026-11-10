#!/usr/bin/env python3
"""Fetch the pharmacy pack's source lists again from their open sources.

Writes rxnorm.tsv and wikidata.tsv beside this script, in the list format
`nota pack build` reads (see crates/nota/src/pack.rs): a term and its review
status make a term row; a third field makes it an alias of that term.
spoken-forms.tsv is written by hand and isn't touched.

  python3 -I packs/pharmacy/refresh.py

Sources (open for redistribution; re-read each licence before shipping):
- RxNorm's prescribable ingredient names (RxNav's Prescribe API, term type
  IN). The normalized names are US public domain; NLM asks for the
  acknowledgement in pack.toml.
- Wikidata (CC0): the English label and lower-case English aliases of each
  item whose RxNorm id (P3345) is one of those ingredients.

The filters are general rules about what a spoken drug name looks like, not
tuned on any recording: letters, spaces, hyphens and apostrophes only, 3 to
40 characters, and no allergen extracts or pollens. Only lower-case aliases
are kept, which drops most brand names and codes, and only those that share
a run of five letters with the label (spelling variants such as
"metamfetamine"), which drops street names ("ice") and unrelated synonyms:
an alias is an exact match, so it must not name another thing. For the
same reason an alias that is itself a term of the pack, or that two labels
share, is dropped, and a Wikidata label that differs from an RxNorm name
only in case takes RxNorm's spelling.

RxNorm's names and Wikidata's labels count as reviewed by their source
(decided on #228); every alias is a draft until a person checks it. A row
already in a list keeps the status it has there, so a review made by
editing rows survives a refresh; REVIEW below gives the status of new rows. After a refresh, give
pack.toml a new version: a session tells packs apart by name and version.
"""

import json
import re
import sys
import urllib.parse
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
AGENT = "nota-pack-refresh/1 (term pack build script)"
RXNAV = "https://rxnav.nlm.nih.gov/REST/Prescribe/allconcepts.json?tty=IN"
WDQS = "https://query.wikidata.org/sparql"
QUERY = """
SELECT ?rxcui ?label (GROUP_CONCAT(DISTINCT ?alias; separator="|") AS ?aliases) WHERE {
  ?item wdt:P3345 ?rxcui .
  ?item rdfs:label ?label . FILTER(LANG(?label) = "en")
  OPTIONAL { ?item skos:altLabel ?alias . FILTER(LANG(?alias) = "en") }
} GROUP BY ?rxcui ?label
"""

# The review status of new rows: names from a source count as reviewed by
# it, aliases don't.
REVIEW = {"rxnorm": "reviewed", "wikidata": "reviewed", "alias": "draft"}

NAME = re.compile(r"^[A-Za-z][A-Za-z' -]{1,38}[A-Za-z]$")
ALIAS = re.compile(r"^[a-z][a-z' -]{1,38}[a-z]$")
# `ext?r?act` also catches the source's two misspellings of "extract".
NOT_DRUGS = re.compile(r"\b(ext?r?act|pollen|allergenic|dander|hair|feathers?|venom|mixture)\b", re.I)


def fetch(url, data=None, accept="application/json"):
    req = urllib.request.Request(url, data=data, headers={"User-Agent": AGENT, "Accept": accept})
    with urllib.request.urlopen(req, timeout=180) as resp:
        return json.load(resp)


def letters(text):
    return re.sub(r"[^a-z]", "", text.lower())


def related(alias, label):
    a, b = letters(alias), letters(label)
    return any(a[i : i + 5] in b for i in range(len(a) - 4))


def drug_name(name):
    return bool(NAME.match(name)) and not NOT_DRUGS.search(name) and "  " not in name


def statuses(name):
    """The status of each row already in the list `name`, by its term and
    alias (None for a term row)."""
    path = HERE / name
    if not path.exists():
        return {}
    found = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        fields = line.split("\t")
        alias = fields[2] if len(fields) > 2 else None
        found[(fields[0], alias)] = fields[1]
    return found


def hand_terms():
    """The terms of the hand-written spoken-forms list."""
    return {key[0] for key in statuses("spoken-forms.tsv")}


def main():
    rx = fetch(RXNAV)["minConceptGroup"]["minConcept"]
    ingredients = {c["rxcui"]: c["name"] for c in rx if drug_name(c["name"])}
    rx_kept = statuses("rxnorm.tsv")
    rx_names = sorted(set(ingredients.values()))
    rx_rows = [(name, rx_kept.get((name, None), REVIEW["rxnorm"])) for name in rx_names]
    rx_by_case = {name.lower(): name for name in rx_names}

    body = urllib.parse.urlencode({"query": QUERY}).encode()
    wd = fetch(WDQS, body, "application/sparql-results+json")["results"]["bindings"]
    wd_kept = statuses("wikidata.tsv")
    labels, alias_of = set(), {}
    for b in wd:
        if b["rxcui"]["value"] not in ingredients:
            continue
        label = b["label"]["value"]
        if not drug_name(label):
            continue
        label = rx_by_case.get(label.lower(), label)
        labels.add(label)
        for alias in b.get("aliases", {}).get("value", "").split("|"):
            if (
                alias.lower() != label.lower()
                and ALIAS.match(alias)
                and not NOT_DRUGS.search(alias)
                and related(alias, label)
            ):
                alias_of.setdefault(alias, set()).add(label)
    terms = {t.lower() for t in labels | set(rx_names) | hand_terms()}
    wd_terms = sorted((label, wd_kept.get((label, None), REVIEW["wikidata"])) for label in labels)
    wd_aliases = sorted(
        (label, wd_kept.get((label, alias), REVIEW["alias"]), alias)
        for alias, of in alias_of.items()
        if len(of) == 1 and alias.lower() not in terms
        for label in of
    )

    write("rxnorm.tsv", "RxNorm prescribable ingredient names, " + RXNAV, rx_rows)
    write(
        "wikidata.tsv",
        "Wikidata labels and lower-case aliases of items with an RxNorm id above, " + WDQS,
        wd_terms + wd_aliases,
    )
    print(f"rxnorm: {len(rx_rows)} terms; wikidata: {len(wd_terms)} terms, {len(wd_aliases)} aliases")
    print("Now give pack.toml a new version.")


def write(name, what, rows):
    lines = [f"# {what}", "# Written by refresh.py; term<TAB>status[<TAB>alias]"]
    lines += ["\t".join(row) for row in rows]
    (HERE / name).write_text("\n".join(lines) + "\n", encoding="utf-8")


if __name__ == "__main__":
    sys.exit(main())
