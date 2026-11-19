# Missing-API census

Finds the web APIs whose absence breaks pages in Snowlark, and ranks them by how many real sites use
them, before users report blank or broken pages. It tells contributors which APIs to implement next; it does
not implement anything.

## Run

From the repository root, with a release build (`.\dev.ps1 build --release`) and Chrome installed:

```powershell
python tools/census/run.py --out <dir>
```

That runs every stage and writes `tools/census/report.md` (pass `--report <file>` to write it
elsewhere). Standard library only, Python 3.13. Options:

| option | default | |
|---|---|---|
| `--out <dir>` | (required) | everything the stages produce; a later run reuses what a stage left out there |
| `--stages` | `surface,fetch,runtime` | which stages run; the report is always rebuilt from `<dir>` |
| `--sites a,b` | all | a corpus subset, by URL or site name (`facebook.com`) |
| `--snowlark` | `target/release/snowlark-cli.exe` | |
| `--chrome` | `C:\Program Files\Google\Chrome\Application\chrome.exe` | |
| `--corpus` | `tools/census/corpus.txt` | one URL per line |
| `--ignore` | `tools/census/ignore.txt` | Chrome members not to count as missing |
| `--jobs` | 4 | Snowlark renders at once |
| `--timeout` | 60 | seconds per Snowlark render (a page that has not settled by then is captured as it is; a process still running a minute later is stopped) |
| `--fetch-timeout` | 45 | seconds per Chrome page load |

A full run of the 25-site corpus takes about 7.5 minutes: surface a few seconds, Chrome fetch about
3.5 minutes, Snowlark renders about 2 minutes with 4 jobs (most of it waiting out a page that hangs),
usage counting about a minute. The tests: `python -m unittest discover tools/census`.

## Stages

1. **surface**: Chrome (headless, DevTools protocol) and Snowlark (`--render`) load `surface.html` from
   a local server on 127.0.0.1 (a secure context, like the sites). `surface.js` enumerates the
   window and a dedicated (blob) worker: every global, every interface object's static and prototype
   members, namespaces (`CSS`, `console`, ...), the global's own and inherited members, and the own
   members of `document`, `location`, `navigator`, `screen`; plus the value type of every getter of
   the global and those instances, to catch stubs. Output `surface-chrome.json`,
   `surface-snowlark.json`, and their diff `diff.json`.
2. **fetch**: each corpus site in a fresh Chrome tab (desktop browser identification, 1280x800) until its load
   event and two quiet seconds. Every script and HTML document it, its workers and its
   out-of-process frames receive is saved under `<dir>/sites/<site>/` (`index.json` lists them, with
   the exceptions Chrome itself threw), with Chrome's screenshot `chrome.png`.
3. **runtime**: each site through `snowlark-cli --render <url> <png> 1280` with content blocking off,
   `SNOWLARK_CONSOLE=tagged` and `SNOWLARK_RENDER_TIMEOUT_MS`; `snowlark.log`, `snowlark.png` and the
   parsed `snowlark.json` per site.
4. **report**: token usage of every Chrome member across the saved scripts (`usage.json`), then
   `report.md`.

## Reading the report

Keys name members the way JavaScript reaches them: `ReadableStream` (an interface object; when it
is missing its members are not listed separately, the count in parentheses says how many),
`Promise.try` (static), `Navigator.prototype.getBattery`, `window.requestIdleCallback` (a member of
the global, wherever on its prototype chain it lives), `document.location` (an own member of an
instance), `CSS.highlights` (a namespace's member); `self.` for the worker global.

- **Table 1** ranks what Snowlark lacks by the number of sites whose scripts mention the name. The
  match is crude by design: an interface counts when its name appears as an identifier, a
  prototype or instance member when its name follows a dot or is a quoted string (`'x' in obj`), a
  global when its name appears at all, a static or namespace member when `Name.member` appears.
  A mention is not a call on the page's path: treat the counts as a ranking, and the **runtime**
  column (sites where Snowlark actually threw naming it, or, for a missing interface, naming one of
  its members read off undefined) as proof.
- **1b** holds missing members whose name Snowlark has on another interface (`remove` on one
  interface but not another): their counts cannot tell the interfaces apart.
- **1c** are ECMAScript built-ins Chrome's newer V8 has; they come with a V8 upgrade, not bindings.
- **1d** are stubs: members Snowlark has but whose getter returns undefined or null where Chrome's
  returns an object.
- **1e** checks the census against past breakage (ReadableStream on linear.app, `window.screen` on
  reddit): the rank each would have had if it were still missing.
- **Table 2** is per site: ink (the share of the first viewport's pixels that differ from its
  dominant colour) for Snowlark and for Chrome's screenshot; BLANK when Snowlark draws next to nothing
  (under 0.05%, or under 1% where Chrome draws over 5%), sparse when Snowlark's ink is under 40% of
  Chrome's (whole parts missing, though a consent dialog or a bot check served to only one of them
  does it too: look at `snowlark.png` and `chrome.png` side by side); the settle time or that the
  page never settled (still had tasks, timers or fetches at the timeout) or hung (the process had to
  be stopped), uncaught exceptions with where they were thrown, other console errors, the names in
  ReferenceError/TypeError messages (for "Cannot read properties of undefined (reading 'x')" that is
  the property read, so the missing thing is the object before it), and how many exceptions
  Chrome threw on the same page (an exception Chrome also throws is the site's, not Snowlark's).
- **Table 3** lists members only Snowlark has. Each is a bug or a deliberate non-standard addition:
  wrong placement (WebIDL puts `[LegacyUnforgeable]` members such as `Location`'s on the instance),
  exposure in the wrong global (`[Exposed=Window]` members in workers), IDL newer than Chrome ships,
  or members that should not be exposed in this context (`SharedArrayBuffer` without cross-origin
  isolation). Kind differences (`accessor` in Chrome, `getter` in Snowlark: a missing setter, often
  `[Replaceable]` or `[PutForwards]`) follow.

The ignore file holds Chrome-only prefixed members and other things Snowlark should not add; keep it
small.

## The engine hook

`SNOWLARK_CONSOLE=tagged` (in `crates/snowlark/src/cli/render.rs`) prints one
`[console.<level>] <text> (<url>:<line>:<column>)` line per console message through the developer
tools' console sink: console API calls with their caller's position, uncaught exceptions and
unhandled rejections as errors with where they were thrown, line breaks escaped. `SNOWLARK_CONSOLE=1`
is unchanged.

## Not covered

Shared and service workers, worklets, iframes' own surfaces, and behaviour behind a present member
(a method that exists but does nothing is invisible here unless it throws). Sites' scripts are as
Chrome received them for a logged-out visitor in this region; sites that serve a different bundle
per engine are counted with Chrome's.
