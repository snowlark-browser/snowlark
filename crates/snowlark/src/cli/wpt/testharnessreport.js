// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Snowlark's testharnessreport.js: the WPT runner serves this in place of /resources/testharnessreport.js.
// It turns off the results table (as wptrunner does) and keeps the results where the runner reads them.
(function () {
  try {
    setup({ output: false });
  } catch (e) {}
  add_completion_callback(function (tests, status) {
    globalThis.__snowlarkWpt = JSON.stringify({
      status: status.status,
      message: status.message || null,
      tests: tests.map(function (t) {
        return { name: t.name, status: t.status, message: t.message || null };
      }),
    });
  });
})();
