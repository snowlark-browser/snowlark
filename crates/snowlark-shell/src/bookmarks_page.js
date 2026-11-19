// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The bookmark manager's own script: selecting rows (Ctrl+click adds one, Shift+click a range),
// the menus (a row's, a tree folder's, the toolbar's Organize), the edit dialog, the delete
// notice with Undo, the folder tree's open folders, the keyboard, and dragging items to reorder
// them or move them into a folder. Every change goes to the browser as a navigation to
// snowlark://bookmarks/apply, which then shows the page again.
(() => {
  const APPLY = 'snowlark://bookmarks/apply?';
  const PAGE = 'snowlark://bookmarks/';
  const list = document.getElementById('list');
  const folder = list.dataset.folder;
  const searching = document.body.dataset.searching === 'true';
  const rows = () => [...document.querySelectorAll('.row')];
  const count = document.querySelector('#selection .count');

  function apply(params) {
    const q = new URLSearchParams();
    for (const [k, v] of params) q.append(k, v);
    location.href = APPLY + q.toString();
  }

  function store(key, value) {
    try {
      if (value === undefined) return JSON.parse(sessionStorage.getItem(key));
      sessionStorage.setItem(key, JSON.stringify(value));
    } catch (e) {
      return null;
    }
  }

  // The selection.
  let anchor = null;
  const selected = () => rows().filter((r) => r.classList.contains('selected'));
  function update() {
    const n = selected().length;
    document.body.classList.toggle('selecting', n > 1);
    count.textContent = n + ' selected';
  }
  function select(row, how) {
    const all = rows();
    if (how === 'range' && anchor) {
      const [a, b] = [all.indexOf(anchor), all.indexOf(row)].sort((x, y) => x - y);
      for (const [i, r] of all.entries()) setSelected(r, i >= a && i <= b);
    } else if (how === 'toggle') {
      setSelected(row, !row.classList.contains('selected'));
      anchor = row;
    } else {
      for (const r of all) setSelected(r, r === row);
      anchor = row;
    }
    row.focus();
    update();
  }
  function setSelected(row, on) {
    row.classList.toggle('selected', on);
    row.setAttribute('aria-selected', on);
  }
  function clearSelection() {
    for (const r of rows()) setSelected(r, false);
    anchor = null;
    update();
  }

  list.addEventListener('click', (e) => {
    const row = e.target.closest('.row');
    if (!row || e.target.closest('button.more')) return;
    select(row, e.shiftKey ? 'range' : e.ctrlKey ? 'toggle' : 'one');
  });
  list.addEventListener('dblclick', (e) => {
    const row = e.target.closest('.row');
    if (!row || e.target.closest('button.more')) return;
    if (!row.classList.contains('selected')) select(row, 'one');
    open([row], true);
  });
  // The middle button opens a page in a new tab behind this one.
  list.addEventListener('mousedown', (e) => {
    if (e.button === 1 && e.target.closest('.row')) e.preventDefault();
  });
  list.addEventListener('auxclick', (e) => {
    const row = e.target.closest('.row');
    if (e.button !== 1 || !row || 'folder' in row.dataset) return;
    select(row, 'one');
    apply([['op', 'open'], ['id', row.dataset.id]]);
  });
  document.addEventListener('click', (e) => {
    // A click on the page outside the list and its menus leaves nothing selected.
    if (!e.target.closest('#list, #menu, #selection, #scrim, #toast')) clearSelection();
  });

  // Opening: a folder (one, by a double click) shows it; pages open in new tabs.
  function open(items, active) {
    if (items.length === 1 && 'folder' in items[0].dataset) {
      location.href = PAGE + '?id=' + items[0].dataset.id;
      return;
    }
    apply([['op', 'open'], ...items.map((r) => ['id', r.dataset.id]), ['active', active ? 1 : 0]]);
  }

  // The menu: a row's ("ITEM"), a tree folder's ("TREE"), the toolbar's Organize ("TOOLBAR").
  const menu = document.getElementById('menu');
  let menuItems = [];
  let menuSource = null;
  let menuButton = null;
  // (a list row of a folder has data-folder, its count of pages; a tree folder data-pages)
  const isFolder = (r) => 'folder' in r.dataset || r.classList.contains('folder');
  function pagesIn(items) {
    let n = 0;
    for (const r of items) n += isFolder(r) ? Number(r.dataset.folder ?? r.dataset.pages) : 1;
    return n;
  }
  function showMenu(source, items, x, y, button) {
    menuSource = source;
    menuItems = items;
    menuButton = button;
    const one = items.length === 1 ? items[0] : null;
    const isRoot = one && one.dataset.id === '0';
    const command = (c) => menu.querySelector(`[data-command=${c}]`);
    const show = (c, visible, enabled = true) => {
      command(c).hidden = !visible;
      command(c).disabled = !enabled;
    };
    const item = source !== 'TOOLBAR';
    show('edit', item && items.length === 1, !isRoot);
    command('edit').textContent = one && isFolder(one) ? 'Rename' : 'Edit';
    show('show', source === 'ITEM' && items.length === 1 && searching);
    show('delete', item, !isRoot);
    const pages = pagesIn(items);
    const single = items.length === 1 && !isFolder(items[0]);
    show('open', item, pages > 0);
    command('open').textContent = single ? 'Open in new tab' : `Open all (${pages})`;
    const listLength = rows().length;
    show('sort', !item, !searching && listLength > 1);
    show('add', !item, !searching);
    show('folder', !item, !searching);
    menu.querySelector('[data-after=delete]').hidden = !item;
    menu.querySelector('[data-after=sort]').hidden = item;
    menu.hidden = false;
    const left = Math.min(x, scrollX + innerWidth - menu.offsetWidth - 8);
    menu.style.left = Math.max(8, left) + 'px';
    menu.style.top = y + 'px';
    const first = [...menu.querySelectorAll('button')].find((b) => !b.hidden && !b.disabled);
    if (first) first.focus();
  }
  function closeMenu() {
    menu.hidden = true;
    menuItems = [];
    menuSource = null;
    menuButton = null;
  }
  function menuAtButton(source, items, button) {
    if (!menu.hidden && menuButton === button) {
      closeMenu();
      return;
    }
    const r = button.getBoundingClientRect();
    showMenu(source, items, r.right + scrollX - menu.offsetWidth, r.bottom + scrollY, button);
  }
  list.addEventListener('click', (e) => {
    const button = e.target.closest('button.more');
    if (!button) return;
    e.stopPropagation();
    const row = button.closest('.row');
    // (a row of a selection of several: the menu is the selection's)
    const many = row.classList.contains('selected') && selected().length > 1;
    if (!many) select(row, 'one');
    menuAtButton('ITEM', many ? selected() : [row], button);
  });
  list.addEventListener('contextmenu', (e) => {
    const row = e.target.closest('.row');
    if (!row) return;
    e.preventDefault();
    if (!row.classList.contains('selected')) select(row, 'one');
    showMenu('ITEM', selected(), e.pageX, e.pageY, null);
  });
  document.getElementById('organize').addEventListener('click', (e) => {
    e.stopPropagation();
    menuAtButton('TOOLBAR', [], e.currentTarget);
  });
  document.addEventListener('click', (e) => {
    if (!menu.hidden && !menu.contains(e.target)) closeMenu();
  });
  menu.addEventListener('click', (e) => {
    const button = e.target.closest('button');
    if (!button || button.disabled) return;
    const items = menuItems;
    const command = button.dataset.command;
    closeMenu();
    run(command, items);
  });

  function run(command, items) {
    switch (command) {
      case 'edit':
        if (items.length === 1 && items[0].dataset.id !== '0') editDialog(items[0]);
        break;
      case 'show': {
        const r = items[0];
        store('bookmarks-highlight', r.dataset.id);
        location.href = PAGE + (r.dataset.parent === '0' ? '' : '?id=' + r.dataset.parent);
        break;
      }
      case 'delete':
        if (items.length && !items.some((r) => r.dataset.id === '0')) {
          apply([['op', 'delete'], ...items.map((r) => ['id', r.dataset.id])]);
        }
        break;
      case 'open':
        if (pagesIn(items) > 0) {
          apply([['op', 'open'], ...items.map((r) => ['id', r.dataset.id])]);
        }
        break;
      case 'sort':
        apply([['op', 'sort'], ['parent', folder]]);
        break;
      case 'add':
        addDialog(false);
        break;
      case 'folder':
        addDialog(true);
        break;
    }
  }

  // The edit dialog: "Edit bookmark" / "Rename folder" / "Add bookmark" / "Add folder".
  const scrim = document.getElementById('scrim');
  const nameInput = document.getElementById('name');
  const urlInput = document.getElementById('url');
  const urlField = document.getElementById('url-field');
  const urlError = document.getElementById('url-error');
  let dialogDone = null;
  function dialog(title, name, url, isFolder, done) {
    document.getElementById('dialog-title').textContent = title;
    nameInput.value = name;
    urlInput.value = url;
    urlField.hidden = isFolder;
    urlField.classList.remove('invalid');
    urlError.hidden = true;
    scrim.hidden = false;
    dialogDone = done;
    nameInput.focus();
    nameInput.select();
  }
  function closeDialog() {
    scrim.hidden = true;
    dialogDone = null;
  }
  // As typed when it is a valid address, otherwise with http:// in front when that is.
  function validUrl(typed) {
    for (const u of [typed, 'http://' + typed]) {
      try {
        if (typed.trim() !== '') return new URL(u.trim()).href;
      } catch (e) {
        // (the next form)
      }
    }
    return null;
  }
  function save() {
    if (!dialogDone) return;
    let url = null;
    if (!urlField.hidden) {
      url = validUrl(urlInput.value);
      if (url === null) {
        urlField.classList.add('invalid');
        urlError.hidden = false;
        urlInput.focus();
        return;
      }
    }
    const done = dialogDone;
    closeDialog();
    done(nameInput.value, url);
  }
  function editDialog(row) {
    const folderRow = isFolder(row);
    dialog(folderRow ? 'Rename folder' : 'Edit bookmark', row.dataset.title, row.dataset.url || '', folderRow,
      (name, url) => {
        const params = [['op', 'edit'], ['id', row.dataset.id], ['title', name]];
        if (url !== null) params.push(['url', url]);
        apply(params);
      });
  }
  function addDialog(isFolder) {
    dialog(isFolder ? 'Add folder' : 'Add bookmark', isFolder ? 'New folder' : '', '', isFolder, (name, url) => {
      const params = [['op', isFolder ? 'folder' : 'add'], ['parent', folder], ['title', name]];
      if (url !== null) params.push(['url', url]);
      apply(params);
    });
  }
  document.getElementById('dialog-cancel').addEventListener('click', closeDialog);
  document.getElementById('dialog-save').addEventListener('click', save);
  urlInput.addEventListener('input', () => {
    urlField.classList.remove('invalid');
    urlError.hidden = true;
  });

  // The selection's toolbar.
  document.getElementById('cancel-x').addEventListener('click', clearSelection);
  document.getElementById('delete').addEventListener('click', () => run('delete', selected()));

  // The delete notice and its Undo.
  const toast = document.getElementById('toast');
  if (!toast.hidden) setTimeout(() => (toast.hidden = true), Number(toast.dataset.ms));
  document.getElementById('undo').addEventListener('click', () => apply([['op', 'undo']]));

  // The tree: a folder shows its items; the arrow opens and closes it (remembered in this tab).
  const tree = document.getElementById('tree');
  const opened = store('bookmarks-open') || {};
  for (const [id, open] of Object.entries(opened)) {
    const node = tree.querySelector(`.node[data-id="${id}"]`);
    if (node && node.querySelector('.children')) setOpen(node, open);
  }
  function setOpen(node, open) {
    node.classList.toggle('open', open);
    node.firstElementChild.setAttribute('aria-expanded', open);
  }
  tree.addEventListener('click', (e) => {
    const row = e.target.closest('.folder');
    if (!row) return;
    e.preventDefault();
    const arrow = e.target.closest('.arrow');
    if (arrow) {
      const node = row.parentElement;
      const open = !node.classList.contains('open');
      setOpen(node, open);
      opened[node.dataset.id] = open;
      store('bookmarks-open', opened);
      return;
    }
    location.href = row.querySelector('a.label').href;
  });
  tree.addEventListener('contextmenu', (e) => {
    const row = e.target.closest('.folder');
    if (!row) return;
    e.preventDefault();
    showMenu('TREE', [row], e.pageX, e.pageY, null);
  });

  // The search: Enter submits its form; the clear button shows the bar again.
  const search = document.getElementById('search');
  const clear = document.getElementById('clear-search');
  search.addEventListener('input', () => {
    clear.hidden = search.value === '';
  });
  clear.addEventListener('click', () => {
    if (searching) location.href = PAGE;
    search.value = '';
    clear.hidden = true;
    search.focus();
  });

  // The keyboard.
  document.addEventListener('keydown', (e) => {
    const typing = e.target instanceof Element && e.target.closest('input');
    if (!scrim.hidden) {
      if (e.key === 'Escape') closeDialog();
      else if (e.key === 'Enter') save();
      return;
    }
    if (e.key === 'Escape') {
      if (dragging) cancelDrag();
      else if (!menu.hidden) closeMenu();
      else clearSelection();
      return;
    }
    if (typing) return;
    const all = rows();
    const sel = selected();
    const ctrl = e.ctrlKey;
    if ((e.key === 'Delete' || e.key === 'Backspace') && sel.length) {
      run('delete', sel);
    } else if (ctrl && e.key.toLowerCase() === 'z') {
      apply([['op', 'undo']]);
    } else if (ctrl && e.key.toLowerCase() === 'a') {
      for (const r of all) setSelected(r, true);
      update();
    } else if (e.key === 'F2' && sel.length === 1) {
      editDialog(sel[0]);
    } else if (e.key === 'Enter' && sel.length) {
      if (ctrl) apply([['op', 'open'], ...sel.map((r) => ['id', r.dataset.id])]);
      else open(sel, true);
    } else if ((e.key === 'ArrowDown' || e.key === 'ArrowUp') && all.length) {
      const at = all.indexOf(document.activeElement);
      const next = at < 0 ? 0 : Math.max(0, Math.min(all.length - 1, at + (e.key === 'ArrowDown' ? 1 : -1)));
      if (ctrl) all[next].focus();
      else select(all[next], e.shiftKey ? 'range' : 'one');
    } else {
      return;
    }
    e.preventDefault();
  });

  // Dragging (the page follows the mouse itself): rows of the list and folders of the tree drop
  // above or below an item of the same kind, or on a folder (a closed tree folder opens after a
  // rest on it), never on themselves or into themselves.
  const DRAG_THRESHOLD = 4;
  const OPEN_DELAY = 400;
  let press = null;
  let dragging = null;
  let suppressClick = false;
  const dragImage = document.createElement('div');
  dragImage.id = 'drag-image';
  dragImage.hidden = true;
  document.body.appendChild(dragImage);

  // The item an element shows: its id, parent, index, and whether it is a folder.
  const itemOf = (el) => ({
    id: el.dataset.id,
    parent: el.dataset.parent,
    index: Number(el.dataset.index),
    folder: isFolder(el),
  });
  // The folders above a tree folder (from the tree itself; a list row's parent is its folder).
  function ancestors(id) {
    const out = [];
    let node = tree.querySelector(`.node[data-id="${id}"]`);
    while (node) {
      out.push(node.dataset.id);
      node = node.parentElement.closest('.node');
    }
    return out;
  }

  list.addEventListener('mousedown', (e) => startPress(e, e.target.closest('.row')));
  tree.addEventListener('mousedown', (e) => {
    const row = e.target.closest('.folder');
    // (the bar itself does not move)
    startPress(e, row && row.dataset.id !== '0' ? row : null);
  });
  function startPress(e, el) {
    if (e.button !== 0 || !el || e.target.closest('button')) return;
    press = { el, x: e.clientX, y: e.clientY };
  }

  document.addEventListener('mousemove', (e) => {
    if (press && !dragging) {
      if (Math.abs(e.clientX - press.x) < DRAG_THRESHOLD && Math.abs(e.clientY - press.y) < DRAG_THRESHOLD) return;
      beginDrag(press.el);
    }
    if (!dragging) return;
    e.preventDefault();
    dragImage.style.left = e.clientX + 12 + 'px';
    dragImage.style.top = e.clientY + 8 + 'px';
    const over = document.elementFromPoint(e.clientX, e.clientY);
    const target = over && over.closest('.row, #tree .folder, #list');
    setDestination(target ? destination(target, e.clientY) : null);
  });
  document.addEventListener('mouseup', (e) => {
    press = null;
    if (!dragging || e.button !== 0) return;
    const dest = dragging.dest;
    const ids = dragging.items.map((i) => i.id);
    cancelDrag();
    suppressClick = true;
    setTimeout(() => (suppressClick = false), 0);
    if (!dest) return;
    const params = [['op', 'move'], ...ids.map((id) => ['id', id]), ['parent', dest.parent]];
    if (dest.index !== undefined) params.push(['index', dest.index]);
    apply(params);
  });
  document.addEventListener('click', (e) => {
    if (suppressClick) {
      e.stopPropagation();
      e.preventDefault();
    }
  }, true);

  // The items dragged: a selected row takes the selection with it; anything else goes alone (and
  // ends up selected, when it is a row).
  function beginDrag(el) {
    let els = [el];
    if (el.classList.contains('row')) {
      if (!el.classList.contains('selected')) select(el, 'one');
      els = selected();
    }
    dragging = { items: els.map(itemOf), els, dest: null, marked: null, timer: null };
    for (const x of els) x.classList.add('dragged');
    dragImage.textContent = els.length === 1 ? (el.dataset.title || el.dataset.url || '') : els.length + ' items';
    dragImage.hidden = false;
    document.body.classList.add('dragging');
    closeMenu();
  }

  function cancelDrag() {
    if (!dragging) return;
    setDestination(null);
    for (const x of dragging.els) x.classList.remove('dragged');
    dragging = null;
    press = null;
    dragImage.hidden = true;
    document.body.classList.remove('dragging');
  }

  const isDragged = (id) => dragging.items.some((i) => i.id === id);
  // A dragged folder is `id` or holds it.
  const intoItself = (id) => {
    const above = ancestors(id);
    return dragging.items.some((i) => i.folder && above.includes(i.id));
  };

  // Where a drop on `el` at height `y` would go: {el, position: 'above'|'below'|'on', parent,
  // index}, or null.
  function destination(el, y) {
    const isList = el.id === 'list';
    const isTree = el.classList.contains('folder');
    // (no drops in a search's list)
    if (!isTree && searching) return null;
    const id = isList ? folder : el.dataset.id;
    if (isDragged(id) || intoItself(id)) return null;
    let above = false;
    let below = false;
    // (the bar has no place above or below it; between tree folders only folders go)
    if (!isList && id !== '0' && (!isTree || dragging.items.some((i) => i.folder))) {
      const siblings = isTree
        ? [...el.parentElement.parentElement.children].map((n) => n.firstElementChild)
        : rows();
      const at = siblings.indexOf(el);
      const prev = siblings[at - 1];
      const next = siblings[at + 1];
      above = !prev || !isDragged(prev.dataset.id);
      // (not below an open tree folder showing its folders)
      const openWithFolders = isTree && el.parentElement.classList.contains('open') && el.nextElementSibling;
      below = !openWithFolders && (!next || !isDragged(next.dataset.id));
    }
    let on;
    if (isList) {
      on = el.classList.contains('empty-list');
    } else {
      // (on a folder, unless a dragged item is in it already)
      on = isFolder(el) && !dragging.items.some((i) => i.parent === id);
    }
    if (!above && !below && !on) return null;
    const r = el.getBoundingClientRect();
    const ratio = (y - r.top) / r.height;
    let position = null;
    if (above && (ratio <= 0.25 || (ratio <= 0.5 && (!below || !on)))) position = 'above';
    else if (below && (ratio > 0.75 || (ratio > 0.5 && (!above || !on)))) position = 'below';
    else if (on) position = 'on';
    if (!position) return null;
    if (position === 'on') return { el, position, parent: id, index: isList ? 0 : undefined };
    const item = itemOf(el);
    return { el, position, parent: item.parent, index: item.index + (position === 'below' ? 1 : 0) };
  }

  function setDestination(dest) {
    const d = dragging;
    if (!d) return;
    if (d.marked) d.marked.classList.remove('drag-above', 'drag-below', 'drag-on');
    const sameFolder = d.dest && dest && d.dest.el === dest.el && d.dest.position === dest.position;
    d.dest = dest;
    d.marked = dest && dest.el;
    if (dest) dest.el.classList.add('drag-' + dest.position);
    // A closed tree folder with folders in it opens after a rest on it.
    if (sameFolder) return;
    clearTimeout(d.timer);
    const node = dest && dest.position === 'on' && dest.el.classList.contains('folder') && dest.el.parentElement;
    if (node && !node.classList.contains('open') && node.querySelector('.children')) {
      d.timer = setTimeout(() => {
        if (dragging === d && d.dest && d.dest.el.parentElement === node) setOpen(node, true);
      }, OPEN_DELAY);
    }
  }

  // "Show in folder" lands here with the item highlighted.
  const highlight = store('bookmarks-highlight');
  if (highlight) {
    store('bookmarks-highlight', '');
    const row = rows().find((r) => r.dataset.id === highlight);
    if (row) {
      select(row, 'one');
      row.scrollIntoView({ block: 'center' });
    }
  }
  update();
})();
