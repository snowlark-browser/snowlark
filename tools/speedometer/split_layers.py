# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# The page thread by layer, from SNOWLARK_PROFILE_FOLDED stacks of a run:
#   python tools/speedometer/split_layers.py <stacks.folded> ["layer" ...]   (a layer named: its top leaf frames)
# The sample period is the 0.608 ms hard-coded below; the run prints its own ("one per N us").
import sys, re, collections
# First the states a whole stack is in, then the innermost frame that names a layer.
WHOLE = [
    ('wait', r'Waker::wait_until'),
    ('profiler itself', r'rt::prof::|print_|\[render\]|eprint|std::io::stdio|render_stats|Profiler::'),
    ('engine GC', r'Engine::collect_garbage|engine::gc::|rt::gc::pass'),
    ('V8 GC', r'v8::internal::Heap::|Scavenger|MarkCompact|Sweeper|v8::internal::GlobalHandles::Post'),
]
LEAF = [
    ('JS (JIT and builtins)', r'^Builtins_|^0x|^\?|^\[|unknown'),
    ('V8 runtime/compile', r'^v8::internal::|^v8__|^v8::'),
    ('layout', r'^snowlark_layout::|taffy'),
    ('style', r'^snowlark_style::|^style::|^selectors::|^stylo|^cssparser'),
    ('render/paint', r'^snowlark_render::|^snowlark_paint::'),
    ('html parser', r'^html5ever|^markup5ever|^tendril|^string_cache'),
    ('DOM', r'^snowlark_dom::'),
    ('bindings', r'^snowlark_bindings::'),
    ('engine (page crate)', r'^snowlark_page::'),
]
WHOLE = [(n, re.compile(p)) for n, p in WHOLE]
LEAF = [(n, re.compile(p)) for n, p in LEAF]
# The native profiler names an inlined function "[inl]<name>@<file>:<line>" (since 94fe8fef). Its layer is
# its name's, not JS's "^\[".
INLINED = re.compile(r'^\[inl\]')


def classify(stack):
    """The layer of a folded stack."""
    for name, rx in WHOLE:
        if rx.search(stack):
            return name
    for f in reversed(stack.split(';')):
        f = INLINED.sub('', f)
        hit = next((name for name, rx in LEAF if rx.search(f)), None)
        if hit:
            return hit
    return 'other'


def main():
    tot = collections.Counter()
    leafs = collections.defaultdict(collections.Counter)
    for line in open(sys.argv[1], encoding='utf-8', errors='replace'):
        i = line.rfind(' ')
        stack, n = line[:i], int(line[i + 1:])
        hit = classify(stack)
        tot[hit] += n
        leafs[hit][stack.split(';')[-1][:62]] += n
    s = sum(tot.values())
    idle = tot['wait'] + tot['profiler itself']
    work = s - idle
    print('samples', s, 'work', work, 'x 0.608 ms = %.1f s' % (work * 0.000608))
    for k, v in tot.most_common():
        print(f'{k:24} {v:7} {v * 0.000608:6.2f} s   ' + ('' if k in ('wait', 'profiler itself') else f'{v / work * 100:5.1f}% of work'))
    for k in sys.argv[2:]:
        print('---', k)
        for f, v in leafs[k].most_common(10):
            print(f'  {v:6} {f}')


if __name__ == '__main__':
    main()
