// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Layout snapshot of the current page, evaluated in the same form in Chrome (CDP Runtime.evaluate) and in
// Snowlark (SNOWLARK_EVAL). Walks the flat tree (open shadow roots, slots' assigned nodes) in document order
// and returns a JSON string: page metrics plus one record per element:
//   pa  index of the parent record (-1 for the root)
//   p   path: tag[nth-of-type] chain from <html>, "#shadow" marking a step into a shadow root
//   t, id, c   tag, id, class list
//   r   getBoundingClientRect() as [x, y, width, height]
//   s   computed values of PROPS, in that order
//   tx, tl   hash and length of the element's own text (its direct text nodes, whitespace collapsed)
//   th, tn   height of the union of those text nodes' client rects, and their number of distinct lines
// No API beyond DOM, CSSOM and Range is used, and failures of individual calls are recorded as empty
// values rather than aborting the walk, so an engine missing a feature still yields a snapshot.
(function () {
  var MAX = __MAX_NODES__;
  var PROPS = [
    'display', 'position', 'float', 'box-sizing',
    'width', 'height', 'min-width', 'min-height', 'max-width', 'max-height',
    'margin-top', 'margin-right', 'margin-bottom', 'margin-left',
    'padding-top', 'padding-right', 'padding-bottom', 'padding-left',
    'border-top-width', 'border-right-width', 'border-bottom-width', 'border-left-width',
    'top', 'right', 'bottom', 'left',
    'font-family', 'font-size', 'font-weight', 'font-style', 'line-height', 'letter-spacing',
    'white-space', 'text-align', 'vertical-align',
    'aspect-ratio', 'object-fit', 'object-position',
    'overflow-x', 'overflow-y',
    'flex-direction', 'flex-wrap', 'flex-grow', 'flex-shrink', 'flex-basis',
    'justify-content', 'align-items', 'align-self', 'align-content', 'row-gap', 'column-gap',
    'grid-template-columns', 'grid-template-rows',
    'grid-column-start', 'grid-column-end', 'grid-row-start', 'grid-row-end',
    'border-top-left-radius', 'transform', 'visibility', 'contain', 'scrollbar-gutter', 'container-type',
    'color', 'background-color', 'color-scheme'
  ];
  var DISPLAY = 0;
  var SKIP = { head: 1, script: 1, style: 1, template: 1, meta: 1, link: 1, title: 1, base: 1 };
  // Recorded, but their insides are not walked.
  var LEAF = { svg: 1, math: 1, iframe: 1, object: 1, embed: 1, canvas: 1, video: 1, audio: 1, select: 1 };

  function fnv(s) {
    var h = 0x811c9dc5;
    for (var i = 0; i < s.length; i++) {
      h ^= s.charCodeAt(i);
      h = Math.imul(h, 16777619);
    }
    return (h >>> 0).toString(16);
  }
  function round(v) {
    return Math.round(v * 100) / 100;
  }
  function childList(el) {
    if (el.localName === 'slot' && typeof el.assignedNodes === 'function') {
      try {
        var a = el.assignedNodes();
        if (a && a.length) return a;
      } catch (e) {}
    }
    var root = null;
    try { root = el.shadowRoot; } catch (e) {}
    return root ? root.childNodes : el.childNodes;
  }
  function textBox(nodes) {
    var top = Infinity, bottom = -Infinity, lines = {}, n = 0;
    try {
      var range = document.createRange();
      for (var i = 0; i < nodes.length; i++) {
        range.selectNodeContents(nodes[i]);
        var rects = range.getClientRects();
        for (var j = 0; j < rects.length; j++) {
          var q = rects[j];
          if (q.width <= 0 && q.height <= 0) continue;
          if (q.top < top) top = q.top;
          if (q.bottom > bottom) bottom = q.bottom;
          var key = Math.round(q.top);
          if (!lines[key]) { lines[key] = 1; n++; }
        }
      }
    } catch (e) {
      return null;
    }
    return bottom >= top ? [round(bottom - top), n] : null;
  }
  function record(el, pa, path, out) {
    var rec = { pa: pa, p: path, t: el.localName, id: '', c: '', r: [0, 0, 0, 0], s: [] };
    try { rec.id = el.id || ''; } catch (e) {}
    try { rec.c = (el.getAttribute('class') || '').trim().replace(/\s+/g, ' '); } catch (e) {}
    try {
      var r = el.getBoundingClientRect();
      rec.r = [round(r.left), round(r.top), round(r.width), round(r.height)];
    } catch (e) {}
    var cs = null;
    try { cs = getComputedStyle(el); } catch (e) {}
    for (var i = 0; i < PROPS.length; i++) {
      var v = '';
      if (cs) {
        try { v = cs.getPropertyValue(PROPS[i]); } catch (e) {}
      }
      rec.s.push(v == null ? '' : String(v));
    }
    out.push(rec);
    return rec;
  }

  var started = Date.now();
  var out = [];
  var truncated = false;
  // Pre-order walk with an explicit stack (deep trees do not overflow the JS stack).
  var stack = [[document.documentElement, -1, '/html[1]']];
  while (stack.length) {
    var item = stack.pop();
    if (out.length >= MAX) { truncated = true; break; }
    var el = item[0];
    var idx = out.length;
    var rec = record(el, item[1], item[2], out);
    if (rec.s[DISPLAY] === 'none' || LEAF[rec.t]) continue;
    var list = childList(el);
    var shadow = false;
    try { shadow = !!el.shadowRoot && el.localName !== 'slot'; } catch (e) {}
    var text = '', textNodes = [], kids = [], counts = {};
    for (var i = 0; i < list.length; i++) {
      var n = list[i];
      if (n.nodeType === 3) {
        text += n.data;
        textNodes.push(n);
      } else if (n.nodeType === 1 && !SKIP[n.localName]) {
        var tag = n.localName;
        counts[tag] = (counts[tag] || 0) + 1;
        kids.push([n, idx, item[2] + (shadow ? '/#shadow' : '') + '/' + tag + '[' + counts[tag] + ']']);
      }
    }
    text = text.replace(/\s+/g, ' ').trim();
    if (text) {
      rec.tx = fnv(text);
      rec.tl = text.length;
      var tb = textBox(textNodes);
      if (tb) { rec.th = tb[0]; rec.tn = tb[1]; }
    }
    for (var k = kids.length - 1; k >= 0; k--) stack.push(kids[k]);
  }
  var de = document.documentElement;
  var sample = '';
  try {
    var b = document.body;
    sample = b ? String(b.innerText || b.textContent || '') : '';
  } catch (e) {}
  return JSON.stringify({
    media: ['(prefers-color-scheme: dark)', '(prefers-reduced-motion: reduce)', '(hover: hover)',
      '(pointer: fine)', '(prefers-contrast: more)'].map(function (q) {
      try { return q + ' ' + matchMedia(q).matches; } catch (e) { return q + ' ?'; }
    }),
    text: sample.replace(/\s+/g, ' ').slice(0, 3000),
    url: location.href,
    title: document.title,
    ua: navigator.userAgent,
    viewport: [innerWidth, innerHeight],
    scroll: [de.scrollWidth, de.scrollHeight],
    dpr: devicePixelRatio,
    props: PROPS,
    walk_ms: Date.now() - started,
    truncated: truncated,
    nodes: out
  });
})()
