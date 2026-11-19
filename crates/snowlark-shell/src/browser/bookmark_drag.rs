// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A bookmark drag: an item taken from the bar or from a folder's drop-down and dropped between
//! the bar's items, into a folder on the bar (its drop-down opens while the pointer rests on it),
//! or in an open drop-down before or after a row or onto a folder row, nested drop-downs
//! included. This is the part the tests drive: what a pointer position means for the drag. The
//! window's half (the press, the timer, the drop and Escape) is in `bookmarks_ui.rs`.

use snowlark_paint::Point;

use super::commands::Command;
use crate::bookmarks::{BookmarkId, Bookmarks, Item, Place};
use crate::chrome::{BarLayout, DROP_EDGE};
use crate::menu::{DropPosition, Entry, MenuStack};

/// How long the pointer rests on a folder (or the "»" button) before its drop-down opens.
pub const OPEN_DELAY_MS: u32 = 400;
/// How far a press moves before it is a drag.
pub const DRAG_THRESHOLD: f32 = 5.0;
/// How far below the bar a drag still drops on it.
const BAR_REACH: f32 = 12.0;

/// Where a dragged bookmark would drop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DropTarget {
    /// Between the bar's items: the insertion marker's x.
    Bar { place: Place, marker_x: f32 },
    /// Onto a folder on the bar (its item index), at the end of it.
    BarFolder { index: usize, place: Place },
    /// Over the "»" button: nothing drops there, but the menu of the items that don't fit opens.
    Overflow,
    /// A row of an open drop-down (menu depth, row): before or after it, or onto a folder row.
    Menu {
        depth: usize,
        row: usize,
        position: DropPosition,
        place: Place,
    },
}

/// What opens when the pointer rests on a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opens {
    /// The drop-down of the bar's folder at this index.
    BarFolder(usize),
    /// The "»" menu.
    Overflow,
    /// The submenu of this (depth, row) folder row.
    Submenu(usize, usize),
}

impl DropTarget {
    /// Where the item goes when dropped here.
    pub fn place(&self) -> Option<Place> {
        match self {
            DropTarget::Bar { place, .. }
            | DropTarget::BarFolder { place, .. }
            | DropTarget::Menu { place, .. } => Some(*place),
            DropTarget::Overflow => None,
        }
    }

    pub fn opens(&self) -> Option<Opens> {
        match self {
            DropTarget::BarFolder { index, .. } => Some(Opens::BarFolder(*index)),
            DropTarget::Overflow => Some(Opens::Overflow),
            DropTarget::Menu {
                depth,
                row,
                position: DropPosition::On,
                ..
            } => Some(Opens::Submenu(*depth, *row)),
            _ => None,
        }
    }

    pub fn on_bar(&self) -> bool {
        !matches!(self, DropTarget::Menu { .. })
    }
}

/// A press on a bookmark (on the bar or in a drop-down) that may become a drag.
#[derive(Clone, Debug)]
pub struct BookmarkDrag {
    pub id: BookmarkId,
    /// The bar item pressed (`None`: a drop-down's row). A press that never becomes a drag is a
    /// click on it.
    pub bar_item: Option<usize>,
    start: Point,
    pub dragging: bool,
    pub target: Option<DropTarget>,
    /// The drop-down open now was opened by this drag resting on a folder: it closes when the
    /// drag moves elsewhere on the bar.
    pub opened: bool,
}

impl BookmarkDrag {
    pub fn new(id: BookmarkId, bar_item: Option<usize>, start: Point) -> BookmarkDrag {
        BookmarkDrag {
            id,
            bar_item,
            start,
            dragging: false,
            target: None,
            opened: false,
        }
    }

    /// The pointer moved to `at`: whether the press is a drag (now or before).
    pub fn moved(&mut self, at: Point) -> bool {
        if !self.dragging
            && (at.x - self.start.x).abs() + (at.y - self.start.y).abs() < DRAG_THRESHOLD
        {
            return false;
        }
        self.dragging = true;
        true
    }
}

/// Where `dragged` would drop with the pointer at `dip` (chrome DIPs) and `cp` (the same point
/// in content-area DIPs, where the menus are): a row of an open drop-down first, then the bar.
/// `None` where nothing drops: outside both, on a row that is not a bookmark, or at a place the
/// item cannot go (a folder into itself, or where it already is).
pub fn drop_target(
    bookmarks: &Bookmarks,
    dragged: BookmarkId,
    bar: Option<&BarLayout>,
    dip: Point,
    menus: Option<&MenuStack<Command>>,
    cp: Point,
) -> Option<DropTarget> {
    if let Some(depth) = menus.and_then(|s| s.menu_at(cp)) {
        let m = &menus?.menus[depth];
        let row = m.row_at(cp)?;
        let rect = m.row_rect(row)?;
        let Entry::Item {
            action, enabled, ..
        } = &m.entries[row]
        else {
            return None;
        };
        // (a disabled folder row is the "(empty)" placeholder of that folder)
        let (id, folder, empty) = match action {
            Command::OpenBookmark(id, _) => (*id, false, false),
            Command::OpenFolder(id) => (*id, true, !*enabled),
            _ => return None,
        };
        let y = cp.y - rect.y;
        let position = if empty || (folder && y > DROP_EDGE && y < rect.height - DROP_EDGE) {
            DropPosition::On
        } else if y < rect.height / 2.0 {
            DropPosition::Before
        } else {
            DropPosition::After
        };
        let place = match position {
            DropPosition::On => Place {
                parent: Some(id),
                index: usize::MAX,
            },
            DropPosition::Before => bookmarks.place_of(id)?,
            DropPosition::After => {
                let p = bookmarks.place_of(id)?;
                Place {
                    parent: p.parent,
                    index: p.index + 1,
                }
            }
        };
        return bookmarks
            .can_move(dragged, place)
            .then_some(DropTarget::Menu {
                depth,
                row,
                position,
                place,
            });
    }
    let bar = bar?;
    if !bar.rect.inflate(0.0, BAR_REACH).contains(dip) {
        return None;
    }
    if bar
        .overflow
        .is_some_and(|o| dip.x >= o.x && dip.x < o.right())
    {
        return Some(DropTarget::Overflow);
    }
    let folders: Vec<bool> = bookmarks.bar().iter().map(Item::is_folder).collect();
    let (index, into) = bar.drop_target(dip.x, &folders);
    let target = match into {
        Some(i) => DropTarget::BarFolder {
            index: i,
            place: Place {
                parent: Some(bookmarks.bar().get(i)?.id),
                index: usize::MAX,
            },
        },
        None => DropTarget::Bar {
            place: Place {
                parent: None,
                index,
            },
            marker_x: bar.marker_x(index),
        },
    };
    bookmarks
        .can_move(dragged, target.place()?)
        .then_some(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::Fonts;
    use crate::menu::{Menu, MenuIcon};
    use snowlark_paint::Size;
    use std::sync::Arc;

    const AREA: Size = Size {
        width: 800.0,
        height: 600.0,
    };

    fn fonts() -> Fonts {
        Fonts::new(Arc::new(snowlark_paint::TextEngine::new().unwrap()))
    }

    /// The bar: A, F (p1, p2, Inner (q)), B.
    struct Tree {
        b: Bookmarks,
        a: BookmarkId,
        f: BookmarkId,
        p1: BookmarkId,
        p2: BookmarkId,
        inner: BookmarkId,
        q: BookmarkId,
        bb: BookmarkId,
    }

    fn tree() -> Tree {
        let mut b = Bookmarks::in_memory();
        let at = |parent, index| Place { parent, index };
        let a = b
            .add(Item::page("A", "https://a/", None), at(None, 0))
            .unwrap();
        let f = b.add(Item::folder("F"), at(None, 1)).unwrap();
        let bb = b
            .add(Item::page("B", "https://b/", None), at(None, 2))
            .unwrap();
        let p1 = b
            .add(Item::page("p1", "https://p1/", None), at(Some(f), 0))
            .unwrap();
        let p2 = b
            .add(Item::page("p2", "https://p2/", None), at(Some(f), 1))
            .unwrap();
        let inner = b.add(Item::folder("Inner"), at(Some(f), 2)).unwrap();
        let q = b
            .add(Item::page("q", "https://q/", None), at(Some(inner), 0))
            .unwrap();
        Tree {
            b,
            a,
            f,
            p1,
            p2,
            inner,
            q,
            bb,
        }
    }

    /// A folder's drop-down as `folder_entries` builds it.
    fn entries(b: &Bookmarks, folder: BookmarkId) -> Vec<Entry<Command>> {
        let children = b.find(folder).unwrap().children.as_ref().unwrap();
        let mut out: Vec<Entry<Command>> = children
            .iter()
            .map(|i| {
                if i.is_folder() {
                    Entry::item(i.label(), Command::OpenFolder(i.id)).submenu()
                } else {
                    Entry::item(i.label(), Command::OpenBookmark(i.id, false))
                        .icon(MenuIcon::Glyph('g'))
                }
            })
            .collect();
        if out.is_empty() {
            out.push(Entry::item("(empty)", Command::OpenFolder(folder)).enabled(false));
        }
        if children.iter().filter(|i| !i.is_folder()).count() > 1 {
            out.push(Entry::Separator);
            out.push(Entry::item(
                "Open all (2)",
                Command::OpenAllInFolder(folder),
            ));
        }
        out
    }

    fn labels(b: &Bookmarks, folder: Option<BookmarkId>) -> Vec<String> {
        let items = match folder {
            None => b.bar(),
            Some(f) => b.find(f).unwrap().children.as_deref().unwrap(),
        };
        items.iter().map(|i| i.label().to_owned()).collect()
    }

    /// The bar's layout under the chrome, 100 DIPs per item, and F's drop-down open under it.
    fn open_f(t: &Tree, fonts: &Fonts) -> (BarLayout, MenuStack<Command>) {
        let bar = BarLayout::new(600.0, 40.0, &[100.0, 100.0, 100.0]);
        let menu = Menu::new(
            entries(&t.b, t.f),
            Point::new(bar.items[1].x, 0.0),
            None,
            AREA,
            fonts,
        );
        (bar, MenuStack::new(menu, false))
    }

    /// The point `frac` of the way down row `row` of menu `depth` (content DIPs), and the same
    /// point in chrome DIPs (the bar is 40 DIPs tall above the content area).
    fn at_row(stack: &MenuStack<Command>, depth: usize, row: usize, frac: f32) -> (Point, Point) {
        let r = stack.menus[depth].row_rect(row).unwrap();
        let cp = Point::new(r.x + 50.0, r.y + r.height * frac);
        (Point::new(cp.x, cp.y + 80.0), cp)
    }

    fn menu_place(t: DropTarget) -> (usize, usize, DropPosition, Place) {
        match t {
            DropTarget::Menu {
                depth,
                row,
                position,
                place,
            } => (depth, row, position, place),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reorders_within_a_folders_drop_down() {
        let f = fonts();
        let mut t = tree();
        let (bar, stack) = open_f(&t, &f);
        // p2 over p1's top half: before p1.
        let (dip, cp) = at_row(&stack, 0, 0, 0.3);
        let target = drop_target(&t.b, t.p2, Some(&bar), dip, Some(&stack), cp).unwrap();
        let (depth, row, position, place) = menu_place(target);
        assert_eq!((depth, row, position), (0, 0, DropPosition::Before));
        assert_eq!(place.parent, Some(t.f));
        assert_eq!(place.index, 0);
        assert!(t.b.move_to(t.p2, place));
        assert_eq!(labels(&t.b, Some(t.f)), ["p2", "p1", "Inner"]);
        // p2 (first now) over p1's bottom half (the rows still stand for p1, p2, Inner: the
        // drop-down closes with a drop, so no row moves): after p1, so last of the pages.
        let (dip, cp) = at_row(&stack, 0, 0, 0.8);
        let target = drop_target(&t.b, t.p2, Some(&bar), dip, Some(&stack), cp).unwrap();
        let (_, _, position, place) = menu_place(target);
        assert_eq!(position, DropPosition::After);
        assert_eq!((place.parent, place.index), (Some(t.f), 2));
        assert!(t.b.move_to(t.p2, place));
        assert_eq!(labels(&t.b, Some(t.f)), ["p1", "p2", "Inner"]);
        // Over its own row, or just after itself: nowhere to go, so no target.
        let (dip, cp) = at_row(&stack, 0, 1, 0.3);
        assert_eq!(
            drop_target(&t.b, t.p2, Some(&bar), dip, Some(&stack), cp),
            None
        );
        // The separator and "Open all" rows take nothing.
        let (dip, cp) = at_row(&stack, 0, 3, 0.5);
        assert_eq!(
            drop_target(&t.b, t.p1, Some(&bar), dip, Some(&stack), cp),
            None
        );
        let (dip, cp) = at_row(&stack, 0, 4, 0.5);
        assert_eq!(
            drop_target(&t.b, t.p1, Some(&bar), dip, Some(&stack), cp),
            None
        );
    }

    #[test]
    fn drops_from_the_bar_into_a_folder_row_and_before_it_by_its_edges() {
        let f = fonts();
        let mut t = tree();
        let (bar, stack) = open_f(&t, &f);
        // A over the middle of the Inner row: into Inner, at the end.
        let (dip, cp) = at_row(&stack, 0, 2, 0.5);
        let target = drop_target(&t.b, t.a, Some(&bar), dip, Some(&stack), cp).unwrap();
        let (_, row, position, place) = menu_place(target);
        assert_eq!((row, position), (2, DropPosition::On));
        assert_eq!(target.opens(), Some(Opens::Submenu(0, 2)));
        assert_eq!(place.parent, Some(t.inner));
        // Within the edge at its top: before it, in F.
        let r = stack.menus[0].row_rect(2).unwrap();
        let cp = Point::new(r.x + 50.0, r.y + DROP_EDGE - 1.0);
        let dip = Point::new(cp.x, cp.y + 80.0);
        let before = drop_target(&t.b, t.a, Some(&bar), dip, Some(&stack), cp).unwrap();
        let (_, _, position, p) = menu_place(before);
        assert_eq!(position, DropPosition::Before);
        assert_eq!((p.parent, p.index), (Some(t.f), 2));
        assert!(t.b.move_to(t.a, place));
        assert_eq!(labels(&t.b, None), ["F", "B"]);
        assert_eq!(labels(&t.b, Some(t.inner)), ["q", "A"]);
    }

    #[test]
    fn drops_out_of_a_folder_onto_the_bar_and_into_a_bar_folder() {
        let f = fonts();
        let mut t = tree();
        let (bar, stack) = open_f(&t, &f);
        // p1 dragged up between A and F (the bar is above the menu).
        let dip = Point::new(bar.items[0].right() - 10.0, bar.rect.y + 10.0);
        let cp = Point::new(dip.x, dip.y - 80.0);
        let target = drop_target(&t.b, t.p1, Some(&bar), dip, Some(&stack), cp).unwrap();
        let DropTarget::Bar { place, marker_x } = target else {
            panic!("{target:?}")
        };
        assert_eq!((place.parent, place.index), (None, 1));
        assert!((marker_x - bar.items[1].x).abs() < 3.0);
        assert!(t.b.move_to(t.p1, place));
        assert_eq!(labels(&t.b, None), ["A", "p1", "F", "B"]);
        assert_eq!(labels(&t.b, Some(t.f)), ["p2", "Inner"]);
        // B over the middle of F's button: into F; the drop-down opens for it.
        let bar = BarLayout::new(600.0, 40.0, &[100.0, 100.0, 100.0, 100.0]);
        let dip = Point::new(bar.items[2].x + 50.0, bar.rect.y + 10.0);
        let target = drop_target(&t.b, t.bb, Some(&bar), dip, None, cp).unwrap();
        assert_eq!(target.opens(), Some(Opens::BarFolder(2)));
        assert!(target.on_bar());
        assert_eq!(target.place().unwrap().parent, Some(t.f));
        // Just inside its left edge: before it instead.
        let dip = Point::new(bar.items[2].x + 2.0, bar.rect.y + 10.0);
        let target = drop_target(&t.b, t.bb, Some(&bar), dip, None, cp).unwrap();
        assert!(matches!(target, DropTarget::Bar { .. }));
        assert_eq!(target.place().unwrap().index, 2);
        // Just below the bar it still drops there; further down it does not.
        let dip = Point::new(bar.items[2].x + 50.0, bar.rect.bottom() + 8.0);
        assert!(drop_target(&t.b, t.bb, Some(&bar), dip, None, cp).is_some());
        let dip = Point::new(bar.items[2].x + 50.0, bar.rect.bottom() + 20.0);
        assert_eq!(drop_target(&t.b, t.bb, Some(&bar), dip, None, cp), None);
        // A narrow bar: over the "»" button the overflow menu opens, nothing drops.
        let bar = BarLayout::new(260.0, 40.0, &[100.0, 100.0, 100.0, 100.0]);
        let o = bar.overflow.unwrap();
        let dip = Point::new(o.x + 5.0, bar.rect.y + 10.0);
        let target = drop_target(&t.b, t.a, Some(&bar), dip, None, cp).unwrap();
        assert_eq!(target, DropTarget::Overflow);
        assert_eq!(target.place(), None);
    }

    #[test]
    fn drops_in_a_nested_drop_down_and_never_a_folder_into_itself() {
        let f = fonts();
        let mut t = tree();
        let (bar, mut stack) = open_f(&t, &f);
        // Inner's submenu open beside its row.
        let row = stack.menus[0].row_rect(2).unwrap();
        stack.open_sub(
            Menu::new(entries(&t.b, t.inner), Point::ZERO, Some(row), AREA, &f),
            false,
        );
        assert_eq!(stack.menus.len(), 2);
        // A over the bottom half of q (depth 1): after q, in Inner.
        let (dip, cp) = at_row(&stack, 1, 0, 0.8);
        let target = drop_target(&t.b, t.a, Some(&bar), dip, Some(&stack), cp).unwrap();
        let (depth, row, position, place) = menu_place(target);
        assert_eq!((depth, row, position), (1, 0, DropPosition::After));
        assert_eq!((place.parent, place.index), (Some(t.inner), 1));
        assert!(t.b.move_to(t.a, place));
        assert_eq!(labels(&t.b, Some(t.inner)), ["q", "A"]);
        // F onto Inner (its own child), and Inner onto itself: no target.
        let (dip, cp) = at_row(&stack, 0, 2, 0.5);
        assert_eq!(
            drop_target(&t.b, t.f, Some(&bar), dip, Some(&stack), cp),
            None
        );
        assert_eq!(
            drop_target(&t.b, t.inner, Some(&bar), dip, Some(&stack), cp),
            None
        );
        // But q before Inner's row (out of Inner, into F) is fine.
        let (dip, cp) = at_row(&stack, 0, 2, 0.05);
        let target = drop_target(&t.b, t.q, Some(&bar), dip, Some(&stack), cp).unwrap();
        assert_eq!(menu_place(target).2, DropPosition::Before);
        // An empty folder's "(empty)" row takes the drop into that folder.
        let empty =
            t.b.add(
                Item::folder("Empty"),
                Place {
                    parent: None,
                    index: 9,
                },
            )
            .unwrap();
        let menu = Menu::new(entries(&t.b, empty), Point::ZERO, None, AREA, &f);
        let stack = MenuStack::new(menu, false);
        let (dip, cp) = at_row(&stack, 0, 0, 0.1);
        let target = drop_target(&t.b, t.q, Some(&bar), dip, Some(&stack), cp).unwrap();
        let (_, _, position, place) = menu_place(target);
        assert_eq!(position, DropPosition::On);
        assert_eq!(place.parent, Some(empty));
        assert_eq!(target.opens(), Some(Opens::Submenu(0, 0)));
    }

    #[test]
    fn a_press_is_a_drag_past_the_threshold_and_nothing_drops_outside() {
        let f = fonts();
        let t = tree();
        let (bar, stack) = open_f(&t, &f);
        let mut d = BookmarkDrag::new(t.p1, None, Point::new(100.0, 100.0));
        assert!(!d.moved(Point::new(102.0, 101.0)));
        assert!(!d.dragging);
        assert!(d.moved(Point::new(104.0, 103.0)));
        assert!(d.dragging);
        assert!(d.moved(Point::new(100.0, 100.0)), "a drag stays one");
        // In the content area past the menu and below the bar: no target (a drop there, or
        // Escape, cancels).
        let dip = Point::new(700.0, 300.0);
        let cp = Point::new(700.0, 220.0);
        assert!(!stack.contains(cp));
        assert_eq!(
            drop_target(&t.b, t.p1, Some(&bar), dip, Some(&stack), cp),
            None
        );
        // In the menu's padding between the frame and the first row: nothing either.
        let m = &stack.menus[0];
        let cp = Point::new(m.rect.x + 20.0, m.rect.y + 2.0);
        assert_eq!(
            drop_target(
                &t.b,
                t.p1,
                Some(&bar),
                Point::new(cp.x, cp.y + 80.0),
                Some(&stack),
                cp
            ),
            None
        );
    }
}
