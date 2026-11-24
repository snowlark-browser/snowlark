# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""The merge gate for Snowlark's WPT runs: which files can be skipped, stored baselines, the flake list and
the per-subtest comparison.

    python tools/wpt/gate.py save-baseline <name> <results.jsonl> [--exe <snowlark.exe>] [--commit <id>]
    python tools/wpt/gate.py compare <baseline> <run> [--recheck 3] [--exe <branch.exe>] [--base-exe <base.exe>]
    python tools/wpt/gate.py flaky <run1.jsonl> <run2.jsonl>... [--out tools/wpt/flaky.json] [--replace]
    python tools/wpt/gate.py list                       # the stored baselines

`run.py --gate [--baseline <name>]` runs the whole suite minus the files that cannot lose anything (those that
passed no subtest in the baseline), split among its processes by the baseline's time per file, and then
compares (see README.md in this folder). A <baseline> or <run> is a result file path, a name in results/, or
(for a baseline) a name in baselines/.

The comparison is by subtest, not by count: a subtest passing in the baseline and not in the run is a loss
even when another subtest gained. It fails loudly (an assertion, exit status 2) when it read no results.
Exit status: 0 no real loss, 1 a real loss, 2 unusable input, 3 the test sets differ (see --allow-set-diff).
"""
import argparse, collections, datetime, heapq, json, os, re, shutil, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))  # tools/
import devenv  # noqa: E402
# Results and baselines: SNOWLARK_WPT_STATE, default <dev dir>/wpt (run.py's STATE). The flake list is in the
# repository, next to this file.
STATE = devenv.setting('SNOWLARK_WPT_STATE', str(devenv.dev_dir() / 'wpt'))
RESULTS = os.path.join(STATE, 'results')
BASELINES = os.path.join(STATE, 'baselines')
FLAKY = os.path.join(HERE, 'flaky.json')

# A file that passed no subtest in the baseline and timed out, or took this long, cannot lose anything.
SLOW_MS = 9000
HARNESS_TIMEOUT = 2


def url_path(url):
    """The test path of a result URL (either origin), query included."""
    return re.sub(r'^https?://[^/]+', '', url)


def area(path):
    """The area a test path belongs to: css/<spec>, or the top-level directory."""
    p = path.strip('/').split('/')
    return '/'.join(p[:2]) if p[0] == 'css' and len(p) > 2 else p[0]


class Rec:
    """What the comparison needs of one result line."""
    __slots__ = ('path', 'reftest', 'ms', 'status', 'passed', 'count', 'ref_pass')

    def __init__(self, path, reftest, ms, status, passed, count, ref_pass):
        self.path, self.reftest, self.ms, self.status = path, reftest, ms, status
        self.passed, self.count, self.ref_pass = passed, count, ref_pass


def parse_line(line):
    r = json.loads(line)
    path = url_path(r['url'])
    res = r.get('result')
    if not isinstance(res, dict):
        raise ValueError(f'no "result" object in the line for {path}')
    tests = res.get('tests') or []
    passed = frozenset(t['name'] for t in tests if t.get('status') == 0)
    is_ref = r.get('kind') == 'reftest'
    ref_pass = (r.get('reftest') or {}).get('status') == 'PASS' if is_ref else None
    return Rec(path, is_ref, r.get('ms') or 0, res.get('status'), passed, len(tests), ref_pass)


def load(path, need_pass=False):
    """{test path: Rec} of a result file. Fails loudly when it holds no results, no subtest at all (or, with
    need_pass, no passing one): a wrong schema once made every comparison look like 'nothing lost'."""
    recs = {}
    with open(path, encoding='utf-8') as f:
        for n, line in enumerate(f, 1):
            line = line.strip()
            if not line:
                continue
            try:
                rec = parse_line(line)
            except (ValueError, KeyError, TypeError) as e:
                raise AssertionError(f'{path}:{n}: not a result line: {e}')
            recs[rec.path] = rec
    check_parsed(recs, path, need_pass)
    return recs


def check_parsed(recs, what, need_pass=False):
    assert recs, f'{what}: no result lines read'
    assert any(r.count for r in recs.values()), f'{what}: no subtest parsed from {len(recs)} lines'
    if need_pass:
        assert any(r.ref_pass if r.reftest else r.passed for r in recs.values()), f'{what}: nothing passing in it'
    refs = [r for r in recs.values() if r.reftest]
    assert all(r.ref_pass is not None for r in refs), f'{what}: a reftest line without its "reftest" status'


# --- The gate: which files can be skipped --------------------------------------------------------------

def skip_reason(rec, slow_ms=SLOW_MS):
    """Why a baseline record's file can be skipped, or None: a testharness file that passed zero subtests
    can only stay the same (nothing passed, so nothing to lose). The reason says what it did instead:
    'timeout', 'slow' (slow_ms or more) or 'failed' (finished or errored sooner)."""
    if rec is None or rec.reftest or rec.passed:
        return None
    if rec.status == HARNESS_TIMEOUT:
        return 'timeout'
    if rec.ms >= slow_ms:
        return 'slow'
    return 'failed'


def select(tests, baseline, slow_ms=SLOW_MS):
    """(kept, skipped) of [(url path, long)] against a baseline {path: Rec}. skipped is
    [(path, reason)]. Files absent from the baseline are kept."""
    kept, skipped = [], []
    for item in tests:
        reason = skip_reason(baseline.get(item[0]), slow_ms)
        if reason:
            skipped.append((item[0], reason))
        else:
            kept.append(item)
    return kept, skipped


# A file the baseline does not know is given this much time in the split.
UNKNOWN_MS = 100


def split_by_time(tests, baseline, jobs):
    """`jobs` lists of [(url path, long)], each for one process: the files longest first by the baseline's
    time per file (UNKNOWN_MS for one it lacks), each to the list with the least time so far, so the lists end
    together (the longest files are the first thing each process runs). Round robin without a baseline.
    (A gate's wall time is its slowest process's: 2-3 of 11 minutes were this imbalance with round robin.)"""
    if not baseline:
        return [tests[i::jobs] for i in range(jobs)]

    def ms(item):
        rec = baseline.get(item[0])
        return rec.ms if rec is not None else UNKNOWN_MS

    chunks, loads = [[] for _ in range(jobs)], [(0, i) for i in range(jobs)]
    heapq.heapify(loads)
    for item in sorted(tests, key=ms, reverse=True):  # (stable: ties keep the discovery order)
        load, i = heapq.heappop(loads)
        chunks[i].append(item)
        heapq.heappush(loads, (load + ms(item), i))
    return chunks


# --- Baselines -----------------------------------------------------------------------------------------

def binary_info(exe):
    if not exe or not os.path.isfile(exe):
        return None
    st = os.stat(exe)
    return {'path': os.path.abspath(exe), 'size': st.st_size,
            'mtime': datetime.datetime.fromtimestamp(st.st_mtime).isoformat(timespec='seconds')}


def binary_matches(info):
    """Whether the binary a baseline was made with is still on disk, unchanged (size and timestamp)."""
    cur = binary_info(info and info.get('path'))
    return bool(cur and cur['size'] == info['size'] and cur['mtime'] == info['mtime'])


def resolve_baseline(name):
    """(results path, meta dict or None) of a baseline given as a stored name, or a result file path."""
    if os.path.isfile(name):
        return name, None
    path = os.path.join(BASELINES, name + '.jsonl')
    if not os.path.isfile(path):
        raise SystemExit(f'no baseline {name!r}: not a file, and not in {BASELINES} (gate.py list)')
    meta_path = os.path.join(BASELINES, name + '.json')
    meta = json.load(open(meta_path, encoding='utf-8')) if os.path.isfile(meta_path) else None
    return path, meta


def latest_baseline():
    """The name of the newest stored baseline, or None."""
    best = None
    if os.path.isdir(BASELINES):
        for f in os.listdir(BASELINES):
            if f.endswith('.json') and not f.endswith('.notrunnable.json'):
                meta = json.load(open(os.path.join(BASELINES, f), encoding='utf-8'))
                if best is None or meta.get('date', '') > best[0]:
                    best = (meta.get('date', ''), f[:-5])
    return best and best[1]


def resolve_run(name):
    for cand in (name, os.path.join(RESULTS, name), os.path.join(RESULTS, name + '.jsonl')):
        if os.path.isfile(cand):
            return cand
    raise SystemExit(f'no result file {name!r} (also tried results/)')


def git_commit(cwd=None):
    try:
        return subprocess.run(['git', 'rev-parse', '--short=12', 'HEAD'], cwd=cwd, capture_output=True, text=True,
                              timeout=20).stdout.strip() or None
    except (OSError, subprocess.SubprocessError):
        return None


def in_scope(path, scope):
    """Whether a test path is in one of the gated directories (`scope` None: all of the suite)."""
    if scope is None:
        return True
    p = path.strip('/')
    return any(p == a or p.startswith(a + '/') for a in scope)


def gate_sidecar(results_path):
    return os.path.splitext(results_path)[0] + '.gate.json'


def save_baseline(name, results_path, exe=None, commit=None, directory=BASELINES):
    """Stores a finished run as baseline `name`. A run made with --gate lacks the files it skipped; their
    lines are copied in from the baseline it was gated by, so a stored baseline always covers the suite."""
    recs = load(results_path)
    os.makedirs(directory, exist_ok=True)
    dest = os.path.join(directory, name + '.jsonl')
    lines = [l for l in open(results_path, encoding='utf-8') if l.strip()]
    filled = 0
    side = gate_sidecar(results_path)
    if os.path.isfile(side):
        info = json.load(open(side, encoding='utf-8'))
        skipped = set(info.get('skipped', {}))
        scope = info.get('scope')  # an --affected run left the other directories out
        for line in open(info['baseline_path'], encoding='utf-8'):
            if line.strip():
                p = url_path(json.loads(line)['url'])
                if p not in recs and (p in skipped or not in_scope(p, scope)):
                    lines.append(line if line.endswith('\n') else line + '\n')
                    filled += 1
    with open(dest, 'w', encoding='utf-8') as f:
        f.writelines(l if l.endswith('\n') else l + '\n' for l in lines)
    all_recs = load(dest)
    meta = {'name': name, 'commit': commit or git_commit(), 'date': datetime.datetime.now().isoformat(timespec='seconds'),
            'binary': binary_info(exe), 'source': os.path.abspath(results_path),
            'files': len(all_recs), 'testharness': sum(not r.reftest for r in all_recs.values()),
            'reftests': sum(r.reftest for r in all_recs.values()), 'filled_from_gate_skips': filled}
    with open(os.path.join(directory, name + '.json'), 'w', encoding='utf-8') as f:
        json.dump(meta, f, indent=2)
        f.write('\n')
    return meta


# --- Flake list ----------------------------------------------------------------------------------------

def flaky_from_runs(runs):
    """{'subtests': {path: [names]}, 'reftests': [paths]} of what differed across runs of one binary.
    runs: [{path: Rec}]. A subtest is flaky when it passed in some of the runs holding its file and not in
    others (absent counts as not passing); a reftest when its verdict differed. Files in one run only say
    nothing."""
    assert len(runs) >= 2, 'a flake list needs at least two runs of the same binary'
    for i, r in enumerate(runs):
        check_parsed(r, f'run {i + 1}', need_pass=True)
    subtests, reftests = {}, set()
    paths = set().union(*[set(r) for r in runs])
    for p in paths:
        recs = [r[p] for r in runs if p in r]
        if len(recs) < 2:
            continue
        if recs[0].reftest:
            if len({x.ref_pass for x in recs}) > 1:
                reftests.add(p)
            continue
        union = frozenset().union(*[x.passed for x in recs])
        inter = recs[0].passed.intersection(*[x.passed for x in recs[1:]])
        diff = union - inter
        if diff:
            subtests[p] = sorted(diff)
    return {'subtests': subtests, 'reftests': sorted(reftests)}


def merge_flaky(old, new):
    out = {'subtests': {k: set(v) for k, v in old.get('subtests', {}).items()}, 'reftests': set(old.get('reftests', []))}
    for k, v in new['subtests'].items():
        out['subtests'].setdefault(k, set()).update(v)
    out['reftests'].update(new['reftests'])
    return {'subtests': {k: sorted(v) for k, v in sorted(out['subtests'].items())}, 'reftests': sorted(out['reftests'])}


def load_flaky(path=FLAKY):
    if not path or not os.path.isfile(path):
        return {'subtests': {}, 'reftests': []}
    return json.load(open(path, encoding='utf-8'))


def write_flaky(flaky, runs_from, path=FLAKY, merge=True):
    data = merge_flaky(load_flaky(path), flaky) if merge else flaky
    data = dict(data)
    data['built'] = datetime.date.today().isoformat()
    data['from'] = (load_flaky(path).get('from', []) if merge else []) + [r for r in runs_from]
    data['note'] = 'Subtests and reftests whose result differed across runs of one binary; the gate reports their losses apart.'
    with open(path, 'w', encoding='utf-8') as f:
        json.dump(data, f, indent=1)
        f.write('\n')
    return data


def build_flaky(paths, out=FLAKY, merge=True):
    flaky = flaky_from_runs([load(p) for p in paths])
    data = write_flaky(flaky, [os.path.basename(p) for p in paths], out, merge)
    return flaky, data


# --- The comparison ------------------------------------------------------------------------------------

def compare(base, run, flaky=None, skipped=()):
    """Compares two {path: Rec}. Returns a dict:
    areas {area: [base passed, run passed, gained, lost]}, losses [(path, name)] split into real and
    flaky, gains, set differences (only_base minus `skipped`, only_run)."""
    flaky = flaky or {'subtests': {}, 'reftests': []}
    fl_sub = {p: set(v) for p, v in flaky.get('subtests', {}).items()}
    fl_ref = set(flaky.get('reftests', []))
    skipped = set(skipped)
    areas = collections.defaultdict(lambda: [0, 0, 0, 0])
    real, flk = [], []
    common = sorted(set(base) & set(run))
    for p in common:
        b, r = base[p], run[p]
        if b.reftest != r.reftest:
            continue
        v = areas[area(p)]
        if b.reftest:
            bp, rp = {'reftest'} if b.ref_pass else set(), {'reftest'} if r.ref_pass else set()
        else:
            bp, rp = b.passed, r.passed
        v[0] += len(bp)
        v[1] += len(rp)
        v[2] += len(rp - bp)
        lost = bp - rp
        v[3] += len(lost)
        for name in sorted(lost):
            is_flaky = p in fl_ref if b.reftest else name in fl_sub.get(p, ())
            (flk if is_flaky else real).append((p, name))
    return {'areas': dict(areas), 'real': real, 'flaky': flk, 'common': len(common),
            'only_base': sorted(set(base) - set(run) - skipped), 'only_run': sorted(set(run) - set(base)),
            'skipped_missing': len(skipped & (set(base) - set(run)))}


# --- Rechecks ------------------------------------------------------------------------------------------

def recheck(losses, exe, base_exe, times, jobs=4, have_slot=False):
    """Re-runs the files of the lost subtests alone, `times` times each, with the branch binary and (when
    given) the baseline binary. Returns {(path, name): 'real'|'recovered'|'unresolved'}:
    recovered = the branch passed the subtest in some re-run (load made the first miss);
    unresolved = it never passed on the branch, and the baseline binary did not pass it in these re-runs
    either, though the baseline run did. Both failing proves nothing, so it fails the gate like a real loss."""
    sys.path.insert(0, HERE)
    import run as runner
    work = os.path.join(runner.work_root(), 'work-recheck')
    os.makedirs(work, exist_ok=True)
    slot = None if have_slot else runner.acquire_slot()  # (held until the process ends)
    server = runner.start_server()
    files = sorted({p for p, _ in losses})
    th = [p for p in files if not is_reftest_path(p, losses)]
    refs = [p for p in files if is_reftest_path(p, losses)]
    longs = {}
    for a in sorted({area(p) for p in th}):
        for u, l in runner.tests_in(a, skip_slow=False):
            longs[u] = l

    def passes(exe_path, tag):
        """{path: set of passing names, or {'reftest'}} over `times` runs: a list of sets."""
        out = []
        for i in range(times):
            got = {}
            if th:
                part = os.path.join(work, f'{tag}-{i}.jsonl')
                if os.path.exists(part):
                    os.remove(part)
                chunks = [[(p, longs.get(p, False)) for p in th[j::jobs]] for j in range(jobs)]
                import threading
                ts = [threading.Thread(target=runner.run_chunk, args=(exe_path, c, part + f'.{j}', work, f'rc{j}'))
                      for j, c in enumerate(chunks) if c]
                [t.start() for t in ts]
                [t.join() for t in ts]
                for j in range(jobs):
                    f = part + f'.{j}'
                    if os.path.exists(f):
                        for k, v in load_unchecked(f).items():
                            got[k] = v.passed
                        os.remove(f)
            if refs:
                lines = runner.run_reftests(exe_path, [r.strip('/').split('?')[0] for r in refs], jobs, work,
                                            os.path.join(work, 'failures'))
                for line in lines:
                    got[url_path(line['url'])] = {'reftest'} if line['reftest']['status'] == 'PASS' else set()
            out.append(got)
        return out

    branch = passes(exe, 'branch')
    base = passes(base_exe, 'base') if base_exe else None
    verdict = {}
    for p, name in losses:
        name_ok = lambda got: name in got.get(p, ())
        if any(name_ok(g) for g in branch):
            verdict[(p, name)] = 'recovered'
        elif base is not None and not any(name_ok(g) for g in base):
            verdict[(p, name)] = 'unresolved'
        else:
            verdict[(p, name)] = 'real'
    if server:
        server.terminate()
    return verdict


def is_reftest_path(p, losses):
    return any(q == p and n == 'reftest' for q, n in losses)


def load_unchecked(path):
    recs = {}
    for line in open(path, encoding='utf-8'):
        if line.strip():
            r = parse_line(line)
            recs[r.path] = r
    return recs


# --- Reporting -----------------------------------------------------------------------------------------

def print_report(result, base_label, run_label, verdicts=None, limit=60, out=sys.stdout):
    P = lambda *a: print(*a, file=out)
    verdicts = verdicts or {}
    tot = [sum(v[i] for v in result['areas'].values()) for i in range(4)]
    P(f'baseline {base_label}  vs  run {run_label}')
    P(f'{result["common"]} files in both; passing {tot[0]} -> {tot[1]}, gained {tot[2]}, lost {tot[3]}')
    real = [x for x in result['real'] if verdicts.get(x, 'real') == 'real']
    unresolved = [x for x in result['real'] if verdicts.get(x) == 'unresolved']
    recovered = [x for x in result['real'] if verdicts.get(x) == 'recovered']
    P('')
    P('per area (gained / lost):')
    for k, v in sorted(result['areas'].items()):
        if v[2] or v[3]:
            P(f'  {k:34} {v[0]:>7} -> {v[1]:>7}   +{v[2]} -{v[3]}')
    P('')
    P(f'REAL LOSSES: {len(real)} in {len({p for p, _ in real})} files')
    show(real, P, limit)
    if unresolved:
        P('')
        P(f'UNRESOLVED LOSSES (the baseline binary failed them too when re-run; run them by hand): {len(unresolved)} '
          f'in {len({p for p, _ in unresolved})} files')
        show(unresolved, P, limit)
    if recovered:
        P('')
        P(f'dropped after re-running alone: {len(recovered)} (' + ', '.join(
            f'{c} {n}' for n, c in collections.Counter(verdicts[x] for x in recovered).items()) + ')')
        show(recovered, P, 20, verdicts)
    P('')
    P(f'FLAKY LOSSES (on the flake list, not counted): {len(result["flaky"])} in {len({p for p, _ in result["flaky"]})} files')
    show(result['flaky'], P, 20)
    return len(real), len(unresolved)


def show(items, P, limit, verdicts=None):
    by = collections.OrderedDict()
    for p, n in items:
        by.setdefault(p, []).append(n)
    for i, (p, names) in enumerate(by.items()):
        if i >= limit:
            P(f'  ... {len(by) - limit} more files')
            break
        extra = f' [{verdicts[(p, names[0])]}]' if verdicts else ''
        P(f'  {p}  ({len(names)}){extra}: ' + '; '.join(n[:70] for n in names[:3]) + (' ...' if len(names) > 3 else ''))


def run_compare(baseline, run, flaky_path=FLAKY, recheck_n=0, exe=None, base_exe=None, allow_set_diff=False, limit=60,
                have_slot=False):
    """The whole compare step; returns the exit status."""
    base_path, meta = resolve_baseline(baseline)
    run_path = resolve_run(run)
    try:
        base, new = load(base_path, need_pass=True), load(run_path)
    except AssertionError as e:
        print(f'gate: refusing to compare: {e}', file=sys.stderr)
        return 2
    skipped = []
    side = gate_sidecar(run_path)
    if os.path.isfile(side):
        info = json.load(open(side, encoding='utf-8'))
        skipped = list(info.get('skipped', {}))
        scope = info.get('scope')
        if scope is not None:
            # An --affected run gated some directories: the baseline's files in the others are out of scope.
            base = {p: r for p, r in base.items() if in_scope(p, scope)}
            print(f'gate: compared within {len(scope)} directories ({len(base)} baseline files)')
    result = compare(base, new, load_flaky(flaky_path), skipped)
    label = (meta or {}).get('name', os.path.basename(base_path))
    if meta:
        label += f' (commit {meta.get("commit")}, {meta.get("date")})'
    verdicts = {}
    if result['real'] and recheck_n > 0:
        if not exe:
            print('gate: no --exe, so the losses are not re-run alone; counting them as found', file=sys.stderr)
        else:
            if base_exe is None and meta and binary_matches(meta.get('binary')):
                base_exe = meta['binary']['path']
            print(f'gate: re-running {len({p for p, _ in result["real"]})} files with losses alone, {recheck_n}x'
                  + (f' (baseline binary {base_exe})' if base_exe else ' (the baseline binary is not available)'),
                  flush=True)
            verdicts = recheck(result['real'], exe, base_exe, recheck_n, have_slot=have_slot)
    real, unresolved = print_report(result, label, os.path.basename(run_path), verdicts, limit)
    status = 1 if real or unresolved else 0
    differ = result['only_base'] or result['only_run']
    if skipped:
        print(f'\ntest set: {len(skipped)} baseline files were skipped by the gate ({result["skipped_missing"]} of them absent from the run, as expected)')
    if differ:
        print(f'\nTEST SET DIFFERS: {len(result["only_base"])} files only in the baseline, {len(result["only_run"])} only in the run '
              f'(not compared). Examples: ' + ', '.join((result['only_base'] + result['only_run'])[:4]))
        if not allow_set_diff:
            print('refusing to call this a pass: re-run with the same areas, or pass --allow-set-diff')
            status = status or 3
    print('\nresult: ' + ('REAL LOSSES' if real else 'UNRESOLVED LOSSES' if unresolved else 'TEST SET DIFFERS (nothing lost in the common files)' if status == 3 else 'no real loss'))
    return status


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest='cmd', required=True)
    c = sub.add_parser('compare')
    c.add_argument('baseline')
    c.add_argument('run')
    c.add_argument('--flaky', default=FLAKY)
    c.add_argument('--recheck', type=int, default=3, metavar='N', help='re-run each lost file alone N times (default 3; 0 = off)')
    c.add_argument('--exe', help='the binary that made the run (needed for --recheck)')
    c.add_argument('--base-exe', help='the baseline binary (default: the one the baseline records, if unchanged)')
    c.add_argument('--allow-set-diff', action='store_true')
    c.add_argument('--limit', type=int, default=60)
    s = sub.add_parser('save-baseline')
    s.add_argument('name')
    s.add_argument('results')
    s.add_argument('--exe')
    s.add_argument('--commit')
    f = sub.add_parser('flaky')
    f.add_argument('runs', nargs='+')
    f.add_argument('--out', default=FLAKY)
    f.add_argument('--replace', action='store_true', help='start a new list instead of adding to the existing one')
    sub.add_parser('list')
    a = ap.parse_args(argv)
    try:
        if a.cmd == 'compare':
            return run_compare(a.baseline, a.run, a.flaky, a.recheck, a.exe, a.base_exe, a.allow_set_diff, a.limit)
        if a.cmd == 'save-baseline':
            meta = save_baseline(a.name, resolve_run(a.results), a.exe, a.commit)
            print(json.dumps(meta, indent=1))
        elif a.cmd == 'flaky':
            flaky, data = build_flaky([resolve_run(r) for r in a.runs], a.out, not a.replace)
            print(f'{sum(len(v) for v in flaky["subtests"].values())} flaky subtests in {len(flaky["subtests"])} files and '
                  f'{len(flaky["reftests"])} flaky reftests from {len(a.runs)} runs; {a.out} now holds '
                  f'{sum(len(v) for v in data["subtests"].values())} subtests, {len(data["reftests"])} reftests')
        elif a.cmd == 'list':
            for fn in sorted(os.listdir(BASELINES)) if os.path.isdir(BASELINES) else []:
                if fn.endswith('.json') and not fn.endswith('.notrunnable.json'):
                    m = json.load(open(os.path.join(BASELINES, fn), encoding='utf-8'))
                    print(f'{m["name"]:20} {m["date"]}  commit {m.get("commit")}  {m["files"]} files  binary {m.get("binary") and m["binary"]["size"]}')
    except AssertionError as e:
        print(f'gate: {e}', file=sys.stderr)
        return 2
    return 0


if __name__ == '__main__':
    sys.exit(main())
