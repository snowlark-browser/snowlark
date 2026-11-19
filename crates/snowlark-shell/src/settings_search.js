// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The settings page's search: the settings page's own script (a web page has no way to it).
// Typing in the search field shows every category's cards that have the text, highlights the text,
// and marks a menu (<select>) whose choices have it with a count; nothing matching says so. The
// query is kept in the address (#search=...), so the page shows the same results when it is
// shown again after a change.
(() => {
  const field = document.getElementById('search');
  const clear = document.getElementById('clear-search');
  const noResults = document.getElementById('no-results');
  const pages = [...document.querySelectorAll('.page')];
  const current = document.querySelector('nav a[aria-current]');
  // (elements whose text is not a setting's: styles, scripts, icons, text being edited)
  const SKIP = new Set(['STYLE', 'SCRIPT', 'TEMPLATE', 'TEXTAREA', 'svg', 'NOSCRIPT']);
  let wrappers = [];
  let bubbles = [];

  const strip = (s) => s.normalize('NFD').replace(/[\u0300-\u036f]/g, '');

  // The original text node goes back in place of each highlight.
  function removeHighlights() {
    for (const w of wrappers) {
      if (w.parentNode) {
        w.parentNode.replaceChild(w.firstChild.firstChild, w);
      }
    }
    wrappers = [];
    for (const b of bubbles) {
      b.remove();
    }
    bubbles = [];
  }

  // Wraps `node` (a text node) with its matches at `ranges` in highlight spans, keeping the node
  // itself, hidden, to put back.
  function highlight(node, ranges) {
    const wrapper = document.createElement('span');
    wrapper.className = 'search-highlight-wrapper';
    node.parentNode.replaceChild(wrapper, node);
    const original = document.createElement('span');
    original.className = 'search-highlight-original';
    original.appendChild(node);
    wrapper.appendChild(original);
    const text = node.nodeValue;
    let at = 0;
    for (const r of ranges) {
      wrapper.appendChild(document.createTextNode(text.substring(at, r.start)));
      const hit = document.createElement('span');
      hit.className = 'search-highlight-hit';
      hit.textContent = text.substr(r.start, r.length);
      wrapper.appendChild(hit);
      at = r.start + r.length;
    }
    wrapper.appendChild(document.createTextNode(text.substring(at)));
    wrappers.push(wrapper);
  }

  // A count of matches beside a menu whose choices have the text.
  function bubble(select, n) {
    let b = select.previousElementSibling;
    if (!b || !b.classList.contains('search-bubble')) {
      b = document.createElement('span');
      b.className = 'search-bubble';
      b.dataset.results = '0';
      select.parentNode.insertBefore(b, select);
      bubbles.push(b);
    }
    const total = Number(b.dataset.results) + n;
    b.dataset.results = String(total);
    b.textContent = total === 1 ? '1 result' : total + ' results';
  }

  // Highlights the matches of `re` under `node`; returns how many there were.
  function find(node, re) {
    if (SKIP.has(node.nodeName)) {
      return 0;
    }
    if (node.nodeType === Node.ELEMENT_NODE &&
        (node.hasAttribute('hidden') || node.style.display === 'none')) {
      return 0;
    }
    if (node.nodeType === Node.TEXT_NODE) {
      const text = node.nodeValue;
      if (text.trim().length === 0) {
        return 0;
      }
      const stripped = strip(text);
      const ranges = [];
      re.lastIndex = 0;
      for (let m; (m = re.exec(stripped));) {
        ranges.push({start: m.index, length: m[0].length});
      }
      if (ranges.length > 0) {
        const parent = node.parentNode;
        if (parent.nodeName === 'OPTION') {
          const select = parent.closest('select');
          if (select) {
            bubble(select, ranges.length);
          }
        } else {
          highlight(node, ranges);
        }
      }
      return ranges.length;
    }
    let n = 0;
    for (let child = node.firstChild; child !== null;) {
      const next = child.nextSibling;
      n += find(child, re);
      child = next;
    }
    return n;
  }

  // Shows the results for `raw`; the empty query shows the category the page was opened at.
  function search(raw) {
    removeHighlights();
    const query = strip(raw.trim());
    clear.hidden = raw.length === 0;
    if (query.length === 0) {
      document.body.classList.remove('searching');
      noResults.hidden = true;
      if (current) {
        current.setAttribute('aria-current', 'page');
      }
      for (const page of pages) {
        page.classList.remove('match');
        for (const card of page.querySelectorAll('.card')) {
          card.classList.remove('no-match');
        }
      }
      return;
    }
    const re = new RegExp('(' + query.replace(/[-[\]{}()*+?.,\\^$|#\s]/g, '\\$&') + ')', 'ig');
    document.body.classList.add('searching');
    if (current) {
      current.removeAttribute('aria-current');
    }
    let total = 0;
    for (const page of pages) {
      // (a category whose title matches shows all its cards)
      const title = find(page.querySelector('h1'), re);
      let cards = 0;
      for (const card of page.querySelectorAll('.card')) {
        const n = find(card, re);
        card.classList.toggle('no-match', n === 0 && title === 0);
        cards += n;
      }
      page.classList.toggle('match', title + cards > 0);
      total += title + cards;
    }
    noResults.hidden = total > 0;
  }

  function update() {
    search(field.value);
    const base = location.href.split('#')[0];
    const q = field.value.trim();
    history.replaceState(history.state, '', q ? base + '#search=' + encodeURIComponent(q) : base);
  }

  field.addEventListener('input', update);
  field.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && field.value !== '') {
      e.preventDefault();
      field.value = '';
      update();
    }
  });
  clear.addEventListener('click', () => {
    field.value = '';
    update();
    field.focus();
  });

  const m = /^#search=(.*)$/.exec(location.hash);
  if (m) {
    try {
      field.value = decodeURIComponent(m[1]);
    } catch (e) {
      field.value = m[1];
    }
    search(field.value);
  }
})();
