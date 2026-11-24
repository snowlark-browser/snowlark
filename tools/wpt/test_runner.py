# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Tests of what the runner starts (run.py): the directories of a whole-suite run, the globals of a `.any.js` file,
the URLs of the `.https.` and `.h2.` files, and the watch that stops a hung process. No browser, no server.

    python tools/wpt/test_runner.py
"""
import os
import shutil
import sys
import tempfile
import unittest
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import run as runner  # noqa: E402


def touch(root, rel, text=''):
    path = os.path.join(root, *rel.split('/'))
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, 'w', encoding='utf-8') as f:
        f.write(text)


class Tree(unittest.TestCase):
    def setUp(self):
        self.root = tempfile.mkdtemp()
        self.patch = mock.patch.object(runner, 'WPT', self.root)
        self.patch.start()

    def tearDown(self):
        self.patch.stop()
        shutil.rmtree(self.root, ignore_errors=True)


class AnyGlobals(unittest.TestCase):
    def test_the_default_is_a_window_and_a_dedicated_worker(self):
        self.assertEqual(runner.any_globals(''), {'window', 'dedicatedworker'})

    def test_worker_stands_for_the_three_kinds_of_worker(self):
        self.assertEqual(runner.any_globals('worker'), {'dedicatedworker', 'sharedworker', 'serviceworker'})
        self.assertEqual(runner.any_globals('window,worker'), {'window', 'dedicatedworker', 'sharedworker', 'serviceworker'})

    def test_shadowrealm_stands_for_its_six_hosts(self):
        self.assertEqual(len(runner.any_globals('shadowrealm')), 6)

    def test_unknown_globals_select_nothing(self):
        self.assertEqual(runner.any_globals('jsshell'), set())
        self.assertEqual(runner.any_globals('nonsense'), set())

    def test_urls_are_the_manifests(self):
        self.assertEqual(runner.any_url('a/b.any.js', 'window'), '/a/b.any.html')
        self.assertEqual(runner.any_url('a/b.any.js', 'dedicatedworker'), '/a/b.any.worker.html')
        self.assertEqual(runner.any_url('a/b.any.js', 'dedicatedworker-module'), '/a/b.any.worker-module.html')
        self.assertEqual(runner.any_url('a/b.any.js', 'sharedworker'), '/a/b.any.sharedworker.html')
        self.assertEqual(runner.any_url('a/b.any.js', 'serviceworker'), '/a/b.any.serviceworker.html')
        self.assertEqual(runner.any_url('a/b.any.js', 'window-module'), '/a/b.any.window-module.html')
        self.assertEqual(runner.any_url('a/b.https.any.js', 'window'), '/a/b.https.any.html')
        self.assertEqual(runner.any_url('a/b.https.any.js', 'shadowrealm-in-serviceworker'),
                         '/a/b.https.any.shadowrealm-in-serviceworker.html')
        self.assertEqual(runner.any_url('a/b.any.js', 'shadowrealm-in-window'), '/a/b.any.shadowrealm-in-window.html')


class Test262(unittest.TestCase):
    def test_a_test_is_served_wrapped_by_its_flags(self):
        url = lambda text: runner.test262_url('third_party/test262/test/a/b.js', text)
        self.assertEqual(url('/*---\ndescription: x\n---*/'), '/third_party/test262/test/a/b.test262.html')
        self.assertEqual(url('/*---\nflags: [onlyStrict]\n---*/'), '/third_party/test262/test/a/b.test262.strict.html')
        self.assertEqual(url('/*---\nflags: [async, module]\n---*/'), '/third_party/test262/test/a/b.test262-module.html')
        self.assertEqual(url('/*---\nflags:\n  - module\n---*/'), '/third_party/test262/test/a/b.test262-module.html')

    def test_a_file_without_frontmatter_or_a_fixture_is_not_a_test(self):
        self.assertIsNone(runner.test262_url('third_party/test262/test/a/b.js', 'var x;'))
        self.assertIsNone(runner.test262_url('third_party/test262/test/a/b_FIXTURE.js', '/*---\ndescription: x\n---*/'))


class Urls(unittest.TestCase):
    def test_the_origin_follows_the_file_name(self):
        self.assertEqual(runner.full_url('/a/b.html'), 'http://web-platform.test:8000/a/b.html')
        self.assertEqual(runner.full_url('/a/b.https.html?x=1'), 'https://web-platform.test:8443/a/b.https.html?x=1')
        self.assertEqual(runner.full_url('/a/b.h2.html'), 'https://web-platform.test:9000/a/b.h2.html')

    def test_a_query_does_not_pick_the_origin(self):
        self.assertEqual(runner.full_url('/a/b.html?type=.https.'), 'http://web-platform.test:8000/a/b.html?type=.https.')

    def test_a_result_url_goes_back_to_its_path(self):
        for origin in ('http://web-platform.test:8000', 'https://web-platform.test:8443', 'https://web-platform.test:9000'):
            self.assertEqual(runner.url_path(origin + '/a/b.html?q'), '/a/b.html?q')


class Discovery(Tree):
    def test_a_plain_run_lists_a_window_and_a_worker_and_leaves_the_variants_out(self):
        touch(self.root, 'x/a.any.js', '// META: global=window,worker\n')
        touch(self.root, 'x/b.https.any.js')
        touch(self.root, 'x/c.h2.html', '<script src="/resources/testharness.js"></script>')
        urls = sorted(u for u, _ in runner.tests_in('x'))
        self.assertEqual(urls, ['/x/a.any.html', '/x/a.any.worker.html'])

    def test_variants_list_every_global_and_the_https_and_h2_files(self):
        touch(self.root, 'x/a.any.js', '// META: global=window,worker\n')
        touch(self.root, 'x/b.https.any.js')
        touch(self.root, 'x/c.h2.html', '<script src="/resources/testharness.js"></script>')
        touch(self.root, 'x/d.any.js', '// META: global=jsshell\n// META: global=window\n')
        urls = sorted(u for u, _ in runner.tests_in('x', variants=True))
        self.assertEqual(urls, ['/x/a.any.html', '/x/a.any.serviceworker.html', '/x/a.any.sharedworker.html',
                                '/x/a.any.worker.html', '/x/b.https.any.html', '/x/b.https.any.worker.html',
                                '/x/c.h2.html', '/x/d.any.html'])

    def test_meta_variants_multiply_the_urls(self):
        touch(self.root, 'x/a.any.js', '// META: variant=?a\n// META: variant=?b\n')
        urls = sorted(u for u, _ in runner.tests_in('x', variants=True))
        self.assertEqual(urls, ['/x/a.any.html?a', '/x/a.any.html?b', '/x/a.any.worker.html?a', '/x/a.any.worker.html?b'])

    def test_test262_files_are_listed_with_variants_only(self):
        touch(self.root, 'third_party/test262/test/a.js', '/*---\ndescription: x\n---*/\n')
        touch(self.root, 'third_party/test262/test/b_FIXTURE.js', '/*---\ndescription: x\n---*/\n')
        self.assertEqual([u for u, _ in runner.tests_in('third_party', variants=True)],
                         ['/third_party/test262/test/a.test262.html'])
        self.assertEqual(list(runner.tests_in('third_party')), [])

    def test_manual_tests_and_support_directories_are_never_run(self):
        touch(self.root, 'x/a-manual.html', '<script src="/resources/testharness.js"></script>')
        touch(self.root, 'x/resources/b.any.js')
        touch(self.root, 'x/support/c.html', '<script src="/resources/testharness.js"></script>')
        self.assertEqual(list(runner.tests_in('x', variants=True)), [])

    def test_all_areas_adds_every_directory_the_curated_list_leaves_out(self):
        for rel in ('dom/a.html', 'wasm/jsapi/a.any.js', 'webcrypto/b.any.js', 'css/css-grid/x.html', 'css/css-new/x.html',
                    'css/support/x.html', 'html/semantics/x.html', 'html/newthing/x.html', 'resources/x.js', 'common/x.js',
                    'webdriver/x.py', '.git/x', 'tools/x.py'):
            touch(self.root, rel)
        areas = runner.all_areas()
        for wanted in ('dom', 'wasm', 'webcrypto', 'css/css-new', 'html/newthing', 'css/css-grid', 'html/semantics'):
            self.assertIn(wanted, areas)
        for unwanted in ('resources', 'common', 'webdriver', '.git', 'tools', 'css/support', 'css', 'html'):
            self.assertNotIn(unwanted, areas)
        self.assertEqual(len(areas), len(set(areas)))

    def test_the_curated_list_comes_first_and_keeps_its_order(self):
        areas = runner.all_areas()
        self.assertEqual(areas[:len(runner.AREAS)], runner.AREAS)


class Watch(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp()

    def tearDown(self):
        shutil.rmtree(self.dir, ignore_errors=True)

    def run_python(self, code, **kw):
        progress = os.path.join(self.dir, 'part.jsonl')
        return runner.run_watched([sys.executable, '-c', code], dict(os.environ), progress,
                                  os.path.join(self.dir, 'err.txt'), poll=0.1, **kw), progress

    def test_a_process_that_finishes_returns_its_stderr_and_code(self):
        done, _ = self.run_python('import sys; sys.stderr.write("[start] u\\n"); sys.exit(3)', stall=30)
        self.assertEqual((done.returncode, done.hung), (3, False))
        self.assertIn(b'[start] u', done.stderr)

    def test_a_silent_process_is_killed_and_called_hung(self):
        done, _ = self.run_python('import time; time.sleep(60)', stall=1)
        self.assertTrue(done.hung)
        self.assertNotEqual(done.returncode, 0)

    def test_a_process_that_keeps_writing_is_left_alone(self):
        code = 'import sys, time\nfor i in range(15):\n    sys.stderr.write("x\\n"); sys.stderr.flush(); time.sleep(0.2)\n'
        done, _ = self.run_python(code, stall=1.5)
        self.assertFalse(done.hung)
        self.assertEqual(done.returncode, 0)


if __name__ == '__main__':
    unittest.main()
