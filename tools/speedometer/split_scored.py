# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Where the score goes: the page thread's samples inside Speedometer's scored steps, by suite and layer.
#   python tools/speedometer/split_scored.py <stacks.folded> [results.json] [iterations]
# From a run with SNOWLARK_PROFILE=1 and SNOWLARK_PROFILE_FOLDED: each stack's outermost frame is the last
# performance.mark the page passed. A step is scored from `<Suite>.<Step>-start` to `-sync-end` (sync) and
# from `-async-start` to `-async-end` (async, with the forced layout); everything else is not scored. (That
# is browserbench.org's 3.1. The local clone's newer runner has no `-async-start`: its async time starts at
# `-sync-end`, and this would count none of it.)
# A profiled run is slower than a plain one and has no Fast API: read shares, not times.
import collections, json, math, os, re, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from split_layers import classify

MS = 0.616  # (the run prints its own: "one per N us")
LAYERS = ['wait', 'style', 'layout', 'render/paint', 'DOM', 'engine (page crate)', 'bindings', 'JS (JIT and builtins)',
          'V8 runtime/compile', 'V8 GC', 'engine GC', 'html parser', 'other', 'profiler itself']
SHORT = {'wait': 'wait', 'style': 'style', 'layout': 'layout', 'render/paint': 'paint', 'DOM': 'DOM', 'engine (page crate)': 'page',
         'bindings': 'bind', 'JS (JIT and builtins)': 'JS', 'V8 runtime/compile': 'V8rt', 'V8 GC': 'V8gc', 'engine GC': 'engGC',
         'html parser': 'html', 'other': 'other', 'profiler itself': 'prof'}


def phase(frame):
    """(suite, step, 'sync' | 'async') of a mark, or None outside the scored steps."""
    m = re.fullmatch(r'mark:(.+?)\.(.+?)-(async-start|start)', frame)
    if not m or m.group(1).startswith('suite-'):
        return None
    return m.group(1), m.group(2), 'sync' if m.group(3) == 'start' else 'async'


scored = collections.defaultdict(collections.Counter)  # suite -> layer
steps = collections.defaultdict(collections.Counter)  # (suite, step, kind) -> layer
kinds = collections.defaultdict(collections.Counter)  # sync/async -> layer
outside = collections.Counter()
for line in open(sys.argv[1], encoding='utf-8', errors='replace'):
    i = line.rfind(' ')
    stack, n = line[:i], int(line[i + 1:])
    layer = classify(stack)
    p = phase(stack.split(';')[0])
    if p is None:
        outside[layer] += n
    else:
        scored[p[0]][layer] += n
        steps[p][layer] += n
        kinds[p[2]][layer] += n


def row(name, c, width=30):
    total = sum(c.values())
    cells = ''.join(f'{c[k] / max(total, 1) * 100:6.0f}' for k in LAYERS[:-1])
    return f'{name[:width]:{width}}{total * MS:8.0f}{cells}'


head = f'{"":30}{"ms":>8}' + ''.join(f'{SHORT[k]:>6}' for k in LAYERS[:-1])
everything = collections.Counter()
for c in scored.values():
    everything.update(c)
work = {k: v for k, v in everything.items() if k not in ('wait', 'profiler itself')}
print('scored %.0f ms, not scored %.0f ms' % (sum(everything.values()) * MS, sum(outside.values()) * MS))
print('not scored, work only: %.0f ms' % (sum(v for k, v in outside.items() if k not in ('wait', 'profiler itself')) * MS))
print()
print('scored work by layer, waits and the profiler left out:')
for k, v in sorted(work.items(), key=lambda x: -x[1]):
    print(f'  {k:24}{v * MS:8.0f} ms {v / sum(work.values()) * 100:5.1f}%')
print()
# (the score is a geometric mean of the suites: a suite counts the same however long it takes, so what a
# layer is worth to the score is its share of each suite, averaged, not its share of the milliseconds)
print('what each layer is worth to the score, every suite counting the same:')
weight = collections.Counter()
for c in scored.values():
    t = sum(c.values())
    for k, v in c.items():
        weight[k] += v / t / len(scored)


def gain(layer, cut):
    """The score's factor with `cut` of the layer's time gone from every suite."""
    return math.exp(-sum(math.log(1 - cut * c[layer] / sum(c.values())) for c in scored.values()) / len(scored))


print(f'  {"":24}{"mean":>6}{"halved":>8}{"gone":>7}')
for k, v in weight.most_common():
    print(f'  {k:24}{v * 100:5.1f}%{gain(k, .5):7.3f}x{gain(k, 1):6.3f}x')
print()
print('% of each row\'s time:')
print(head)
print(row('all scored', everything))
for k in ('sync', 'async'):
    print(row(k, kinds[k]))
print()
for s, c in sorted(scored.items(), key=lambda x: -sum(x[1].values())):
    print(row(s, c))
print()
print('steps that take longest:')
for p, c in sorted(steps.items(), key=lambda x: -sum(x[1].values()))[:14]:
    print(row(f'{p[0]}/{p[1]}/{p[2]}'[-44:], c, 44))
if len(sys.argv) > 2:
    # (the check on the marks: the runner's own times against the samples')
    r = json.load(open(sys.argv[2], encoding='utf-8'))
    print()
    print('check, ms over the run: suite, samples, runner')
    a = b = 0
    for s, c in sorted(scored.items(), key=lambda x: -sum(x[1].values())):
        x, y = sum(c.values()) * MS, sum(r[s]['values'])
        a, b = a + x, b + y
        print(f'  {s:44}{x:8.0f}{y:8.0f}  {x / y * 100:4.0f}%')
    print(f'  {"all":44}{a:8.0f}{b:8.0f}  {a / b * 100:4.0f}%')
