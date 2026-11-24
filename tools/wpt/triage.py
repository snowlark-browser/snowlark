# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Groups the failing subtests of a WPT run by what failed.

    python tools/wpt/triage.py main-5a9561cc                      # the biggest groups over the whole run
    python tools/wpt/triage.py main-5a9561cc css/css-values       # one directory
    python tools/wpt/triage.py main-5a9561cc --kind member        # only the groups of one kind
    python tools/wpt/triage.py main-5a9561cc --files css/css-values --top 20   # the files with most failures

The run is a stored baseline or results name (`<state>/baselines/<name>.jsonl`, `<state>/results/<name>.jsonl`) or a
path to a `.jsonl` file of `run.py`. A failing subtest's message is normalized (numbers, quoted text, URLs and hex
values become placeholders) and given a kind:

  member     a missing or non-callable member ("x is not a function", "undefined is not an object", "no property").
  exception  a thrown error of the wrong type (`assert_throws_*`, "Expected DOMException/TypeError").
  parsing    a CSS value that was not accepted, or was serialized differently (`parsing/`, `-computed`, `-valid`).
  timeout    a timeout, or a subtest that never ran.
  harness    the harness itself failed (a file with no subtests).
  other      everything else.

Each group prints its failing subtests, its files, and the directory where it is biggest, with one example.
"""
import argparse
import collections
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))  # tools/
import devenv  # noqa: E402

STATE = devenv.setting('SNOWLARK_WPT_STATE', str(devenv.dev_dir() / 'wpt'))
FAIL, TIMEOUT, NOTRUN, PRECONDITION = 1, 2, 3, 4

MEMBER = re.compile(r'is not a function|is not a constructor|is not defined|is undefined|undefined is not|'
                    r'Cannot read propert|not implemented|NOT_YET|has no method|\.\w+ is not iterable|'
                    r'assert_(?:true|equals|own_property|inherits|idl_attribute)[^:]*: .*(?:property|attribute|method|member|interface)', re.I)
THROWS = re.compile(r'assert_throws|expected (?:a )?(?:DOMException|TypeError|RangeError|SyntaxError)|'
                    r'did not throw|should (?:throw|reject)|Promise did not reject|DOMException|TypeError|NotSupportedError', re.I)


def normalize(message):
    """A message with what varies between subtests replaced by placeholders."""
    m = (message or '').strip().replace('\n', ' ')
    m = re.sub(r'"(?:[^"\\]|\\.)*"|\'(?:[^\'\\]|\\.)*\'', '"S"', m)
    m = re.sub(r'https?://\S+', 'URL', m)
    m = re.sub(r'0x[0-9a-fA-F]+|\b[0-9a-fA-F]{8,}\b', 'HEX', m)
    m = re.sub(r'-?\d+(?:\.\d+)?(?:e[+-]?\d+)?', 'N', m)
    m = re.sub(r'\s+', ' ', m)
    return m[:140]


def kind_of(path, name, status, message):
    text = '%s %s' % (name or '', message or '')
    if status in (TIMEOUT, NOTRUN):
        return 'timeout'
    if MEMBER.search(message or ''):
        return 'member'
    if '/parsing/' in path or '-computed' in path or re.search(r'-(?:in)?valid\.', path):
        return 'parsing'
    if THROWS.search(text):
        return 'exception'
    return 'other'


def load(name):
    candidates = [name, os.path.join(STATE, 'baselines', name + '.jsonl'), os.path.join(STATE, 'results', name + '.jsonl')]
    path = next((c for c in candidates if os.path.isfile(c)), None)
    if not path:
        sys.exit('no results for ' + name)
    for line in open(path, encoding='utf-8'):
        yield json.loads(line)


def area_of(path, depth=3):
    return '/'.join(path.lstrip('/').split('?')[0].split('/')[:-1][:depth]) or '(root)'


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('run')
    ap.add_argument('prefix', nargs='?', default='', help='only tests under this WPT path')
    ap.add_argument('--kind', help='only groups of this kind')
    ap.add_argument('--top', type=int, default=40, help='groups (or files) to print')
    ap.add_argument('--files', action='store_true', help='list the files with most failing subtests instead')
    ap.add_argument('--min', type=int, default=3, help='smallest group to print')
    args = ap.parse_args()
    prefix = '/' + args.prefix.strip('/') if args.prefix else ''

    groups = collections.defaultdict(lambda: {'n': 0, 'files': set(), 'areas': collections.Counter(), 'example': None})
    per_file = collections.Counter()
    kinds = collections.Counter()
    total = failing = 0
    for rec in load(args.run):
        path = re.sub(r'^https?://[^/]+', '', rec['url'])
        if not path.startswith(prefix):
            continue
        res = rec.get('result') or {}
        tests = res.get('tests') or []
        if rec.get('kind') == 'reftest':
            tests = [{'name': 'reftest', 'status': FAIL if res.get('status') != 0 else 0,
                      'message': (rec.get('reftest') or {}).get('error') or 'reftest mismatch'}]
        elif not tests:
            tests = [{'name': '(file)', 'status': res.get('status', FAIL), 'message': res.get('message') or 'no subtests'}]
            if res.get('status') == 0:
                tests[0]['status'] = 0
        for t in tests:
            total += 1
            if t['status'] == 0:
                continue
            failing += 1
            if not t.get('message') and tests[0]['name'] == '(file)':
                kind = 'harness'
            else:
                kind = kind_of(path, t.get('name'), t['status'], t.get('message'))
            norm = normalize(t.get('message')) or '(no message)'
            g = groups[(kind, norm)]
            g['n'] += 1
            g['files'].add(path)
            g['areas'][area_of(path)] += 1
            if g['example'] is None:
                g['example'] = (path, t.get('name'))
            per_file[path] += 1
            kinds[kind] += 1

    print('%s%s: %d failing of %d subtests (reftests count one each); by kind: %s' % (
        args.run, ' ' + prefix if prefix else '', failing, total,
        ', '.join('%s %d' % kv for kv in kinds.most_common())))
    if args.files:
        for path, n in per_file.most_common(args.top):
            print('%6d  %s' % (n, path))
        return
    shown = 0
    for (kind, norm), g in sorted(groups.items(), key=lambda kv: -kv[1]['n']):
        if (args.kind and kind != args.kind) or g['n'] < args.min:
            continue
        top = ', '.join('%s %d' % kv for kv in g['areas'].most_common(2))
        print('\n%6d  [%s] %s\n        %d files; %s\n        e.g. %s | %s' % (
            g['n'], kind, norm, len(g['files']), top, g['example'][0], (g['example'][1] or '')[:70]))
        shown += 1
        if shown >= args.top:
            break


if __name__ == '__main__':
    main()
