// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

(() => {
  const root = document.documentElement;
  const v = document.querySelector('video');
  const s = { pm: 0, mm: 0, raf: 0, seeks: 0, co: 0, muts: new Map(), styles: new Map() };
  addEventListener('pointermove', (e) => { s.pm++; s.co += e.getCoalescedEvents ? e.getCoalescedEvents().length : 0; }, true);
  addEventListener('mousemove', () => s.mm++, true);
  let on = true;
  const tick = () => { s.raf++; if (on) requestAnimationFrame(tick); };
  requestAnimationFrame(tick);
  if (v) v.addEventListener('seeking', () => s.seeks++);
  const name = (t) => t.tagName.toLowerCase() +
    (typeof t.className === 'string' && t.className ? '.' + t.className.trim().split(/\s+/)[0] : '');
  const mo = new MutationObserver((list) => {
    for (const m of list) {
      const t = m.target.nodeType === 1 ? m.target : m.target.parentElement;
      if (!t) continue;
      const k = name(t) + ' ' + (m.attributeName || m.type);
      s.muts.set(k, (s.muts.get(k) || 0) + 1);
      if (m.attributeName === 'style') s.styles.set(name(t), t.getAttribute('style'));
    }
  });
  mo.observe(root, { attributes: true, childList: true, characterData: true, subtree: true });
  const t0 = performance.now();
  setTimeout(() => {
    on = false; mo.disconnect();
    const secs = (performance.now() - t0) / 1000;
    const sb = [...root.querySelectorAll('*')].filter((e) =>
      getComputedStyle(e).backgroundImage.includes('/sb/') ||
      (e.tagName === 'IMG' && (e.currentSrc || e.src).includes('/sb/')));
    window.__A = {
      secs: +secs.toFixed(2),
      pointermovePerSec: +(s.pm / secs).toFixed(1),
      coalescedPerMove: +(s.co / Math.max(1, s.pm)).toFixed(2),
      mousemovePerSec: +(s.mm / secs).toFixed(1),
      rafPerSec: +(s.raf / secs).toFixed(1),
      seeks: s.seeks,
      storyboard: sb.slice(0, 8).map((e) => name(e) + ' ' + e.getBoundingClientRect().width.toFixed(0) +
        'x' + e.getBoundingClientRect().height.toFixed(0) + ' ' +
        (e.tagName === 'IMG' ? 'img' : getComputedStyle(e).backgroundPosition + ' / ' +
          getComputedStyle(e).backgroundSize)),
      topChanges: [...s.muts].sort((a, b) => b[1] - a[1]).slice(0, 20),
      lastStyles: [...s.styles].slice(0, 20),
    };
  }, 5000);
  return 'drag the seek bar now, for 5 seconds';
})();
