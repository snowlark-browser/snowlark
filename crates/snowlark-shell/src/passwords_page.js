// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The passwords page's own script: the search as you type and the buttons of a login and of a
// site on the Never list. A button asks the browser through an address that names the login or
// the site; no password is ever in an address.
(() => {
  const apply = (target) => {
    let url = 'snowlark://passwords/apply?op=' + target.dataset.op;
    if (target.dataset.id !== undefined) url += '&id=' + encodeURIComponent(target.dataset.id);
    if (target.dataset.on !== undefined) url += '&on=' + encodeURIComponent(target.dataset.on);
    if (target.dataset.origin !== undefined) {
      url += '&origin=' + encodeURIComponent(target.dataset.origin);
    }
    location.href = url;
  };
  document.querySelector('main').addEventListener('click', (e) => {
    const target = e.target.closest('[data-op]');
    if (!target) return;
    e.preventDefault();
    apply(target);
  });

  // The search: the list follows each keystroke. Words are compared in any case and without their
  // accents, as the logins' search text is (lower case, decomposed, combining marks left out).
  const search = document.getElementById('search');
  const clear = document.getElementById('clear-search');
  const empty = document.getElementById('empty');
  const MARKS = /[̀-ͯ᪰-᫿᷀-᷿⃐-⃿︠-︯]/g;
  const fold = (s) => s.toLowerCase().normalize('NFD').replace(MARKS, '');
  function filter() {
    const words = fold(search.value).split(/\s+/).filter((w) => w);
    let shown = 0;
    let total = 0;
    for (const item of document.querySelectorAll('#list .item')) {
      const hit = words.every((w) => item.dataset.search.includes(w));
      item.hidden = !hit;
      total++;
      if (hit) shown++;
    }
    empty.hidden = shown > 0 || empty.dataset.off !== undefined;
    empty.textContent = words.length && total ? empty.dataset.none : empty.dataset.all;
    clear.hidden = search.value === '';
  }
  search.addEventListener('input', filter);
  search.form.addEventListener('submit', (e) => {
    e.preventDefault();
    filter();
  });
  clear.addEventListener('click', () => {
    search.value = '';
    filter();
    search.focus();
  });
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && document.activeElement === search && search.value) {
      search.value = '';
      filter();
    }
  });
  if (empty.hidden && !document.querySelector('#list .item')) empty.dataset.off = '';

  // The browser shows the page again after an action: the search field keeps its text through
  // that (and its focus and selection when it had them).
  const KEPT = 'passwords-search';
  addEventListener('pagehide', () => {
    try {
      const kept = { value: search.value, at: Date.now() };
      if (document.activeElement === search) {
        kept.start = search.selectionStart;
        kept.end = search.selectionEnd;
        kept.direction = search.selectionDirection;
      }
      sessionStorage.setItem(KEPT, JSON.stringify(kept));
    } catch (e) {}
  });
  let kept = null;
  try {
    kept = JSON.parse(sessionStorage.getItem(KEPT));
    sessionStorage.removeItem(KEPT);
  } catch (e) {}
  if (kept && typeof kept.value === 'string' && Math.abs(Date.now() - kept.at) < 5000) {
    if (kept.value !== search.value) {
      search.value = kept.value;
      filter();
    }
    if (typeof kept.start === 'number') {
      search.focus();
      search.setSelectionRange(kept.start, kept.end, kept.direction || 'none');
    }
  }
})();
