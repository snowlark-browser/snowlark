// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The bookmarks: a tree of pages and folders that the bookmarks bar shows (its top level is the
//! bar), kept in the profile as `bookmarks.json`.
//!
//! The file is the C# engine's format (a JSON list of `{"Title", "Url", "Icon"}`, the icon as the
//! base64 of the site's icon file) with folders added as items with `"Children"` and no URL, so the
//! C# profile's file is imported as it is, once, when this profile has none. Changes are saved at
//! once (a temporary file renamed over the old one).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A bookmark id, stable while the program runs (not stored).
pub type BookmarkId = u64;

/// A page or a folder.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Item {
    #[serde(skip)]
    pub id: BookmarkId,
    #[serde(default)]
    pub title: String,
    /// Empty for a folder.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// The site's icon file (PNG, ICO or SVG), base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// A folder's items (`None` for a page).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<Item>>,
}

impl Item {
    pub fn page(title: &str, url: &str, icon: Option<String>) -> Item {
        Item {
            id: 0,
            title: title.to_owned(),
            url: url.to_owned(),
            icon,
            children: None,
        }
    }

    pub fn folder(title: &str) -> Item {
        Item {
            id: 0,
            title: title.to_owned(),
            url: String::new(),
            icon: None,
            children: Some(Vec::new()),
        }
    }

    pub fn is_folder(&self) -> bool {
        self.children.is_some()
    }

    /// What the bar shows for it.
    pub fn label(&self) -> &str {
        if self.title.trim().is_empty() {
            &self.url
        } else {
            &self.title
        }
    }
}

/// Where an item goes: into `parent` (`None`: the bar) before the item at `index`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub parent: Option<BookmarkId>,
    pub index: usize,
}

#[derive(Debug)]
pub struct Bookmarks {
    file: Option<PathBuf>,
    items: Vec<Item>,
    next_id: BookmarkId,
    /// Bumped by every change (the window redraws what shows them).
    version: u64,
    /// The last [`Bookmarks::delete`]: each item removed with where it was, in the order removed
    /// (what [`Bookmarks::undo_delete`] puts back).
    deleted: Vec<(Place, Item)>,
}

impl Default for Bookmarks {
    fn default() -> Bookmarks {
        Bookmarks::in_memory()
    }
}

impl Bookmarks {
    pub fn in_memory() -> Bookmarks {
        Bookmarks {
            file: None,
            items: Vec::new(),
            next_id: 1,
            version: 0,
            deleted: Vec::new(),
        }
    }

    /// A copy of the tree, ids included, that saves nowhere (what the bookmark manager page is
    /// made from on another thread).
    pub fn snapshot(&self) -> Bookmarks {
        Bookmarks {
            file: None,
            items: self.items.clone(),
            next_id: self.next_id,
            version: self.version,
            deleted: Vec::new(),
        }
    }

    /// Reads `file`; when it doesn't exist, imports `import` (the C# profile's) once and saves it as
    /// this profile's.
    pub fn load(file: &Path, import: Option<&Path>) -> Bookmarks {
        let mut b = Bookmarks {
            file: Some(file.to_owned()),
            ..Bookmarks::in_memory()
        };
        let parse = |p: &Path| -> Option<Vec<Item>> {
            serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
        };
        if file.exists() {
            b.items = parse(file).unwrap_or_default();
        } else if let Some(items) = import.and_then(parse) {
            b.items = items;
            b.number();
            let _ = b.save();
        }
        // (bookmarks of internal pages made before the rename: under today's scheme)
        fn renamed(items: &mut [Item]) {
            for i in items {
                if let std::borrow::Cow::Owned(url) = crate::address::renamed(&i.url) {
                    i.url = url;
                }
                if let Some(c) = &mut i.children {
                    renamed(c);
                }
            }
        }
        renamed(&mut b.items);
        b.number();
        b
    }

    fn number(&mut self) {
        fn walk(items: &mut [Item], next: &mut BookmarkId) {
            for i in items {
                if i.id == 0 {
                    i.id = *next;
                    *next += 1;
                }
                if let Some(c) = &mut i.children {
                    walk(c, next);
                }
            }
        }
        let mut next = self.next_id;
        walk(&mut self.items, &mut next);
        self.next_id = next;
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// The bar's items.
    pub fn bar(&self) -> &[Item] {
        &self.items
    }

    pub fn find(&self, id: BookmarkId) -> Option<&Item> {
        fn walk(items: &[Item], id: BookmarkId) -> Option<&Item> {
            for i in items {
                if i.id == id {
                    return Some(i);
                }
                if let Some(found) = i.children.as_deref().and_then(|c| walk(c, id)) {
                    return Some(found);
                }
            }
            None
        }
        walk(&self.items, id)
    }

    fn list_mut(&mut self, parent: Option<BookmarkId>) -> Option<&mut Vec<Item>> {
        fn walk(items: &mut [Item], id: BookmarkId) -> Option<&mut Vec<Item>> {
            for i in items {
                if i.id == id {
                    return i.children.as_mut();
                }
                if let Some(c) = &mut i.children {
                    if let Some(found) = walk(c, id) {
                        return Some(found);
                    }
                }
            }
            None
        }
        match parent {
            None => Some(&mut self.items),
            Some(id) => walk(&mut self.items, id),
        }
    }

    /// Where `id` is.
    pub fn place_of(&self, id: BookmarkId) -> Option<Place> {
        fn walk(items: &[Item], parent: Option<BookmarkId>, id: BookmarkId) -> Option<Place> {
            for (k, i) in items.iter().enumerate() {
                if i.id == id {
                    return Some(Place { parent, index: k });
                }
                if let Some(p) = i.children.as_deref().and_then(|c| walk(c, Some(i.id), id)) {
                    return Some(p);
                }
            }
            None
        }
        walk(&self.items, None, id)
    }

    /// The first bookmark of `url` (the star shows filled for it).
    pub fn find_url(&self, url: &str) -> Option<BookmarkId> {
        fn walk(items: &[Item], url: &str) -> Option<BookmarkId> {
            for i in items {
                if !i.is_folder() && i.url == url {
                    return Some(i.id);
                }
                if let Some(found) = i.children.as_deref().and_then(|c| walk(c, url)) {
                    return Some(found);
                }
            }
            None
        }
        walk(&self.items, url)
    }

    /// Every folder with its depth, in tree order (for the folder chooser), the bar first as `None`.
    pub fn folders(&self) -> Vec<(Option<BookmarkId>, usize, String)> {
        fn walk(items: &[Item], depth: usize, out: &mut Vec<(Option<BookmarkId>, usize, String)>) {
            for i in items {
                if let Some(c) = &i.children {
                    out.push((Some(i.id), depth, i.label().to_owned()));
                    walk(c, depth + 1, out);
                }
            }
        }
        let mut out = vec![(None, 0, "Bookmarks bar".to_owned())];
        walk(&self.items, 1, &mut out);
        out
    }

    fn changed(&mut self) {
        self.version += 1;
        let _ = self.save();
    }

    /// Adds `item` at `place` (at the end of the list when the index is past it). Returns its id,
    /// or `None` when the parent is not a folder.
    pub fn add(&mut self, mut item: Item, place: Place) -> Option<BookmarkId> {
        item.id = 0;
        let list = self.list_mut(place.parent)?;
        let at = place.index.min(list.len());
        list.insert(at, item);
        self.number();
        let id = self.list_mut(place.parent)?.get(at)?.id;
        self.changed();
        Some(id)
    }

    /// Removes an item (a folder with what is in it); returns it.
    pub fn remove(&mut self, id: BookmarkId) -> Option<Item> {
        let place = self.place_of(id)?;
        let item = self.list_mut(place.parent)?.remove(place.index);
        self.changed();
        Some(item)
    }

    /// Renames an item and, for a page, changes its address.
    pub fn edit(&mut self, id: BookmarkId, title: &str, url: Option<&str>) -> bool {
        let place = match self.place_of(id) {
            Some(p) => p,
            None => return false,
        };
        let Some(item) = self
            .list_mut(place.parent)
            .and_then(|l| l.get_mut(place.index))
        else {
            return false;
        };
        item.title = title.to_owned();
        if let (Some(u), false) = (url, item.is_folder()) {
            item.url = u.to_owned();
        }
        self.changed();
        true
    }

    /// Whether `ancestor` is `id` or a folder containing it.
    fn contains(&self, ancestor: BookmarkId, id: BookmarkId) -> bool {
        let mut cur = Some(id);
        while let Some(c) = cur {
            if c == ancestor {
                return true;
            }
            cur = self.place_of(c).and_then(|p| p.parent);
        }
        false
    }

    /// Whether [`Bookmarks::move_to`] would move `id` to `to`: the item exists, the parent is a
    /// folder that is not the item or inside it, and the place is not where the item already is.
    pub fn can_move(&self, id: BookmarkId, to: Place) -> bool {
        let Some(from) = self.place_of(id) else {
            return false;
        };
        if to.parent.is_some_and(|p| self.contains(id, p)) {
            return false;
        }
        if to
            .parent
            .is_some_and(|p| !self.find(p).is_some_and(Item::is_folder))
        {
            return false;
        }
        from.parent != to.parent || (to.index != from.index && to.index != from.index + 1)
    }

    /// Moves an item to `to` (the index counted before the move takes it out). A folder never goes
    /// into itself. Returns whether it moved.
    pub fn move_to(&mut self, id: BookmarkId, to: Place) -> bool {
        let moved = self.move_quietly(id, to);
        if moved {
            self.changed();
        }
        moved
    }

    fn move_quietly(&mut self, id: BookmarkId, to: Place) -> bool {
        if !self.can_move(id, to) {
            return false;
        }
        let Some(from) = self.place_of(id) else {
            return false;
        };
        let mut index = to.index;
        if from.parent == to.parent && index > from.index {
            index -= 1;
        }
        let Some(item) = self.list_mut(from.parent).map(|l| l.remove(from.index)) else {
            return false;
        };
        let list = self.list_mut(to.parent).expect("the folder");
        let at = index.min(list.len());
        list.insert(at, item);
        true
    }

    /// Moves `ids` to `to`, one after another in that order (the index counted before the moves,
    /// as [`Bookmarks::move_to`]'s): what a drop of several items does. An item that cannot go
    /// there (a folder into itself) stays. Returns whether any moved.
    pub fn move_all(&mut self, ids: &[BookmarkId], to: Place) -> bool {
        let mut index = to.index;
        let mut moved = false;
        for &id in ids {
            moved |= self.move_quietly(
                id,
                Place {
                    parent: to.parent,
                    index,
                },
            );
            // (the next one goes after this one, wherever it is now)
            if let Some(p) = self.place_of(id).filter(|p| p.parent == to.parent) {
                index = p.index + 1;
            }
        }
        if moved {
            self.changed();
        }
        moved
    }

    /// Deletes `ids` (an item inside a folder deleted with it is not counted again), keeping them
    /// for [`Bookmarks::undo_delete`] until the next delete. Returns how many went.
    pub fn delete(&mut self, ids: &[BookmarkId]) -> usize {
        let mut top: Vec<BookmarkId> = Vec::new();
        for &id in ids {
            let inside = ids.iter().any(|&o| o != id && self.contains(o, id));
            if !inside && !top.contains(&id) && self.find(id).is_some() {
                top.push(id);
            }
        }
        let mut removed = Vec::new();
        for id in top {
            let Some(place) = self.place_of(id) else {
                continue;
            };
            if let Some(list) = self.list_mut(place.parent) {
                removed.push((place, list.remove(place.index)));
            }
        }
        let n = removed.len();
        if n > 0 {
            self.deleted = removed;
            self.changed();
        }
        n
    }

    /// What the last [`Bookmarks::delete`] removed (until it is put back or another delete).
    pub fn last_deleted(&self) -> Vec<&Item> {
        self.deleted.iter().map(|(_, i)| i).collect()
    }

    /// Whether [`Bookmarks::undo_delete`] has something to put back.
    pub fn can_undo_delete(&self) -> bool {
        !self.deleted.is_empty()
    }

    /// Puts what the last [`Bookmarks::delete`] removed back where it was, with its ids (in a
    /// folder deleted since: at the bar's end). Returns how many came back.
    pub fn undo_delete(&mut self) -> usize {
        let deleted = std::mem::take(&mut self.deleted);
        let n = deleted.len();
        for (place, item) in deleted.into_iter().rev() {
            let gone = place
                .parent
                .is_some_and(|p| !self.find(p).is_some_and(Item::is_folder));
            let (parent, index) = if gone {
                (None, usize::MAX)
            } else {
                (place.parent, place.index)
            };
            if let Some(list) = self.list_mut(parent) {
                let at = index.min(list.len());
                list.insert(at, item);
            }
        }
        if n > 0 {
            self.changed();
        }
        n
    }

    /// Sorts a folder's items (`None`: the bar's) by title, folders first. Returns whether the
    /// order changed.
    pub fn sort(&mut self, parent: Option<BookmarkId>) -> bool {
        let Some(list) = self.list_mut(parent) else {
            return false;
        };
        let before: Vec<BookmarkId> = list.iter().map(|i| i.id).collect();
        list.sort_by(|a, b| {
            b.is_folder()
                .cmp(&a.is_folder())
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
                .then_with(|| a.title.cmp(&b.title))
        });
        let changed = list.iter().map(|i| i.id).ne(before);
        if changed {
            self.changed();
        }
        changed
    }

    /// The items, folders included, whose title holds every word of `query`, or whose address
    /// does, ignoring case; in tree order.
    pub fn search(&self, query: &str) -> Vec<&Item> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let mut out = Vec::new();
        if words.is_empty() {
            return out;
        }
        fn walk<'a>(items: &'a [Item], words: &[String], out: &mut Vec<&'a Item>) {
            for i in items {
                let all = |text: &str| {
                    let text = text.to_lowercase();
                    words.iter().all(|w| text.contains(w.as_str()))
                };
                if all(&i.title) || (!i.url.is_empty() && all(&i.url)) {
                    out.push(i);
                }
                if let Some(c) = &i.children {
                    walk(c, words, out);
                }
            }
        }
        walk(&self.items, &words, &mut out);
        out
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(&self.items).map_err(std::io::Error::other)?;
        let tmp = file.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(b: &Bookmarks) -> Vec<String> {
        b.bar().iter().map(|i| i.label().to_owned()).collect()
    }

    #[test]
    fn imports_the_csharp_file_once() {
        let dir = std::env::temp_dir().join(format!("snowlark-bookmarks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("legacy.json");
        std::fs::write(
            &legacy,
            r#"[{"Title":"Hacker News","Url":"https://news.ycombinator.com/","Icon":"iVBORw0KGgo="},
                {"Title":"","Url":"https://flingup.com/"}]"#,
        )
        .unwrap();
        let file = dir.join("bookmarks.json");
        let _ = std::fs::remove_file(&file);
        let mut b = Bookmarks::load(&file, Some(&legacy));
        assert_eq!(bar(&b), ["Hacker News", "https://flingup.com/"]);
        assert_eq!(b.bar()[0].icon.as_deref(), Some("iVBORw0KGgo="));
        assert!(file.exists());
        // A folder survives the round trip; the legacy file is not read again.
        let f = b
            .add(
                Item::folder("News"),
                Place {
                    parent: None,
                    index: 9,
                },
            )
            .unwrap();
        b.add(
            Item::page("Lobsters", "https://lobste.rs/", None),
            Place {
                parent: Some(f),
                index: 0,
            },
        );
        std::fs::write(&legacy, "[]").unwrap();
        let again = Bookmarks::load(&file, Some(&legacy));
        assert_eq!(bar(&again), ["Hacker News", "https://flingup.com/", "News"]);
        let news = &again.bar()[2];
        assert!(news.is_folder());
        assert_eq!(news.children.as_ref().unwrap()[0].url, "https://lobste.rs/");
        // Pages are written without the folder keys, as the C# engine writes them.
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains(r#""Url": "https://lobste.rs/""#));
        assert!(!text.contains(r#""Children": null"#));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_edit_move_remove() {
        let mut b = Bookmarks::in_memory();
        let top = |i| Place {
            parent: None,
            index: i,
        };
        let a = b.add(Item::page("A", "https://a/", None), top(0)).unwrap();
        let c = b.add(Item::page("C", "https://c/", None), top(1)).unwrap();
        let f = b.add(Item::folder("F"), top(1)).unwrap();
        assert_eq!(bar(&b), ["A", "F", "C"]);
        assert_eq!(b.find_url("https://c/"), Some(c));
        assert!(b.edit(c, "See", Some("https://see/")));
        assert_eq!(b.find_url("https://see/"), Some(c));
        // Reordering on the bar: indexes count the item still in place.
        assert!(b.move_to(c, top(0)));
        assert_eq!(bar(&b), ["See", "A", "F"]);
        assert!(b.move_to(c, top(3)));
        assert_eq!(bar(&b), ["A", "F", "See"]);
        assert!(!b.move_to(c, top(2)), "dropping it where it is");
        // Into a folder, and a folder never into itself.
        assert!(b.move_to(
            a,
            Place {
                parent: Some(f),
                index: 0
            }
        ));
        assert_eq!(bar(&b), ["F", "See"]);
        assert_eq!(b.place_of(a).unwrap().parent, Some(f));
        let inner = b
            .add(
                Item::folder("Inner"),
                Place {
                    parent: Some(f),
                    index: 1,
                },
            )
            .unwrap();
        assert!(!b.move_to(
            f,
            Place {
                parent: Some(inner),
                index: 0
            }
        ));
        assert!(!b.move_to(
            c,
            Place {
                parent: Some(a),
                index: 0
            }
        ));
        let folders: Vec<String> = b
            .folders()
            .iter()
            .map(|(_, d, t)| format!("{d}{t}"))
            .collect();
        assert_eq!(folders, ["0Bookmarks bar", "1F", "2Inner"]);
        // Removing a folder removes what is in it.
        let v = b.version();
        assert!(b.remove(f).is_some());
        assert!(b.version() > v);
        assert_eq!(bar(&b), ["See"]);
        assert!(b.find(a).is_none());
    }

    fn tree() -> (Bookmarks, [BookmarkId; 5]) {
        let mut b = Bookmarks::in_memory();
        let at = |parent, index| Place { parent, index };
        let a = b
            .add(Item::page("Alpha", "https://a.example/", None), at(None, 9))
            .unwrap();
        let f = b.add(Item::folder("News"), at(None, 9)).unwrap();
        let c = b
            .add(
                Item::page("Charlie", "https://c.example/x", None),
                at(None, 9),
            )
            .unwrap();
        let n1 = b
            .add(
                Item::page("Paper", "https://news.example/", None),
                at(Some(f), 9),
            )
            .unwrap();
        let n2 = b.add(Item::folder("Old"), at(Some(f), 9)).unwrap();
        (b, [a, f, c, n1, n2])
    }

    fn labels(items: &[Item]) -> Vec<&str> {
        items.iter().map(|i| i.label()).collect()
    }

    #[test]
    fn a_delete_comes_back_where_it_was_with_its_ids() {
        let (mut b, [a, f, c, n1, _]) = tree();
        assert!(!b.can_undo_delete());
        // The page inside the deleted folder is not deleted twice.
        assert_eq!(b.delete(&[c, f, n1, a, 999]), 3);
        assert!(b.bar().is_empty());
        assert!(b.can_undo_delete());
        assert_eq!(b.undo_delete(), 3);
        assert_eq!(labels(b.bar()), ["Alpha", "News", "Charlie"]);
        assert_eq!(b.place_of(n1).map(|p| p.parent), Some(Some(f)));
        assert_eq!(
            b.find(c).map(|i| i.url.as_str()),
            Some("https://c.example/x")
        );
        assert!(!b.can_undo_delete());
        assert_eq!(b.undo_delete(), 0);
        // In a folder that went since: back at the bar's end.
        assert_eq!(b.delete(&[n1]), 1);
        b.remove(f);
        assert_eq!(b.undo_delete(), 1);
        assert_eq!(labels(b.bar()), ["Alpha", "Charlie", "Paper"]);
    }

    #[test]
    fn several_items_move_together_in_their_order() {
        let (mut b, [a, f, c, n1, n2]) = tree();
        let to = |parent, index| Place { parent, index };
        // Down the same list, to its end.
        assert!(b.move_all(&[a, f], to(None, 3)));
        assert_eq!(labels(b.bar()), ["Charlie", "Alpha", "News"]);
        // Up, to its start.
        assert!(b.move_all(&[a, f], to(None, 0)));
        assert_eq!(labels(b.bar()), ["Alpha", "News", "Charlie"]);
        // Into a folder; the folder itself does not go into itself.
        assert!(b.move_all(&[c, f, a], to(Some(f), 1)));
        assert_eq!(labels(b.bar()), ["News"]);
        let news = b.find(f).and_then(|i| i.children.as_deref()).unwrap();
        assert_eq!(labels(news), ["Paper", "Charlie", "Alpha", "Old"]);
        assert!(!b.move_all(&[f], to(Some(n2), 0)));
        let _ = n1;
    }

    #[test]
    fn sorting_puts_folders_first_then_titles_ignoring_case() {
        let mut b = Bookmarks::in_memory();
        for (t, folder) in [
            ("beta", false),
            ("Zed", true),
            ("Alpha", false),
            ("apple", true),
        ] {
            let item = if folder {
                Item::folder(t)
            } else {
                Item::page(t, "https://x.example/", None)
            };
            b.add(
                item,
                Place {
                    parent: None,
                    index: 9,
                },
            );
        }
        assert!(b.sort(None));
        assert_eq!(labels(b.bar()), ["apple", "Zed", "Alpha", "beta"]);
        assert!(!b.sort(None));
        assert!(!b.sort(Some(12345)));
    }

    #[test]
    fn a_search_matches_every_word_in_the_title_or_in_the_address() {
        let (b, _) = tree();
        let found = |q: &str| {
            b.search(q)
                .iter()
                .map(|i| i.label().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(found("news"), ["News", "Paper"]);
        assert_eq!(found("PAP"), ["Paper"]);
        assert_eq!(found("c.example x"), ["Charlie"]);
        // A word in the title and another in the address: no match.
        assert!(found("charlie c.example").is_empty());
        assert!(found("  ").is_empty());
    }
}
