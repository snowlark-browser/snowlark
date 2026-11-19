# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Compares two frame-trace recordings (`reference.py`, `snowlark.py`) realm by realm.

    python tools/frame-trace/diff.py <reference.jsonl> <snowlark.jsonl> [--types console,exception,...]

Realms are matched by a label: the top page, each other document by its URL without query (`about:blank`
and `about:srcdoc` as they are), each worker as `worker <script origin and path>` (a `blob:` worker as
`worker blob:<origin>`). Messages are compared after numbers, hex runs and long tokens are folded, so a
run's ids do not count as a difference. For each realm: the messages only one side has, then the first
position where the two sequences part.
"""

import argparse
import collections
import json
import re
import urllib.parse

TOKEN = re.compile(r"[A-Za-z0-9_\-]{24,}|[0-9a-f]{8,}|\d+(?:\.\d+)?")


def label(rec):
    realm = rec["realm"]
    if realm.startswith("chrome-extension:"):
        return None
    if realm.startswith("blob:"):
        u = urllib.parse.urlsplit(realm[5:])
        return f"worker blob:{u.scheme}://{u.netloc}"
    if not realm:
        return "(no realm)"
    u = urllib.parse.urlsplit(realm)
    if u.scheme in ("about", "data"):
        return realm.split("?")[0]
    path = TOKEN.sub("*", u.path)
    prefix = "worker " if rec["kind"] in ("worker", "shared_worker", "service_worker") else ""
    return f"{prefix}{u.scheme}://{u.netloc}{path}"


def norm(rec):
    text = rec["text"]
    # The reference prints a console.* call's arguments; Snowlark formats them (Console Standard). Both
    # are reduced to their words.
    text = re.sub(r"%c\S*\s+font-size:0;color:transparent\s*", "", text)
    return f"{rec['type']}.{rec['level']} {TOKEN.sub('*', text)[:160]}"


def load(path, types):
    by_realm = collections.defaultdict(list)
    for line in open(path, encoding="utf-8"):
        rec = json.loads(line)
        if rec["type"] not in types:
            continue
        lab = label(rec)
        if lab:
            by_realm[lab].append(norm(rec))
    return by_realm


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("reference")
    ap.add_argument("snowlark")
    ap.add_argument("--types", default="console,exception,log,issue,failed")
    a = ap.parse_args()
    types = set(a.types.split(","))
    ref, osm = load(a.reference, types), load(a.snowlark, types)
    for realm in sorted(set(ref) | set(osm)):
        r, o = ref.get(realm, []), osm.get(realm, [])
        print(f"== {realm}: reference {len(r)}, snowlark {len(o)}")
        rc, oc = collections.Counter(r), collections.Counter(o)
        for msg, n in (rc - oc).items():
            print(f"  only reference x{n}: {msg}")
        for msg, n in (oc - rc).items():
            print(f"  only snowlark    x{n}: {msg}")
        for i, (x, y) in enumerate(zip(r, o)):
            if x != y:
                print(f"  first divergence at #{i}:\n    reference: {x}\n    snowlark:    {y}")
                break


if __name__ == "__main__":
    main()
