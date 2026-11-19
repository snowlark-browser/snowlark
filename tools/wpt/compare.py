# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Compares two WPT runs of run.py by subtest: python tools/wpt/compare.py <name-before> <name-after>
import json, sys, os, glob

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import gate  # noqa: E402

base = gate.STATE


def load(name):
    hits = glob.glob(os.path.join(base, 'results', name + '.jsonl'))
    if not hits:
        sys.exit('no results for ' + name)
    out, files = {}, {}
    for line in open(hits[0], encoding='utf-8'):
        r = json.loads(line)
        res = r.get('result') or {}
        files[r['url']] = res.get('status')
        for t in res.get('tests') or []:
            out[(r['url'], t['name'])] = t['status']
    return out, files


a, fa = load(sys.argv[1])
b, fb = load(sys.argv[2])
pa = sum(1 for v in a.values() if v == 0)
pb = sum(1 for v in b.values() if v == 0)
print(f'{sys.argv[1]}: {pa}/{len(a)} in {len(fa)} files')
print(f'{sys.argv[2]}: {pb}/{len(b)} in {len(fb)} files')
lost = [k for k, v in a.items() if v == 0 and b.get(k) != 0]
won = [k for k, v in b.items() if v == 0 and a.get(k) != 0]
print('lost', len(lost), 'won', len(won))
for k in lost[:25]:
    print('  -', k[0], '|', k[1][:70], '->', b.get(k))
for k in won[:12]:
    print('  +', k[0], '|', k[1][:70])
ff = [u for u in fa if fa[u] != fb.get(u)]
print('file status changed', len(ff))
for u in ff[:12]:
    print('  ~', u, fa[u], '->', fb.get(u))
