# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Engine GC by part, in scored time, and what each part is worth to the score if it went from every suite:
#   python tools/speedometer/split_gc.py <stacks.folded>   (SNOWLARK_PROFILE_FOLDED of a Speedometer run with marks)
import os, sys, re, math, collections
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from split_layers import classify
SC = re.compile(r'mark:([A-Za-z0-9-]+)[.][^;]+?-(async-)?start')
PARTS = {
 'all engine GC': lambda s, l: l == 'engine GC',
 'snapshot_traced': lambda s, l: l == 'engine GC' and 'snapshot_traced' in s,
 '  discarded_realms': lambda s, l: l == 'engine GC' and 'discarded_realms' in s,
 'rt::gc::pass (strong/weak)': lambda s, l: l == 'engine GC' and 'rt::gc::pass' in s,
 'owned_nodes': lambda s, l: l == 'engine GC' and 'owned_nodes' in s,
 'Dom::release': lambda s, l: l == 'engine GC' and 'Dom::release' in s,
 'detached_roots_of': lambda s, l: l == 'engine GC' and 'detached_roots_of' in s,
 'from V8 interrupt': lambda s, l: l == 'engine GC' and 'interrupt_callback' in s,
 'from Page::turn': lambda s, l: l == 'engine GC' and 'Page::turn' in s,
 'V8 GC': lambda s, l: l == 'V8 GC',
}
tot = collections.Counter(); part = collections.defaultdict(collections.Counter)
for line in open(sys.argv[1], encoding='utf-8', errors='replace'):
    stack, _, n = line.rstrip('\n').rpartition(' '); n = int(n)
    m = SC.search(stack)
    if not m: continue
    l = classify(stack)
    if l in ('wait', 'profiler itself'): continue
    su = m.group(1); tot[su] += n
    for k, f in PARTS.items():
        if f(stack, l): part[k][su] += n
S = sum(tot.values()); print('scored work samples', S, 'suites', len(tot))
print(f"{'part':30} {'samples':>7} {'share':>6} {'factor':>7}")
for k in PARTS:
    x = part[k]; fac = math.exp(sum(math.log(tot[s] / (tot[s] - x[s])) for s in tot) / len(tot))
    print(f'{k:30} {sum(x.values()):7} {100*sum(x.values())/S:5.1f}% x{fac:.3f}')
g = part['all engine GC']
print('engine GC share by suite:', ', '.join(f'{s[:14]} {100*g[s]/tot[s]:.0f}%' for s in sorted(tot, key=lambda s: -g[s]/tot[s])[:10]))
