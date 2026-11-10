#!/usr/bin/env python3
"""Check a random sample of the pharmacy pack's offered terms against their
sources' own records.

  python3 -I packs/pharmacy/sample_check.py SEED [N]

Draws N (default 100) of the terms the pack offers (term rows marked
`reviewed`), with Python's random.Random(SEED), and for each source that
gives the term:

- rxnorm: RxNav's Prescribe API must find exactly that name, and the
  concept's term type must be IN (an ingredient).
- wikidata (through the Wikidata API): an item with an RxNorm id (P3345)
  must have that English label,
  compared ignoring case only where the term took RxNorm's spelling (see
  refresh.py); its RxNorm id must be an ingredient in RxNav's Prescribe API.

Prints one line per term, then the error rate. Exits 1 if any term is wrong.
"""

import json
import random
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
AGENT = "nota-pack-refresh/1 (term pack build script)"
RXNAV = "https://rxnav.nlm.nih.gov/REST/Prescribe"
WDAPI = "https://www.wikidata.org/w/api.php"


def get(url, accept="application/json"):
    # RxNav asks for at most 20 requests a second; Wikidata for fewer.
    time.sleep(1.0 if url.startswith(WDAPI) else 0.1)
    req = urllib.request.Request(url, headers={"User-Agent": AGENT, "Accept": accept})
    for attempt in range(6):
        try:
            with urllib.request.urlopen(req, timeout=120) as resp:
                return json.load(resp)
        except urllib.error.HTTPError as e:
            if e.code != 429 or attempt == 5:
                raise
            time.sleep(int(e.headers.get("Retry-After") or 0) or 10 * 2**attempt)


def offered():
    """Each reviewed term and the sources that give it."""
    terms = {}
    for source in ("rxnorm", "wikidata", "spoken-forms"):
        for line in (HERE / f"{source}.tsv").read_text(encoding="utf-8").splitlines():
            fields = line.split("\t")
            if line.startswith("#") or len(fields) != 2 or fields[1] != "reviewed":
                continue
            terms.setdefault(fields[0], set()).add(source)
    return terms


def ingredient(rxcui):
    props = get(f"{RXNAV}/rxcui/{rxcui}/properties.json").get("properties") or {}
    return props.get("tty") == "IN", props.get("name")


def check_rxnorm(term):
    q = urllib.parse.urlencode({"name": term, "search": 0})
    ids = (get(f"{RXNAV}/rxcui.json?{q}").get("idGroup") or {}).get("rxnormId") or []
    for rxcui in ids:
        is_in, name = ingredient(rxcui)
        if is_in and name == term:
            return rxcui, None
    return None, f"no Prescribe IN concept named {term!r} (ids {ids})"


def entities(ids):
    """Each item's English label and RxNorm ids (P3345)."""
    if not ids:
        return {}
    q = urllib.parse.urlencode(
        {"action": "wbgetentities", "ids": "|".join(ids[:50]), "props": "labels|claims", "languages": "en", "format": "json"}
    )
    found = {}
    for qid, e in get(f"{WDAPI}?{q}").get("entities", {}).items():
        label = (e.get("labels", {}).get("en") or {}).get("value")
        rx = [c["mainsnak"].get("datavalue", {}).get("value") for c in e.get("claims", {}).get("P3345", [])]
        found[qid] = (label, [r for r in rx if r])
    return found


def check_wikidata(term, rxcui):
    if rxcui:
        search = f"haswbstatement:P3345={rxcui}"
        q = urllib.parse.urlencode({"action": "query", "list": "search", "srsearch": search, "format": "json"})
        ids = [h["title"] for h in get(f"{WDAPI}?{q}")["query"]["search"]]
        if any(label and label.lower() == term.lower() for label, _ in entities(ids).values()):
            return None
        return f"no item with RxNorm id {rxcui} labelled {term!r}"
    q = urllib.parse.urlencode({"action": "wbsearchentities", "search": term, "language": "en", "limit": 20, "format": "json"})
    ids = [h["id"] for h in get(f"{WDAPI}?{q}").get("search", [])]
    for label, rx in entities(ids).values():
        if label == term and any(ingredient(r)[0] for r in rx):
            return None
    return f"no item labelled {term!r} with an RxNorm ingredient id"


def main():
    seed = int(sys.argv[1])
    n = int(sys.argv[2]) if len(sys.argv) > 2 else 100
    terms = offered()
    sample = random.Random(seed).sample(sorted(terms), n)
    wrong = 0
    for term in sample:
        sources = terms[term]
        errors, rxcui = [], None
        if "rxnorm" in sources:
            rxcui, err = check_rxnorm(term)
            if err:
                errors.append(err)
        if "wikidata" in sources:
            err = check_wikidata(term, rxcui)
            if err:
                errors.append(err)
        if "spoken-forms" in sources:
            errors.append("a spoken form is offered; spoken forms are drafts")
        wrong += bool(errors)
        mark = "WRONG" if errors else "ok"
        print(f"{mark}\t{term}\t{'+'.join(sorted(sources))}\t{rxcui or '-'}\t{'; '.join(errors)}")
    print(f"seed {seed}: {wrong} of {n} wrong ({100 * wrong / n:.1f}%) out of {len(terms)} offered terms")
    return 1 if wrong else 0


if __name__ == "__main__":
    sys.exit(main())
