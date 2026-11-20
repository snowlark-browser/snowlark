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

  // The search: the list follows each keystroke; the address keeps it, so the page shows it again
  // when it is made anew.
  const search = document.getElementById('search');
  const clear = document.getElementById('clear-search');
  const empty = document.getElementById('empty');
  function filter() {
    const words = search.value.toLowerCase().split(/\s+/).filter((w) => w);
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
    const q = search.value.trim();
    history.replaceState(null, '', q ? '?q=' + encodeURIComponent(q) : 'snowlark://downloads/');
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
})();
