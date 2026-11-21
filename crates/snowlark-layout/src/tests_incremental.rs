// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Incremental box tree updates ([`LayoutTree::update`]) and the shaping cache.

use std::sync::Arc;

use snowlark_paint::{Color, FontFeature, FontListId, ShapeCache, ShapeKey, ShapedText, Size};

use crate::style::*;
use crate::tests::*;
use crate::*;

const LONG: &str = "A paragraph long enough that the old cache of short strings never kept its \
                    shaping, so before the shaping cache every regeneration reshaped it.";

/// A document of three paragraphs, the middle one with a span.
fn page() -> (T, [snowlark_dom::NodeId; 4]) {
    let mut t = T::new();
    let root = t.root();
    let a = t.doc.block(root, |_| {});
    t.doc.text(a, LONG);
    let b = t.doc.block(root, |_| {});
    t.doc.text(b, "before ");
    let span = t.doc.span(b, |_| {});
    t.doc.text(span, "span");
    t.doc.text(b, " after");
    let c = t.doc.block(root, |_| {});
    t.doc.text(c, LONG);
    (t, [a, b, span, c])
}

fn update(t: &mut T, changes: TreeChanges) -> UpdateStats {
    t.layout
        .update(&t.doc.dom, t.doc.document, &t.doc.styles, &changes)
}

fn restyled(node: snowlark_dom::NodeId, damage: BoxDamage) -> TreeChanges {
    TreeChanges {
        restyled: vec![ElementChange::restyled(node, damage, false)],
        ..Default::default()
    }
}

#[test]
fn a_repaint_change_takes_the_new_style_in_place() {
    let (mut t, [a, b, span, c]) = page();
    t.run(200.0, 400.0);
    let boxes = t.layout.boxes().len();
    t.doc.restyle(span, |s| s.color = Color::rgb(200, 0, 0));
    let stats = update(&mut t, restyled(span, BoxDamage::Repaint));
    assert_eq!(stats.restyled_in_place, 1);
    assert_eq!(stats.boxes_built, 0);
    assert_eq!(stats.containers_rebuilt, 0);
    assert_eq!(t.layout.boxes().len(), boxes);
    let f = t.relayout();
    // The span's paragraph and its ancestors are laid out again; the other paragraphs are reused.
    let s = t.layout.stats();
    assert!(s.laid_out <= 3, "{s:?}");
    assert!(s.reused >= 2, "{s:?}");
    assert_eq!(s.shaped, 0, "{s:?}");
    let mut colour = None;
    f.walk(&mut |fr, _| {
        if let FragmentKind::Text(text) = &fr.kind {
            if text.node.is_some_and(|n| t.doc.dom.parent(n) == Some(span)) {
                colour = Some(text.style.color());
            }
        }
    });
    assert_eq!(colour, Some(Color::rgb(200, 0, 0)));
    let _ = (a, b, c);
}

/// The colour of the text fragments of `parent`'s own text, and of `parent`'s box fragments.
fn colours(
    f: &F,
    dom: &snowlark_dom::Dom,
    parent: snowlark_dom::NodeId,
) -> (Vec<Color>, Vec<Color>) {
    let (mut text, mut boxes) = (Vec::new(), Vec::new());
    f.walk(&mut |fr, _| match &fr.kind {
        FragmentKind::Text(t) if t.node.is_some_and(|n| dom.parent(n) == Some(parent)) => {
            text.push(t.style.color())
        }
        FragmentKind::Box(b) if b.node == Some(parent) => boxes.push(b.style.color()),
        _ => {}
    });
    (text, boxes)
}

/// A paint-only change lays nothing out: the fragments take the new style where they stand, and
/// keep it when they are reused by a later layout (their boxes' caches took it too).
#[test]
fn a_paint_change_takes_the_new_style_without_layout() {
    let (mut t, [a, b, span, c]) = page();
    t.run(200.0, 400.0);
    let red = Color::rgb(200, 0, 0);
    t.doc.restyle(span, |s| s.color = red);
    let stats = update(&mut t, restyled(span, BoxDamage::Paint));
    assert_eq!(stats.restyled_in_place, 1);
    assert_eq!(stats.boxes_built, 0);
    let f = t.relayout();
    let s = t.layout.stats();
    assert_eq!(s.laid_out, 0, "{s:?}");
    assert_eq!(colours(&f, &t.doc.dom, span), (vec![red], vec![red]));
    // The paragraph's own text keeps its colour.
    assert_eq!(colours(&f, &t.doc.dom, b).0, vec![Color::BLACK; 2]);
    // A change elsewhere lays the root out again, reusing the paragraph's cached fragments.
    t.doc
        .restyle(c, |s| s.padding[0] = LengthPercentage::px(10.0));
    update(&mut t, restyled(c, BoxDamage::Relayout));
    let f = t.relayout();
    assert!(t.layout.stats().reused >= 2, "{:?}", t.layout.stats());
    assert_eq!(colours(&f, &t.doc.dom, span), (vec![red], vec![red]));
    // And when the paragraph itself is laid out again, its prepared lines have the new style.
    t.doc
        .restyle(b, |s| s.padding[0] = LengthPercentage::px(10.0));
    update(&mut t, restyled(b, BoxDamage::Relayout));
    let f = t.relayout();
    assert_eq!(t.layout.stats().shaped, 0);
    assert_eq!(colours(&f, &t.doc.dom, span), (vec![red], vec![red]));
    let _ = a;
}

/// Line breaking measures text with the `letter-spacing` and `word-spacing` its items read when
/// they were prepared: a change of either (layout damage) prepares them again, so the text is
/// measured and wrapped with the new spacing.
#[test]
fn a_spacing_change_measures_the_text_again() {
    let (mut t, [_, b, span, _]) = page();
    t.run(200.0, 400.0);
    let width = |f: &F, dom: &snowlark_dom::Dom| {
        let mut w = 0.0;
        f.walk(&mut |fr, _| {
            if let FragmentKind::Text(text) = &fr.kind {
                if text.node.is_some_and(|n| dom.parent(n) == Some(span)) {
                    w += fr.size.width;
                }
            }
        });
        w
    };
    let f = t.relayout();
    let plain = width(&f, &t.doc.dom);
    t.doc.restyle(span, |s| s.letter_spacing = 10.0);
    update(&mut t, restyled(span, BoxDamage::Relayout));
    let f = t.relayout();
    // ("span": four letters, each 10px wider)
    assert_eq!(width(&f, &t.doc.dom), plain + 40.0);
    // Word spacing on the paragraph: its two spaces widen, and the line no longer fits 200px.
    t.doc.restyle(b, |s| s.word_spacing = 100.0);
    update(&mut t, restyled(b, BoxDamage::Relayout));
    let f = t.relayout();
    let mut lines = 0;
    f.walk(&mut |fr, _| {
        if let FragmentKind::Box(bx) = &fr.kind {
            if bx.node == Some(b) {
                lines = fr.children.len();
            }
        }
    });
    assert!(lines >= 2, "{lines} line(s)");
}

/// A layout change that prepares inline content the same way (padding, here) keeps the prepared
/// items, which take the new style: the line is laid out with the span's new padding. A change
/// preparation reads (the font size) prepares the content again.
#[test]
fn a_layout_change_keeps_the_prepared_inline_content() {
    let (mut t, [_, b, span, _]) = page();
    t.run(400.0, 400.0);
    let span_x = |f: &F, dom: &snowlark_dom::Dom| {
        let mut x = None;
        f.walk(&mut |fr, at| {
            if let FragmentKind::Text(text) = &fr.kind {
                if text.node.is_some_and(|n| dom.parent(n) == Some(span)) {
                    x = Some(at.x);
                }
            }
        });
        x.expect("the span's text")
    };
    let f = t.relayout();
    let before = span_x(&f, &t.doc.dom);
    t.doc
        .restyle(span, |s| s.padding[3] = LengthPercentage::px(30.0));
    update(&mut t, restyled(span, BoxDamage::Relayout));
    let f = t.relayout();
    let s = t.layout.stats();
    assert_eq!(s.prepared, 0, "{s:?}");
    assert_eq!(span_x(&f, &t.doc.dom), before + 30.0);
    // The paragraph's own padding: kept too.
    t.doc
        .restyle(b, |s| s.padding[3] = LengthPercentage::px(5.0));
    update(&mut t, restyled(b, BoxDamage::Relayout));
    let f = t.relayout();
    assert_eq!(t.layout.stats().prepared, 0);
    assert_eq!(span_x(&f, &t.doc.dom), before + 35.0);
    // A new font size is prepared (and shaped) again.
    t.doc.restyle(span, |s| s.font_size = 30.0);
    update(&mut t, restyled(span, BoxDamage::Relayout));
    t.relayout();
    let s = t.layout.stats();
    assert_eq!(s.prepared, 1, "{s:?}");
    assert_eq!(s.shaped, 1, "{s:?}");
}

/// The same in a flex container, whose items' fragments are also kept in its own caches.
#[test]
fn a_paint_change_in_a_flex_item_keeps_through_the_containers_layout() {
    let mut t = T::new();
    let root = t.root();
    let flex = t.doc.block(root, |s| s.display = Display::FLEX);
    let items: Vec<_> = (0..3)
        .map(|i| {
            let item = t.doc.block(flex, |_| {});
            t.doc.text(item, &format!("item {i}"));
            item
        })
        .collect();
    t.run(400.0, 400.0);
    let blue = Color::rgb(0, 0, 200);
    t.doc.restyle(items[1], |s| {
        s.color = blue;
        s.background_color = blue;
    });
    update(&mut t, restyled(items[1], BoxDamage::Paint));
    let f = t.relayout();
    assert_eq!(t.layout.stats().laid_out, 0);
    let bg = |f: &F| {
        let mut out = Vec::new();
        f.walk(&mut |fr, _| {
            if let FragmentKind::Box(b) = &fr.kind {
                if b.node == Some(items[1]) {
                    out.push(b.style.background_color());
                }
            }
        });
        out
    };
    assert_eq!(colours(&f, &t.doc.dom, items[1]), (vec![blue], vec![blue]));
    assert_eq!(bg(&f), vec![blue]);
    // Another item grows: the container runs its algorithm again over its items' kept layouts.
    t.doc
        .restyle(items[2], |s| s.padding[0] = LengthPercentage::px(20.0));
    update(&mut t, restyled(items[2], BoxDamage::Relayout));
    let f = t.relayout();
    assert!(t.layout.stats().laid_out >= 1);
    assert_eq!(colours(&f, &t.doc.dom, items[1]), (vec![blue], vec![blue]));
    assert_eq!(bg(&f), vec![blue]);
}

/// The number of decorations each text fragment of `el`'s descendants draws, in tree order.
fn decorations(f: &F, dom: &snowlark_dom::Dom, el: snowlark_dom::NodeId) -> Vec<usize> {
    let mut out = Vec::new();
    f.walk(&mut |fr, _| {
        if let FragmentKind::Text(t) = &fr.kind {
            if t.node.is_some_and(|n| dom.ancestors(n).any(|a| a == el)) {
                out.push(t.decorations.len());
            }
        }
    });
    out
}

/// A text decoration change lays nothing out and regenerates no box: the boxes inside the element
/// (its own text, and a child element's that does not inherit the property) take the decorations
/// in place, and their fragments draw them, also when a later layout reuses them.
#[test]
fn a_decoration_change_takes_the_new_decorations_without_layout() {
    let (mut t, [a, b, span, c]) = page();
    t.run(200.0, 400.0);
    t.doc.restyle(b, |s| {
        s.text_decoration_line = TextDecorationLine::UNDERLINE
    });
    let stats = update(
        &mut t,
        TreeChanges {
            restyled: vec![ElementChange::restyled(b, BoxDamage::Paint, false).decorations(true)],
            ..Default::default()
        },
    );
    assert_eq!(stats.containers_rebuilt, 0, "{stats:?}");
    assert_eq!(stats.boxes_built, 0, "{stats:?}");
    let f = t.relayout();
    let s = t.layout.stats();
    assert_eq!(s.laid_out, 0, "{s:?}");
    assert_eq!(decorations(&f, &t.doc.dom, b), vec![1; 3]);
    // A change elsewhere reuses the paragraph's cached fragments, which drew them too.
    t.doc
        .restyle(c, |s| s.padding[0] = LengthPercentage::px(10.0));
    update(&mut t, restyled(c, BoxDamage::Relayout));
    let f = t.relayout();
    assert!(t.layout.stats().reused >= 2, "{:?}", t.layout.stats());
    assert_eq!(decorations(&f, &t.doc.dom, b), vec![1; 3]);
    // And so does the paragraph laid out again, and the decoration taken away.
    t.doc.restyle(b, |s| {
        s.text_decoration_line = TextDecorationLine::NONE;
        s.padding[0] = LengthPercentage::px(10.0);
    });
    update(
        &mut t,
        TreeChanges {
            restyled: vec![ElementChange::restyled(b, BoxDamage::Relayout, false).decorations(true)],
            ..Default::default()
        },
    );
    let f = t.relayout();
    assert_eq!(decorations(&f, &t.doc.dom, b), vec![0; 3]);
    let _ = (a, span);
}

#[test]
fn a_relayout_change_moves_what_follows() {
    let (mut t, [a, _, _, c]) = page();
    let f = t.run(200.0, 400.0);
    let before = rect(&f, c);
    t.doc
        .restyle(a, |s| s.padding[0] = LengthPercentage::px(30.0));
    let stats = update(&mut t, restyled(a, BoxDamage::Relayout));
    assert_eq!(stats.boxes_built, 0);
    let f = t.relayout();
    assert_eq!(rect(&f, c).y, before.y + 30.0);
    assert_eq!(t.layout.stats().shaped, 0);
}

#[test]
fn a_reconstruct_change_regenerates_the_parent_container_only() {
    let (mut t, [_, b, span, _]) = page();
    t.run(200.0, 400.0);
    t.doc.restyle(span, |s| s.display = Display::BLOCK);
    let stats = update(&mut t, restyled(span, BoxDamage::Reconstruct));
    assert!(!stats.full_build);
    assert_eq!(stats.containers_rebuilt, 1);
    // The paragraph's contents: anonymous blocks around the text, the span's block and its text.
    assert!(stats.boxes_built <= 6, "{stats:?}");
    let f = t.relayout();
    assert_eq!(lines(&f, b).len(), 0, "the paragraph now holds blocks");
    // Its long neighbours were not regenerated or shaped again; at most the paragraph's own
    // runs, whose white space now collapses at block edges ("before " becomes "before").
    assert!(t.layout.stats().shaped <= 2, "{:?}", t.layout.stats());
}

#[test]
fn regenerated_long_paragraphs_take_their_shaping_from_the_cache() {
    let (mut t, [a, ..]) = page();
    t.run(200.0, 400.0);
    t.layout
        .children_changed(&t.doc.dom, t.doc.root, &t.doc.styles);
    t.relayout();
    let s = t.layout.stats();
    assert_eq!(s.shaped, 0, "{s:?}");
    assert!(s.shape_cache_hits >= 2, "{s:?}");
    // A change of the font size shapes again (another key).
    t.doc.restyle(a, |s| s.font_size = 13.0);
    update(&mut t, restyled(a, BoxDamage::Relayout));
    t.relayout();
    assert_eq!(t.layout.stats().shaped, 1);
}

#[test]
fn inserted_and_removed_children_update_their_container() {
    let (mut t, [a, b, _, c]) = page();
    let root = t.root();
    t.run(200.0, 400.0);
    let p = t.doc.block(root, |_| {});
    t.doc.text(p, "new");
    // Moving `p` before `c`: the root's children changed.
    t.doc.dom.insert_before(t.doc.root, p, Some(c)).unwrap();
    let stats = update(
        &mut t,
        TreeChanges {
            children: vec![root],
            restyled: vec![ElementChange::reconstruct(p)],
            ..Default::default()
        },
    );
    assert_eq!(stats.containers_rebuilt, 1, "outermost only: {stats:?}");
    let f = t.relayout();
    assert!(rect(&f, p).y < rect(&f, c).y);
    t.doc.dom.remove_node(b);
    update(
        &mut t,
        TreeChanges {
            children: vec![root],
            ..Default::default()
        },
    );
    let f = t.relayout();
    assert!(f.find(b).is_none());
    assert!(rect(&f, a).y < rect(&f, p).y);
}

/// An element restyled in the same batch that removed and freed one of its text children: the
/// restyle in place passes over the freed node's box (its container is regenerated for the
/// removal) rather than asking the DOM about it.
#[test]
fn a_restyle_in_place_skips_the_box_of_a_freed_text_child() {
    let (mut t, [_, b, _, _]) = page();
    t.run(200.0, 400.0);
    let before = t.doc.dom.first_child(b).unwrap();
    t.doc.dom.remove_node(before);
    t.doc.dom.free_subtree(before);
    t.doc.restyle(b, |s| s.color = Color::rgb(200, 0, 0));
    let mut changes = restyled(b, BoxDamage::Repaint);
    changes.children.push(b);
    update(&mut t, changes);
    let f = t.relayout();
    let mut texts = Vec::new();
    f.walk(&mut |fr, _| {
        if let FragmentKind::Text(text) = &fr.kind {
            if let Some(n) = text.node {
                texts.push(n);
            }
        }
    });
    assert!(!texts.contains(&before));
}

#[test]
fn text_changes_update_text_boxes_in_place() {
    let (mut t, [_, b, ..]) = page();
    t.run(200.0, 400.0);
    let text = t.doc.dom.first_child(b).unwrap();
    t.doc.dom.set_data(text, "changed ");
    let stats = update(
        &mut t,
        TreeChanges {
            text: vec![b],
            ..Default::default()
        },
    );
    assert_eq!(stats.text_updated, 1);
    assert_eq!(stats.boxes_built, 0);
    t.relayout();
    assert_eq!(t.layout.stats().shaped, 1);
}

/// A style's font is resolved once while its boxes keep it: content prepared again takes the fonts
/// of styles seen before from the tree's font cache, and a style with new font properties is
/// resolved (with its new size).
#[test]
fn fonts_are_resolved_once_per_style() {
    use std::sync::atomic::Ordering::Relaxed;
    let (mut t, [_, b, span, _]) = page();
    t.run(200.0, 400.0);
    let resolved = t.layout.tree.fonts.resolved.load(Relaxed);
    assert!(resolved > 0);
    let text = t.doc.dom.first_child(b).unwrap();
    t.doc.dom.set_data(text, "changed ");
    update(
        &mut t,
        TreeChanges {
            text: vec![b],
            ..Default::default()
        },
    );
    t.relayout();
    assert_eq!(t.layout.stats().prepared, 1);
    assert_eq!(t.layout.tree.fonts.resolved.load(Relaxed), resolved);
    // A new font size: one new style, resolved, and its text laid out at that size.
    t.doc.restyle(span, |s| s.font_size = 40.0);
    update(&mut t, restyled(span, BoxDamage::Relayout));
    let f = t.relayout();
    assert_eq!(t.layout.tree.fonts.resolved.load(Relaxed), resolved + 1);
    let mut height = 0.0;
    f.walk(&mut |fr, _| {
        if let FragmentKind::Text(text) = &fr.kind {
            if text.node.is_some_and(|n| t.doc.dom.parent(n) == Some(span)) {
                height = fr.size.height;
            }
        }
    });
    // (a text fragment is its font's em box tall)
    assert_eq!(height, 40.0);
}

/// The `text-overflow` ellipsis (CSS UI 4 §5.2) is shaped through the tree's text cache as the
/// line's text is: a clipped line laid out again takes it from there.
#[test]
fn a_clipped_lines_ellipsis_is_shaped_once() {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| {
        s.width = SizeValue::px(50.0);
        s.overflow_x = Overflow::Hidden;
        s.text_wrap_mode = TextWrapMode::Nowrap;
        s.text_overflow = TextOverflow::Ellipsis;
    });
    t.doc.text(p, "aaaaaaaaaa");
    t.run(200.0, 200.0);
    let first = t.layout.stats();
    t.layout.mark_dirty(p);
    t.relayout();
    let s = t.layout.stats();
    assert!(s.laid_out >= 1, "{s:?}");
    assert_eq!(s.shaped, 0, "first {first:?}, then {s:?}");
    assert_eq!(s.shape_cache_hits, 1, "{s:?}");
}

/// Inline boxes with `unicode-bidi: isolate` (as the UA sheet gives `li` and `[dir]`) around
/// left-to-right text make a paragraph whose levels are all 0: its text keeps the shaping cache
/// when its boxes are generated again, as text without isolates does.
#[test]
fn isolated_left_to_right_text_is_shaped_from_the_cache() {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |_| {});
    for label in ["All", "Active", "Completed"] {
        let li = t.doc.span(p, |s| s.unicode_bidi = UnicodeBidi::Isolate);
        t.doc.text(li, label);
    }
    // (and a box around right-to-left text keeps its levels: that text is laid out right to left)
    let q = t.doc.block(r0, |_| {});
    let rtl = t.doc.span(q, |s| s.unicode_bidi = UnicodeBidi::Isolate);
    t.doc.text(rtl, "abc \u{5D0}\u{5D1}\u{5D2}");
    t.run(400.0, 200.0);
    t.layout
        .children_changed(&t.doc.dom, t.doc.root, &t.doc.styles);
    let f = t.relayout();
    let s = t.layout.stats();
    assert_eq!(s.shaped, 1, "only the right-to-left text: {s:?}");
    assert_eq!(s.shape_cache_hits, 3, "{s:?}");
    assert_eq!(lines(&f, p).len(), 1);
}

#[test]
fn an_edit_in_a_paragraph_shapes_only_its_changed_word() {
    let (mut t, [a, ..]) = page();
    t.run(200.0, 400.0);
    let text = t.doc.dom.first_child(a).unwrap();
    let edited = LONG.replace("regeneration", "rebuild");
    t.doc.dom.set_data(text, &edited);
    update(
        &mut t,
        TreeChanges {
            text: vec![a],
            ..Default::default()
        },
    );
    let f = t.relayout();
    let s = t.layout.stats();
    // The paragraph's run is shaped again, but of its 24 words only the new one by DirectWrite.
    assert_eq!(s.shaped, 1, "{s:?}");
    assert_eq!(s.words_shaped, 1, "{s:?}");
    assert!(s.words_cached >= 20, "{s:?}");
    assert!(t.layout.word_cache_stats().hits >= 20);
    assert!(!lines(&f, a).is_empty());
}

#[test]
fn restyle_all_relays_out_only_what_the_function_marks() {
    let (mut t, [a, ..]) = page();
    t.run(200.0, 400.0);
    // Every handle replaced with an equal one; only `a`'s counts as a layout change.
    let a_style = t.doc.styles.elements[&a].clone();
    let changed = t.layout.restyle_all(|s| {
        let same = Arc::ptr_eq(s, &a_style);
        Some((Arc::new((**s).clone()), same))
    });
    assert!(changed >= 1);
    t.relayout();
    let s = t.layout.stats();
    assert!(s.laid_out <= 2, "{s:?}");
    assert_eq!(s.shaped, 0, "{s:?}");
}

fn shaped(glyphs: usize) -> Arc<ShapedText> {
    Arc::new(ShapedText {
        glyphs: vec![0; glyphs],
        advances: vec![0.0; glyphs],
        ..Default::default()
    })
}

#[test]
fn the_shaping_cache_keys_on_everything_shaping_depends_on() {
    let mut cache = ShapeCache::default();
    let liga = [FontFeature::new(b"liga", 0)];
    let key = |text, size, locale, features| ShapeKey {
        text,
        list: FontListId(1),
        size,
        rtl: false,
        vertical: Default::default(),
        locale,
        features,
    };
    cache.insert(&key("hello world", 16.0, "en", &[]), shaped(11));
    assert!(cache.get(&key("hello world", 16.0, "en", &[])).is_some());
    assert!(cache.get(&key("hello world", 17.0, "en", &[])).is_none());
    assert!(cache.get(&key("hello world", 16.0, "ja", &[])).is_none());
    assert!(cache.get(&key("hello world", 16.0, "en", &liga)).is_none());
    assert!(cache.get(&key("hello", 16.0, "en", &[])).is_none());
    let mut rtl = key("hello world", 16.0, "en", &[]);
    rtl.rtl = true;
    assert!(cache.get(&rtl).is_none());
    let mut other_font = key("hello world", 16.0, "en", &[]);
    other_font.list = FontListId(2);
    assert!(cache.get(&other_font).is_none());
    assert_eq!((cache.hits, cache.misses), (1, 1));
}

#[test]
fn the_shaping_cache_is_bounded_and_keeps_what_is_used() {
    let mut cache = ShapeCache::default();
    let text: Vec<String> = (0..4000)
        .map(|i| format!("run {i} {}", "x".repeat(200)))
        .collect();
    fn key(t: &str) -> ShapeKey<'_> {
        ShapeKey {
            text: t,
            list: FontListId(1),
            size: 16.0,
            rtl: false,
            vertical: Default::default(),
            locale: "en",
            features: &[],
        }
    }
    for (i, t) in text.iter().enumerate() {
        cache.insert(&key(t), shaped(2000));
        // The first run is used all along: it stays.
        assert!(cache.get(&key(&text[0])).is_some(), "lost at {i}");
    }
    assert!(cache.current_bytes() <= ShapeCache::BUDGET / 2);
    // About 12 KB per entry: both generations together hold at most the budget's worth.
    assert!(
        cache.len() * 12_000 <= ShapeCache::BUDGET,
        "{}",
        cache.len()
    );
    assert!(cache.len() < 4000);
    assert!(
        cache.get(&key(&text[1])).is_none(),
        "an unused early run was dropped"
    );
    assert!(cache.get(&key(&text[3999])).is_some());
    // Text longer than the limit is not kept.
    let huge = "y".repeat(ShapeCache::MAX_TEXT + 1);
    cache.insert(&key(&huge), shaped(1));
    assert!(cache.get(&key(&huge)).is_none());
}

#[test]
fn a_style_change_in_a_wide_tree_lays_out_a_handful_of_boxes() {
    let mut t = T::new();
    let root = t.root();
    let mut target = None;
    for i in 0..300 {
        let p = t.doc.block(root, |_| {});
        t.doc.text(p, LONG);
        if i == 150 {
            target = Some(p);
        }
    }
    let target = target.unwrap();
    t.run(300.0, 400.0);
    t.doc
        .restyle(target, |s| s.background_color = Color::rgb(0, 0, 255));
    update(&mut t, restyled(target, BoxDamage::Repaint));
    let viewport = Size::new(300.0, 400.0);
    t.layout.layout(&t.text, viewport);
    let s = t.layout.stats();
    assert!(s.laid_out <= 3, "{s:?}");
    assert!(s.reused >= 299, "{s:?}");
}

#[test]
fn boxes_inserted_early_leave_later_positioned_boxes_clean() {
    let mut t = T::new();
    let root = t.root();
    let menu = t.doc.block(root, |_| {});
    t.doc.text(menu, "menu");
    let mut later = Vec::new();
    for _ in 0..20 {
        let p = t.doc.block(root, |s| s.position = Position::Relative);
        t.doc.text(p, "positioned");
        later.push(p);
    }
    t.run(200.0, 2000.0);
    let before = rect(&t.relayout(), later[19]);
    // A menu opens: items are inserted at the top, ahead of every positioned box.
    for _ in 0..5 {
        let item = t.doc.block(menu, |_| {});
        t.doc.text(item, "item");
    }
    update(
        &mut t,
        TreeChanges {
            children: vec![menu],
            ..Default::default()
        },
    );
    let f = t.relayout();
    // The positioned boxes keep their numbers and cached layout: only the menu's chain and the new
    // boxes are laid out.
    let s = t.layout.stats();
    assert!(s.laid_out < 20, "{s:?}");
    assert!(rect(&f, later[19]).y > before.y);
    // Painting order follows the tree: numbers increase in pre-order.
    let orders: Vec<u32> = t
        .layout
        .boxes()
        .preorder()
        .into_iter()
        .map(|id| t.layout.boxes().b(id).order)
        .collect();
    assert!(orders.windows(2).all(|w| w[0] < w[1]), "{orders:?}");
}

/// Inserting into the middle paragraph numbers its boxes again between its own number and the next
/// paragraph's; the boxes outside it keep theirs. Inserting more boxes than that gap has room for
/// numbers everything again. Either way numbers increase in pre-order (which every update in these
/// tests also checks).
#[test]
fn an_insertion_numbers_the_regenerated_container_between_its_neighbours() {
    let (mut t, [_, b, _, c]) = page();
    t.run(200.0, 400.0);
    let numbers = |t: &T| -> Vec<(BoxId, u32)> {
        let tree = t.layout.boxes();
        tree.preorder()
            .into_iter()
            .map(|id| (id, tree.b(id).order))
            .collect()
    };
    let box_of = |t: &T, n| {
        let tree = t.layout.boxes();
        tree.preorder()
            .into_iter()
            .find(|&id| tree.b(id).node == Some(n))
            .unwrap()
    };
    let outside = |t: &T| -> Vec<(BoxId, u32)> {
        let tree = t.layout.boxes();
        let bb = box_of(t, b);
        let (lo, hi) = (tree.b(bb).order, tree.subtree_end(bb));
        numbers(t)
            .into_iter()
            .filter(|&(_, o)| o <= lo || o > hi)
            .collect()
    };
    let before = outside(&t);
    let s = t.doc.span(b, |_| {});
    t.doc.text(s, "new");
    let children = |n| TreeChanges {
        children: vec![n],
        ..Default::default()
    };
    assert_eq!(update(&mut t, children(b)).containers_rebuilt, 1);
    assert_eq!(outside(&t), before);
    let f = t.relayout();
    assert!(rect(&f, c).y > rect(&f, b).y);
    // (far more boxes than the gap before the next paragraph holds)
    for _ in 0..300 {
        let s = t.doc.span(b, |_| {});
        t.doc.text(s, "x ");
    }
    update(&mut t, children(b));
    let orders: Vec<u32> = numbers(&t).into_iter().map(|(_, o)| o).collect();
    assert!(orders.windows(2).all(|w| w[0] < w[1]), "{orders:?}");
    let tree = t.layout.boxes();
    assert!(tree.b(box_of(&t, c)).order > tree.subtree_end(box_of(&t, b)));
}

/// A page of three blocks whose middle one is a shadow host: its boxes come from its shadow tree, two
/// blocks of text. Returns the host, its shadow root, the shadow tree's blocks and the last paragraph.
fn page_with_a_shadow_host() -> (T, [snowlark_dom::NodeId; 5]) {
    let mut t = T::new();
    let root = t.root();
    let a = t.doc.block(root, |_| {});
    t.doc.text(a, LONG);
    let host = t.doc.block(root, |_| {});
    let shadow = t
        .doc
        .dom
        .attach_shadow(
            host,
            snowlark_dom::ShadowRootInit::new(snowlark_dom::ShadowRootMode::Open),
        )
        .unwrap();
    let x = t.doc.block(shadow, |_| {});
    t.doc.text(x, "first in the shadow tree");
    let y = t.doc.block(shadow, |_| {});
    t.doc.text(y, "second in the shadow tree");
    t.doc.styles.flat.insert(host, vec![x, y]);
    let c = t.doc.block(root, |_| {});
    t.doc.text(c, LONG);
    (t, [host, shadow, x, y, c])
}

#[test]
fn children_changed_in_a_shadow_root_regenerate_its_host_only() {
    // (Lit renders straight into its components' shadow roots)
    let (mut t, [host, shadow, x, y, _]) = page_with_a_shadow_host();
    t.run(200.0, 400.0);
    let z = t.doc.block(shadow, |_| {});
    t.doc.text(z, "added");
    t.doc.styles.flat.insert(host, vec![x, y, z]);
    let stats = update(
        &mut t,
        TreeChanges {
            children: vec![shadow],
            ..Default::default()
        },
    );
    assert!(!stats.full_build, "{stats:?}");
    assert_eq!(stats.containers_rebuilt, 1, "{stats:?}");
    // The host's contents: three blocks and their text, not the page's.
    assert!(stats.boxes_built <= 8, "{stats:?}");
    let f = t.relayout();
    assert!(rect(&f, z).height > 0.0);
    // The paragraphs around the host are reused.
    assert!(t.layout.stats().reused >= 2, "{:?}", t.layout.stats());
}

#[test]
fn a_reconstruct_at_the_top_of_a_shadow_tree_regenerates_its_host_only() {
    let (mut t, [_, _, x, _, _]) = page_with_a_shadow_host();
    t.run(200.0, 400.0);
    t.doc.restyle(x, |s| s.display = Display::INLINE);
    let stats = update(&mut t, restyled(x, BoxDamage::Reconstruct));
    assert!(!stats.full_build, "{stats:?}");
    assert_eq!(stats.containers_rebuilt, 1, "{stats:?}");
    assert!(stats.boxes_built <= 8, "{stats:?}");
    t.relayout();
}

#[test]
fn a_change_deep_in_a_shadow_tree_stays_in_its_container() {
    let (mut t, [_, _, x, _, _]) = page_with_a_shadow_host();
    t.run(200.0, 400.0);
    let span = t.doc.span(x, |_| {});
    t.doc.text(span, " and a span");
    let stats = update(
        &mut t,
        TreeChanges {
            children: vec![x],
            ..Default::default()
        },
    );
    assert!(!stats.full_build, "{stats:?}");
    assert_eq!(stats.containers_rebuilt, 1, "{stats:?}");
    assert!(stats.boxes_built <= 4, "{stats:?}");
    t.relayout();
}

/// A list of `n` items, each `<li><span>Item i</span></li>` with `list-style-type` `ty`.
fn list(n: usize, tag: &str, ty: ListStyleType) -> (T, snowlark_dom::NodeId) {
    let mut t = T::new();
    let root = t.root();
    let ul = t.doc.element(root, tag, |s| s.display = Display::BLOCK);
    for i in 0..n {
        list_item(&mut t, ul, &format!("Item {i}"), ty.clone());
    }
    (t, ul)
}

fn list_item(
    t: &mut T,
    list: snowlark_dom::NodeId,
    text: &str,
    ty: ListStyleType,
) -> snowlark_dom::NodeId {
    let li = t.doc.element(list, "li", |s| {
        s.display = Display::LIST_ITEM;
        s.list_style_type = ty;
    });
    let span = t.doc.span(li, |_| {});
    t.doc.text(span, text);
    li
}

/// Inserting `li` (styled now) and the update the engine sends for it.
fn inserted(list: snowlark_dom::NodeId, li: snowlark_dom::NodeId) -> TreeChanges {
    TreeChanges {
        restyled: vec![ElementChange::reconstruct(li)],
        children: vec![list],
        ..Default::default()
    }
}

/// The incremental tree, laid out, against one built from scratch.
fn assert_same_as_a_full_build(t: &mut T) {
    let incremental = (t.dump(), t.relayout().dump());
    let viewport = t.viewport;
    let full = (
        {
            t.layout.build(&t.doc.dom, t.doc.document, &t.doc.styles);
            t.dump()
        },
        t.layout.layout(&t.text, viewport).dump(),
    );
    assert_eq!(incremental.0, full.0, "box trees differ");
    assert_eq!(incremental.1, full.1, "fragments differ");
}

#[test]
fn an_item_appended_to_a_list_keeps_the_other_items_boxes() {
    let (mut t, ul) = list(100, "ul", ListStyleType::Disc);
    t.run(400.0, 400.0);
    let li = list_item(&mut t, ul, "new item", ListStyleType::Disc);
    let stats = update(&mut t, inserted(ul, li));
    assert!(!stats.full_build, "{stats:?}");
    assert_eq!(stats.containers_rebuilt, 1, "{stats:?}");
    // The new item, its marker, span and text: not the hundred others.
    assert!(stats.boxes_built <= 6, "{stats:?}");
    t.relayout();
    let s = t.layout.stats();
    assert!(s.laid_out <= 8, "{s:?}");
    assert!(s.reused >= 100, "{s:?}");
    // And removing it again.
    t.doc.dom.remove_child(ul, li).unwrap();
    let stats = update(
        &mut t,
        TreeChanges {
            children: vec![ul],
            ..Default::default()
        },
    );
    assert_eq!(stats.boxes_built, 0, "{stats:?}");
    assert_same_as_a_full_build(&mut t);
}

#[test]
fn an_item_inserted_at_the_top_of_an_ordered_list_renumbers_the_others() {
    let (mut t, ol) = list(3, "ol", ListStyleType::Decimal);
    t.run(400.0, 400.0);
    let li = list_item(&mut t, ol, "first", ListStyleType::Decimal);
    let first = t.doc.dom.first_child(ol);
    t.doc.dom.insert_before(ol, li, first).unwrap();
    update(&mut t, inserted(ol, li));
    let d = t.dump();
    for n in 1..=4 {
        assert!(d.contains(&format!("\"{n}. \"")), "{d}");
    }
    assert_same_as_a_full_build(&mut t);
}

#[test]
fn a_kept_item_with_a_change_inside_is_regenerated() {
    // Children change in the list and in an item (the item's container is covered by the list's).
    let (mut t, ul) = list(3, "ul", ListStyleType::Disc);
    t.run(400.0, 400.0);
    let second = t.doc.dom.element_children(ul).nth(1).unwrap();
    let span = t.doc.span(second, |_| {});
    t.doc.text(span, " more");
    let li = list_item(&mut t, ul, "new", ListStyleType::Disc);
    let mut changes = inserted(ul, li);
    changes.children.push(second);
    update(&mut t, changes);
    assert!(t.dump().contains("\" more\""), "{}", t.dump());
    assert_same_as_a_full_build(&mut t);
}

#[test]
fn a_kept_item_in_a_flex_container_stays_a_flex_item() {
    let mut t = T::new();
    let root = t.root();
    let row = t.doc.block(root, |s| s.display = Display::FLEX);
    for i in 0..3 {
        let b = t.doc.block(row, |_| {});
        t.doc.text(b, &format!("item {i}"));
    }
    t.run(400.0, 400.0);
    let b = t.doc.block(row, |_| {});
    t.doc.text(b, "new");
    let first = t.doc.dom.first_child(row);
    t.doc.dom.insert_before(row, b, first).unwrap();
    let stats = update(&mut t, inserted(row, b));
    assert!(stats.boxes_built <= 3, "{stats:?}");
    assert_same_as_a_full_build(&mut t);
}
