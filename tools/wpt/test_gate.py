# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Tests of the merge gate (gate.py): the skip rule, the per-subtest comparison, the flake list and stored
baselines, on hand-made results (no browser, no server).

    python Tests/wpt/test_gate.py
"""
import json
import os
import shutil
import sys
import tempfile
import unittest
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import gate  # noqa: E402
import run as runner  # noqa: E402

HOST = 'http://web-platform.test:8000'


def th(path, passed=(), failed=(), status=0, ms=50):
    """A testharness result line: `passed` and `failed` are subtest names."""
    tests = [{'name': n, 'status': 0, 'message': None} for n in passed] + \
            [{'name': n, 'status': 1, 'message': 'x'} for n in failed]
    return {'url': HOST + path, 'ms': ms, 'result': {'status': status, 'message': None, 'tests': tests}}


def ref(path, ok=True):
    st = 'PASS' if ok else 'FAIL'
    return {'url': HOST + path, 'ms': 5, 'kind': 'reftest',
            'result': {'status': 0, 'message': None, 'tests': [{'name': 'reftest', 'status': 0 if ok else 1, 'message': None}]},
            'reftest': {'status': st}}


def write(directory, name, lines):
    path = os.path.join(directory, name)
    with open(path, 'w', encoding='utf-8') as f:
        for line in lines:
            f.write(json.dumps(line) + '\n')
    return path


def recs(lines):
    out = {}
    for line in lines:
        r = gate.parse_line(json.dumps(line))
        out[r.path] = r
    return out


class SkipRule(unittest.TestCase):
    def rec(self, **kw):
        return gate.parse_line(json.dumps(th('/a/b.html', **kw)))

    def test_nothing_passed_and_timed_out_is_skipped(self):
        self.assertEqual(gate.skip_reason(self.rec(failed=['a', 'b'], status=2, ms=15000)), 'timeout')

    def test_nothing_passed_and_slow_is_skipped(self):
        self.assertEqual(gate.skip_reason(self.rec(failed=['a'], ms=9000)), 'slow')
        self.assertEqual(gate.skip_reason(self.rec(failed=['a'], ms=8999)), 'failed')

    def test_a_timeout_with_no_subtests_at_all_is_skipped(self):
        self.assertEqual(gate.skip_reason(self.rec(status=2, ms=15000)), 'timeout')

    def test_one_passing_subtest_keeps_the_file_however_slow(self):
        self.assertIsNone(gate.skip_reason(self.rec(passed=['a'], failed=['b'], status=2, ms=65000)))

    def test_a_fast_failing_file_is_skipped_too(self):
        # (nothing passed, so nothing can be lost, however it failed and however fast)
        self.assertEqual(gate.skip_reason(self.rec(failed=['a'], status=1, ms=20)), 'failed')
        self.assertEqual(gate.skip_reason(self.rec(failed=['a', 'b'], ms=20)), 'failed')
        self.assertEqual(gate.skip_reason(self.rec(ms=20)), 'failed')  # (no subtests at all)

    def test_reftests_and_unknown_files_always_run(self):
        slow_ref = gate.parse_line(json.dumps(dict(ref('/r.html', ok=False), ms=60000)))
        self.assertIsNone(gate.skip_reason(slow_ref))
        self.assertIsNone(gate.skip_reason(None))

    def test_select_splits_a_test_list(self):
        base = recs([th('/slow.html', failed=['a'], status=2, ms=15000), th('/ok.html', passed=['a']),
                     th('/slowok.html', passed=['a'], status=2, ms=15000), th('/fail.html', failed=['a'], ms=30)])
        kept, skipped = gate.select([('/slow.html', False), ('/ok.html', False), ('/slowok.html', True), ('/new.html', False),
                                     ('/fail.html', False)], base)
        self.assertEqual(skipped, [('/slow.html', 'timeout'), ('/fail.html', 'failed')])
        self.assertEqual([p for p, _ in kept], ['/ok.html', '/slowok.html', '/new.html'])


class SplitByTime(unittest.TestCase):
    def test_files_go_longest_first_to_the_least_loaded_process(self):
        base = recs([th('/a.html', passed=['x'], ms=60000), th('/b.html', passed=['x'], ms=10000),
                     th('/c.html', passed=['x'], ms=10000), th('/d.html', passed=['x'], ms=10000), th('/e.html', passed=['x'], ms=5)])
        tests = [('/e.html', False), ('/d.html', False), ('/c.html', False), ('/b.html', False), ('/a.html', True), ('/new.html', False)]
        chunks = gate.split_by_time(tests, base, 2)
        # 60 s alone; the three 10 s files (ties in their given order), the unknown (100 ms) and the 5 ms one
        # together: 30.1 s.
        self.assertEqual(chunks[0], [('/a.html', True)])
        self.assertEqual(chunks[1], [('/d.html', False), ('/c.html', False), ('/b.html', False), ('/new.html', False), ('/e.html', False)])
        self.assertEqual(sorted(sum(chunks, [])), sorted(tests))

    def test_without_a_baseline_the_split_is_round_robin(self):
        tests = [(f'/{i}.html', False) for i in range(5)]
        self.assertEqual(gate.split_by_time(tests, {}, 2), [tests[0::2], tests[1::2]])

    def test_every_process_gets_a_list_even_when_there_are_few_files(self):
        base = recs([th('/a.html', passed=['x'], ms=50)])
        self.assertEqual(gate.split_by_time([('/a.html', False)], base, 3), [[('/a.html', False)], [], []])


class Compare(unittest.TestCase):
    def test_identical_input_reports_nothing(self):
        lines = [th('/dom/a.html', passed=['x', 'y']), ref('/css/css-grid/r.html')]
        r = gate.compare(recs(lines), recs(lines))
        self.assertEqual((r['real'], r['flaky'], r['only_base'], r['only_run']), ([], [], [], []))
        self.assertEqual(r['common'], 2)

    def test_a_flipped_subtest_is_a_loss_even_when_another_gained(self):
        base = recs([th('/dom/a.html', passed=['x', 'y'], failed=['z'])])
        new = recs([th('/dom/a.html', passed=['x', 'z'], failed=['y'])])
        r = gate.compare(base, new)
        self.assertEqual(r['real'], [('/dom/a.html', 'y')])
        self.assertEqual(r['areas']['dom'], [2, 2, 1, 1])

    def test_a_newly_timed_out_file_loses_its_subtests(self):
        base = recs([th('/dom/a.html', passed=['x', 'y'])])
        new = recs([th('/dom/a.html', passed=['x'], status=2, ms=15000)])
        self.assertEqual(gate.compare(base, new)['real'], [('/dom/a.html', 'y')])

    def test_reftest_loss_counts_in_its_area(self):
        base = recs([ref('/css/css-grid/r.html')])
        new = recs([ref('/css/css-grid/r.html', ok=False)])
        r = gate.compare(base, new)
        self.assertEqual(r['real'], [('/css/css-grid/r.html', 'reftest')])
        self.assertEqual(r['areas']['css/css-grid'][3], 1)

    def test_flaky_losses_are_kept_apart(self):
        base = recs([th('/dom/a.html', passed=['x', 'y']), ref('/css/css-grid/r.html')])
        new = recs([th('/dom/a.html', passed=['x']), ref('/css/css-grid/r.html', ok=False)])
        flaky = {'subtests': {'/dom/a.html': ['y']}, 'reftests': ['/css/css-grid/r.html']}
        r = gate.compare(base, new, flaky)
        self.assertEqual(r['real'], [])
        self.assertEqual(sorted(r['flaky']), [('/css/css-grid/r.html', 'reftest'), ('/dom/a.html', 'y')])

    def test_a_flaky_file_does_not_hide_its_other_subtests(self):
        base = recs([th('/dom/a.html', passed=['x', 'y'])])
        new = recs([th('/dom/a.html')])
        r = gate.compare(base, new, {'subtests': {'/dom/a.html': ['y']}, 'reftests': []})
        self.assertEqual(r['real'], [('/dom/a.html', 'x')])

    def test_files_the_gate_skipped_are_not_a_set_difference(self):
        base = recs([th('/dom/a.html', passed=['x']), th('/dom/slow.html', status=2, ms=15000)])
        new = recs([th('/dom/a.html', passed=['x'])])
        self.assertEqual(gate.compare(base, new)['only_base'], ['/dom/slow.html'])
        r = gate.compare(base, new, skipped=['/dom/slow.html'])
        self.assertEqual((r['only_base'], r['skipped_missing']), ([], 1))

    def test_https_and_http_origins_are_the_same_test(self):
        self.assertEqual(gate.url_path('https://web-platform.test:8443/a/b.https.html?x'), '/a/b.https.html?x')


class Loading(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.dir, True)

    def test_an_empty_file_fails_loudly(self):
        with self.assertRaises(AssertionError):
            gate.load(write(self.dir, 'e.jsonl', []))

    def test_a_file_with_no_passing_subtest_fails_loudly(self):
        with self.assertRaises(AssertionError):
            gate.load(write(self.dir, 'z.jsonl', [th('/a.html', failed=['x'])]), need_pass=True)

    def test_a_wrong_schema_fails_loudly(self):
        with self.assertRaises(AssertionError):
            gate.load(write(self.dir, 'w.jsonl', [{'url': HOST + '/a.html', 'tests': [{'name': 'x', 'status': 0}]}]))

    def test_compare_refuses_unusable_input_with_status_2(self):
        good = write(self.dir, 'g.jsonl', [th('/a.html', passed=['x'])])
        bad = write(self.dir, 'b.jsonl', [])
        self.assertEqual(gate.run_compare(good, bad), 2)


class Flaky(unittest.TestCase):
    def test_subtests_that_differ_between_runs_of_one_binary(self):
        a = recs([th('/dom/a.html', passed=['x', 'y']), th('/dom/b.html', passed=['p']), ref('/css/r.html')])
        b = recs([th('/dom/a.html', passed=['x']), th('/dom/b.html', passed=['p']), ref('/css/r.html', ok=False)])
        f = gate.flaky_from_runs([a, b])
        self.assertEqual(f, {'subtests': {'/dom/a.html': ['y']}, 'reftests': ['/css/r.html']})

    def test_a_subtest_missing_from_a_run_counts_as_not_passing(self):
        a = recs([th('/dom/a.html', passed=['x', 'y'])])
        b = recs([th('/dom/a.html', passed=['x'], status=2)])
        self.assertEqual(gate.flaky_from_runs([a, b])['subtests'], {'/dom/a.html': ['y']})

    def test_a_file_in_only_one_run_says_nothing(self):
        a = recs([th('/dom/a.html', passed=['x']), th('/dom/only.html', passed=['x'])])
        b = recs([th('/dom/a.html', passed=['x'])])
        self.assertEqual(gate.flaky_from_runs([a, b])['subtests'], {})

    def test_one_run_is_not_enough(self):
        with self.assertRaises(AssertionError):
            gate.flaky_from_runs([recs([th('/a.html', passed=['x'])])])

    def test_merge_adds_to_the_existing_list(self):
        old = {'subtests': {'/a.html': ['x']}, 'reftests': ['/r1.html']}
        new = {'subtests': {'/a.html': ['y'], '/b.html': ['z']}, 'reftests': ['/r2.html']}
        self.assertEqual(gate.merge_flaky(old, new), {'subtests': {'/a.html': ['x', 'y'], '/b.html': ['z']},
                                                      'reftests': ['/r1.html', '/r2.html']})

    def test_build_from_files_writes_the_list(self):
        d = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, d, True)
        p1 = write(d, '1.jsonl', [th('/dom/a.html', passed=['x', 'y'])])
        p2 = write(d, '2.jsonl', [th('/dom/a.html', passed=['x'])])
        out = os.path.join(d, 'flaky.json')
        gate.build_flaky([p1, p2], out)
        self.assertEqual(json.load(open(out))['subtests'], {'/dom/a.html': ['y']})


class Baselines(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.dir, True)

    def test_a_saved_baseline_records_commit_date_and_binary(self):
        exe = os.path.join(self.dir, 'snowlark.exe')
        open(exe, 'wb').write(b'x' * 123)
        run = write(self.dir, 'run.jsonl', [th('/dom/a.html', passed=['x']), ref('/css/r.html')])
        out = os.path.join(self.dir, 'baselines')
        meta = gate.save_baseline('main', run, exe, 'abc123', directory=out)
        self.assertEqual((meta['commit'], meta['files'], meta['testharness'], meta['reftests']), ('abc123', 2, 1, 1))
        self.assertEqual(meta['binary']['size'], 123)
        self.assertTrue(gate.binary_matches(meta['binary']))
        open(exe, 'ab').write(b'y')
        self.assertFalse(gate.binary_matches(meta['binary']))
        self.assertTrue(os.path.isfile(os.path.join(out, 'main.jsonl')))

    def test_saving_a_gated_run_adds_back_the_skipped_files(self):
        base = write(self.dir, 'base.jsonl', [th('/dom/a.html', passed=['x']), th('/dom/slow.html', status=2, ms=15000)])
        run = write(self.dir, 'gate.jsonl', [th('/dom/a.html', passed=['x'])])
        json.dump({'baseline': 'base', 'baseline_path': base, 'skipped': {'/dom/slow.html': 'timeout'}},
                  open(os.path.join(self.dir, 'gate.gate.json'), 'w'))
        meta = gate.save_baseline('next', run, None, 'c', directory=os.path.join(self.dir, 'b'))
        self.assertEqual((meta['files'], meta['filled_from_gate_skips']), (2, 1))

    def test_compare_flags_a_test_set_that_differs(self):
        base = write(self.dir, 'base.jsonl', [th('/dom/a.html', passed=['x']), th('/dom/b.html', passed=['x'])])
        run = write(self.dir, 'run.jsonl', [th('/dom/a.html', passed=['x'])])
        self.assertEqual(gate.run_compare(base, run, flaky_path=None), 3)
        self.assertEqual(gate.run_compare(base, run, flaky_path=None, allow_set_diff=True), 0)

    def test_compare_exits_nonzero_on_a_planted_loss_and_zero_on_identical(self):
        lines = [th('/dom/a.html', passed=['x', 'y'])]
        base = write(self.dir, 'base.jsonl', lines)
        same = write(self.dir, 'same.jsonl', lines)
        planted = write(self.dir, 'planted.jsonl', [th('/dom/a.html', passed=['x'], failed=['y'])])
        self.assertEqual(gate.run_compare(base, same, flaky_path=None), 0)
        self.assertEqual(gate.run_compare(base, planted, flaky_path=None), 1)


class Rechecks(unittest.TestCase):
    PAGE = '/css/a.html'

    def setUp(self):
        self.dir = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.dir, ignore_errors=True)

    def verdict(self, branch, base):
        """The verdict on a reftest whose re-runs give `branch` and then `base`."""
        results = iter([branch, base])

        def reftests(exe, areas, jobs, work, failures):
            return [{'url': HOST + self.PAGE, 'reftest': {'status': next(results)}}]

        with mock.patch.object(runner, 'run_reftests', reftests), \
                mock.patch.object(runner, 'start_server', lambda: None), \
                mock.patch.object(runner, 'work_root', lambda: self.dir):
            key = (self.PAGE, 'reftest')
            return gate.recheck([key], 'branch.exe', 'base.exe', 1, have_slot=True)[key]

    def test_a_loss_only_the_branch_shows_is_real(self):
        self.assertEqual(self.verdict('FAIL', 'PASS'), 'real')

    def test_a_loss_the_branch_passes_alone_was_load(self):
        self.assertEqual(self.verdict('PASS', 'PASS'), 'recovered')

    def test_a_loss_the_baseline_fails_too_stays_open(self):
        self.assertEqual(self.verdict('FAIL', 'FAIL'), 'unresolved')

    def test_unresolved_losses_fail_the_gate(self):
        base = write(self.dir, 'base.jsonl', [th('/dom/a.html', passed=['x'])])
        run = write(self.dir, 'run.jsonl', [th('/dom/a.html', failed=['x'])])
        with mock.patch.object(gate, 'recheck', lambda *a, **k: {('/dom/a.html', 'x'): 'unresolved'}):
            status = gate.run_compare(base, run, flaky_path=None, recheck_n=1, exe='b.exe', base_exe='a.exe')
        self.assertEqual(status, 1)

    def test_recovered_losses_do_not_fail_the_gate(self):
        base = write(self.dir, 'base.jsonl', [th('/dom/a.html', passed=['x'])])
        run = write(self.dir, 'run.jsonl', [th('/dom/a.html', failed=['x'])])
        with mock.patch.object(gate, 'recheck', lambda *a, **k: {('/dom/a.html', 'x'): 'recovered'}):
            status = gate.run_compare(base, run, flaky_path=None, recheck_n=1, exe='b.exe', base_exe='a.exe')
        self.assertEqual(status, 0)


class ReftestRuns(unittest.TestCase):
    def test_each_run_compares_its_own_screenshots(self):
        """Two runs in one process write the same file names: the second must not see the first's."""
        from PIL import Image
        work = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, work, ignore_errors=True)
        size = (800, 600)
        colors = iter(['red', 'blue'])

        def shoot(exe, chunk, results, work, index):
            color = next(colors)
            for key, url, png, _ in chunk:
                Image.new('RGB', (4, 4), color if url == '/t.html' else 'blue').save(os.path.join(work, 'shots', png))
                results[key] = {'url': url, 'png': png, 'status': 'OK', 'wait': None, 'ms': 0, 'message': '',
                                'error': None}

        def evaluate(test, size, shots, shot_dir):
            pngs = [os.path.join(shot_dir, shots[(u, size)]['png']) for u in (test, '/r.html')]
            return {'status': 'PASS' if runner.differences(*pngs) == (0, 0) else 'FAIL'}

        node = mock.Mock(error=None)
        with mock.patch.multiple(runner, reftests_in=lambda a: [('/t.html', '')],
                                 reftest_graph=lambda t: ({('/t.html', size): 1, ('/r.html', size): 1}, {'/t.html': size}),
                                 ref_node=lambda u: node, run_reftest_chunk=shoot, evaluate_reftest=evaluate,
                                 keep_failure=lambda *a: work):
            statuses = [runner.run_reftests('x.exe', ['css'], 1, work, os.path.join(work, 'failures'))[0]['reftest']['status']
                        for _ in range(2)]
        self.assertEqual(statuses, ['FAIL', 'PASS'])


if __name__ == '__main__':
    unittest.main()
