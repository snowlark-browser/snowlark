// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Find in page over real fragments: the rendered text, and the highlights painted from node ranges.

use std::collections::HashMap;
use std::sync::Arc;

use snowlark_dom::NodeId;
use snowlark_paint::{Color, DisplayListBuilder, Item, Size};

use crate::find::*;
use crate::style::*;
use crate::tests::*;
use crate::*;

fn index(t: &T, f: &F) -> FindIndex {
    let items = find_items(f);
    FindIndex::build(&items, |n| t.doc.dom.character_data(n))
}

fn matches(idx: &FindIndex, q: &str) -> Vec<FindMatch> {
    let mut out = Vec::new();
    let mut c = idx.start(q).unwrap();
    assert!(idx.search(&mut c, &mut out, None, usize::MAX));
    out
}

#[test]
fn text_across_inline_elements_is_one_run_and_blocks_are_separate() {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |_| {});
    t.doc.text(p, "Hello ");
    let b = t.doc.span(p, |_| {});
    t.doc.text(b, "big");
    t.doc.text(p, " World");
    let q = t.doc.block(r0, |_| {});
    t.doc.text(q, "next");
    let f = t.run(400.0, 300.0);
    let idx = index(&t, &f);
    assert_eq!(idx.run_count(), 2);
    assert_eq!(idx.run_text(0), "Hello big World");
    assert_eq!(idx.run_text(1), "next");
    assert_eq!(matches(&idx, "O BIG w").len(), 1);
    // (a match never crosses a block)
    assert!(matches(&idx, "world next").is_empty());
}

#[test]
fn unrendered_and_hidden_text_does_not_match() {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |_| {});
    t.doc.text(p, "shown ");
    let gone = t.doc.span(p, |s| s.display = Display::None);
    t.doc.text(gone, "secret");
    let hidden = t.doc.span(p, |s| s.visibility = Visibility::Hidden);
    t.doc.text(hidden, "ghost");
    t.doc.text(p, " text");
    let f = t.run(400.0, 300.0);
    let idx = index(&t, &f);
    assert!(matches(&idx, "secret").is_empty());
    assert!(matches(&idx, "ghost").is_empty());
    assert_eq!(matches(&idx, "shown").len(), 1);
    assert_eq!(matches(&idx, "text").len(), 1);
}

#[test]
fn a_match_may_wrap_onto_the_next_line() {
    let mut t = T::new();
    let r0 = t.root();
    // (Ahem: 10px per character; 70px holds 7 characters)
    let p = t.doc.block(r0, |s| s.width = SizeValue::px(70.0));
    t.doc.text(p, "aaaa bbbb cccc");
    let f = t.run(400.0, 300.0);
    assert!(lines(&f, p).len() >= 2);
    let idx = index(&t, &f);
    assert_eq!(idx.run_text(0), "aaaa bbbb cccc");
    assert_eq!(matches(&idx, "bbbb cccc").len(), 1);
}

#[test]
fn highlights_paint_behind_the_text_in_yellow_and_orange() {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |_| {});
    let text = t.doc.text(p, "abc abc");
    let f = t.run(400.0, 300.0);
    let idx = index(&t, &f);
    let ms = matches(&idx, "abc");
    assert_eq!(ms.len(), 2);
    let mut ranges: HashMap<NodeId, Vec<(u32, u32, u32)>> = HashMap::new();
    for (id, m) in ms.iter().enumerate() {
        idx.ranges(m, |n, lo, hi| {
            ranges.entry(n).or_default().push((lo, hi, id as u32))
        });
    }
    assert_eq!(ranges[&text], vec![(0, 3, 0), (4, 7, 1)]);
    let highlights = FindHighlights {
        ranges: Arc::new(ranges),
        current: Some(1),
    };
    let size = Size::new(400.0, 300.0);
    let mut opts = PaintOptions::viewport(size);
    opts.find = Some(&highlights);
    let mut b = DisplayListBuilder::new();
    paint_with(&f, &t.text, &opts, &mut b);
    let list = b.build();
    let fills: Vec<(f32, f32, Color)> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::FillRect { rect, color } if *color == MATCH_COLOR || *color == ACTIVE_COLOR => {
                Some((rect.x, rect.width, *color))
            }
            _ => None,
        })
        .collect();
    // 10px characters: "abc" at 0..30 (yellow), the second at 40..70 (orange).
    assert_eq!(
        fills,
        vec![(0.0, 30.0, MATCH_COLOR), (40.0, 30.0, ACTIVE_COLOR)]
    );
    // Without highlights nothing is added.
    let mut b = DisplayListBuilder::new();
    paint_with(&f, &t.text, &PaintOptions::viewport(size), &mut b);
    assert!(!b.build().items.iter().any(|i| matches!(
        i,
        Item::FillRect { color, .. } if *color == MATCH_COLOR || *color == ACTIVE_COLOR
    )));
}
