// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Visibility snapshot of the current page, evaluated in the same form in Chrome (CDP Runtime.evaluate) and in
// Snowlark (SNOWLARK_EVAL). Walks the flat tree (open shadow roots, slots' assigned nodes) in document order
// and returns a JSON string: page metrics plus one record per element:
//   pa   index of the parent record (-1 for the root)
//   p    path: tag[nth-of-type] chain from <html>, "#shadow" marking a step into a shadow root
//   t, id, c   tag, id, class list
//   r    border box in document coordinates [x, y, width, height]
//   cl   the clip the element is painted in (overflow ancestors' padding boxes), [x0, y0, x1, y1]
//   op   own opacity; eo the product of the ancestors' and its own
//   d, v display and visibility
//   an   animation-name (when not none), tr transition-property with a non-zero duration (when any)
//   da   its data-*, aria-hidden and hidden attributes (sorted, values cut), when it has any
//   ia   inline style: 1 when the style attribute sets display, visibility or opacity
//   ov   1 when the element clips its contents (overflow other than visible)
//   k    content: t own text, i image (loaded), I image (not loaded), c canvas, v video, s svg,
//        b background image, g background color, f form control
//   tx, tl   hash and length of the element's own text (its direct text nodes, whitespace collapsed)
//   src  image source (the URL without its query), for matching and for the format of a missing image
// Failures of individual calls are recorded as empty values rather than aborting the walk, so an engine
// missing a feature still yields a snapshot.
(function () {
  var MAX = __MAX_NODES__;
  var SKIP = { head: 1, script: 1, style: 1, template: 1, meta: 1, link: 1, title: 1, base: 1, noscript: 1 };
  var LEAF = { svg: 1, math: 1, iframe: 1, object: 1, embed: 1, canvas: 1, video: 1, audio: 1, select: 1,
    img: 1, input: 1, textarea: 1 };
  var FORM = { input: 1, textarea: 1, select: 1, button: 1 };
  var sx = window.scrollX || 0, sy = window.scrollY || 0;

  function fnv(s) {
    var h = 0x811c9dc5;
    for (var i = 0; i < s.length; i++) {
      h ^= s.charCodeAt(i);
      h = Math.imul(h, 16777619);
    }
    return (h >>> 0).toString(16);
  }
  function round(v) {
    return Math.round(v * 10) / 10;
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
  function stripQuery(u) {
    u = String(u || '');
    var q = u.search(/[?#]/);
    return (q >= 0 ? u.slice(0, q) : u).slice(0, 300);
  }
  function transparent(c) {
    return !c || c === 'transparent' || /rgba\([^)]*,\s*0(\.0+)?\)$/.test(c) || /\/\s*0\)$/.test(c);
  }
  function cs(el) {
    try { return getComputedStyle(el); } catch (e) { return null; }
  }
  function prop(s, name) {
    if (!s) return '';
    try { var v = s.getPropertyValue(name); return v == null ? '' : String(v).trim(); } catch (e) { return ''; }
  }
  function nonZeroDuration(d) {
    return d.split(',').some(function (x) { return parseFloat(x) > 0; });
  }

  var started = Date.now();
  var out = [];
  var truncated = false;
  var full = [-1e9, -1e9, 1e9, 1e9];
  // Pre-order walk with an explicit stack: [element, parent index, path, parent's effective opacity, clip].
  var stack = [[document.documentElement, -1, '/html[1]', 1, full]];
  while (stack.length) {
    var item = stack.pop();
    if (out.length >= MAX) { truncated = true; break; }
    var el = item[0];
    var idx = out.length;
    var rec = { pa: item[1], p: item[2], t: el.localName, id: '', c: '', r: [0, 0, 0, 0], k: '' };
    // (the attribute: a form's `id` property can be a control named "id")
    try { rec.id = String(el.getAttribute('id') || '').slice(0, 100); } catch (e) {}
    try { rec.c = (el.getAttribute('class') || '').trim().replace(/\s+/g, ' ').slice(0, 200); } catch (e) {}
    try {
      var b = el.getBoundingClientRect();
      rec.r = [round(b.left + sx), round(b.top + sy), round(b.width), round(b.height)];
    } catch (e) {}
    var s = cs(el);
    rec.d = prop(s, 'display');
    rec.v = prop(s, 'visibility');
    var op = parseFloat(prop(s, 'opacity'));
    rec.op = isNaN(op) ? 1 : op;
    rec.eo = Math.round(item[3] * rec.op * 1000) / 1000;
    var an = prop(s, 'animation-name');
    if (an && an !== 'none') rec.an = an.slice(0, 120);
    var td = prop(s, 'transition-duration');
    if (td && nonZeroDuration(td)) rec.tr = prop(s, 'transition-property').slice(0, 120);
    try {
      var da = [];
      for (var ai = 0; ai < el.attributes.length; ai++) {
        var at = el.attributes[ai];
        if (at.name.lastIndexOf('data-', 0) === 0 || at.name.lastIndexOf('aria-hidden', 0) === 0 || at.name === 'hidden')
          da.push(at.name + '=' + String(at.value).slice(0, 40));
      }
      if (da.length) rec.da = da.sort().join(' ').slice(0, 300);
    } catch (e) {}
    try {
      var st = el.getAttribute('style') || '';
      if (/(^|;|\s)(display|visibility|opacity)\s*:/i.test(st)) rec.ia = 1;
    } catch (e) {}
    var clip = item[4];
    // position: fixed escapes its ancestors' overflow clips (to the viewport).
    if (prop(s, 'position') === 'fixed') clip = full;
    rec.cl = clip === full ? null : clip;
    out.push(rec);

    var t = rec.t, k = '';
    if (t === 'img') {
      var ok = false;
      try { ok = el.complete && el.naturalWidth > 0; } catch (e) {}
      k += ok ? 'i' : 'I';
      try { rec.src = stripQuery(el.currentSrc || el.src); } catch (e) {}
    } else if (t === 'canvas') k += 'c';
    else if (t === 'video') k += 'v';
    else if (t === 'svg') k += 's';
    if (FORM[t]) k += 'f';
    var bi = prop(s, 'background-image');
    if (bi && bi !== 'none') {
      k += 'b';
      if (!rec.src) {
        var m = /url\(["']?([^"')]+)/.exec(bi);
        if (m) rec.src = stripQuery(m[1]);
      }
    }
    if (!transparent(prop(s, 'background-color'))) k += 'g';

    if (rec.d === 'none' || LEAF[t]) { rec.k = k; continue; }
    // The clip for the children: this element's padding box when it clips (overflow other than visible).
    var ox = prop(s, 'overflow-x'), oy = prop(s, 'overflow-y');
    var kidClip = clip;
    // (html's and body's overflow usually applies to the viewport, not to their own boxes)
    if (t !== 'html' && t !== 'body' && ((ox && ox !== 'visible') ||
        (oy && oy !== 'visible'))) {
      rec.ov = 1;
      var x0 = rec.r[0], y0 = rec.r[1], x1 = x0 + rec.r[2], y1 = y0 + rec.r[3];
      if (ox === 'visible') { x0 = -1e9; x1 = 1e9; }
      if (oy === 'visible') { y0 = -1e9; y1 = 1e9; }
      kidClip = [Math.max(clip[0], x0), Math.max(clip[1], y0), Math.min(clip[2], x1), Math.min(clip[3], y1)];
    }
    var list = childList(el);
    var shadow = false;
    try { shadow = !!el.shadowRoot && el.localName !== 'slot'; } catch (e) {}
    var text = '', kids = [], counts = {};
    for (var i = 0; i < list.length; i++) {
      var n = list[i];
      if (n.nodeType === 3) {
        text += n.data;
      } else if (n.nodeType === 1 && !SKIP[n.localName]) {
        var tag = n.localName;
        counts[tag] = (counts[tag] || 0) + 1;
        kids.push([n, idx, item[2] + (shadow ? '/#shadow' : '') + '/' + tag + '[' + counts[tag] + ']', rec.eo, kidClip]);
      }
    }
    text = text.replace(/\s+/g, ' ').trim();
    if (text) {
      k = 't' + k;
      rec.tx = fnv(text);
      rec.tl = text.length;
    }
    rec.k = k;
    for (var j = kids.length - 1; j >= 0; j--) stack.push(kids[j]);
  }
  var de = document.documentElement;
  var sample = '';
  try {
    var body = document.body;
    sample = body ? String(body.innerText || body.textContent || '') : '';
  } catch (e) {}
  var running = -1;
  try {
    running = document.getAnimations().filter(function (a) { return a.playState === 'running'; }).length;
  } catch (e) {}
  return JSON.stringify({
    text: sample.replace(/\s+/g, ' ').slice(0, 2000),
    url: String(location.href),
    title: String(document.title),
    ua: navigator.userAgent,
    viewport: [innerWidth, innerHeight],
    scroll: [de.scrollWidth, de.scrollHeight],
    scrolled: [sx, sy],
    running_animations: running,
    walk_ms: Date.now() - started,
    truncated: truncated,
    nodes: out
  });
})()
