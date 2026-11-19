// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// ns per access of DOM getters on 4,000 elements whose wrappers already exist: what a binding call costs
// around the engine call. Run by getters.ps1 in snowlark-cli --eval and headless Chrome. No double quotes
// (PowerShell 5.1 strips them from a native program's argument).
(() => {
  const d = document.createElement('div');
  for (let i = 0; i < 4000; i++) d.appendChild(document.createElement('span'));
  document.body.appendChild(d);
  const arr = [];
  for (let c = d.firstChild; c; c = c.nextSibling) arr.push(c);
  const n = arr.length;
  const out = {};
  let sink = 0;
  function t(name, f) {
    for (let w = 0; w < 20; w++) sink += f();
    const reps = 400;
    const t0 = performance.now();
    for (let k = 0; k < reps; k++) sink += f();
    const ms = performance.now() - t0;
    out[name] = Math.round((ms * 1e6) / (reps * n) * 100) / 100;
  }
  t('loop', () => { let s = 0; for (let i = 0; i < n; i++) s += arr[i] ? 1 : 0; return s; });
  t('nextSibling', () => { let s = 0; for (let i = 0; i < n; i++) s += arr[i].nextSibling ? 1 : 0; return s; });
  t('parentNode', () => { let s = 0; for (let i = 0; i < n; i++) s += arr[i].parentNode ? 1 : 0; return s; });
  t('firstChild_null', () => { let s = 0; for (let i = 0; i < n; i++) s += arr[i].firstChild ? 1 : 0; return s; });
  t('nodeType_fast', () => { let s = 0; for (let i = 0; i < n; i++) s += arr[i].nodeType; return s; });
  t('nodeName', () => { let s = 0; for (let i = 0; i < n; i++) s += arr[i].nodeName.length; return s; });
  t('walk', () => { let s = 0; for (let c = d.firstChild; c; c = c.nextSibling) s++; return s; });
  // Operations: one argument converted, the engine call, the result.
  for (let i = 0; i < n; i++) arr[i].setAttribute('title', 't');
  t('contains', () => { let s = 0; for (let i = 0; i < n; i++) s += d.contains(arr[i]) ? 1 : 0; return s; });
  t('hasChildNodes', () => { let s = 0; for (let i = 0; i < n; i++) s += arr[i].hasChildNodes() ? 1 : 0; return s; });
  t('getAttribute', () => { let s = 0; for (let i = 0; i < n; i++) s += arr[i].getAttribute('title').length; return s; });
  t('getAttribute_null', () => { let s = 0; for (let i = 0; i < n; i++) s += arr[i].getAttribute('lang') ? 1 : 0; return s; });
  t('hasAttribute', () => { let s = 0; for (let i = 0; i < n; i++) s += arr[i].hasAttribute('title') ? 1 : 0; return s; });
  t('setAttribute', () => { for (let i = 0; i < n; i++) arr[i].setAttribute('title', 't'); return 1; });
  const kids = d.childNodes;
  t('item', () => { let s = 0; for (let i = 0; i < n; i++) s += kids.item(i) ? 1 : 0; return s; });
  t('appendChild', () => { for (let i = 0; i < n; i++) d.appendChild(arr[i]); return 1; });
  t('insertBefore', () => { for (let i = 0; i < n; i++) d.insertBefore(arr[i], null); return 1; });
  out.sink = sink > 0 ? 1 : 0;
  return JSON.stringify(out);
})()
