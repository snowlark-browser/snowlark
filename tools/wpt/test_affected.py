# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Tests of the dependency-aware gate's map (affected.py, areas-by-crate.json): which directories a diff gates,
that the map keeps up with the crates, and what the gate's scope means for the comparison.

    python tools/wpt/test_affected.py
"""
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import affected  # noqa: E402
import gate  # noqa: E402

AREAS = ['css/css-grid', 'css/css-fonts', 'css/css-images', 'fetch', 'xhr', 'html/semantics', 'html/canvas', 'svg',
         'IndexedDB', 'webaudio', 'webgl', 'wasm', 'third_party', 'dom']


def gated(*files):
    return set(affected.plan(list(files), AREAS)[0])


class Plan(unittest.TestCase):
    def test_a_net_only_diff_skips_css_layout_and_the_engine_only_directories(self):
        got = gated('crates/snowlark-net/src/lib.rs')
        self.assertIn('fetch', got)
        self.assertIn('xhr', got)
        self.assertNotIn('css/css-grid', got)
        self.assertNotIn('wasm', got)
        self.assertNotIn('dom', got)

    def test_a_style_diff_gates_everything_but_the_engine_only_directories(self):
        got = gated('crates/snowlark-style/src/lib.rs')
        self.assertEqual(got, set(AREAS) - {'wasm', 'third_party'})

    def test_the_javascript_engine_gates_the_engine_only_directories_too(self):
        for f in ('crates/snowlark-js/src/lib.rs', 'vendor/v8/BUILD.gn', 'tools/v8-pgo/pin.txt'):
            self.assertEqual(gated(f), set(AREAS), f)

    def test_a_file_the_map_does_not_know_gates_everything(self):
        self.assertEqual(gated('some/new/place/file.rs'), set(AREAS))
        self.assertEqual(gated('crates/snowlark-new/src/lib.rs'), set(AREAS))

    def test_build_files_and_the_idl_gate_everything(self):
        self.assertEqual(gated('Cargo.lock'), set(AREAS))
        self.assertEqual(gated('dev.ps1'), set(AREAS))
        self.assertEqual(gated('idl/Element.idl'), set(AREAS) - {'wasm', 'third_party'})
        self.assertEqual(gated('vendor/stylo/style/lib.rs'), set(AREAS) - {'wasm', 'third_party'})

    def test_notes_tests_and_the_browser_window_gate_nothing(self):
        self.assertEqual(gated('docs/internal/x.md', 'README.md', 'crates/snowlark-page/tests/it.rs'), set())
        self.assertEqual(gated('crates/snowlark-shell/src/lib.rs'), set())
        self.assertEqual(gated('crates/snowlark/src/cli/bench.rs'), set())

    def test_the_union_of_several_files_is_gated(self):
        got = gated('crates/snowlark-gl/src/lib.rs', 'crates/snowlark-idb/src/lib.rs', 'docs/x.md')
        self.assertEqual(got, {'webgl', 'html/canvas', 'IndexedDB'})

    def test_the_longest_prefix_wins(self):
        self.assertEqual(gated('crates/snowlark/src/cli/wpt.rs'), set(AREAS) - {'wasm', 'third_party'})
        self.assertEqual(gated('crates/snowlark/src/cli/speedometer.rs'), set())

    def test_the_runner_and_the_map_gate_everything(self):
        self.assertEqual(gated('tools/wpt/run.py'), set(AREAS))
        self.assertEqual(gated('tools/wpt/areas-by-crate.json'), set(AREAS))

    def test_a_coarser_area_covers_a_finer_rule(self):
        # (the area list names html/semantics, the rule names html/semantics/embedded-content)
        self.assertIn('html/semantics', gated('crates/snowlark-media/src/lib.rs'))

    def test_the_description_names_what_is_gated_and_why(self):
        import io
        selected, why = affected.plan(['crates/snowlark-net/src/a.rs'], AREAS)
        out = io.StringIO()
        affected.describe(selected, why, AREAS, out)
        text = out.getvalue()
        self.assertIn('not gated:', text)
        self.assertIn('crates/snowlark-net/', text)
        self.assertIn('css/css-grid', text.split('not gated:')[1])


class MapIsSound(unittest.TestCase):
    def test_every_crate_has_a_rule_and_every_restricted_rule_matches_the_dependency_graph(self):
        self.assertEqual(affected.check_map(affected.load_map()), [])

    def test_a_new_dependent_of_a_restricted_crate_is_noticed(self):
        data = affected.load_map()
        for rule in data['rules']:
            if rule['path'] == 'crates/snowlark-gl/':
                rule['used_by'] = []
        problems = affected.check_map(data)
        self.assertTrue(any('snowlark-gl' in p for p in problems), problems)

    def test_a_crate_without_a_rule_is_noticed(self):
        data = affected.load_map()
        data['rules'] = [r for r in data['rules'] if r['path'] != 'crates/snowlark-audio/']
        self.assertTrue(any('snowlark-audio' in p for p in affected.check_map(data)))

    def test_every_directory_the_map_names_exists_in_the_checkout_or_is_a_known_absence(self):
        # (a stale name would silently gate nothing: the names must be directories of the suite, when it is here)
        import run
        if not os.path.isdir(os.path.join(run.WPT, 'fetch')):
            self.skipTest('no WPT checkout')
        data = affected.load_map()
        names = {n for items in data['groups'].values() for n in items if not n.startswith('@')} | set(data['v8_only'])
        missing = sorted(n for n in names if not os.path.isdir(os.path.join(run.WPT, n)))
        # Directories newer or older than the pinned revision.
        self.assertLessEqual(len(missing), 6, missing)


class Scope(unittest.TestCase):
    def test_a_path_is_in_scope_under_a_gated_directory_only(self):
        self.assertTrue(gate.in_scope('/fetch/api/a.html', ['fetch']))
        self.assertTrue(gate.in_scope('/css/css-fonts/a.html', ['css/css-fonts']))
        self.assertFalse(gate.in_scope('/css/css-grid/a.html', ['css/css-fonts']))
        self.assertFalse(gate.in_scope('/fetchy/a.html', ['fetch']))
        self.assertTrue(gate.in_scope('/anything/a.html', None))


if __name__ == '__main__':
    unittest.main()
