// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The micro benchmark runner: one page times one operation.
//
//   <script src="../resources/runner.js"></script>
//   <script>
//   MicroBench.run({
//     setup() { ... },        // once, untimed (build the tree the test works on)
//     run() { ... },          // one unit of work; must leave the page as it found it
//     check() { return ok; }, // optional, after the rounds: a test that stopped doing its work fails
//   });
//   </script>
//
// After the load event it runs `run` in rounds: each round calls it back to back until at least
// `roundMs` (default 50) have passed and records the mean time per call; the first `warmup` rounds
// (default 2) are dropped, then `rounds` (default 10) are kept. Between rounds it yields to the event
// loop (a zero-delay timer), so the browser may run a rendering update there, untimed. The result,
// JSON `{"unit":"ms","values":[ms per run, one per round],"runs":n,"counts":[runs, one per round]}` or
// `{"error":"..."}`, goes to `globalThis.__microBenchResult` (read by tools/micro-bench/run.py in both
// browsers) and to the page. `globalThis.__microBenchRound` names the round running (its index, as a
// string): `snowlark --bench --cycles` charges each round the CPU cycles of the event loop turn it ran in.
// URL parameters override the defaults: `?rounds=3&warmup=1&roundMs=20`.
(function () {
  'use strict';
  const params = new URLSearchParams(location.search);
  const num = (name, d) => (params.has(name) ? Number(params.get(name)) : d);
  const ROUNDS = num('rounds', 10);
  const WARMUP = num('warmup', 2);
  const ROUND_MS = num('roundMs', 50);

  function finish(result) {
    const json = JSON.stringify(result);
    globalThis.__microBenchResult = json;
    const pre = document.createElement('pre');
    pre.id = 'micro-bench-result';
    pre.textContent = json;
    document.documentElement.appendChild(pre);
  }

  const yieldToLoop = () => new Promise((resolve) => setTimeout(resolve, 0));

  async function go(test) {
    try {
      if (test.setup) await test.setup();
      const values = [];
      const counts = [];
      let runs = 0;
      for (let r = 0; r < WARMUP + ROUNDS; r++) {
        let n = 0;
        globalThis.__microBenchRound = String(r);
        const start = performance.now();
        let now;
        do {
          test.run();
          n++;
          now = performance.now();
        } while (now - start < ROUND_MS);
        if (r >= WARMUP) {
          values.push((now - start) / n);
          counts.push(n);
          runs += n;
        }
        await yieldToLoop();
      }
      if (test.check && !test.check()) throw new Error('check() failed after the rounds');
      finish({ unit: 'ms', values, runs, counts });
    } catch (e) {
      finish({ error: String((e && e.stack) || e) });
    }
  }

  globalThis.MicroBench = {
    run(test) {
      if (document.readyState === 'complete') go(test);
      else addEventListener('load', () => go(test), { once: true });
    },
    // Small helpers shared by the pages.
    repeat(n, f) {
      let s = '';
      for (let i = 0; i < n; i++) s += f(i);
      return s;
    },
    // A forced style and layout read.
    flush(el) {
      return (el || document.body).offsetHeight;
    },
  };
})();
