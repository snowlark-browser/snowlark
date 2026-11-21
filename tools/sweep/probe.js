// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The support probe: fetches the items from the sweep's server, asks the engine about each one and posts
// the answers back, in the items' order. Only standard API: CSS.supports, matchMedia, CSSOM.
(async function () {
  var engine = new URLSearchParams(location.search).get('engine') || 'unknown';
  var items = await (await fetch('/items.json?engine=' + engine, { cache: 'no-store' })).json();
  var out = { engine: engine, ua: navigator.userAgent, decl: [], sel: [], media: [], rule: [], prop: [], props: [] };

  function supports(a, b) {
    try {
      return (b === undefined ? CSS.supports(a) : CSS.supports(a, b)) ? 1 : 0;
    } catch (e) {
      return 0;
    }
  }

  // Every property this engine names, longhands and shorthands, as dashed names.
  function ownProperties() {
    var names = {};
    var add = function (n) {
      if (n && /^-?[a-z][a-z0-9-]*$/.test(n)) names[n] = 1;
    };
    try {
      var cs = getComputedStyle(document.documentElement);
      for (var i = 0; i < cs.length; i++) add(cs[i]);
    } catch (e) {}
    var st = document.documentElement.style;
    for (var k in st) {
      if (typeof k !== 'string' || /^\d/.test(k)) continue;
      if (k.indexOf('-') >= 0) { add(k); continue; }
      var d = k.replace(/[A-Z]/g, function (m) { return '-' + m.toLowerCase(); });
      if (/^(webkit|moz|ms)-/.test(d)) d = '-' + d;
      add(d);
    }
    add('float');
    return Object.keys(names).filter(function (n) { return supports(n, 'inherit'); }).sort();
  }

  for (var i = 0; i < items.decl.length; i++) out.decl.push(supports(items.decl[i][0], items.decl[i][1]));
  for (i = 0; i < items.sel.length; i++) out.sel.push(supports('selector(' + items.sel[i] + ')'));
  for (i = 0; i < items.media.length; i++) {
    var m = '';
    try { m = matchMedia(items.media[i]).media; } catch (e) { m = 'error'; }
    out.media.push(m === 'not all' || m === 'error' ? 0 : 1);
  }
  var style = document.createElement('style');
  document.head.appendChild(style);
  for (i = 0; i < items.rule.length; i++) {
    var res = [0, '', -1];
    try {
      style.textContent = items.rule[i];
      var rules = style.sheet.cssRules;
      res[0] = rules.length;
      if (rules.length) {
        res[1] = rules[0].constructor.name;
        if (rules[0].cssRules) res[2] = rules[0].cssRules.length;
      }
    } catch (e) {}
    out.rule.push(res);
  }
  style.remove();
  for (i = 0; i < items.prop.length; i++) out.prop.push(supports(items.prop[i], 'inherit'));
  out.props = ownProperties();

  await fetch('/result?engine=' + engine, { method: 'POST', body: JSON.stringify(out) });
  document.title = 'done';
})().catch(function (e) {
  fetch('/result?engine=' + (new URLSearchParams(location.search).get('engine') || 'unknown'),
        { method: 'POST', body: JSON.stringify({ error: String(e && e.stack || e) }) });
});
