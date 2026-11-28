# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Runs web-platform-tests (testharness.js tests and reftests) in Snowlark and summarises the results per spec area.

    python tools/wpt/run.py dom url encoding            # those directories of the WPT checkout
    python tools/wpt/run.py dom/nodes --jobs 4
    python tools/wpt/run.py --all                       # every directory with tests, and every variant (--variants)

The WPT checkout is SNOWLARK_WPT (default: `wpt` next to this repository; tools/devenv.py); a run needs the
directories it tests plus resources/, common/, tools/ and docs/commands.json. wptserve is started if it
isn't running (tools/wpt/serve-config.json: http on 8000, web-platform.test mapped to 127.0.0.1 inside
Snowlark through SNOWLARK_HOST_RESOLVER_RULES, so the system's hosts file stays untouched).

Reftests (see REFTESTS.md) run after the testharness tests with the engine's `--reftest` mode:
    python tools/wpt/run.py --snowlark target/iter/snowlark.exe css/css-inline --jobs 8
    python tools/wpt/run.py --snowlark <exe> css/css-inline --reftests-only     # or --no-reftests

Merge checks (see README.md, gate.py):
    python tools/wpt/run.py --gate --baseline <name> --snowlark <exe> --jobs auto   # the suite minus files that cannot lose
    python tools/wpt/run.py --gate --affected --baseline <name> --snowlark <exe>    # only the directories the branch can change
    python tools/wpt/run.py --gate --baseline <old> --save-baseline <new> --snowlark <main exe>   # the next baseline, from a gate run
    python tools/wpt/run.py --all --save-baseline <name> --snowlark <exe>           # a full run as a baseline (now and then)
    python tools/wpt/run.py css/css-grid --repeat 3 --snowlark <exe>                # repeat runs: updates flaky.json

Without --snowlark, the runner tests this checkout's `target/iter/snowlark.exe`.

Results: <state>/results/<name>.jsonl, where <state> is SNOWLARK_WPT_STATE (default <dev dir>/wpt), as are the
stored baselines (<state>/baselines). Results (one line per test file; reftests carry "kind": "reftest") and
<name>-summary.md; the screenshots of reftests that did not pass are kept in <name>-reftests/.
"""
import argparse, collections, hashlib, html, json, os, re, shutil, socket, subprocess, sys, tempfile, threading, time, urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))  # tools/
import devenv  # noqa: E402
WPT = str(devenv.wpt_dir())
# Results and baselines: SNOWLARK_WPT_STATE, default <dev dir>/wpt.
STATE = devenv.setting('SNOWLARK_WPT_STATE', str(devenv.dev_dir() / 'wpt'))
BASE = 'http://web-platform.test:8000'
# `--https`: the `.https.` files load from wptserve's https port, trusting WPT's test CA (tools/certs).
HTTPS_BASE = 'https://web-platform.test:8443'
HOST_RULES = 'MAP *.web-platform.test 127.0.0.1, MAP web-platform.test 127.0.0.1, MAP *.not-web-platform.test 127.0.0.1, MAP not-web-platform.test 127.0.0.1'

WPT_CA = os.path.join(WPT, 'tools', 'certs', 'cacert.pem')


# `.h2.` files load from wptserve's HTTP/2 port (TLS with ALPN, the same test CA).
H2_BASE = 'https://web-platform.test:9000'


def full_url(path):
    """The URL a test path loads from: HTTP/2 for `.h2.` files, https for `.https.` files (both only listed with
    --https or --variants), else http."""
    plain = path.split('?')[0]
    return (H2_BASE if '.h2.' in plain else HTTPS_BASE if '.https.' in plain else BASE) + path


def url_path(url):
    """The test path of a result URL (either origin)."""
    return re.sub(r'^https?://[^/]+', '', url)


AREAS = ['IndexedDB', 'dom', 'url', 'encoding', 'domparsing', 'custom-elements', 'shadow-dom', 'css/selectors', 'css/cssom',
         'css/css-flexbox', 'css/css-display', 'css/css-cascade', 'css/css-variables', 'html/semantics', 'html/dom',
         'html/syntax', 'html/webappapis/timers', 'html/webappapis/microtask-queuing', 'html/browsers/the-window-object', 'html/browsers/origin', 'html/browsers/history', 'html/browsers/windows', 'html/browsers/sandboxing', 'html/browsers/the-windowproxy-exotic-object', 'webmessaging', 'css/motion', 'css/css-images',
         'webidl', 'fetch', 'streams', 'FileAPI', 'hr-time', 'intersection-observer', 'uievents', 'console',
         'webstorage', 'xhr', 'css/css-transitions', 'css/css-animations', 'web-animations',
         'css/css-grid', 'css/css-position', 'css/css-sizing', 'css/css-overflow', 'css/css-masking', 'css/css-align', 'svg',
         'css/css-tables', 'css/css-text', 'css/css-backgrounds', 'css/css-transforms', 'css/css-multicol', 'css/css-inline',
         'css/css-pseudo', 'css/css-lists', 'css/css-ui', 'css/css-fonts', 'css/css-nesting', 'css/geometry', 'css/css-properties-values-api', 'workers', 'css/css-conditional', 'user-timing', 'navigation-timing', 'performance-timeline', 'css/css-values', 'css/mediaqueries', 'css/filter-effects', 'css/compositing', 'css/css-writing-modes',
         'pointerevents', 'inert', 'css/cssom-view', 'html/interaction/focus',
         'editing', 'selection', 'input-events', 'clipboard-apis', 'css/css-shadow', 'css/css-scroll-anchoring']

# Thousands of 60-second variants that all need legacy-encoding form submission in iframes: left out
# unless named directly (python run.py encoding/legacy-mb-japanese).
SLOW = ['encoding/legacy-mb-']

SKIP_DIRS = {'resources', 'support', 'tools', 'reference', 'references', 'crashtests', 'manual'}
STATUS = {0: 'PASS', 1: 'FAIL', 2: 'TIMEOUT', 3: 'NOTRUN', 4: 'PRECONDITION_FAILED'}


def meta_lines(js):
    return [m.groups() for m in re.finditer(r'^//\s*META:\s*(\w+)=(.*)$', js, re.M)]


# Top-level directories of the checkout that hold no tests of the suite (support code, the WebDriver tests, which
# need a driver, and tooling).
NOT_AREAS = {'resources', 'common', 'tools', 'docs', 'webdriver', 'conformance-checkers', '_venv3', 'node_modules'}


def all_areas():
    """Every directory with tests, for `--all`: AREAS (the curated list, which keeps the finer split of css/ and
    html/ in the summaries), then each top-level directory of the checkout it does not cover, and the
    subdirectories of css/ and html/ (and of the other directories AREAS only partly names) it leaves out."""
    covered = set(AREAS)

    def partly(path):
        return any(a.startswith(path + '/') for a in covered)

    out = list(AREAS)

    def add_missing(rel):
        root = os.path.join(WPT, rel) if rel else WPT
        try:
            names = sorted(os.listdir(root))
        except OSError:
            return
        for name in names:
            child = f'{rel}/{name}' if rel else name
            if name.startswith('.') or name in SKIP_DIRS or (not rel and name in NOT_AREAS):
                continue
            if not os.path.isdir(os.path.join(root, name)):
                continue
            if child in covered:
                continue
            if partly(child):
                add_missing(child)
            else:
                out.append(child)

    add_missing('')
    return out


# The globals a `.any.js` file runs in (its `// META: global=` line), as the WPT manifest names them: the
# suffix of the test URL (`.any.<global>.html` unless given). The default is a window and a dedicated worker; a
# keyword that stands for several (`worker`, `shadowrealm`) lists them. (tools/manifest/sourcefile.py)
ANY_SUFFIX = {'window': '.any.html', 'dedicatedworker': '.any.worker.html', 'dedicatedworker-module': '.any.worker-module.html',
              'shadowrealm-in-serviceworker': '.https.any.shadowrealm-in-serviceworker.html',
              'shadowrealm-in-audioworklet': '.https.any.shadowrealm-in-audioworklet.html'}
ANY_GLOBALS = {'window', 'window-module', 'serviceworker', 'serviceworker-module', 'sharedworker', 'sharedworker-module',
               'dedicatedworker', 'dedicatedworker-module', 'worker-module', 'shadowrealm-in-window',
               'shadowrealm-in-shadowrealm', 'shadowrealm-in-dedicatedworker', 'shadowrealm-in-sharedworker',
               'shadowrealm-in-serviceworker', 'shadowrealm-in-audioworklet'}
ANY_LONGHAND = {'worker': {'dedicatedworker', 'sharedworker', 'serviceworker'},
                'shadowrealm': {'shadowrealm-in-window', 'shadowrealm-in-shadowrealm', 'shadowrealm-in-dedicatedworker',
                                'shadowrealm-in-sharedworker', 'shadowrealm-in-serviceworker', 'shadowrealm-in-audioworklet'}}


def any_globals(value):
    """The globals a `// META: global=` value (all such lines joined by commas; '' without one) selects."""
    if not value:
        return {'window', 'dedicatedworker'}
    out = set()
    for item in (v.strip() for v in value.split(',')):
        out |= ANY_LONGHAND.get(item, {item} if item in ANY_GLOBALS else set())
    return out


def any_url(rel, global_name):
    """The URL of `a/b.any.js` (rel) in a global, as the manifest forms it."""
    url = rel.replace('.any.', '.')
    suffix = ANY_SUFFIX.get(global_name, f'.any.{global_name}.html')
    if '.https.' in url and suffix.startswith('.https.'):
        url = url.replace('.https.', '.')
    elif '.h2.' in url and suffix.startswith('.h2.'):
        url = url.replace('.h2.', '.')
    return '/' + url[:-3] + suffix


TEST262_FRONTMATTER = re.compile(r'/\*---(.*)---\*/', re.S)


def test262_url(rel, text):
    """The URL of a test262 test (a `.js` file under a `test262` directory) as the manifest forms it, or None for a
    file that is not one (no frontmatter, or a `_FIXTURE.js`). `.test262[-module|.strict].html` is served by
    wptserve around the file. (tools/manifest/test262.py)"""
    if rel.endswith('_FIXTURE.js'):
        return None
    m = TEST262_FRONTMATTER.search(text)
    if not m:
        return None
    flags = set()
    inline = re.search(r'^flags:\s*\[([^\]]*)\]', m.group(1), re.M)
    if inline:
        flags = {f.strip() for f in inline.group(1).split(',')}
    else:
        block = re.search(r'^flags:[ \t]*\r?\n((?:[ \t]+-[^\n]*\n?)+)', m.group(1), re.M)
        if block:
            flags = {line.strip()[1:].strip() for line in block.group(1).splitlines()}
    suffix = '.test262' + ('-module' if 'module' in flags else '.strict' if 'onlyStrict' in flags else '') + '.html'
    return '/' + rel[:-3] + suffix


def tests_in(area, skip_slow=False, https=False, variants=False):
    """(url, long) for each testharness test in an area that runs in a window over plain http (and, with
    `https`, the `.https.` ones over https). With `variants` every file the manifest lists: the `.https.` and
    `.h2.` ones, and each global of a `.any.js` file (shared and service workers, module workers, ShadowRealm)."""
    https = https or variants
    root = os.path.join(WPT, area)
    # An area is a directory, or a single test file.
    walk = os.walk(root) if os.path.isdir(root) else [(os.path.dirname(root), [], [os.path.basename(root)])]
    for dirpath, dirnames, filenames in walk:
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS and not d.startswith('.'))
        for name in sorted(filenames):
            path = os.path.join(dirpath, name)
            rel = os.path.relpath(path, WPT).replace('\\', '/')
            if skip_slow and any(rel.startswith(prefix) for prefix in SLOW):
                continue
            if '-manual' in name or ('.https.' in name and not https):
                continue
            if not variants and ('.h2.' in name or '.serviceworker.' in name or '.sharedworker.' in name):
                continue
            try:
                text = open(path, encoding='utf-8', errors='replace').read()
            except OSError:
                continue
            if variants and name.endswith('.js') and 'test262' in rel.split('/')[:-1]:
                url = test262_url(rel, text)
                if url:
                    yield url, False
            elif name.endswith(('.any.js', '.window.js', '.worker.js')):
                metas = meta_lines(text)
                # .any.js runs in each global its META says (by default a window and a dedicated worker:
                # x.any.html and x.any.worker.html); .window.js in a window, .worker.js in a dedicated worker.
                scopes = {g.strip() for v in (v for k, v in metas if k == 'global') for g in v.split(',')}
                if name.endswith('.any.js') and variants:
                    urls = [any_url(rel, g) for g in sorted(any_globals(','.join(v for k, v in metas if k == 'global')))
                            if g != 'jsshell']
                elif name.endswith('.any.js'):
                    if not scopes: scopes = {'window', 'dedicatedworker'}
                    urls = (['/' + rel[:-3] + '.html'] if 'window' in scopes else []) + \
                           (['/' + rel[:-3] + '.worker.html'] if scopes & {'worker', 'dedicatedworker'} else [])
                else:
                    urls = ['/' + rel[:-3] + '.html']
                long = any(k == 'timeout' and v.strip() == 'long' for k, v in metas)
                query_variants = [v.strip() for k, v in metas if k == 'variant'] or ['']
                for url in urls:
                    for v in query_variants:
                        yield url + v, long
            elif name.endswith(('.html', '.htm')) and '/resources/testharness.js' in text:
                long = bool(re.search(r'<meta\s+name=["\']?timeout["\']?\s+content=["\']?long', text, re.I))
                query_variants = re.findall(r'<meta\s+name=["\']?variant["\']?\s+content=["\']([^"\']*)', text, re.I) or ['']
                for v in query_variants:
                    yield '/' + rel + v, long


# --- Reftests (see REFTESTS.md) ------------------------------------------------------------------------
# A reftest is a markup file with <link rel=match|mismatch href=…>; references can have such links too
# (a chain: each step must hold). Siblings are alternatives: the test passes when one path of the graph
# passes, as wptrunner's RefTestImplementation walks it. Discovery follows tools/manifest/sourcefile.py
# with regular expressions instead of html5lib (seconds instead of minutes per area).

MARKUP = ('.html', '.htm', '.xht', '.xhtml', '.xml', '.svg')
REFTEST_TIMEOUT, REFTEST_LONG_TIMEOUT = 10, 60  # wptrunner's RefTest timeouts, seconds
NON_TEST_DIRS = {'resources', 'support', 'tools'}
NON_TEST_PATHS = [('css', 'common'), ('css', 'CSS2', 'archive'), ('css21', 'archive'), ('common',)]
REFTEST_STATUS = {'PASS': 0, 'FAIL': 1, 'TIMEOUT': 2, 'ERROR': 1}
AHEM = os.path.join(WPT, 'fonts', 'Ahem.ttf')

COMMENT_RE = re.compile(r'<!--.*?-->', re.S)
TAG_RE = re.compile(r'<(?:[A-Za-z][\w.-]*:)?(link|meta|script)\b((?:[^>"\']++|"[^"]*+"|\'[^\']*+\'|["\'])*+)>', re.I)
ATTR_RE = re.compile(r'([^\s=/>"\']+)(?:\s*=\s*(?:"([^"]*)"|\'([^\']*)\'|([^\s>]+)))?')
SPACE = ' \t\n\f\r'


def tag_attrs(text):
    """(tag name, {attribute: value}) of each link, meta and script start tag outside comments."""
    for m in TAG_RE.finditer(COMMENT_RE.sub('', text)):
        attrs = {}
        for a in ATTR_RE.finditer(m.group(2)):
            name = a.group(1).lower()
            if name not in attrs:
                value = next((v for v in a.groups()[1:] if v is not None), '')
                attrs[name] = html.unescape(value)
        yield m.group(1).lower(), attrs


def parse_fuzzy(content, url, references):
    """A <meta name=fuzzy> value as the manifest reads it: (key, ((min, max) difference, (min, max) pixels)),
    the key None or (file URL, reference URL, relation)."""
    parts = content.rsplit(':', 1)
    key = None
    if len(parts) == 2:
        ref = urllib.parse.urljoin(url, parts[0].strip())
        rel = next((r for u, r in references if u == ref), None)
        if rel is None:
            raise ValueError(f'fuzzy key {parts[0]} is not a reference')
        key = (url, ref, rel)
    ranges = parts[-1].split(';')
    if len(ranges) != 2:
        raise ValueError(f'malformed fuzzy value {content}')
    named, positional = {}, []
    for item in ranges:
        name = None
        if '=' in item:
            name, item = (p.strip() for p in item.split('=', 1))
            if name not in ('maxDifference', 'totalPixels'):
                raise ValueError(f'{name} is not a fuzzy property')
        low, _, high = item.partition('-')
        value = (int(low.strip()), int((high or low).strip()))
        if name:
            named[name] = value
        else:
            positional.append(value)
    values = []
    for name in ('maxDifference', 'totalPixels'):
        values.append(named.pop(name) if name in named else positional.pop(0))
    return key, tuple(values)


class RefNode:
    """A test or reference file: its references, fuzziness, timeout and viewport, or why it can't be used."""
    __slots__ = ('url', 'references', 'fuzzy', 'long', 'viewport', 'testharness', 'error')

    def __init__(self, url):
        self.url, self.references, self.fuzzy, self.long, self.viewport = url, [], {}, False, None
        self.testharness, self.error = False, None


_nodes = {}


def ref_node(url):
    """The parsed file behind a test or reference URL (path only: query and fragment are variants)."""
    path_url = urllib.parse.urlsplit(url)._replace(query='', fragment='').geturl()
    if path_url in _nodes:
        return _nodes[path_url]
    node = RefNode(path_url)
    _nodes[path_url] = node
    if path_url == 'about:blank':
        return node  # (an empty page: run_reftests draws it)
    split = urllib.parse.urlsplit(path_url)
    path = os.path.join(WPT, *urllib.parse.unquote(split.path).lstrip('/').split('/'))
    if split.scheme or split.netloc or not os.path.isfile(path):
        node.error = f'missing: {path_url}' + ('' if split.netloc else ' (not in the sparse checkout?)')
        return node
    try:
        text = open(path, encoding='utf-8', errors='replace').read()
    except OSError as e:
        node.error = str(e)
        return node
    fuzzy_contents, timeout_seen = [], False
    for tag, attrs in tag_attrs(text):
        if tag == 'link' and attrs.get('rel') in ('match', 'mismatch') and 'href' in attrs:
            ref = urllib.parse.urljoin(path_url, attrs['href'].strip(SPACE))
            node.references.append((ref, '==' if attrs['rel'] == 'match' else '!='))
        elif tag == 'meta':
            name, content = attrs.get('name'), attrs.get('content')
            if name == 'fuzzy' and content is not None:
                fuzzy_contents.append(content)
            elif name == 'timeout' and content is not None and not timeout_seen:
                node.long = content.strip().lower() == 'long'
                timeout_seen = True
            elif name == 'viewport-size' and content and node.viewport is None:
                w, _, h = content.strip().partition('x')
                try:
                    node.viewport = (int(w), int(h))
                except ValueError:
                    pass
        elif tag == 'script' and attrs.get('src') == '/resources/testharness.js':
            node.testharness = True
    if path.endswith(('.html', '.htm')) and '/resources/testharness.js' in text:
        node.testharness = True  # (what tests_in runs as a testharness test)
    try:
        for content in fuzzy_contents:
            key, value = parse_fuzzy(content, path_url, node.references)
            node.fuzzy[key] = value
    except (ValueError, IndexError) as e:
        node.error = f'bad fuzzy metadata: {e}'
    return node


def reftests_in(area):
    """(test URL with its variant, variant) for each reftest in an area, by the manifest's rules."""
    root = os.path.join(WPT, area)
    walk = os.walk(root) if os.path.isdir(root) else [(os.path.dirname(root), [], [os.path.basename(root)])]
    for dirpath, dirnames, filenames in walk:
        dirnames[:] = sorted(d for d in dirnames if d not in NON_TEST_DIRS and not d.startswith('.')
                             and d not in ('crashtests', 'print'))
        rel_dir = tuple(os.path.relpath(dirpath, WPT).replace('\\', '/').split('/'))
        if any(rel_dir[:len(p)] == p for p in NON_TEST_PATHS) or any(d in NON_TEST_DIRS for d in rel_dir):
            continue
        for name in sorted(filenames):
            stem, ext = os.path.splitext(name)
            if ext not in MARKUP or name.startswith('.') or '.https.' in name or '.h2.' in name:
                continue
            flag = stem.rsplit('-', 1)[1].split('.')[0] if '-' in stem else None
            if flag in ('manual', 'visual', 'crash', 'print'):
                continue
            url = '/' + '/'.join(rel_dir + (name,))
            node = ref_node(url)
            if node.error and node.error.startswith('missing'):
                continue
            if node.testharness or not node.references:
                continue
            text = open(os.path.join(dirpath, name), encoding='utf-8', errors='replace').read()
            variants = [a.get('content', '') for t, a in tag_attrs(text) if t == 'meta' and a.get('name') == 'variant'] or ['']
            for v in variants:
                yield url + v, v


def reftest_graph(tests):
    """The screenshots the tests need: {(url, size): timeout seconds}, and each test's size."""
    shots, sizes = {}, {}
    for test, variant in tests:
        root = ref_node(test)
        size = root.viewport or (800, 600)
        sizes[test] = size
        timeout = REFTEST_LONG_TIMEOUT if root.long else REFTEST_TIMEOUT
        seen, todo = set(), [test]
        while todo:
            url = todo.pop()
            if url in seen:
                continue
            seen.add(url)
            key = (url, size)
            shots[key] = max(shots.get(key, 0), timeout)
            todo.extend(ref + variant for ref, _ in ref_node(url).references)
    return shots, sizes


def browser_env(env, **values):
    """Sets the browser's variables under both of its names: SNOWLARK_<name>, which builds since the rename
    read, and OSMIUM_<name>, which builds from before the engine was renamed read (kept baseline binaries). None removes one."""
    for name, value in values.items():
        for prefix in ('SNOWLARK_', 'OSMIUM_'):
            if value is None:
                env.pop(prefix + name, None)
            else:
                env[prefix + name] = value
    return env


def run_reftest_chunk(exe, chunk, results, work, index):
    """Screenshots a list of pages in one Snowlark process, restarting after the page that crashed it.
    chunk: [(key, url, png, timeout)]; results gets {key: result}."""
    env = browser_env(dict(os.environ), HOST_RESOLVER_RULES=HOST_RULES, PROFILE_DIR=os.path.join(work, f'profile-r{index}'))
    shots = os.path.join(work, 'shots')
    remaining = list(chunk)
    while remaining:
        list_path = os.path.join(work, f'list-r{index}.txt')
        with open(list_path, 'w', encoding='utf-8') as f:
            for key, url, png, timeout in remaining:
                size = key[1]
                # (a space in a variant's query would split the list line: css-view-transitions has some)
                f.write(f'{BASE}{url.replace(" ", "%20")} {png} timeout={timeout * 1000}' + ('' if size == (800, 600) else f' size={size[0]}x{size[1]}') + '\n')
        part = os.path.join(work, f'part-r{index}.jsonl')
        if os.path.exists(part):
            os.remove(part)
        cmd = [exe, '--reftest', list_path, shots, part] + (['--font', AHEM] if os.path.exists(AHEM) else [])
        proc = run_watched(cmd, env, part, os.path.join(work, f'stderr-r{index}.txt'),
                           limit=max(600, sum(t for *_, t in remaining) + 5 * len(remaining)))
        stderr = proc.stderr.decode('utf-8', 'replace')
        if proc.hung:
            stderr += f'\n[runner] the process was stopped: {proc.reason}' + (' (and did not end)' if proc.stuck else '')
        if proc.stuck:
            env = browser_env(env, PROFILE_DIR=os.path.join(work, f'profile-r{index}-{len(remaining)}'))
        done = {}
        if os.path.exists(part):
            with open(part, encoding='utf-8') as f:
                for line in f:
                    try:
                        r = json.loads(line)
                    except ValueError:
                        continue
                    done[r['png']] = r
        by_png = {png: key for key, _, png, _ in remaining}
        for png, r in done.items():
            if png in by_png:
                results[by_png[png]] = r
        next_remaining = [c for c in remaining if c[2] not in done]
        started = [u.strip() for u in re.findall(r'^\[start\] (.*)$', stderr, re.M)]
        if next_remaining and started and started[-1] == BASE + next_remaining[0][1]:
            crashed = next_remaining[0]
            tail = stderr.strip().splitlines()[-3:]
            results[crashed[0]] = {'url': BASE + crashed[1], 'png': crashed[2], 'status': 'ERROR', 'wait': None, 'ms': 0,
                                   'message': 'CRASH: ' + ' / '.join(tail)[:300]}
            next_remaining = next_remaining[1:]
        elif next_remaining and not done:
            for c in next_remaining:
                results[c[0]] = {'url': BASE + c[1], 'png': c[2], 'status': 'ERROR', 'wait': None, 'ms': 0,
                                 'message': 'runner: Snowlark did not start: ' + stderr.strip()[-200:]}
            break
        remaining = next_remaining


_image_cache = collections.OrderedDict()


def load_image(path):
    """(pixel hash, RGB image) of a screenshot, the last few kept."""
    from PIL import Image
    if path in _image_cache:
        _image_cache.move_to_end(path)
        return _image_cache[path]
    image = Image.open(path).convert('RGB')
    value = (hashlib.sha1(image.tobytes()).hexdigest(), image)
    _image_cache[path] = value
    if len(_image_cache) > 64:
        _image_cache.popitem(last=False)
    return value


def differences(lhs_path, rhs_path):
    """(maximum channel difference, differing pixels) as wptrunner's get_differences counts them: over
    the pixels whose difference has a nonzero luma (Pillow's RGB→L conversion)."""
    from PIL import ImageChops, ImageStat
    lhs_hash, lhs = load_image(lhs_path)
    rhs_hash, rhs = load_image(rhs_path)
    if lhs_hash == rhs_hash:
        return 0, 0
    diff = ImageChops.difference(lhs, rhs)
    bbox = diff.getbbox()
    if bbox is None:
        return 0, 0
    minimal = diff.crop(bbox)
    mask = minimal.convert('L', dither=None)
    stat = ImageStat.Stat(minimal, mask)
    return max(e[1] for e in stat.extrema), stat.count[0]


def reftest_fuzzy(lhs, rhs, relation):
    """wptrunner's get_fuzzy (without .ini overrides): the left file's value for this comparison, or its
    global one."""
    fuzzy = ref_node(lhs).fuzzy
    lhs_path = urllib.parse.urlsplit(lhs)._replace(query='', fragment='').geturl()
    rhs_path = urllib.parse.urlsplit(rhs)._replace(query='', fragment='').geturl()
    for key in ((lhs_path, rhs_path, relation), None):
        if key in fuzzy:
            return fuzzy[key]
    return None


def check_pass(max_channel, pixels, relation, fuzzy):
    if not fuzzy or fuzzy == ((0, 0), (0, 0)):
        equal = pixels == 0 and max_channel == 0
    else:
        (dmin, dmax), (pmin, pmax) = fuzzy
        equal = ((pixels == 0 and pmin == 0) or (max_channel == 0 and dmin == 0) or
                 (dmin <= max_channel <= dmax and pmin <= pixels <= pmax))
    return equal if relation == '==' else not equal


def evaluate_reftest(test, size, shots, shot_dir):
    """Walks the test's reference graph depth first, as wptrunner does: the test passes when a path
    reaches a reference without references with every comparison on it holding."""
    root = ref_node(test)
    if root.error:
        return {'status': 'ERROR', 'message': root.error}
    variant = test[len(urllib.parse.urlsplit(test)._replace(query='', fragment='').geturl()):]
    stack = [((test, ref + variant), rel) for ref, rel in reversed(root.references)]
    seen, last = set(), None
    while stack:
        (lhs, rhs), relation = stack.pop()
        if (lhs, rhs, relation) in seen:
            continue
        seen.add((lhs, rhs, relation))
        for url in (lhs, rhs):
            node = ref_node(url)
            if node.error:
                return {'status': 'ERROR', 'message': node.error, 'pair': (lhs, relation, rhs)}
            shot = shots.get((url, size))
            if shot is None or shot['status'] != 'OK':
                status = 'TIMEOUT' if shot and shot['status'] == 'TIMEOUT' else 'ERROR'
                message = (shot or {}).get('message') or 'no screenshot'
                if (shot or {}).get('error'):
                    message += '; ' + shot['error']
                return {'status': status, 'message': f'{url}: {message}', 'pair': (lhs, relation, rhs), 'timed_out': url}
        lhs_png = os.path.join(shot_dir, shots[(lhs, size)]['png'])
        rhs_png = os.path.join(shot_dir, shots[(rhs, size)]['png'])
        max_channel, pixels = differences(lhs_png, rhs_png)
        fuzzy = reftest_fuzzy(lhs, rhs, relation)
        last = {'pair': (lhs, relation, rhs), 'max_difference': max_channel, 'pixels': pixels,
                'fuzzy': fuzzy}
        if check_pass(max_channel, pixels, relation, fuzzy):
            node = ref_node(rhs)
            if node.references:
                stack.extend(((rhs, ref + variant), rel) for ref, rel in reversed(node.references))
            else:
                return dict(last, status='PASS', message=None)
    last = last or {}
    pair = last.get('pair')
    message = (f'{pair[0]} {pair[1]} {pair[2]}: {last["pixels"]} pixels differ, by at most {last["max_difference"]}'
               + (f' (fuzzy {last["fuzzy"]})' if last.get('fuzzy') else '')) if pair else 'no references'
    return dict(last, status='FAIL', message=message)


def keep_failure(test, outcome, size, shots, shot_dir, failures_dir):
    """The screenshots of a test that did not pass, and where they differ, for a person to look at."""
    from PIL import Image, ImageChops
    name = re.sub(r'[^\w.-]+', '_', test.strip('/'))
    if len(name) > 100:  # (a long query string would pass the path length limit)
        name = name[:90] + '-' + hashlib.sha1(test.encode()).hexdigest()[:8]
    folder = os.path.join(failures_dir, name)
    os.makedirs(folder, exist_ok=True)
    pair = outcome.get('pair')
    urls = [pair[0], pair[2]] if pair else [test]
    pngs = []
    for url, name in zip(urls, ('test.png', 'ref.png')):
        shot = shots.get((url, size))
        src = shot and os.path.join(shot_dir, shot['png'])
        if src and os.path.exists(src):
            shutil.copyfile(src, os.path.join(folder, name))
            pngs.append(src)
    if len(pngs) == 2 and outcome['status'] == 'FAIL':
        lhs, rhs = load_image(pngs[0])[1], load_image(pngs[1])[1]
        if lhs.size == rhs.size:
            # Differing pixels red over a faded copy of the test.
            mask = ImageChops.difference(lhs, rhs).point(lambda v: 255 if v else 0).convert('L').point(lambda v: 255 if v else 0)
            faded = Image.blend(lhs, Image.new('RGB', lhs.size, 'white'), 0.75)
            faded.paste(Image.new('RGB', lhs.size, (255, 0, 0)), mask=mask)
            faded.save(os.path.join(folder, 'diff.png'))
    with open(os.path.join(folder, 'info.txt'), 'w', encoding='utf-8') as f:
        f.write(f'{outcome["status"]}: {outcome.get("message") or ""}\n')
        if pair:
            f.write(f'test.png: {BASE}{pair[0]}\n{pair[1]}\nref.png: {BASE}{pair[2]}\n')
        for url in urls:
            error = (shots.get((url, size)) or {}).get('error')
            if error:
                f.write(f'{url}: {error}\n')
    return folder


def run_reftests(exe, areas, jobs, work, failures_dir):
    """Discovers, screenshots and compares the areas' reftests; returns their result lines."""
    tests = [t for a in areas for t in reftests_in(a)]
    shots_needed, sizes = reftest_graph(tests)
    print(f'{len(tests)} reftests ({len(shots_needed)} screenshots) in {", ".join(areas)}')
    if not tests:
        return []
    shot_dir = os.path.join(work, 'shots')
    if os.path.isdir(shot_dir):
        shutil.rmtree(shot_dir)
    os.makedirs(shot_dir)
    # (a later run in this process writes the same file names: gate.py's re-runs)
    _image_cache.clear()
    # Pages that cannot be loaded are not sent to the engine.
    entries = [(key, key[0], f's{i}.png', timeout) for i, (key, timeout) in enumerate(sorted(shots_needed.items()))
               if not ref_node(key[0]).error]
    shots = {}
    for key, url, png, _ in [e for e in entries if e[1] == 'about:blank']:
        from PIL import Image
        Image.new('RGB', key[1], 'white').save(os.path.join(shot_dir, png))
        shots[key] = {'url': url, 'png': png, 'status': 'OK', 'wait': None, 'ms': 0, 'message': '', 'error': None}
    entries = [e for e in entries if e[1] != 'about:blank']
    chunks = [entries[i::jobs] for i in range(jobs)]
    threads = [threading.Thread(target=run_reftest_chunk, args=(exe, c, shots, work, i)) for i, c in enumerate(chunks) if c]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    if os.path.isdir(failures_dir):
        shutil.rmtree(failures_dir)
    lines = []
    for test, _ in tests:
        size = sizes[test]
        outcome = evaluate_reftest(test, size, shots, shot_dir)
        status = outcome['status']
        own = shots.get((test, size)) or {}
        if status != 'PASS':
            kept = keep_failure(test, outcome, size, shots, shot_dir, failures_dir)
            # (a gate re-run keeps its failures in the work folder, on another drive)
            outcome['artifacts'] = os.path.relpath(kept, STATE) if os.path.splitdrive(kept)[0].lower() == os.path.splitdrive(STATE)[0].lower() else kept
        harness = {'PASS': 0, 'FAIL': 0, 'TIMEOUT': 2, 'ERROR': 1}[status]
        lines.append({'url': BASE + test, 'ms': own.get('ms', 0), 'kind': 'reftest',
                      'result': {'status': harness, 'message': outcome.get('message'),
                                 'tests': [{'name': 'reftest', 'status': REFTEST_STATUS[status], 'message': outcome.get('message')}]},
                      'reftest': {'status': status, 'wait': own.get('wait'), 'error': own.get('error'),
                                  **{k: outcome[k] for k in ('pair', 'max_difference', 'pixels', 'artifacts') if k in outcome}}})
    shutil.rmtree(shot_dir, ignore_errors=True)
    return lines


def area_of(path, areas):
    """The area (as named on the command line) a test path is in."""
    return next((a for a in sorted(areas, key=len, reverse=True) if path.startswith('/' + a + '/') or path == '/' + a),
                path.split('/')[1])


H2_PORT = 9000


def port_open(port):
    try:
        with socket.create_connection(('127.0.0.1', port), timeout=1):
            return True
    except OSError:
        return False


def server_running():
    return port_open(8000)


def start_server(h2=True):
    """Starts wptserve unless it is running. With `h2` it also serves HTTP/2 (port 9000, for the `.h2.` files);
    if it will not start that way, it is started without and the `.h2.` files fail."""
    if server_running():
        return None
    for with_h2 in ([True, False] if h2 else [False]):
        log = open(os.path.join(STATE, 'results', 'wptserve.log'), 'w')
        proc = subprocess.Popen([sys.executable, 'wpt.py', 'serve', '--config', os.path.join(HERE, 'serve-config.json')]
                                + ([] if with_h2 else ['--no-h2']), cwd=WPT, stdout=log, stderr=subprocess.STDOUT)
        for _ in range(120):
            if server_running():
                if h2 and not with_h2:
                    print('warning: wptserve would not start with HTTP/2: the .h2. files will fail', file=sys.stderr)
                return proc
            if proc.poll() is not None:
                break
            time.sleep(0.5)
        if proc.poll() is None:
            proc.terminate()
    raise SystemExit(f'wptserve did not start; see {os.path.join(STATE, "results", "wptserve.log")}')


def find_browser(explicit):
    if explicit:
        return os.path.abspath(explicit)
    exe = devenv.exe('iter', cli=False)
    if not exe.exists():
        raise SystemExit(f'{exe} not found: build it (.\\dev.ps1 build --profile iter) or pass --snowlark <exe>')
    return str(exe)


def wpt_slot_count():
    """How many WPT runs may hold a slot at once: SNOWLARK_WPT_SLOTS (1 makes every run, gates included, wait
    for the one before it), 2 by default."""
    try:
        return max(1, int(devenv.setting('SNOWLARK_WPT_SLOTS') or 2))
    except ValueError:
        return 2


def acquire_slot(kind='wpt', count=None):
    """One of the machine-wide slots for heavy work (the same scheme dev.ps1 uses for builds and test runs, on
    with SNOWLARK_SLOTS=1): at most `count` (by default wpt_slot_count()) WPT runs at a time, so parallel runs
    don't oversubscribe the machine and time each other's tests out. A slot is a byte lock on a file, released
    when the process ends, however it ends; keep the returned file open. SNOWLARK_NO_SLOTS=1 skips the queue."""
    if not slots_enabled():
        return None
    if count is None:
        count = wpt_slot_count()
    import msvcrt
    directory = lock_dir()
    os.makedirs(directory, exist_ok=True)
    waiting = None
    while True:
        for i in range(1, count + 1):
            f = open(os.path.join(directory, f'{kind}-{i}.lock'), 'a+')
            try:
                msvcrt.locking(f.fileno(), msvcrt.LK_NBLCK, 1)
                if waiting:
                    print(f'got a {kind} slot after {time.time() - waiting:.0f} s', flush=True)
                return f
            except OSError:
                f.close()
        if not waiting:
            waiting = time.time()
            print(f'all {count} {kind} slots are in use; waiting', flush=True)
        time.sleep(15)


def work_root():
    """Where a run's scratch goes: the processes' profile folders (each with its script code cache, written on
    every page load), their per-process outputs and every reftest screenshot. Tens of processes writing those
    at once saturate a spinning disk, so it goes to SNOWLARK_WPT_WORK (default <dev dir>/wpt-work); put it on an
    SSD. The results (jsonl, summary, failure screenshots) go to <state>/results."""
    return devenv.setting('SNOWLARK_WPT_WORK', str(devenv.dev_dir() / 'wpt-work'))


STALL_SECONDS = 300  # a test is stopped by the engine's watchdog after at most its timeout and 20 s: a process silent this long is hung


class Finished:
    """What run_watched returns: the process's stderr, its exit code (None when it could not be stopped), whether
    it was stopped for hanging or for running past its limit, and whether it outlived every attempt to stop it."""
    def __init__(self, stderr, returncode, hung, stuck=False, reason=''):
        self.stderr, self.returncode, self.hung, self.stuck, self.reason = stderr, returncode, hung, stuck, reason


def kill_tree(pid):
    """Ends a process and its children (Windows: taskkill /T /F; elsewhere the process alone)."""
    if os.name == 'nt':
        subprocess.run(['taskkill', '/PID', str(pid), '/T', '/F'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                       timeout=60)
    else:
        try:
            os.kill(pid, 9)
        except OSError:
            pass


def stop_process(proc, grace=30.0, tries=3, kill=kill_tree):
    """Stops `proc` and waits at most `grace` seconds per try for it to end. A process caught in its own exit can
    survive a first kill, and one stuck in the kernel can survive every kill: after `tries` the runner gives up on
    it and goes on, rather than wait on it for ever. Returns whether it ended."""
    for _ in range(tries):
        try:
            kill(proc.pid)
        except (OSError, subprocess.SubprocessError):
            pass
        try:
            proc.kill()  # through the runner's own handle, which keeps every right whatever the tree walk could do
        except OSError:
            pass
        try:
            proc.wait(timeout=grace)
            return True
        except subprocess.TimeoutExpired:
            continue
    print(f'[runner] process {proc.pid} did not end after {tries} attempts to stop it: going on without it', file=sys.stderr)
    return False


def run_watched(cmd, env, progress_path, stderr_path, stall=STALL_SECONDS, poll=2.0, limit=None, stop=stop_process):
    """Runs `cmd`, killing it (and its children) when neither its results file (`progress_path`) nor its stderr
    has grown for `stall` seconds, or when it has run for `limit` seconds, so one deadlocked page costs one test
    and a few minutes, not the chunk. It never waits without bound: a process that cannot be stopped is left
    behind (`stuck`) and its results so far are returned."""
    with open(stderr_path, 'wb') as err:
        proc = subprocess.Popen(cmd, env=env, stdout=subprocess.DEVNULL, stderr=err)
        start = last = time.time()
        seen = (-1, -1)
        hung, stuck, reason = False, False, ''
        while proc.poll() is None:
            time.sleep(poll)
            try:
                now = (os.path.getsize(progress_path) if os.path.exists(progress_path) else 0, os.path.getsize(stderr_path))
            except OSError:
                now = seen
            if now != seen:
                seen, last = now, time.time()
            if time.time() - last > stall:
                reason = f'no output for {stall:g} s'
            elif limit is not None and time.time() - start > limit:
                reason = f'still running after {limit:g} s'
            else:
                continue
            hung = True
            stuck = not stop(proc)
            break
    with open(stderr_path, 'rb') as f:
        return Finished(f.read(), None if stuck else proc.returncode, hung, stuck, reason)


def run_chunk(exe, chunk, out_path, work, index, https=False):
    """Runs a list of tests in one Snowlark process, restarting after the test that crashed it."""
    # A profile of its own (cookies, HSTS), so tests never touch the user's.
    env = browser_env(dict(os.environ), HOST_RESOLVER_RULES=HOST_RULES, PROFILE_DIR=os.path.join(work, f'profile-{index}'))
    # The test CA is trusted only in a --https run, and only through this variable.
    browser_env(env, EXTRA_ROOT_CERTS=WPT_CA if https else None)
    remaining = list(chunk)
    restarts = 0
    while remaining:
        list_path = os.path.join(work, f'list-{index}.txt')
        with open(list_path, 'w', encoding='utf-8') as f:
            f.write('\n'.join(full_url(u) + (' long' if long else '') for u, long in remaining))
        part = os.path.join(work, f'part-{index}.jsonl')
        if os.path.exists(part):
            os.remove(part)
        # (WPT expects Ahem installed, as the reftest runs do: the engine installs the file given with --font;
        # an older binary ignores the extra arguments)
        proc = run_watched([exe, '--wpt', list_path, part] + (['--font', AHEM] if os.path.exists(AHEM) else []), env, part,
                           os.path.join(work, f'stderr-{index}.txt'))
        if proc.stuck:
            # The process left behind may still hold its profile's files: the next one gets a profile of its own.
            restarts += 1
            browser_env(env, PROFILE_DIR=os.path.join(work, f'profile-{index}-{restarts}'))
        done = {}
        if os.path.exists(part):
            with open(part, encoding='utf-8') as f:
                for line in f:
                    try:
                        r = json.loads(line)
                    except ValueError:
                        continue  # (a line cut by a kill)
                    done[r['url']] = r
        started = [u.strip() for u in re.findall(r'^\[start\] (.*)$', proc.stderr.decode('utf-8', 'replace'), re.M)]
        with open(out_path, 'a', encoding='utf-8') as out:
            for r in done.values():
                out.write(json.dumps(r) + '\n')
            next_remaining = [(u, l) for u, l in remaining if full_url(u) not in done]
            if next_remaining and started and started[-1] not in done:
                crashed = started[-1]
                tail = proc.stderr.decode('utf-8', 'replace').strip().splitlines()[-3:]
                label = f'HANG ({proc.reason}, process {"left running" if proc.stuck else "killed"})' if proc.hung else 'CRASH'
                out.write(json.dumps({'url': crashed, 'ms': 0, 'result': {'status': 1, 'message': label + ': ' + ' / '.join(tail)[:300], 'tests': []}}) + '\n')
                next_remaining = [(u, l) for u, l in next_remaining if full_url(u) != crashed]
            elif next_remaining and done:
                # The process died between tests (after finishing one): go on with the rest, and say so.
                tail = proc.stderr.decode('utf-8', 'replace').strip().splitlines()[-2:]
                print(f'[runner] Snowlark exited with {proc.returncode} after {url_path(list(done)[-1])}: {" / ".join(tail)[:200]}', file=sys.stderr)
            elif next_remaining:
                break  # nothing started: give up on this chunk rather than loop
        remaining = next_remaining


def summarise(results_path, summary_path, areas, https=False):
    files = collections.defaultdict(lambda: [0, 0, 0, 0])  # files, harness OK, subtests passed, subtests
    failures = collections.Counter()
    reftests = collections.defaultdict(collections.Counter)  # area: status counts
    with open(results_path, encoding='utf-8') as f:
        for line in f:
            r = json.loads(line)
            path = url_path(r['url'])
            if r.get('kind') == 'reftest':
                reftests[area_of(path, areas)][r['reftest']['status']] += 1
                continue
            area = next((a for a in sorted(areas, key=len, reverse=True) if path.startswith('/' + a + '/')), path.split('/')[1])
            # One level below the area, so big areas are broken down.
            rest = path[len(area) + 2:].split('/')
            key = area + ('/' + rest[0] if len(rest) > 1 else '')
            res = r['result']
            tests = res.get('tests', [])
            stats = files[key]
            stats[0] += 1
            stats[1] += res['status'] == 0
            stats[2] += sum(t['status'] == 0 for t in tests)
            stats[3] += len(tests) if tests else 1  # a file that errored counts as one failed subtest
            for t in tests:
                if t['status'] != 0 and t.get('message'):
                    failures[re.sub(r'\d+', 'N', t['message'])[:110]] += 1
            if res['status'] != 0:
                failures['[harness] ' + (res.get('message') or STATUS.get(res['status'], '?'))[:100]] += 1
    total = [sum(v[i] for v in files.values()) for i in range(4)]
    lines = ['# WPT results', '', f'{total[2]} / {total[3]} subtests pass ({100 * total[2] / max(1, total[3]):.1f}%), '
             f'{total[0]} test files ({total[1]} ran to completion)', '']
    if https:
        lines += ['With `--https`: the `.https.` files ran too (over https), so these totals are not comparable with an '
                  'http-only run.', '']
    lines += ['| Area | Files | Completed | Subtests passed | % |', '|---|---:|---:|---:|---:|']
    for key in sorted(files):
        n, ok, p, t = files[key]
        lines.append(f'| {key} | {n} | {ok} | {p} / {t} | {100 * p / max(1, t):.0f}% |')
    lines += ['', '## Most common failure messages', '']
    for message, count in failures.most_common(40):
        lines.append(f'- {count} × `{message.replace("`", "")}`')
    ref_total = collections.Counter()
    for counts in reftests.values():
        ref_total.update(counts)
    if reftests:
        n = sum(ref_total.values())
        lines += ['', '## Reftests', '', f'{ref_total["PASS"]} / {n} reftests pass ({100 * ref_total["PASS"] / max(1, n):.1f}%)', '',
                  '| Area | Reftests | Pass | Fail | Timeout | Error | % |', '|---|---:|---:|---:|---:|---:|---:|']
        for key in sorted(reftests):
            c = reftests[key]
            t = sum(c.values())
            lines.append(f'| {key} | {t} | {c["PASS"]} | {c["FAIL"]} | {c["TIMEOUT"]} | {c["ERROR"]} | {100 * c["PASS"] / max(1, t):.0f}% |')
    with open(summary_path, 'w', encoding='utf-8') as f:
        f.write('\n'.join(lines) + '\n')
    return total, ref_total


def slots_enabled():
    return bool(devenv.setting('SNOWLARK_SLOTS')) and not os.environ.get('SNOWLARK_NO_SLOTS')


def lock_dir():
    dev = devenv.setting('SNOWLARK_DEV_DIR')
    return os.path.join(dev, 'locks') if dev else os.path.join(tempfile.gettempdir(), 'snowlark-locks')


def slots_held(kind):
    """How many of the machine-wide `kind` slots (lock files in lock_dir) are held right now, by anyone.
    The slots are Windows byte locks: elsewhere none are held."""
    if os.name != 'nt':
        return 0
    import msvcrt
    held = 0
    try:
        names = sorted(n for n in os.listdir(lock_dir()) if n.startswith(kind + '-') and n.endswith('.lock'))
    except OSError:
        return 0
    for n in names:
        try:
            f = open(os.path.join(lock_dir(), n), 'a+')
        except OSError:
            held += 1  # (a holder that opened it without sharing: dev.ps1)
            continue
        try:
            msvcrt.locking(f.fileno(), msvcrt.LK_NBLCK, 1)
            msvcrt.locking(f.fileno(), msvcrt.LK_UNLCK, 1)
        except OSError:
            held += 1
        finally:
            f.close()
    return held


def baseline_times():
    """The newest stored baseline's records, for the split of a plain run; {} when there is none."""
    import gate
    name = gate.latest_baseline()
    if not name:
        return {}
    try:
        return gate.load(gate.resolve_baseline(name)[0])
    except (AssertionError, OSError):
        return {}


def auto_jobs(own_slot):
    """--jobs auto: 16 when no other run holds a WPT slot, else 8, so the machine runs 16 WPT jobs at most
    either way. A run is mostly waiting on timers and timeouts, so builds and test runs beside it do not
    lower it (measured: 16 jobs beside a build cost the fast files 7 %). Without slots: the logical CPUs, at
    most 16."""
    if not slots_enabled():
        return max(1, min(16, os.cpu_count() or 8))
    others = slots_held('wpt') - (1 if own_slot else 0)
    jobs = 16 if others == 0 else 8
    print(f'--jobs auto: {jobs} ({others} other WPT runs)', flush=True)
    return jobs


def jobs_arg(value):
    return value if value == 'auto' else int(value)


def run_once(args, areas, name, jobs, baseline, exe):
    results = os.path.join(STATE, 'results', name + '.jsonl')
    summary = os.path.join(STATE, 'results', name + '-summary.md')
    work = os.path.join(work_root(), 'work-' + name)
    os.makedirs(work, exist_ok=True)
    if os.path.exists(results):
        os.remove(results)
    sidecar = os.path.join(STATE, 'results', name + '.gate.json')
    if os.path.exists(sidecar):
        os.remove(sidecar)  # (a stale one would tell compare that files were skipped)

    tests = [] if args.reftests_only else \
        [t for a in areas for t in tests_in(a, skip_slow=not any(a.startswith(prefix) for prefix in SLOW), https=args.https,
                                            variants=args.variants)]
    import gate
    if baseline is not None:
        total = len(tests)
        tests, skipped = gate.select(tests, baseline[1])
        reasons = collections.Counter(r for _, r in skipped)
        print(f'gate: skipping {len(skipped)} of {total} testharness files that passed no subtest in baseline {baseline[0]} '
              f'({reasons["timeout"]} timed out, {reasons["slow"]} took {gate.SLOW_MS // 1000} s or more, {reasons["failed"]} '
              f'finished or errored sooner); reftests all run')
        with open(sidecar, 'w', encoding='utf-8') as f:
            # `scope`: with --affected, the directories gated; the baseline's files elsewhere are not compared.
            json.dump({'baseline': baseline[0], 'baseline_path': baseline[2], 'slow_ms': gate.SLOW_MS,
                       'skipped': dict(skipped), 'scope': list(areas) if args.affected else None}, f)
    server = start_server(h2=args.variants)
    # What cannot run here is listed, not counted as failing: the `.h2.` files need the HTTP/2 port, which a
    # wptserve started elsewhere (without it) does not serve.
    not_runnable = {}
    if args.variants and not port_open(H2_PORT):
        h2 = [t[0] for t in tests if '.h2.' in t[0].split('?')[0]]
        if h2:
            tests = [t for t in tests if t[0] not in set(h2)]
            not_runnable['no HTTP/2 server: the wptserve on port 8000 was started without it (port 9000 is closed)'] = h2
            print(f'not run: {len(h2)} .h2. files (no HTTP/2 server on port {H2_PORT})')
    with open(os.path.join(STATE, 'results', name + '.notrunnable.json'), 'w', encoding='utf-8') as f:
        json.dump(not_runnable, f, indent=1)
    print(f'{len(tests)} test files in {", ".join(areas[:6])}' + (f' and {len(areas) - 6} more' if len(areas) > 6 else ''))
    print('using', exe)
    reftests = not args.no_reftests and 'net10.0' not in exe
    if not reftests and not args.no_reftests:
        print('reftests skipped: this build has no --reftest mode')
    # Each process gets files adding up to the same time by the baseline (the gate's, or for a plain run the
    # newest stored one), longest first, so the processes end together (gate.split_by_time).
    chunks = gate.split_by_time(tests, baseline[1] if baseline is not None else baseline_times(), jobs)
    started = time.time()
    threads = []
    for i, chunk in enumerate(chunks):
        out = os.path.join(work, f'out-{i}.jsonl')
        if os.path.exists(out):
            os.remove(out)
        t = threading.Thread(target=run_chunk, args=(exe, chunk, out, work, i, args.https or args.variants))
        t.start()
        threads.append((t, out))
    for t, _ in threads:
        t.join()
    with open(results, 'w', encoding='utf-8') as f:
        for _, out in threads:
            if os.path.exists(out):
                f.write(open(out, encoding='utf-8').read())
    harness_seconds = time.time() - started
    if reftests:
        ref_started = time.time()
        lines = run_reftests(exe, areas, jobs, work, os.path.join(STATE, 'results', name + '-reftests'))
        with open(results, 'a', encoding='utf-8') as f:
            for line in lines:
                f.write(json.dumps(line) + '\n')
        print(f'reftests: {time.time() - ref_started:.0f} s')
    total, ref_total = summarise(results, summary, areas, args.https and not args.variants)
    if tests:
        print(f'{total[2]} / {total[3]} subtests pass ({100 * total[2] / max(1, total[3]):.1f}%) in {total[0]} files, '
              f'{harness_seconds:.0f} s')
    if ref_total:
        n = sum(ref_total.values())
        print(f'{ref_total["PASS"]} / {n} reftests pass ({100 * ref_total["PASS"] / max(1, n):.1f}%): '
              f'{ref_total["FAIL"]} fail, {ref_total["TIMEOUT"]} time out, {ref_total["ERROR"]} errors')
    print(f'summary: {summary}')
    if server:
        server.terminate()
    return results


def check_disk_space(min_gb=5.0):
    """Stops the run, naming the disk, when a drive it writes to is nearly full. Without this a full disk shows
    up as hundreds of failed or crashed tests (Errno 28 / os error 112) that look like regressions."""
    seen = set()
    for path in (STATE, tempfile.gettempdir(), work_root()):
        if not os.path.exists(path):
            continue
        drive = os.path.splitdrive(os.path.abspath(path))[0].upper()
        if drive in seen:
            continue
        seen.add(drive)
        free = shutil.disk_usage(path).free / 2**30
        if free < min_gb:
            print(f'ERROR: DISK FULL, not a test failure: {free:.1f} GB free on {drive or path} (need {min_gb:g} GB). '
                  'Free space and run again; nothing was run.', file=sys.stderr, flush=True)
            sys.exit(3)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('areas', nargs='*')
    parser.add_argument('--all', action='store_true')
    # Most of a run is tests waiting for their harness timeout, not CPU, so twice as many processes as cores.
    # `auto`: 16 when no other WPT run is going, else 8.
    parser.add_argument('--jobs', type=jobs_arg, default=max(1, (os.cpu_count() or 2) * 2))
    parser.add_argument('--name', default=None, help='results file name (default: the areas)')
    parser.add_argument('--snowlark', '--osmium', dest='snowlark', metavar='EXE', default=None,
                        help='the browser to test (snowlark.exe; default target/iter/snowlark.exe); --osmium is an old name of this option')
    parser.add_argument('--no-reftests', action='store_true', help='testharness tests only')
    parser.add_argument('--reftests-only', action='store_true', help='reftests only')
    parser.add_argument('--https', action='store_true',
                        help='also run the .https. files, over https (results named <name>-https)')
    parser.add_argument('--variants', action='store_true',
                        help='also the files that need more of the server or the browser: .https. over https, .h2. over HTTP/2, and '
                             'every global of a .any.js file (shared and service workers, module workers, ShadowRealm); '
                             'implied by --all and --gate')
    parser.add_argument('--affected', action='store_true',
                        help='--gate: only the directories the branch can change (tools/wpt/areas-by-crate.json; affected.py says '
                             'which and why); the full gate stays the backstop')
    parser.add_argument('--base', default='origin/main', help='--affected: what the branch is compared with (its merge base with it)')
    parser.add_argument('--changed', nargs='+', metavar='PATH', default=None,
                        help='--affected: these repository paths instead of the diff of the branch (to see or time a gate)')
    parser.add_argument('--gate', action='store_true',
                        help='the merge check: the areas (default all) minus testharness files that passed nothing in the '
                             'baseline; then compares with it (gate.py compare)')
    parser.add_argument('--baseline', default=None,
                        help='for --gate: a stored baseline name (Tests/wpt/baselines/) or a result file (default: the newest stored)')
    parser.add_argument('--save-baseline', default=None, metavar='NAME',
                        help='after the run, store its results as baseline NAME (with commit, date and binary)')
    parser.add_argument('--commit', default=None, help='commit id recorded by --save-baseline (default: HEAD of the current directory)')
    parser.add_argument('--repeat', type=int, default=1, metavar='N',
                        help='run N times (results <name>-r1.. ) and add what differed to flaky.json')
    parser.add_argument('--recheck', type=int, default=3, metavar='N',
                        help='--gate: re-run each file with a loss alone N times before calling it real (0 = off)')
    parser.add_argument('--no-learn-flaky', action='store_true',
                        help='--gate: do not add the flakes the re-check proves to tools/wpt/flaky.json')
    parser.add_argument('--base-exe', default=None, help='--gate: the baseline binary for the re-runs (default: the one the baseline records)')
    parser.add_argument('--no-compare', action='store_true', help='--gate: only run, do not compare')
    parser.add_argument('--allow-set-diff', action='store_true', help='--gate: a changed test set does not fail the comparison')
    args = parser.parse_args()
    sys.path.insert(0, HERE)
    areas = all_areas() if (args.all or (args.gate and not args.areas)) else args.areas
    if not areas:
        parser.error('name some WPT directories, or --all')
    # The whole-suite runs are the baselines' kind: everything the manifest lists. A run of named directories
    # stays what it was (http, a window and a dedicated worker) unless it asks for more.
    args.variants = args.variants or args.all or args.gate
    if args.affected:
        if not (args.gate and not args.areas):
            parser.error('--affected goes with --gate and no named directories')
        import affected
        every = list(areas)
        areas, why = affected.plan(args.changed if args.changed is not None else affected.changed_files(args.base), every)
        affected.describe(areas, why, every)
        if not areas:
            print('gate: nothing the branch changes can alter a page: nothing to run')
            return
    if args.repeat < 1:
        parser.error('--repeat needs at least 1')
    os.makedirs(os.path.join(STATE, 'results'), exist_ok=True)
    os.makedirs(work_root(), exist_ok=True)
    name = args.name or ('all' if args.all else 'gate' if args.gate and not args.areas else '_'.join(a.replace('/', '-') for a in areas))
    if args.affected and not args.name:
        name = 'gate-affected'
    if args.gate and not args.name:
        # Two gates started without --name would share one results file and one work folder and spoil
        # each other's runs; the process id keeps them apart.
        name += f'-{os.getpid()}'
    if args.https and not args.variants and not name.endswith('-https'):
        name += '-https'  # never compared with an http-only run of the same name
    baseline = None
    sys.path.insert(0, HERE)
    if args.gate:
        import gate
        chosen = args.baseline or gate.latest_baseline()
        if not chosen:
            parser.error('--gate needs a baseline: --baseline <name>, or store one with --save-baseline <name>')
        path, meta = gate.resolve_baseline(chosen)
        baseline = (chosen, gate.load(path), os.path.abspath(path))
        print(f'gate: baseline {chosen}' + (f' (commit {meta.get("commit")}, {meta.get("date")})' if meta else '') + f', {len(baseline[1])} files')
    check_disk_space()
    slot = acquire_slot()  # held until the process ends
    jobs = auto_jobs(slot) if args.jobs == 'auto' else args.jobs
    exe = find_browser(args.snowlark)
    # (a gate learns flakes only when no other build, test run or WPT run held a slot during its run and re-check)
    watch = gate.LoadWatch(own_wpt=1 if slot else 0).start() if baseline is not None and not args.no_compare else None
    names = [name] if args.repeat == 1 else [f'{name}-r{i}' for i in range(1, args.repeat + 1)]
    paths = []
    for n in names:
        paths.append(run_once(args, areas, n, jobs, baseline, exe))
    if args.repeat > 1:
        import gate
        flaky, data = gate.build_flaky(paths)
        print(f'flaky.json: {sum(len(v) for v in flaky["subtests"].values())} subtests in {len(flaky["subtests"])} files and '
              f'{len(flaky["reftests"])} reftests differed across the {args.repeat} runs; the list now holds '
              f'{sum(len(v) for v in data["subtests"].values())} subtests and {len(data["reftests"])} reftests')
    if args.save_baseline:
        import gate
        meta = gate.save_baseline(args.save_baseline, paths[-1], exe, args.commit)
        side = os.path.splitext(paths[-1])[0] + '.notrunnable.json'
        if os.path.isfile(side):  # (what the run could not start travels with the baseline: parity.py reads it)
            shutil.copyfile(side, os.path.join(gate.BASELINES, args.save_baseline + '.notrunnable.json'))
        print(f'baseline {meta["name"]} saved: {meta["files"]} files, commit {meta["commit"]}, binary {meta["binary"] and meta["binary"]["size"]} bytes')
    if baseline is not None and not args.no_compare:
        status = gate.run_compare(baseline[0], paths[-1], recheck_n=args.recheck, exe=exe, base_exe=args.base_exe,
                                  allow_set_diff=args.allow_set_diff, have_slot=True,
                                  learn=not args.no_learn_flaky, watch=watch)
        sys.exit(status)


if __name__ == '__main__':
    try:
        main()
    except OSError as e:
        if e.errno == 28 or getattr(e, 'winerror', None) == 112:
            print(f'ERROR: DISK FULL, not a test failure: {e}', file=sys.stderr, flush=True)
            sys.exit(3)
        raise
