// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Inline layout tests: line box heights, font metric rounding, quirks mode, phantom lines, baselines.
//! The cases mirror what Chrome measures for the same markup (noted per case), on Ahem (ascent 0.8em,
//! descent 0.2em, no line gap).

use snowlark_dom::{NodeId, QuirksMode};
use snowlark_paint::FontMetrics;

use crate::inline::line_metrics;
use crate::style::*;
use crate::tests::*;

/// A block with `line-height: 50px` in a quirks-mode (or standards-mode) document, filled by `f`;
/// returns the block's height.
fn quirks_block(quirks: bool, f: impl FnOnce(&mut T, NodeId)) -> f32 {
    let mut t = T::new();
    if quirks {
        t.doc
            .dom
            .set_quirks_mode(t.doc.document, QuirksMode::Quirks);
    }
    let r0 = t.root();
    let p = t
        .doc
        .block(r0, |s| s.line_height = LineHeight::Length(50.0));
    f(&mut t, p);
    let fr = t.run(400.0, 400.0);
    rect(&fr, p).height
}

fn lh(s: &mut crate::testing::TestStyle, px: f32) {
    s.line_height = LineHeight::Length(px);
}

#[test]
fn metrics_round_to_whole_pixels() {
    // Times New Roman at 16px: ascent 14.26, descent 3.46, line gap 0.67 → 14, 3, 1 (a normal line
    // is 18px with the baseline at 14, as Chrome lays it out).
    let m = line_metrics(FontMetrics {
        ascent: 14.26,
        descent: 3.456,
        line_gap: 0.67,
        ..Default::default()
    });
    assert_eq!((m.ascent, m.descent, m.line_gap), (14.0, 3.0, 1.0));
    let m = line_metrics(FontMetrics {
        ascent: 10.5,
        descent: 2.49,
        line_gap: 0.0,
        ..Default::default()
    });
    assert_eq!((m.ascent, m.descent), (11.0, 2.0));
}

#[test]
fn ahem_13px_line_is_whole_pixels() {
    // Ahem 13px: ascent 10.4 → 10, descent 2.6 → 3; `normal` line-height 13; the text's content area
    // is 13px with its baseline 10px down.
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| {
        s.font_size = 13.0;
        s.line_height = LineHeight::Normal;
    });
    let a = t.doc.text(p, "x");
    let span = t.doc.span(p, |_| {});
    t.doc.text(span, "y");
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, p).height, 13.0);
    assert_eq!(texts(&f, a)[0], r(0.0, 0.0, 13.0, 13.0));
    assert_eq!(rect(&f, span), r(13.0, 0.0, 13.0, 13.0));
}

#[test]
fn half_leading_above_is_floored() {
    // Ahem 10px (A+D = 10) with line-height 15px: 2.5px of half-leading becomes 2 above, 3 below.
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| lh(s, 15.0));
    let a = t.doc.text(p, "x");
    let f = t.run(400.0, 400.0);
    assert_eq!(texts(&f, a)[0], r(0.0, 2.0, 10.0, 10.0));
    assert_eq!(rect(&f, p).height, 15.0);
}

#[test]
fn quirks_line_height_from_text_boxes_only() {
    // Quirks Mode §3.3/§3.4: the block's own strut and empty inline boxes don't count.
    // <div lh50><span lh20>X</span></div>: 20 (standards: 50)
    let case = |t: &mut T, p: NodeId| {
        let s = t.doc.span(p, |s| lh(s, 20.0));
        t.doc.text(s, "X");
    };
    assert_eq!(quirks_block(true, case), 20.0);
    assert_eq!(quirks_block(false, case), 50.0);
    // Text directly in the block: its strut counts.
    assert_eq!(
        quirks_block(true, |t, p| {
            t.doc.text(p, "Y");
            let s = t.doc.span(p, |s| lh(s, 20.0));
            t.doc.text(s, "X");
        }),
        50.0
    );
    // An empty span: nothing.
    assert_eq!(
        quirks_block(true, |t, p| {
            t.doc.span(p, |_| {});
            let s = t.doc.span(p, |s| lh(s, 20.0));
            t.doc.text(s, "X");
        }),
        20.0
    );
    // An empty span with inline-axis padding counts (Chrome: 100 with padding-left, 20 with
    // padding-top only)...
    assert_eq!(
        quirks_block(true, |t, p| {
            t.doc.span(p, |s| s.padding[3] = LengthPercentage::px(1.0));
            let s = t.doc.span(p, |s| lh(s, 20.0));
            t.doc.text(s, "X");
        }),
        50.0
    );
    // ...block-axis padding does not.
    assert_eq!(
        quirks_block(true, |t, p| {
            t.doc.span(p, |s| s.padding[0] = LengthPercentage::px(1.0));
            let s = t.doc.span(p, |s| lh(s, 20.0));
            t.doc.text(s, "X");
        }),
        20.0
    );
    // A span holding only another span with text: only the inner one counts.
    assert_eq!(
        quirks_block(true, |t, p| {
            let o = t.doc.span(p, |s| lh(s, 40.0));
            let s = t.doc.span(o, |s| lh(s, 20.0));
            t.doc.text(s, "X");
        }),
        20.0
    );
    // An atomic inline alone: its own height, no strut (images in quirks-mode cells).
    assert_eq!(
        quirks_block(true, |t, p| {
            t.doc.element(p, "span", |s| {
                s.display = Display::INLINE_BLOCK;
                s.width = SizeValue::px(10.0);
                s.height = SizeValue::px(10.0);
            });
        }),
        10.0
    );
}

#[test]
fn quirks_br_gives_its_box_a_strut_only_when_it_has_none() {
    // <div lh50><br></div>: the line keeps the <br>'s line-height (50).
    assert_eq!(
        quirks_block(true, |t, p| {
            t.doc.element(p, "br", |_| {});
        }),
        50.0
    );
    // ...its own line-height when it has one.
    assert_eq!(
        quirks_block(true, |t, p| {
            t.doc.element(p, "br", |s| lh(s, 10.0));
        }),
        10.0
    );
    // <span lh20>X</span><br>: the block already has metrics from the span: 20.
    assert_eq!(
        quirks_block(true, |t, p| {
            let s = t.doc.span(p, |s| lh(s, 20.0));
            t.doc.text(s, "X");
            t.doc.element(p, "br", |_| {});
        }),
        20.0
    );
    // <span lh20>X</span><span lh30><br></span>: the second span has none: 30.
    assert_eq!(
        quirks_block(true, |t, p| {
            let s = t.doc.span(p, |s| lh(s, 20.0));
            t.doc.text(s, "X");
            let s2 = t.doc.span(p, |s| lh(s, 30.0));
            t.doc.element(s2, "br", |_| {});
        }),
        30.0
    );
    // Two lines: X<br> (20) and a line with only a <br> (50).
    assert_eq!(
        quirks_block(true, |t, p| {
            let s = t.doc.span(p, |s| lh(s, 20.0));
            t.doc.text(s, "X");
            t.doc.element(p, "br", |_| {});
            t.doc.element(p, "br", |_| {});
        }),
        70.0
    );
}

#[test]
fn negative_inline_margins_keep_a_line_from_being_phantom() {
    // CSS Inline 3 §2.4: an empty span with a non-zero inline-axis margin (even negative) makes the
    // line box real: it gets the block's strut.
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |_| {});
    t.doc
        .span(p, |s| s.margin[3] = LengthPercentageAuto::px(-1.0));
    let q = t.doc.block(r0, |_| {});
    t.doc
        .span(q, |s| s.margin[0] = LengthPercentageAuto::px(-1.0));
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, p).height, 10.0);
    // A block-axis margin does not.
    assert_eq!(rect(&f, q).height, 0.0);
}

#[test]
fn no_break_space_is_not_collapsible() {
    // CSS Text 3 §4.1: only spaces and tabs collapse; a line holding a lone U+00A0 is not empty.
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |_| {});
    t.doc.text(p, "\u{a0}");
    let q = t.doc.block(r0, |_| {});
    t.doc.text(q, " \u{a0} ");
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, p).height, 10.0);
    assert_eq!(rect(&f, q).height, 10.0);
}

#[test]
fn inline_boxes_on_phantom_lines_have_no_height() {
    // CSS Inline 3 §2.4: a line with only an empty inline box is phantom (zero-height), and so is
    // the box (Chrome: 0×0 at the line's top).
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |_| {});
    let s = t.doc.span(p, |_| {});
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, p).height, 0.0);
    assert_eq!(rect(&f, s), r(0.0, 0.0, 0.0, 0.0));
}

/// A line `x<inline-flex or -grid ...>`: returns the container's top and the line's height.
fn inline_container(
    display: Display,
    container: impl FnOnce(&mut crate::testing::TestStyle),
    items: impl FnOnce(&mut T, NodeId),
) -> (f32, f32) {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |_| {});
    t.doc.text(p, "x");
    let c = t.doc.element(p, "span", |s| {
        s.display = display;
        container(s);
    });
    items(&mut t, c);
    let f = t.run(400.0, 400.0);
    (rect(&f, c).y, rect(&f, p).height)
}

fn empty_item(s: &mut crate::testing::TestStyle) {
    s.display = Display::BLOCK;
    s.width = SizeValue::px(20.0);
    s.height = SizeValue::px(20.0);
}

#[test]
fn flex_and_grid_baselines() {
    // css-flexbox-1 §8.5: the first item's baseline, synthesized from its border box's bottom when
    // it has none (not the next item's): the 20px empty item's bottom sits on the baseline, so the
    // container rises 20px above it, 12 more than the strut's 8: the line is 20 + 2 tall.
    let (top, h) = inline_container(
        Display::INLINE_FLEX,
        |_| {},
        |t, c| {
            t.doc.element(c, "div", empty_item);
            let b = t.doc.element(c, "div", |s| s.display = Display::BLOCK);
            t.doc.text(b, "ab");
        },
    );
    assert_eq!((top, h), (0.0, 22.0));
    // The same for a grid's first item.
    let (top, h) = inline_container(
        Display::INLINE_GRID,
        |_| {},
        |t, c| {
            t.doc.element(c, "div", empty_item);
            let b = t.doc.element(c, "div", |s| s.display = Display::BLOCK);
            t.doc.text(b, "ab");
        },
    );
    // (the grid stacks its items in two rows: 20px above the baseline, 10 below)
    assert_eq!((top, h), (0.0, 30.0));
    // The first item in order-modified document order.
    let (top, h) = inline_container(
        Display::INLINE_FLEX,
        |_| {},
        |t, c| {
            let b = t.doc.element(c, "div", |s| {
                s.display = Display::BLOCK;
                s.order = 2;
            });
            t.doc.text(b, "ab");
            t.doc.element(c, "div", |s| {
                empty_item(s);
                s.order = 1;
            });
        },
    );
    assert_eq!((top, h), (0.0, 22.0));
    // An item participating in baseline alignment gives the shared baseline.
    let (top, h) = inline_container(
        Display::INLINE_FLEX,
        |_| {},
        |t, c| {
            let b = t.doc.element(c, "div", |s| s.display = Display::BLOCK);
            t.doc.text(b, "ab");
            t.doc.element(c, "div", |s| {
                empty_item(s);
                s.align_self = Align::Baseline;
            });
        },
    );
    assert_eq!((top, h), (0.0, 22.0));
    // A flex container that clips keeps its items' baseline (an inline-block that clips would
    // sit on its bottom margin edge).
    let (top, h) = inline_container(
        Display::INLINE_FLEX,
        |s| {
            s.overflow_x = Overflow::Hidden;
            s.overflow_y = Overflow::Hidden;
        },
        |t, c| {
            let b = t.doc.element(c, "div", |s| s.display = Display::BLOCK);
            t.doc.text(b, "ab");
        },
    );
    assert_eq!((top, h), (0.0, 10.0));
    let (top, h) = inline_container(
        Display::INLINE_BLOCK,
        |s| {
            s.overflow_x = Overflow::Hidden;
            s.overflow_y = Overflow::Hidden;
        },
        |t, c| {
            t.doc.text(c, "ab");
        },
    );
    assert_eq!((top, h), (0.0, 12.0));
}

#[test]
fn br_has_a_client_rect_where_its_line_ends() {
    // A <br> reports its content area at the end of its line (as Chrome does), empty inline-wise.
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |_| {});
    t.doc.text(p, "abc");
    let br = t.doc.element(p, "br", |_| {});
    t.doc.text(p, "d");
    let br2 = t.doc.element(p, "br", |_| {});
    t.run(400.0, 400.0);
    assert_eq!(t.layout.client_rects(br), vec![r(30.0, 0.0, 0.0, 10.0)]);
    assert_eq!(t.layout.client_rects(br2), vec![r(10.0, 10.0, 0.0, 10.0)]);
}

fn trim(start: bool, end: bool) -> TextBoxTrim {
    TextBoxTrim { start, end }
}

#[test]
fn text_box_trim_block_container() {
    // CSS Inline 3 §6.1: Ahem 10px in 30px lines has 10px of half-leading on each side; trim-both
    // takes it off the first line's top and the last line's bottom.
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| {
        lh(s, 30.0);
        s.text_box_trim = trim(true, true);
    });
    let a = t.doc.text(p, "a");
    t.doc.element(p, "br", |_| {});
    t.doc.text(p, "b");
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, p).height, 40.0);
    assert_eq!(texts(&f, a)[0].y, 0.0);

    // trim-end to the alphabetic baseline also drops the descent.
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| {
        lh(s, 30.0);
        s.text_box_trim = trim(false, true);
        s.text_box_edge = TextBoxEdge {
            over: TextEdgeMetric::Text,
            under: TextEdgeMetric::Alphabetic,
        };
    });
    t.doc.text(p, "a");
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, p).height, 18.0);
}

#[test]
fn text_box_trim_reaches_nested_lines_without_intervening_borders() {
    // The first and last formatted lines are in the children; a child's own border stops it, the
    // trimming box's own border does not (text-box-trim border-padding-001).
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| {
        lh(s, 30.0);
        s.text_box_trim = trim(true, true);
        s.border(5.0, snowlark_paint::Color::BLACK);
    });
    let c1 = t.doc.block(p, |_| {});
    t.doc.text(c1, "a");
    let c2 = t
        .doc
        .block(p, |s| s.border(1.0, snowlark_paint::Color::BLACK));
    t.doc.text(c2, "b");
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, c1).height, 20.0);
    assert_eq!(rect(&f, c2).height, 32.0);
    assert_eq!(rect(&f, p).height, 62.0);
}

#[test]
fn text_box_trim_on_inline_box_sets_its_content_area() {
    // An inline box's content area runs from the text-box-edge over to the under metric when
    // trimmed (to the alphabetic baseline: the ascent only).
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |_| {});
    let s = t.doc.span(p, |s| {
        s.text_box_trim = trim(true, true);
        s.text_box_edge = TextBoxEdge {
            over: TextEdgeMetric::Text,
            under: TextEdgeMetric::Alphabetic,
        };
    });
    t.doc.text(s, "x");
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, s), r(0.0, 0.0, 10.0, 8.0));
    assert_eq!(rect(&f, p).height, 10.0);
}

#[test]
fn inline_split_around_a_block_reports_the_block_too() {
    // <a><div 20x20></div></a>: the link's client rects include the block it was split around
    // (Hacker News' logo and vote links; Chrome reports the block's box for the inline).
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |_| {});
    let a = t.doc.span(p, |_| {});
    let d = t.doc.block(a, |s| {
        s.width = SizeValue::px(20.0);
        s.height = SizeValue::px(20.0);
        s.margin = [
            LengthPercentageAuto::px(3.0),
            LengthPercentageAuto::px(2.0),
            LengthPercentageAuto::px(6.0),
            LengthPercentageAuto::px(2.0),
        ];
    });
    t.run(400.0, 400.0);
    // (with its inline-axis margins, as Chrome reports it)
    let rects = t.layout.client_rects(a);
    assert!(rects.contains(&r(0.0, 3.0, 24.0, 20.0)), "{rects:?}");
    assert_eq!(t.layout.client_rects(d), vec![r(2.0, 3.0, 20.0, 20.0)]);
}

#[test]
fn fallback_fonts_count_for_normal_line_height() {
    // CSS Inline 3 §5.1: with `line-height: normal`, the metrics of the fonts the text falls back
    // to count. Ahem has no CJK glyphs: a system font takes 中, and its ascent and descent are
    // taller than Ahem's 10px em (the line grows past 10px, as in Chrome).
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| s.line_height = LineHeight::Normal);
    t.doc.text(p, "x");
    let q = t.doc.block(r0, |s| s.line_height = LineHeight::Normal);
    t.doc.text(q, "x\u{4e2d}");
    let f = t.run(400.0, 400.0);
    assert_eq!(rect(&f, p).height, 10.0);
    assert!(rect(&f, q).height > 10.0, "{}", rect(&f, q).height);
}

/// Four 40px words in a 150px wide block of 10px Ahem, with `text-wrap-style` and `text-align`;
/// returns the rects of the text on each line.
fn four_words(wrap: TextWrapStyle, align: TextAlign) -> Vec<snowlark_paint::Rect> {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| {
        lh(s, 10.0);
        s.text_wrap_style = wrap;
        s.text_align = align;
    });
    let a = t.doc.text(p, "aaaa bbbb cccc dddd");
    let f = t.run(150.0, 400.0);
    texts(&f, a)
}

#[test]
fn balanced_lines_break_as_narrow_as_their_count_allows() {
    // CSS Text 4 §6.2: `balance` takes as many lines as `auto` (two here), with the space left at
    // their ends evened out: two words on each line, not three and one.
    let auto = four_words(TextWrapStyle::Auto, TextAlign::Start);
    assert_eq!(auto.len(), 2, "{auto:?}");
    assert_eq!(auto[1], r(0.0, 10.0, 40.0, 10.0));
    let balanced = four_words(TextWrapStyle::Balance, TextAlign::Start);
    assert_eq!(balanced.len(), 2, "{balanced:?}");
    assert_eq!(balanced[1], r(0.0, 10.0, 90.0, 10.0));
    // `stable` leaves this paragraph as `auto` does.
    assert_eq!(four_words(TextWrapStyle::Stable, TextAlign::Start), auto);
}

#[test]
fn balanced_lines_align_in_the_whole_line_box() {
    // Balancing narrows only where lines break: `text-align: center` still centers each 90px line
    // in the 150px line box.
    let balanced = four_words(TextWrapStyle::Balance, TextAlign::Center);
    assert_eq!(balanced.len(), 2, "{balanced:?}");
    assert_eq!((balanced[0].x, balanced[1].x), (30.0, 30.0), "{balanced:?}");
}

/// `pre-wrap` text in a 150px wide block of 10px Ahem with `text-wrap: balance`: the rects of its
/// lines.
fn balanced_pre_wrap(text: &str) -> Vec<snowlark_paint::Rect> {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| {
        lh(s, 10.0);
        s.text_wrap_style = TextWrapStyle::Balance;
        s.white_space_collapse = WhiteSpaceCollapse::Preserve;
    });
    let a = t.doc.text(p, text);
    let f = t.run(150.0, 400.0);
    texts(&f, a)
}

#[test]
fn balancing_leaves_paragraphs_longer_than_the_limit_alone() {
    // Seven lines in one paragraph, the last a single word: past the limit of lines balanced,
    // they break as `auto` does. Six lines with the same last one are balanced.
    let lines = |words: usize| {
        let mut t = T::new();
        let r0 = t.root();
        let p = t.doc.block(r0, |s| {
            lh(s, 10.0);
            s.text_wrap_style = TextWrapStyle::Balance;
        });
        let a = t.doc.text(p, &["a"; 60][..words].join(" "));
        let f = t.run(150.0, 400.0);
        texts(&f, a)
    };
    // (eight one-letter words fit a line: 41 words make 8+8+8+8+8+1, balanced 7+7+7+7+7+6; 49
    // make 8 * 6 + 1)
    let six = lines(41);
    assert_eq!(six.len(), 6, "{six:?}");
    assert_eq!(six[5].width, 110.0, "{six:?}");
    let seven = lines(49);
    assert_eq!(seven.len(), 7, "{seven:?}");
    assert_eq!(seven[6].width, 10.0, "{seven:?}");
}

#[test]
fn balancing_takes_each_paragraph_between_forced_breaks_alone() {
    // CSS Text 4 §6.2: the lines a forced break separates balance apart. The one-letter lines
    // before the four words do not count toward their limit either.
    let lines = balanced_pre_wrap(&format!("{}aaaa bbbb cccc dddd", "x\n".repeat(6)));
    assert_eq!(lines.len(), 8, "{lines:?}");
    assert_eq!(lines[7], r(0.0, 70.0, 90.0, 10.0));
    // A short paragraph does not narrow the long one.
    let lines = balanced_pre_wrap("aaaa bbbb cccc dddd\nxx");
    assert_eq!(lines[1], r(0.0, 10.0, 90.0, 10.0), "{lines:?}");
}

/// `text` in a 150px wide block of 10px Ahem with `text-wrap-style`; the rects of its lines.
fn wrapped(text: &str, wrap: TextWrapStyle) -> Vec<snowlark_paint::Rect> {
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| {
        lh(s, 10.0);
        s.text_wrap_style = wrap;
    });
    let a = t.doc.text(p, text);
    let f = t.run(150.0, 400.0);
    texts(&f, a)
}

#[test]
fn pretty_lines_do_not_leave_a_short_last_word_alone() {
    // CSS Text 4 §6.2: `auto` leaves "gg" (20px, under a third of the line) alone on the last
    // line; `pretty` keeps three lines and brings a word down to it.
    let text = "aaaa bbbb cccc dddd eeee ffff gg";
    let auto = wrapped(text, TextWrapStyle::Auto);
    assert_eq!(auto.len(), 3, "{auto:?}");
    assert_eq!(auto[2], r(0.0, 20.0, 20.0, 10.0));
    let pretty = wrapped(text, TextWrapStyle::Pretty);
    assert_eq!(pretty.len(), 3, "{pretty:?}");
    assert_eq!(pretty[2], r(0.0, 20.0, 70.0, 10.0));
    // (two lines: the same)
    let pretty = wrapped("aaaa bbbb cccc dddd", TextWrapStyle::Pretty);
    assert_eq!(pretty[1], r(0.0, 10.0, 90.0, 10.0), "{pretty:?}");
}

#[test]
fn pretty_lines_keep_a_last_line_of_several_words() {
    let text = "aaaa bbbb cccc dddd eeee";
    let auto = wrapped(text, TextWrapStyle::Auto);
    assert_eq!(auto.len(), 2, "{auto:?}");
    assert_eq!(wrapped(text, TextWrapStyle::Pretty), auto);
    // A last word wider than a third of the line stays alone too.
    let text = "aaaa bbbb cccc dddddd";
    assert_eq!(
        wrapped(text, TextWrapStyle::Pretty),
        wrapped(text, TextWrapStyle::Auto)
    );
}

#[test]
fn balancing_does_not_break_inside_words() {
    // `overflow-wrap: break-word` keeps two lines however narrow they get, breaking inside the
    // long word; balancing narrows no further than lines break at soft wrap opportunities.
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| {
        lh(s, 10.0);
        s.text_wrap_style = TextWrapStyle::Balance;
        s.overflow_wrap = OverflowWrap::BreakWord;
    });
    let a = t.doc.text(p, "aaaaaaaaaaa bbbb cc");
    let f = t.run(150.0, 400.0);
    let lines = texts(&f, a);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[1], r(0.0, 10.0, 70.0, 10.0));
}
