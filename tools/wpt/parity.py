# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""The gap between Snowlark and Chrome stable on web-platform-tests, per directory.

    python tools/wpt/parity.py                       # newest stored baseline vs. the Chrome run nearest the checkout
    python tools/wpt/parity.py --ours main-5a9561cc  # a stored baseline (or results name, or a .jsonl path)
    python tools/wpt/parity.py --out docs/internal/parity.md

Chrome's side is wpt.fyi's public API: `/api/runs?product=chrome&label=stable&label=master` lists runs; the run
whose WPT revision is nearest the checkout's commit date is used (`--run <id>` or `--chrome-json <file>` pick
another), and its `results_url` summary gives, per test file, the harness status and [passed, total] subtests. The
summary is cached under `<state>/parity-cache/`. Our side is a full run of `run.py` (one line per test file).

Counting: a file with subtests counts its subtests; a file without (a reftest, a single-page test) counts as one
test, passing when its status is PASS or OK. The gap of a file is max(0, Chrome's passing - ours passing), summed per
directory: the subtests Chrome passes and we do not. Chrome's files are put in one of five groups:

  ran      the file is in our results.
  variant  its directory is one our runner covers and the file is in the checkout, but our run has no result for
           it (`.https.`, workers, `.h2.`, service workers: variants `run.py` does not start without options).
  unrun    the file is in the checkout, but its directory is not one of `run.py`'s AREAS.
  absent   its directory is not in the sparse checkout (Chrome's whole count is the gap).
  newer    a directory we cover, a file that is not in the checkout: the checkout is older than Chrome's run, so it
           is left out of the gap.

Crashtests, manual tests and `webdriver/` are not measured. The "mech" column is a path heuristic (files under
`parsing/`, `-computed`, `-valid`, `-invalid`, `idlharness` and `interfaces`: parsing, computed values and IDL
members) and names where routine work is likely, not what the failures are.
"""
import argparse
import collections
import gzip
import json
import os
import re
import subprocess
import sys
import urllib.request
from datetime import datetime, timedelta, timezone

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))  # tools/
import devenv  # noqa: E402

WPT = str(devenv.wpt_dir())
STATE = devenv.setting('SNOWLARK_WPT_STATE', str(devenv.dev_dir() / 'wpt'))
API = 'https://wpt.fyi/api/runs'

NOT_MEASURED = re.compile(r'(^|/)(crashtests|manual|webdriver|tools|resources|support)/|-manual[.-]|^/encoding/legacy-mb-')
MECH = re.compile(r'/parsing/|-computed|-valid\.|-invalid\.|idlharness')
VARIANT_KINDS = [('https', re.compile(r'\.https\.')), ('worker', re.compile(r'\.(worker|serviceworker|sharedworker)(-module)?\.')),
                 ('h2', re.compile(r'\.h2\.'))]
VIRTUAL = re.compile(r'^(.*?\.(?:any|window|worker))(?:\.[A-Za-z0-9-]+)*\.html$')


def areas():
    """`run.py`'s AREAS: the directories a full run covers."""
    sys.path.insert(0, HERE)
    import run
    return list(run.AREAS)


def checkout_commit():
    try:
        out = subprocess.run(['git', '-C', WPT, 'log', '-1', '--format=%H %cI'], capture_output=True, text=True,
                             check=True).stdout.split()
        return out[0], datetime.fromisoformat(out[1])
    except (OSError, subprocess.CalledProcessError, ValueError, IndexError):
        sys.exit('cannot read the commit of the WPT checkout ' + WPT)


def fetch(url):
    request = urllib.request.Request(url, headers={'User-Agent': 'snowlark-wpt-parity'})
    data = urllib.request.urlopen(request, timeout=300).read()
    return gzip.decompress(data) if data[:2] == b'\x1f\x8b' else data


def pick_run(when, run_id=None):
    """The wpt.fyi run of Chrome stable nearest `when`."""
    if run_id:
        runs = json.loads(fetch('https://wpt.fyi/api/runs/%s' % run_id))
        return runs if isinstance(runs, dict) else runs[0]
    for days in (3, 10, 40):
        query = '?product=chrome&label=stable&label=master&max-count=100&from=%s&to=%s' % (
            (when - timedelta(days=days)).strftime('%Y-%m-%dT%H:%M:%SZ'),
            (when + timedelta(days=days)).strftime('%Y-%m-%dT%H:%M:%SZ'))
        runs = [r for r in json.loads(fetch(API + query)) if r.get('os_name', 'linux') == 'linux']
        if runs:
            return min(runs, key=lambda r: abs(datetime.fromisoformat(r['time_start'].replace('Z', '+00:00')) - when))
    sys.exit('wpt.fyi lists no Chrome stable run near ' + when.isoformat())


def chrome_results(args, when):
    """({path: (passed, total)}, description of the run)."""
    if args.chrome_json:
        raw, desc = json.loads(open(args.chrome_json, encoding='utf-8').read()), args.chrome_json
    else:
        run = pick_run(when, args.run)
        desc = 'Chrome %s, WPT revision %s, run %s of %s' % (run['browser_version'], run['revision'], run['id'],
                                                           run['time_start'][:10])
        cache = os.path.join(STATE, 'parity-cache')
        os.makedirs(cache, exist_ok=True)
        path = os.path.join(cache, '%s.json' % run['id'])
        if not os.path.exists(path):
            with open(path, 'wb') as f:
                f.write(fetch(run['results_url']))
        raw = json.load(open(path, encoding='utf-8'))
    out = {}
    for path, v in raw.items():
        passed, total = v['c']
        if total == 0:
            total, passed = 1, int(v['s'] in ('P', 'O'))
        out[path] = (passed, total)
    return out, desc


def our_results(name):
    """({path: (passed, total)}, path of the file)."""
    candidates = []
    if name:
        candidates = [name, os.path.join(STATE, 'baselines', name + '.jsonl'), os.path.join(STATE, 'results', name + '.jsonl')]
    else:
        base = os.path.join(STATE, 'baselines')
        found = [f for f in os.listdir(base) if f.startswith('main-') and f.endswith('.jsonl')]
        if not found:
            sys.exit('no stored baseline in ' + base)
        candidates = [os.path.join(base, max(found, key=lambda f: os.path.getmtime(os.path.join(base, f))))]
    path = next((c for c in candidates if os.path.isfile(c)), None)
    if not path:
        sys.exit('no results for ' + str(name))
    out = {}
    for line in open(path, encoding='utf-8'):
        d = json.loads(line)
        res = d.get('result') or {}
        tests = res.get('tests') or []
        key = re.sub(r'^https?://[^/]+', '', d['url'])
        if d.get('kind') == 'reftest' or not tests:
            out[key] = (int(res.get('status') == 0), 1)
        else:
            out[key] = (sum(1 for t in tests if t['status'] == 0), len(tests))
    return out, path


def on_disk(path):
    """Whether the checkout has the source of a test URL path (`foo.any.html` comes from `foo.any.js`)."""
    path = path.split('?')[0].split('#')[0]
    if os.path.exists(WPT + path):
        return True
    m = VIRTUAL.match(path)
    return bool(m and os.path.exists(WPT + m.group(1) + '.js'))


def classify(path, ours, covered):
    if path in ours:
        return 'ran'
    if any(path.lstrip('/').startswith(a + '/') for a in covered):
        return 'variant' if on_disk(path) else 'newer'
    folder = os.path.dirname(path.split('?')[0])
    if not os.path.isdir(WPT + folder):
        return 'absent'
    return 'unrun' if on_disk(path) else 'absent'


def key_of(path, depth):
    parts = path.split('?')[0].lstrip('/').split('/')[:-1]
    return '/'.join(parts[:depth]) if parts else '(root)'


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('--ours', help='a stored baseline or results name, or a .jsonl path (default: the newest baseline)')
    ap.add_argument('--run', help='a wpt.fyi run id instead of the one nearest the checkout')
    ap.add_argument('--chrome-json', help='a summary_v2 file (JSON) instead of fetching one')
    ap.add_argument('--out', help='write the markdown table here (default: print)')
    ap.add_argument('--top2', type=int, default=120, help='rows of the two-level table')
    ap.add_argument('--top3', type=int, default=150, help='rows of the three-level table')
    args = ap.parse_args()

    sha, when = checkout_commit()
    when = when.astimezone(timezone.utc)
    chrome, chrome_desc = chrome_results(args, when)
    ours, ours_path = our_results(args.ours)
    covered = areas()

    rows = {depth: collections.defaultdict(lambda: collections.Counter()) for depth in (1, 2, 3)}
    skipped = collections.Counter()
    kinds = collections.Counter()
    for path, (cp, ct) in chrome.items():
        if NOT_MEASURED.search(path):
            skipped['chrome files not measured (crashtests, manual, webdriver, the thousands of `encoding/legacy-mb-` variants)'] += 1
            continue
        group = classify(path, ours, covered)
        op = ours[path][0] if group == 'ran' else 0
        if group == 'newer':
            skipped['chrome files not in the checkout revision, in directories we cover'] += 1
            continue
        gap = max(0, cp - op)
        mech = gap if MECH.search(path) else 0
        if group == 'variant':
            kind = next((k for k, rx in VARIANT_KINDS if rx.search(path)), 'other')
            kinds[kind] += gap
        for depth in rows:
            c = rows[depth][key_of(path, depth)]
            c['chrome'] += cp
            c['ours'] += min(op, cp) if group == 'ran' else 0
            c['gap'] += gap
            c['gap_' + group] += gap
            c['mech'] += mech
    ours_total = sum(p for path, (p, t) in ours.items() if not NOT_MEASURED.search(path))
    total = collections.Counter()
    for c in rows[1].values():
        total.update(c)

    def table(depth, limit, by='gap'):
        items = sorted(rows[depth].items(), key=lambda kv: -kv[1][by])
        if limit:
            items = items[:limit]
        lines = ['| directory | Chrome passes | we pass | gap | gap: in files we ran | not run (variant / area) | not in checkout | mech |',
                 '|---|--:|--:|--:|--:|--:|--:|:-:|']
        for name, c in items:
            if c[by] == 0:
                continue
            flag = 'mech' if c['mech'] * 2 >= c['gap'] else ('part' if c['mech'] * 5 >= c['gap'] else '')
            lines.append('| %s | %d | %d | %d | %d | %d | %d | %s |' % (
                name, c['chrome'], c['ours'], c['gap'], c['gap_ran'], c['gap_variant'] + c['gap_unrun'],
                c['gap_absent'], flag))
        return '\n'.join(lines)

    text = [
        '# WPT parity: the gap to Chrome stable, per directory',
        '',
        'Generated %s by `python tools/wpt/parity.py`.' % datetime.now().strftime('%Y-%m-%d'),
        '',
        '- Ours: `%s` (%d subtests pass, reftests counted one each).' % (os.path.basename(ours_path), ours_total),
        '- Chrome: %s. Checkout: WPT %s, committed %s.' % (chrome_desc, sha[:10], when.strftime('%Y-%m-%d')),
        '- **Chrome passes %d, we match on %d; the gap (subtests Chrome passes and we fail) is %d**: %d in files we ran, '
        '%d in files in the checkout that our run does not start (variants, areas outside `run.py`\'s list), %d in '
        'directories that are not in the sparse checkout.' % (
            total['chrome'], total['ours'], total['gap'], total['gap_ran'], total['gap_variant'] + total['gap_unrun'],
            total['gap_absent']),
        '- Not counted: ' + '; '.join('%d %s' % (n, what) for what, n in skipped.items()) + '.',
        '- Variants our run does not start, by kind (gap): ' + ', '.join('%s %d' % kv for kv in kinds.most_common()) + '.',
        '- "mech" = at least half of the gap is in files under `parsing/` or named `-computed`, `-valid`, `-invalid`, '
        '`idlharness` (part: a fifth). A path heuristic: the failures decide, see the notes.',
        '- The gap of a file is max(0, Chrome passing - ours passing), so it can be a little low where we pass other '
        'subtests than Chrome does.',
        '',
        '## By top-level directory',
        '', table(1, 0), '',
        '## Two levels (top %d by gap)' % args.top2,
        '', table(2, args.top2), '',
        '## Three levels (top %d by gap)' % args.top3,
        '', table(3, args.top3), '',
        '## Three levels, by the gap in files we already run (top %d)' % args.top3,
        '', 'Work that needs no runner change.', '', table(3, args.top3, 'gap_ran'), '',
    ]
    out = '\n'.join(text)
    if args.out:
        with open(args.out, 'w', encoding='utf-8', newline='\n') as f:
            f.write(out)
        print('wrote', args.out)
    else:
        print(out)


if __name__ == '__main__':
    main()
