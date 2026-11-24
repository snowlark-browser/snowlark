# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Which WPT directories a change can affect: the dependency-aware gate (`run.py --gate --affected`).

    python tools/wpt/affected.py [--base origin/main]            # what the branch's diff gates, and why
    python tools/wpt/affected.py --files crates/snowlark-net/src/lib.rs docs/x.md

The changed files (the branch's diff against its merge base with `--base`, plus untracked files) are matched
against `areas-by-crate.json`, the checked-in map from source paths to the WPT directories whose results they can
change. The gate runs the union of those directories and compares them with the baseline; a file the map does
not know gates everything. See that file's own notes for how to read a rule.

Reading a rule: `gate` lists what a change under the path can alter. `@all` is every directory except the engine-only
ones (`v8_only`), `@v8` those, `@<name>` a named group of directories, anything else a directory of the checkout
(`css/css-grid`, `fetch`). A restricted rule (one that does not name `@all`) is a statement about the crate's
dependents too: pages observe the crate only through the directories it names. `used_by` records which crates
depend on it today; `check_map` (run by the tests) fails when the dependency graph grows a new dependent, so
the claim is looked at again.
"""
import argparse
import fnmatch
import json
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
MAP_PATH = os.path.join(HERE, 'areas-by-crate.json')


def load_map(path=MAP_PATH):
    with open(path, encoding='utf-8') as f:
        return json.load(f)


def changed_files(base='origin/main', repo=REPO):
    """The files the branch changes against its merge base with `base` (committed, staged, edited and untracked)."""
    def git(*args):
        return subprocess.run(['git', '-C', repo, *args], capture_output=True, text=True, check=True).stdout
    try:
        merge_base = git('merge-base', base, 'HEAD').strip() or base
    except subprocess.CalledProcessError:
        merge_base = base
    files = set(git('diff', '--name-only', merge_base).split('\n')) | set(git('ls-files', '--others', '--exclude-standard').split('\n'))
    return sorted(f for f in files if f)


def crate_dependents(repo=REPO):
    """{crate directory name: set of crate directory names that depend on it (directly)} from the path
    dependencies in each crates/*/Cargo.toml."""
    deps = {}
    crates = os.path.join(repo, 'crates')
    for name in sorted(os.listdir(crates)):
        toml = os.path.join(crates, name, 'Cargo.toml')
        if not os.path.isfile(toml):
            continue
        text = open(toml, encoding='utf-8').read()
        for m in re.finditer(r'path\s*=\s*"\.\./([^"/]+)"', text):
            deps.setdefault(m.group(1), set()).add(name)
        deps.setdefault(name, set())
    return deps


def match_rule(path, data):
    """(kind, rule): kind 'ignore' for a file no page can observe, 'rule' with the longest matching path prefix,
    or 'unknown'."""
    if any(fnmatch.fnmatchcase(path, pattern) for pattern in data['ignore']):
        return 'ignore', None
    best = None
    for rule in data['rules']:
        if path.startswith(rule['path']) and (best is None or len(rule['path']) > len(best['path'])):
            best = rule
    return ('rule', best) if best else ('unknown', None)


def expand(items, data, areas):
    """The areas (names from run.py's `all_areas()`) selected by a list of rule entries."""
    wanted = set()
    for item in items:
        if item == '@all':
            wanted.update(a for a in areas if not is_under_any(a, data['v8_only']))
        elif item == '@v8':
            wanted.update(a for a in areas if is_under_any(a, data['v8_only']))
        elif item.startswith('@'):
            wanted.update(expand(data['groups'][item[1:]], data, areas))
        else:
            wanted.update(a for a in areas if a == item or a.startswith(item + '/') or item.startswith(a + '/'))
    return wanted


def is_under_any(area, dirs):
    return any(area == d or area.startswith(d + '/') for d in dirs)


def plan(files, areas, data=None):
    """(selected areas as a sorted list, why: {file: (what it matched, what it gates)}). A file the map does not
    know gates every area."""
    data = data or load_map()
    selected, why = set(), {}
    for f in files:
        kind, rule = match_rule(f, data)
        if kind == 'ignore':
            why[f] = ('no page observes it', [])
            continue
        gate = rule['gate'] if rule else ['@all', '@v8']
        selected |= expand(gate, data, areas)
        why[f] = (rule['path'] if rule else 'not in the map: everything', gate)
    return sorted(selected), why


def check_map(data, repo=REPO):
    """Problems with the map (a list of strings; empty when sound): every crate needs a rule, every named group
    and `used_by` crate must exist and match the dependency graph, and `v8_only` must be directories."""
    problems = []
    dependents = crate_dependents(repo)
    for crate in sorted(dependents):
        prefix = f'crates/{crate}/'
        if not any(r['path'] == prefix for r in data['rules']):
            problems.append(f'crate {crate} has no rule (it would gate everything): add one to areas-by-crate.json')
    for rule in data['rules']:
        for item in rule['gate']:
            if item.startswith('@') and item not in ('@all', '@v8') and item[1:] not in data['groups']:
                problems.append(f'rule {rule["path"]}: unknown group {item}')
        m = re.match(r'crates/([^/]+)/$', rule['path'])
        if m and '@all' not in rule['gate']:
            crate = m.group(1)
            actual = sorted(dependents.get(crate, set()))
            claimed = sorted(rule.get('used_by', []))
            if actual != claimed:
                problems.append(f'{crate} is used by {actual} but its rule says {claimed}: a restricted rule is a claim '
                                f'about its dependents, so look at it again and update used_by')
    for name, items in data['groups'].items():
        if any(i.startswith('@') and i[1:] not in data['groups'] for i in items):
            problems.append(f'group {name} names an unknown group')
    return problems


def describe(selected, why, areas, out=sys.stdout):
    P = lambda *a: print(*a, file=out)
    everything = set(areas)
    if set(selected) == everything:
        P(f'affected: all {len(areas)} directories (the change reaches the whole suite)')
    else:
        P(f'affected: {len(selected)} of {len(areas)} directories: ' + ', '.join(selected))
        P('not gated: ' + (', '.join(sorted(everything - set(selected))) or 'none'))
    by_reason = {}
    for f, (matched, gate) in why.items():
        by_reason.setdefault((matched, tuple(gate)), []).append(f)
    for (matched, gate), files in sorted(by_reason.items()):
        shown = ', '.join(files[:3]) + (f' and {len(files) - 3} more' if len(files) > 3 else '')
        P(f'  {matched} -> {" ".join(gate) if gate else "nothing"}: {shown}')


def main(argv=None):
    sys.path.insert(0, HERE)
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('--base', default='origin/main')
    ap.add_argument('--files', nargs='*', help='use these paths instead of the git diff')
    ap.add_argument('--check', action='store_true', help='only check the map against the crates')
    args = ap.parse_args(argv)
    data = load_map()
    if args.check:
        problems = check_map(data)
        print('\n'.join(problems) or 'the map is sound')
        return 1 if problems else 0
    import run
    areas = run.all_areas()
    files = args.files if args.files is not None else changed_files(args.base)
    selected, why = plan(files, areas, data)
    describe(selected, why, areas)
    return 0


if __name__ == '__main__':
    sys.exit(main())
