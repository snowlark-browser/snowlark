# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Compares alternating Speedometer runs: scores, suites, growth, leak lines. Folders base-1, new-1, base-2, new-2, ...
# (every base-N and new-N there) under the folder given as the argument, each with the report.txt of an
# `snowlark-cli --speedometer --iterations 5 --out <folder>` run.
import os, re, statistics as st, sys
d = sys.argv[1]
pairs = sorted({int(m.group(1)) for f in os.listdir(d) if (m := re.fullmatch(r'(?:base|new)-(\d+)', f))
                and all(os.path.exists(os.path.join(d, f'{k}-{m.group(1)}', 'report.txt')) for k in ('base', 'new'))})
runs = [f'{k}-{i}' for i in pairs for k in ('base', 'new')]
scores, suites, tails = {}, {}, {}
for r in runs:
    txt = open(f'{d}\\{r}\\report.txt', encoding='utf-8', errors='replace').read()
    lines = txt.split('\n')
    s = [float(x) for x in re.findall(r'[\d.]+', next(l for l in lines if l.startswith('score by iteration')).split(':')[1])]
    scores[r] = s
    i = next(n for n, l in enumerate(lines) if l.startswith('suite totals'))
    su = {}
    for l in lines[i + 1:]:
        m = re.match(r'(\S+)\s+((?:[\d.]+\s+)+)x', l)
        if not m:
            break
        su[m.group(1)] = [float(x) for x in m.group(2).split()]
    suites[r] = su
    tails[r] = [l for l in lines if re.search(r'removed frame|frames? (alive|stayed)|heap', l, re.I)][-3:]
for r in runs:
    print(r, scores[r], 'mean %.2f' % st.mean(scores[r]))
b = [x for i in pairs for x in scores[f'base-{i}']]
n = [x for i in pairs for x in scores[f'new-{i}']]
print('base mean %.2f  new mean %.2f  change %+.1f%%' % (st.mean(b), st.mean(n), (st.mean(n) / st.mean(b) - 1) * 100))
print('by pair: ' + '  '.join('%+.1f%%' % ((st.mean(scores[f'new-{i}']) / st.mean(scores[f'base-{i}']) - 1) * 100) for i in pairs))
print()
rows = []
for k in suites['base-1']:
    x = st.mean([v for i in pairs for v in suites[f'base-{i}'][k]])
    y = st.mean([v for i in pairs for v in suites[f'new-{i}'][k]])
    rows.append((y / x - 1, k, x, y, suites['new-1'][k][-1] / suites['new-1'][k][0], suites['base-1'][k][-1] / suites['base-1'][k][0]))
for c, k, x, y, gn, gb in sorted(rows):
    print(f'{k:46} {x:8.1f} {y:8.1f} {c * 100:+6.1f}%  growth {gb:.2f} -> {gn:.2f}')
print('sum', round(sum(r[2] for r in rows), 1), round(sum(r[3] for r in rows), 1))
print()
for r in runs:
    print(r, [t[:110] for t in tails[r]])
