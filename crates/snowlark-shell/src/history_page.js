// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The history page's own script: selecting rows (a click, Shift+click for a range), the
// selection's toolbar and its confirmation, a row's menu, and the search field.
(() => {
  const form = document.getElementById('rows');
  const boxes = () => [...document.querySelectorAll('.row input[type=checkbox]')];
  const count = document.querySelector('#selection .count');
  let last = null;

  function update() {
    const n = boxes().filter((b) => b.checked).length;
    document.body.classList.toggle('selecting', n > 0);
    count.textContent = n + ' selected';
  }

  function clearSelection() {
    for (const b of boxes()) b.checked = false;
    last = null;
    update();
  }

  form.addEventListener('click', (e) => {
    const box = e.target.closest('.row input[type=checkbox]');
    if (!box) return;
    // Shift: the rows between the last one clicked and this one take this one's state.
    if (e.shiftKey && last && last !== box) {
      const all = boxes();
      const [a, b] = [all.indexOf(last), all.indexOf(box)].sort((x, y) => x - y);
      for (let i = a; i <= b; i++) all[i].checked = box.checked;
    }
    last = box;
    update();
  });

  // The confirmation before the selected rows go.
  const scrim = document.getElementById('scrim');
  function confirmDelete() {
    if (!boxes().some((b) => b.checked)) return;
    scrim.hidden = false;
    document.getElementById('dialog-delete').focus();
  }
  document.getElementById('delete').addEventListener('click', confirmDelete);
  document.getElementById('cancel').addEventListener('click', clearSelection);
  document.getElementById('cancel-x').addEventListener('click', clearSelection);
  document.getElementById('dialog-cancel').addEventListener('click', () => {
    scrim.hidden = true;
  });
  document.getElementById('dialog-delete').addEventListener('click', () => {
    scrim.hidden = true;
    form.submit();
  });

  // A row's menu.
  const menu = document.getElementById('menu');
  let menuRow = null;
  function closeMenu() {
    menu.hidden = true;
    menuRow = null;
  }
  form.addEventListener('click', (e) => {
    const button = e.target.closest('button.more');
    if (!button) return;
    e.stopPropagation();
    if (menuRow === button.closest('.row')) {
      closeMenu();
      return;
    }
    menuRow = button.closest('.row');
    const r = button.getBoundingClientRect();
    menu.hidden = false;
    menu.style.left = Math.max(8, r.right + scrollX - menu.offsetWidth) + 'px';
    menu.style.top = r.bottom + scrollY + 'px';
  });
  document.addEventListener('click', (e) => {
    if (!menu.hidden && !menu.contains(e.target)) closeMenu();
  });
  document.getElementById('menu-more').addEventListener('click', () => {
    const domain = menuRow.querySelector('button.more').dataset.domain;
    closeMenu();
    location.href = 'snowlark://history/?q=' + encodeURIComponent('host:' + domain);
  });
  // Removing one row asks nothing.
  document.getElementById('menu-remove').addEventListener('click', () => {
    const row = menuRow;
    closeMenu();
    for (const b of boxes()) b.checked = b.closest('.row') === row;
    form.submit();
  });

  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') {
      if (!scrim.hidden) scrim.hidden = true;
      else if (!menu.hidden) closeMenu();
      else if (document.body.classList.contains('selecting')) clearSelection();
    } else if (e.key === 'Delete' && document.activeElement?.type !== 'search') {
      confirmDelete();
    }
  });

  // The search: Enter submits its form; the clear button shows every page again.
  const search = document.getElementById('search');
  const clear = document.getElementById('clear-search');
  search.addEventListener('input', () => {
    clear.hidden = search.value === '';
  });
  clear.addEventListener('click', () => {
    if (new URLSearchParams(location.search).get('q')) location.href = 'snowlark://history/';
    search.value = '';
    clear.hidden = true;
    search.focus();
  });
  update();
})();
