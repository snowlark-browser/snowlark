// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The downloads page's own script: the search as you type, an entry's buttons, Clear all, and the
// removal notice with its Undo (also Ctrl+Z).
(() => {
  const apply = (op, id) => {
    let url = 'snowlark://downloads/apply?op=' + op;
    if (id !== undefined) url += '&id=' + id;
    location.href = url;
  };

  // An entry's buttons and its file's name.
  document.getElementById('list').addEventListener('click', (e) => {
    const target = e.target.closest('[data-op]');
    if (!target) return;
    e.preventDefault();
    apply(target.dataset.op, target.dataset.id);
    if (target.dataset.op === 'copy') notice('Copied download link');
  });
  document.getElementById('clear-all').addEventListener('click', () => apply('clear'));

  // The notice at the bottom: a removal's (with Undo) for the time it has left, or a short one.
  const toast = document.getElementById('toast');
  let timer = 0;
  function hideLater(ms) {
    clearTimeout(timer);
    timer = setTimeout(() => {
      toast.hidden = true;
    }, ms);
  }
  function notice(text) {
    toast.querySelector('span').textContent = text;
    toast.querySelector('#undo')?.remove();
    toast.hidden = false;
    hideLater(3000);
  }
  const undo = document.getElementById('undo');
  if (undo) {
    undo.addEventListener('click', () => apply('undo'));
    hideLater(Number(toast.dataset.left) || 0);
  }
  document.addEventListener('keydown', (e) => {
    const editing = document.activeElement?.tagName === 'INPUT';
    if (e.ctrlKey && !e.shiftKey && !e.altKey && e.key.toLowerCase() === 'z' && !editing) {
      e.preventDefault();
      apply('undo');
    }
  });

  // The search: the list follows each keystroke (an address's `?q=` starts it; the history API
  // cannot change the query of an address that is not HTTP(S), so what is typed is kept for the
  // page's next showing in the tab's session storage, below). Words are compared in any case and without their accents, as the
  // entries' search text is (lower case, decomposed, combining marks left out).
  const search = document.getElementById('search');
  const clear = document.getElementById('clear-search');
  const empty = document.getElementById('empty');
  const MARKS = /[̀-ͯ᪰-᫿᷀-᷿⃐-⃿︠-︯]/g;
  const fold = (s) => s.toLowerCase().normalize('NFD').replace(MARKS, '');
  function filter() {
    const words = fold(search.value).split(/\s+/).filter((w) => w);
    let shown = 0;
    for (const day of document.querySelectorAll('.day')) {
      let any = false;
      for (const item of day.querySelectorAll('.item')) {
        const hit = words.every((w) => item.dataset.search.includes(w));
        item.hidden = !hit;
        any ||= hit;
        if (hit) shown++;
      }
      day.hidden = !any;
    }
    empty.hidden = shown > 0;
    empty.textContent = words.length ? empty.dataset.none : empty.dataset.all;
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

  // The browser shows the page again while downloads run (their progress) and after an action: the
  // search field keeps its text, and its focus and selection when it had them, through that.
  const KEPT = 'downloads-search';
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
  // (only when the page is shown again at once: not when the tab comes back to it later)
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
