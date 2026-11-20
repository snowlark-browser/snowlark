// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! How often a pass lays out each box of flex containers. LayoutNG takes intrinsic widths without
//! layout and reuses a measured result for the final layout when the sizes match
//! (`CalculateSizeBasedLayoutCacheStatus`); a box laid out twice in one pass shows in
//! `LayoutStats::relaid`.

use crate::style::*;
use crate::tests::*;

/// A float (sized shrink-to-fit, so from its content's intrinsic widths) holding a flex container
/// in `direction` whose two items are blocks of Ahem text.
fn float_around_flex(
    direction: FlexDirection,
) -> (T, snowlark_dom::NodeId, [snowlark_dom::NodeId; 2]) {
    let mut t = T::new();
    let root = t.root();
    let float = t.doc.block(root, |s| s.float = Float::Left);
    let flex = t.doc.block(float, |s| {
        s.display = Display::FLEX;
        s.flex_direction = direction;
    });
    let a = t.doc.block(flex, |_| {});
    t.doc.text(a, "ab cd");
    let b = t.doc.block(flex, |_| {});
    t.doc.text(b, "efg");
    (t, float, [a, b])
}

#[test]
fn a_column_containers_intrinsic_width_lays_out_no_item() {
    // css-flexbox-1 §9.9.2: a single-line container's max-content cross size is its items' largest
    // max-content contribution, which takes no layout. Each item is then laid out once, at the
    // float's width.
    let (mut t, float, [a, b]) = float_around_flex(FlexDirection::Column);
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, float), r(0.0, 0.0, 50.0, 20.0));
    assert_eq!(rect(&f, a), r(0.0, 0.0, 50.0, 10.0));
    assert_eq!(rect(&f, b), r(0.0, 10.0, 50.0, 10.0));
    let s = t.layout.stats();
    assert_eq!(s.relaid, 0, "{s:?}");
}

#[test]
fn a_row_containers_intrinsic_width_lays_out_no_item() {
    // The items' max-content widths (css-flexbox-1 §9.9.1) are their intrinsic sizes; asked for a
    // width only, an item is not laid out for a height nobody reads. Measured for its height at its
    // final width and then laid out at that height, an item is laid out once (the size-based hit).
    let (mut t, float, [a, b]) = float_around_flex(FlexDirection::Row);
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, float), r(0.0, 0.0, 80.0, 10.0));
    assert_eq!(rect(&f, a), r(0.0, 0.0, 50.0, 10.0));
    assert_eq!(rect(&f, b), r(50.0, 0.0, 30.0, 10.0));
    let s = t.layout.stats();
    assert_eq!(s.relaid, 0, "{s:?}");
}

#[test]
fn an_item_measured_for_its_height_is_laid_out_once() {
    // A column container's items are measured for their content height at the stretched width, then
    // laid out at that height: the measured layout holds (css-flexbox-1 §9.2 step 3, §9.4).
    let mut t = T::new();
    let root = t.root();
    let flex = t.doc.block(root, |s| {
        s.display = Display::FLEX;
        s.flex_direction = FlexDirection::Column;
    });
    let items: Vec<_> = (0..3)
        .map(|_| {
            let item = t.doc.block(flex, |_| {});
            let p = t.doc.block(item, |_| {});
            t.doc.text(p, "ab cd ef");
            item
        })
        .collect();
    let f = t.run(40.0, 400.0);
    for (i, &item) in items.iter().enumerate() {
        assert_eq!(rect(&f, item), r(0.0, 30.0 * i as f32, 40.0, 30.0));
    }
    let s = t.layout.stats();
    assert_eq!(s.relaid, 0, "{s:?}");
}

#[test]
fn an_item_whose_content_resolves_a_percentage_height_is_laid_out_again() {
    // An item with a definite flex basis in a container of definite height has a definite height
    // (css-flexbox-1 §9.8), so its child's `height: 50%` resolves at the final layout though it was
    // `auto` when the item was measured for its automatic minimum size (§4.5): the final layout is
    // not the measured one, even at the height the item measured.
    let mut t = T::new();
    let root = t.root();
    let flex = t.doc.block(root, |s| {
        s.display = Display::FLEX;
        s.flex_direction = FlexDirection::Column;
        s.height = SizeValue::px(100.0);
    });
    let item = t.doc.block(flex, |s| {
        s.flex_grow = 1.0;
        s.flex_basis = FlexBasis::LengthPercentage(LengthPercentage::px(0.0));
    });
    t.doc.block(item, |s| s.height = SizeValue::px(100.0));
    let half = t.doc.block(item, |s| s.height = SizeValue::percent(50.0));
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, item), r(0.0, 0.0, 400.0, 100.0));
    assert_eq!(rect(&f, half), r(0.0, 100.0, 400.0, 50.0));
}

#[test]
fn a_layout_under_another_containing_block_width_holds_without_percentages() {
    // Blink compares the resolved inline size and padding: a box with no percentage in its widths,
    // margins or padding lays out the same under any containing block width; one with percentage
    // padding does not.
    let mut t = T::new();
    let root = t.root();
    let plain = t.doc.block(root, |s| s.width = SizeValue::px(50.0));
    let padded = t.doc.block(root, |s| {
        s.width = SizeValue::px(50.0);
        s.padding[3] = LengthPercentage::percent(10.0);
    });
    t.run(400.0, 400.0);
    let tree = t.layout.boxes();
    let same = |node| {
        let id = tree.box_of(node).unwrap();
        let cached = tree.b(id).cache.layout.as_deref().unwrap();
        let mut space = cached.space;
        space.cb_width += 100.0;
        crate::layout::same_layout_in(tree, crate::logical::Flip::IDENTITY, id, cached, &space)
    };
    assert!(same(plain));
    assert!(!same(padded));
}
#[test]
fn an_item_with_percentage_padding_takes_its_containers_width() {
    // Percentage padding resolves against the containing block's width (CSS 2 §8.4), so a layout
    // under another containing block width is not reused for it. (It counts as zero in the item's
    // intrinsic width, CSS Sizing 3 §5.2.1, so the item is 20px wide and its content overflows.)
    let mut t = T::new();
    let root = t.root();
    let flex = t.doc.block(root, |s| {
        s.display = Display::FLEX;
        s.width = SizeValue::px(200.0);
    });
    let item = t.doc.block(flex, |s| {
        s.padding[3] = LengthPercentage::percent(10.0);
    });
    let inner = t.doc.block(item, |_| {});
    t.doc.text(inner, "ab");
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, item), r(0.0, 0.0, 20.0, 10.0));
    assert_eq!(rect(&f, inner).x, 20.0);
}

/// A flex row of eight 40px "cards" (each a flex container holding a flex container holding a
/// block of text) and a block of text, before the cards when `text_first`.
fn cards(
    text_first: bool,
) -> (
    T,
    Vec<snowlark_dom::NodeId>,
    snowlark_dom::NodeId,
    snowlark_dom::NodeId,
) {
    let mut t = T::new();
    let root = t.root();
    let row = t.doc.block(root, |s| s.display = Display::FLEX);
    let item = |t: &mut T| {
        let item = t.doc.block(row, |_| {});
        (item, t.doc.text(item, "cd"))
    };
    let first = text_first.then(|| item(&mut t));
    let cards = (0..8)
        .map(|_| {
            let card = t.doc.block(row, |s| {
                s.display = Display::FLEX;
                s.width = SizeValue::px(40.0);
            });
            let inner = t.doc.block(card, |s| s.display = Display::FLEX);
            let p = t.doc.block(inner, |_| {});
            t.doc.text(p, "ab");
            card
        })
        .collect();
    let (item, text) = first.unwrap_or_else(|| item(&mut t));
    (t, cards, item, text)
}

/// The tree laid out again from a full build gives the same fragments.
fn assert_same_as_a_full_build(t: &mut T, incremental: &F) {
    t.layout.build(&t.doc.dom, t.doc.document, &t.doc.styles);
    let full = t.layout.layout(&t.text, t.viewport);
    assert_eq!(incremental.dump(), full.dump());
}

#[test]
fn a_change_in_one_item_keeps_the_fragments_of_the_nested_containers_taffy_kept() {
    // The text item's new width lays the row out again; the cards' layouts are taffy cache hits
    // (their sizes did not change), so their fragments stand, as a clean subtree's do in our flow.
    // Only the row's own fragment is built again.
    for text_first in [false, true] {
        let (mut t, cards, item, text) = cards(text_first);
        t.run(400.0, 400.0);
        t.doc.dom.set_data(text, "cdef");
        assert!(t.layout.text_changed(&t.doc.dom, text));
        let f = t.relayout();
        let s = t.layout.stats();
        assert_eq!(s.containers_built, 1, "text first: {text_first} {s:?}");
        // (the cards after the item moved by its extra 20px)
        let x0 = if text_first { 40.0 } else { 0.0 };
        for (i, &card) in cards.iter().enumerate() {
            assert_eq!(rect(&f, card), r(x0 + 40.0 * i as f32, 0.0, 40.0, 10.0));
        }
        assert_eq!(rect(&f, item).width, 40.0);
        assert_same_as_a_full_build(&mut t, &f);
    }
}

#[test]
fn a_change_in_one_row_of_a_column_runs_no_other_row_again() {
    // A column container of flex rows (a page of cards): text in one card changes. The column is
    // laid out again; every other row keeps its size, so its layout is a cache hit and its
    // fragment stands. Only the column and the changed row run their algorithms.
    let mut t = T::new();
    let root = t.root();
    let column = t.doc.block(root, |s| {
        s.display = Display::FLEX;
        s.flex_direction = FlexDirection::Column;
    });
    let texts: Vec<_> = (0..12)
        .map(|_| {
            let row = t.doc.block(column, |s| s.display = Display::FLEX);
            (0..3)
                .map(|_| {
                    let card = t.doc.block(row, |s| s.flex_grow = 1.0);
                    t.doc.text(card, "ab cd")
                })
                .collect::<Vec<_>>()
        })
        .collect();
    t.run(400.0, 400.0);
    let text = texts[5][1];
    t.doc.dom.set_data(
        text,
        "ab cd ef gh ij kl mn op qr st uv wx yz ab cd ef gh ij kl mn",
    );
    assert!(t.layout.text_changed(&t.doc.dom, text));
    let f = t.relayout();
    let s = t.layout.stats();
    assert_eq!(s.containers_built, 2, "{s:?}");
    // (the column's and the changed row's measurements and layouts; the eleven other rows: none)
    assert!((2..=4).contains(&s.algorithm_runs), "{s:?}");
    assert_same_as_a_full_build(&mut t, &f);
}

/// A row of a nested row container (holding `item`) and a 73px block, in a column: the nested
/// row is measured for its flex base size with no height available, then again within the
/// row's height once that is known.
fn nested_rows(
    item: impl FnOnce(&mut crate::testing::TestStyle),
) -> (T, [snowlark_dom::NodeId; 3]) {
    let mut t = T::new();
    let root = t.root();
    let column = t.doc.block(root, |s| {
        s.display = Display::FLEX;
        s.flex_direction = FlexDirection::Column;
    });
    let row = t.doc.block(column, |s| {
        s.display = Display::FLEX;
        s.align_items = Align::Center;
    });
    let nested = t.doc.block(row, |s| s.display = Display::FLEX);
    let a = t.doc.block(nested, item);
    t.doc.text(a, "ab cd");
    let b = t.doc.block(nested, |_| {});
    t.doc.text(b, "efg");
    t.doc.block(row, |s| {
        s.width = SizeValue::px(30.0);
        s.height = SizeValue::px(73.0);
    });
    (t, [nested, a, b])
}

#[test]
fn a_flex_container_is_measured_once_whatever_height_is_available() {
    // Its items turn no height into a width, so the nested row's measurements do not change with
    // the height available to it (css-flexbox-1 §9.2 step 3, §9.4): each is a cache hit the
    // second time.
    let (mut t, [nested, a, b]) = nested_rows(|_| {});
    let f = t.run(400.0, 400.0);
    let s = t.layout.stats();
    assert_eq!(rect(&f, nested), r(0.0, 31.5, 80.0, 10.0));
    assert_eq!(rect(&f, a), r(0.0, 31.5, 50.0, 10.0));
    assert_eq!(rect(&f, b), r(50.0, 31.5, 30.0, 10.0));
    // (the column, the row and the nested row each measured and laid out; without the fold the
    // nested row runs three more times, for the row's height)
    assert_eq!(s.algorithm_runs, 7, "{s:?}");
    assert_same_as_a_full_build(&mut t, &f);
}

#[test]
fn a_flex_container_whose_item_has_a_ratio_is_measured_in_the_height_available() {
    // An item with `aspect-ratio` takes its width from the height it is stretched to (css-sizing-4
    // §5): the nested row's measurements depend on the height available to it.
    let (mut t, [nested, a, _]) = nested_rows(|s| {
        s.aspect_ratio = AspectRatio {
            auto: false,
            ratio: Some(1.0),
        };
    });
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, nested), r(0.0, 11.5, 80.0, 50.0));
    assert_eq!(rect(&f, a), r(0.0, 11.5, 50.0, 50.0));
    let s = t.layout.stats();
    assert_eq!(s.algorithm_runs, 10, "{s:?}");
    assert_same_as_a_full_build(&mut t, &f);
}

#[test]
fn a_nested_container_laid_out_again_builds_its_fragment_again() {
    // A change inside a card marks it dirty: taffy runs it again and its fragment is built again.
    let (mut t, cards, _, _) = cards(false);
    t.run(400.0, 400.0);
    let card = cards[3];
    let inner = t.doc.dom.first_child(card).unwrap();
    let p = t.doc.dom.first_child(inner).unwrap();
    let text = t.doc.dom.first_child(p).unwrap();
    t.doc.dom.set_data(text, "abcdef");
    assert!(t.layout.text_changed(&t.doc.dom, text));
    let f = t.relayout();
    let s = t.layout.stats();
    // The row, the card and its inner container.
    assert_eq!(s.containers_built, 3, "{s:?}");
    assert_eq!(rect(&f, p).width, 60.0);
    assert_same_as_a_full_build(&mut t, &f);
}
