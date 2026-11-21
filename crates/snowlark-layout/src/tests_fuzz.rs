// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Randomized trees mixing every kind of box and many property values: layout, relayout at another
//! width, incremental relayout after text edits (compared with a fresh layout), painting and hit
//! testing must not panic, must produce finite geometry and a valid display list, and must agree.
//!
//! `FUZZ_COUNT=n` runs more seeds (200 by default), `FUZZ_SEED=n` one seed, `FUZZ_VERBOSE=1` prints
//! each tree.

use snowlark_dom::NodeId;
use snowlark_paint::{DisplayListBuilder, Point, Size};

use crate::style::*;
use crate::testing::TestStyle;
use crate::tests::*;
use crate::*;

/// xorshift64*, deterministic.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn pick<T: Copy>(&mut self, v: &[T]) -> T {
        v[self.below(v.len() as u64) as usize]
    }
}

const WORDS: &[&str] = &[
    "a",
    "bb",
    "ccc",
    "dddd",
    " ",
    "  ",
    "\n",
    "e\tf",
    "long-word-with-hyphens",
    "日本語",
    "مرحبا",
    "x y z",
    "",
];

fn random_style(rng: &mut Rng, s: &mut TestStyle, depth: usize) {
    s.display = rng.pick(&[
        Display::BLOCK,
        Display::INLINE,
        Display::INLINE,
        Display::INLINE_BLOCK,
        Display::FLEX,
        Display::GRID,
        Display::TABLE,
        Display::TABLE_ROW,
        Display::TABLE_CELL,
        Display::LIST_ITEM,
        Display::FLOW_ROOT,
        Display::Contents,
        Display::INLINE_FLEX,
        Display::TABLE_CAPTION,
        Display::TABLE_ROW_GROUP,
        Display::TABLE_COLUMN,
    ]);
    if depth > 4 && rng.chance(50) {
        s.display = Display::INLINE;
    }
    if rng.chance(15) {
        s.float = rng.pick(&[Float::Left, Float::Right]);
    }
    if rng.chance(12) {
        s.position = rng.pick(&[
            Position::Relative,
            Position::Absolute,
            Position::Fixed,
            Position::Sticky,
        ]);
        let (side, a) = (rng.below(4) as usize, rng.below(30) as f32 - 10.0);
        s.inset[side] = LengthPercentageAuto::px(a);
        if rng.chance(30) {
            let (side, a) = (rng.below(4) as usize, rng.below(60) as f32);
            s.inset[side] = LengthPercentageAuto::percent(a);
        }
        if rng.chance(40) {
            s.z_index = Some(rng.below(5) as i32 - 2);
        }
    }
    if rng.chance(30) {
        let (a, b) = (rng.below(200) as f32, rng.below(120) as f32);
        s.width = rng.pick(&[
            SizeValue::px(a),
            SizeValue::percent(b),
            SizeValue::MinContent,
            SizeValue::MaxContent,
            SizeValue::FitContent(None),
        ]);
    }
    if rng.chance(20) {
        let (a, b) = (rng.below(100) as f32, rng.below(100) as f32);
        s.height = rng.pick(&[SizeValue::px(a), SizeValue::percent(b)]);
    }
    if rng.chance(15) {
        s.min_width = SizeValue::px(rng.below(80) as f32);
    }
    if rng.chance(15) {
        s.max_width = MaxSizeValue::px(rng.below(80) as f32);
    }
    if rng.chance(30) {
        let side = rng.below(4) as usize;
        let a = rng.below(30) as f32 - 10.0;
        s.margin[side] = rng.pick(&[
            LengthPercentageAuto::px(a),
            LengthPercentageAuto::Auto,
            LengthPercentageAuto::percent(10.0),
        ]);
    }
    if rng.chance(30) {
        let (side, a) = (rng.below(4) as usize, rng.below(12) as f32);
        s.padding[side] = LengthPercentage::px(a);
    }
    if rng.chance(20) {
        s.border(rng.below(5) as f32, snowlark_paint::Color::BLACK);
    }
    if rng.chance(10) {
        s.overflow_x = Overflow::Hidden;
        s.overflow_y = rng.pick(&[Overflow::Hidden, Overflow::Auto, Overflow::Scroll]);
        s.text_overflow = TextOverflow::Ellipsis;
    }
    if rng.chance(10) {
        s.white_space_collapse = rng.pick(&[
            WhiteSpaceCollapse::Preserve,
            WhiteSpaceCollapse::PreserveBreaks,
            WhiteSpaceCollapse::BreakSpaces,
        ]);
    }
    if rng.chance(10) {
        s.text_wrap_mode = TextWrapMode::Nowrap;
    }
    if rng.chance(10) {
        s.text_align = rng.pick(&[
            TextAlign::Center,
            TextAlign::Right,
            TextAlign::Justify,
            TextAlign::End,
        ]);
    }
    if rng.chance(8) {
        s.direction = snowlark_paint::Direction::Rtl;
    }
    if rng.chance(10) {
        s.vertical_align = rng.pick(&[
            VerticalAlign::Top,
            VerticalAlign::Middle,
            VerticalAlign::Bottom,
            VerticalAlign::Super,
            VerticalAlign::LengthPercentage(LengthPercentage::px(3.0)),
        ]);
    }
    if rng.chance(8) {
        s.word_break = WordBreak::BreakAll;
    }
    if rng.chance(8) {
        s.overflow_wrap = OverflowWrap::Anywhere;
    }
    if rng.chance(10) {
        s.clear = rng.pick(&[Clear::Left, Clear::Right, Clear::Both]);
    }
    if rng.chance(8) {
        s.flex_wrap = FlexWrap::Wrap;
        s.flex_grow = rng.below(3) as f32;
    }
    if rng.chance(8) {
        s.grid_template_columns = TrackList {
            items: vec![
                TrackListItem::Track(TrackSize::Breadth(TrackBreadth::Fr(1.0))),
                TrackListItem::Track(TrackSize::Breadth(TrackBreadth::Auto)),
            ],
            line_names: Vec::new(),
        };
    }
    if rng.chance(8) {
        s.border_collapse = BorderCollapse::Collapse;
        s.border_spacing = (2.0, 3.0);
    }
    if rng.chance(5) {
        s.table_layout = TableLayoutMode::Fixed;
    }
    if rng.chance(5) {
        s.opacity = 0.5;
    }
    if rng.chance(5) {
        // (each shape: a rectangle, a rounded one, curves, a polygon with crossing edges)
        let lp = LengthPercentage::px;
        let shape = match rng.below(5) {
            0 => ClipShape::Box,
            1 => ClipShape::Inset {
                sides: Box::new([lp(2.0), LengthPercentage::percent(10.0), lp(0.0), lp(-5.0)]),
                radii: Box::new([(lp(0.0), lp(0.0)); 4]),
            },
            2 => ClipShape::Inset {
                sides: Box::new([lp(1.0); 4]),
                radii: Box::new([(lp(6.0), LengthPercentage::percent(20.0)); 4]),
            },
            3 => ClipShape::Circle {
                center: None,
                radius: rng.pick(&[ShapeRadius::ClosestSide, ShapeRadius::Length(lp(15.0))]),
            },
            _ => ClipShape::Polygon {
                even_odd: rng.chance(50),
                points: vec![
                    (lp(0.0), lp(0.0)),
                    (
                        LengthPercentage::percent(100.0),
                        LengthPercentage::percent(100.0),
                    ),
                    (LengthPercentage::percent(100.0), lp(0.0)),
                    (lp(0.0), LengthPercentage::percent(100.0)),
                ],
            },
        };
        s.clip_path = Some(ClipPath {
            shape,
            reference: rng.pick(&[ClipBox::Border, ClipBox::Padding, ClipBox::Content]),
            zoom: 1.0,
        });
    }
    if rng.chance(5) {
        s.transform = Some(snowlark_paint::Matrix::rotation(0.3));
    }
    if rng.chance(8) {
        s.text_indent = LengthPercentage::px(rng.below(40) as f32 - 20.0);
    }
    if rng.chance(8) {
        s.line_height = rng.pick(&[
            LineHeight::Normal,
            LineHeight::Number(2.0),
            LineHeight::Length(0.0),
        ]);
    }
    if rng.chance(5) {
        s.box_sizing = BoxSizing::BorderBox;
    }
}

fn grow(t: &mut T, rng: &mut Rng, parent: NodeId, depth: usize, budget: &mut usize) {
    let kids = rng.below(5) + 1;
    for _ in 0..kids {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        if rng.chance(35) || depth > 6 {
            let w = rng.pick(WORDS);
            let w2 = rng.pick(WORDS);
            t.doc.text(parent, &format!("{w} {w2}"));
            continue;
        }
        let tag = rng.pick(&[
            "div", "span", "td", "img", "br", "li", "ol", "col", "colgroup",
        ]);
        let el = t.doc.element(parent, tag, |s| random_style(rng, s, depth));
        if tag == "img" {
            let (w, h) = (rng.below(50) as f32, rng.below(50) as f32);
            t.doc
                .replace(el, ReplacedContent::image(snowlark_paint::ImageId(1), w, h));
        }
        if tag == "td" && rng.chance(50) {
            let span = rng.pick(&["0", "1", "2", "3", "x"]);
            t.doc.dom.set_attribute(el, "colspan", span).unwrap();
            let span = rng.pick(&["0", "1", "2", "3"]);
            t.doc.dom.set_attribute(el, "rowspan", span).unwrap();
        }
        if rng.chance(70) {
            grow(t, rng, el, depth + 1, budget);
        }
    }
}

/// Fragment dumps equal up to float rounding (sums in a different order differ in the last bits).
#[track_caller]
fn assert_same(a: &str, b: &str, what: &str) {
    let tokens = |s: &str| -> Vec<String> {
        s.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',' || c == 'x')
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect()
    };
    let (ta, tb) = (tokens(a), tokens(b));
    let same = ta.len() == tb.len()
        && ta
            .iter()
            .zip(&tb)
            .all(|(x, y)| match (x.parse::<f32>(), y.parse::<f32>()) {
                (Ok(p), Ok(q)) => (p - q).abs() < 0.01,
                _ => x == y,
            });
    assert!(
        same,
        "{what}
--- incremental
{a}
--- fresh
{b}"
    );
}

fn check_finite(f: &Fragment<std::sync::Arc<TestStyle>>) {
    f.walk(&mut |fr, at| {
        let ok = at.x.is_finite()
            && at.y.is_finite()
            && fr.size.width.is_finite()
            && fr.size.height.is_finite()
            && fr.size.width >= 0.0
            && fr.size.height >= 0.0;
        let what = match &fr.kind {
            FragmentKind::Box(b) => format!("box {:?} flags {:x}", b.node, b.flags),
            FragmentKind::Line { .. } => "line".to_string(),
            FragmentKind::Text(_) => "text".to_string(),
        };
        assert!(ok, "bad geometry {:?} {:?} {what}", at, fr.size);
    });
}

#[test]
fn random_trees_lay_out_paint_and_hit_test() {
    let only: Option<u64> = std::env::var("FUZZ_SEED").ok().and_then(|v| v.parse().ok());
    let count: u64 = std::env::var("FUZZ_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    for seed in 1..=count {
        if only.is_some_and(|o| o != seed) {
            continue;
        }
        if std::env::var("FUZZ_VERBOSE").is_ok() {
            eprintln!("seed {seed}");
        }
        let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
        let mut t = T::new();
        let root = t.root();
        let mut budget = 40 + rng.below(200) as usize;
        grow(&mut t, &mut rng, root, 0, &mut budget);
        let width = 50.0 + rng.below(400) as f32;
        if std::env::var("FUZZ_VERBOSE").is_ok() {
            fn show(t: &T, n: NodeId, d: usize) {
                let dom = &t.doc.dom;
                if dom.is_text(n) {
                    eprintln!("{:w$}{:?}", "", dom.character_data(n).unwrap(), w = d * 2);
                    return;
                }
                if let Some(s) = t.doc.styles.elements.get(&n) {
                    eprintln!(
                        "{:w$}{} {:?} pos={:?} float={:?}",
                        "",
                        dom.local_name(n),
                        s.display,
                        s.position,
                        s.float,
                        w = d * 2
                    );
                }
                let mut c = dom.first_child(n);
                while let Some(k) = c {
                    show(t, k, d + 1);
                    c = dom.next_sibling(k);
                }
            }
            show(&t, t.doc.root, 0);
            eprintln!("building");
            t.layout.build(&t.doc.dom, t.doc.document, &t.doc.styles);
            eprintln!("built");
            let tree = t.layout.boxes();
            let mut seen = std::collections::HashSet::new();
            let mut stack = vec![tree.root().unwrap()];
            while let Some(b) = stack.pop() {
                assert!(seen.insert(b), "cycle at {b:?}");
                let lb = tree.get(b).unwrap();
                stack.extend(lb.children().iter().copied());
            }
            eprintln!("{}", t.dump());
        }
        let f = t.run(width, 300.0);
        if std::env::var("FUZZ_VERBOSE").is_ok() {
            eprintln!("{}", f.dump());
        }
        check_finite(&f);
        let mut b = DisplayListBuilder::new();
        paint(&f, &t.text, Size::new(width, 300.0), &mut b);
        let list = b.build();
        assert_eq!(list.validate(), Ok(()), "seed {seed}");
        for _ in 0..5 {
            let p = Point::new(rng.below(width as u64) as f32, rng.below(300) as f32);
            let _ = hit_test(&f, p, Size::new(width, 300.0));
        }
        // Relayout at another width reuses nothing it shouldn't and agrees with a fresh layout.
        let f2 = t.layout.layout(&t.text, Size::new(width + 37.0, 300.0));
        check_finite(&f2);
        let mut fresh = LayoutTree::new();
        fresh.build(&t.doc.dom, t.doc.document, &t.doc.styles);
        let f3 = fresh.layout(&t.text, Size::new(width + 37.0, 300.0));
        assert_same(
            &f2.dump(),
            &f3.dump(),
            &format!("seed {seed}: relayout differs from a fresh layout"),
        );

        // Edit a few text nodes incrementally; each relayout must equal a fresh one.
        let text_nodes: Vec<NodeId> = t
            .doc
            .dom
            .descendants(t.doc.root)
            .filter(|&n| t.doc.dom.is_text(n))
            .collect();
        let size = Size::new(width + 37.0, 300.0);
        for _ in 0..3 {
            if text_nodes.is_empty() {
                break;
            }
            let n = text_nodes[rng.below(text_nodes.len() as u64) as usize];
            let w = rng.pick(WORDS);
            let w2 = rng.pick(WORDS);
            let old = t.doc.dom.character_data(n).unwrap_or("").to_string();
            t.doc.dom.set_data(n, &format!("{w2} {w} {w2}"));
            let handled = t.layout.text_changed(&t.doc.dom, n);
            if std::env::var("FUZZ_VERBOSE").is_ok() {
                eprintln!(
                    "edit {n:?} {old:?} -> {:?}: {handled}",
                    t.doc.dom.character_data(n).unwrap_or("")
                );
            }
            if !handled {
                let parent = t.doc.dom.parent(n).unwrap();
                t.layout.children_changed(&t.doc.dom, parent, &t.doc.styles);
            }
            let inc = t.layout.layout(&t.text, size);
            let mut fresh = LayoutTree::new();
            fresh.build(&t.doc.dom, t.doc.document, &t.doc.styles);
            let full = fresh.layout(&t.text, size);
            assert_same(
                &inc.dump(),
                &full.dump(),
                &format!("seed {seed}: incremental relayout after a text edit differs"),
            );
        }
    }
}

/// What painting reads from each fragment's style (colours, stacking, decorations), in tree order: an
/// incremental update that kept a stale style handle somewhere shows here even when the geometry
/// agrees.
fn paint_dump(f: &Fragment<std::sync::Arc<TestStyle>>) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    f.walk(&mut |fr, _| match &fr.kind {
        FragmentKind::Box(b) => {
            let s = &b.style;
            let _ = writeln!(
                out,
                "box {:?} {:?} {:?} {:?} {:?} {}",
                b.node,
                s.color(),
                s.background_color(),
                s.z_index(),
                s.text_decoration_line(),
                s.opacity()
            );
        }
        FragmentKind::Text(t) => {
            let _ = writeln!(
                out,
                "text {:?} {:?} {:?}",
                t.node,
                t.style.color(),
                t.decorations
            );
        }
        FragmentKind::Line { .. } => {}
    });
    out
}

/// One random change to an element's style, and what the style engine would classify it as (the
/// rules of `snowlark-style`'s classification: own damage, whether its contents regenerate, and
/// whether the decorations its contents take changed).
fn mutate(rng: &mut Rng, s: &mut TestStyle) -> (BoxDamage, bool, bool) {
    let color = snowlark_paint::Color::rgb(rng.below(256) as u8, 0, rng.below(256) as u8);
    match rng.below(13) {
        0 => {
            s.color = color;
            // Decorations resolve `currentcolor` and are stored on the boxes they reach.
            let decorated = !s.text_decoration_line.is_none();
            (BoxDamage::Paint, false, decorated)
        }
        1 => {
            s.background_color = color;
            (BoxDamage::Paint, false, false)
        }
        2 => {
            s.z_index = Some(rng.below(5) as i32 - 2);
            (BoxDamage::Repaint, false, false)
        }
        3 => {
            let (a, b) = (rng.below(200) as f32, rng.below(100) as f32);
            s.width = rng.pick(&[SizeValue::Auto, SizeValue::px(a), SizeValue::percent(b)]);
            (BoxDamage::Relayout, false, false)
        }
        4 => {
            let side = rng.below(4) as usize;
            s.padding[side] = LengthPercentage::px(rng.below(15) as f32);
            (BoxDamage::Relayout, false, false)
        }
        5 => {
            s.font_size = 5.0 + rng.below(20) as f32;
            (BoxDamage::Relayout, false, false)
        }
        6 => {
            s.text_align = rng.pick(&[TextAlign::Center, TextAlign::Right, TextAlign::Start]);
            (BoxDamage::Relayout, false, false)
        }
        7 => {
            s.margin[rng.below(4) as usize] = LengthPercentageAuto::px(rng.below(20) as f32);
            s.border_width[rng.below(4) as usize] = rng.below(4) as f32;
            s.border_style = [BorderStyle::Solid; 4];
            (BoxDamage::Relayout, false, false)
        }
        8 => {
            s.display = rng.pick(&[
                Display::BLOCK,
                Display::INLINE,
                Display::INLINE_BLOCK,
                Display::FLEX,
                Display::TABLE_CELL,
                Display::LIST_ITEM,
                Display::None,
                Display::Contents,
            ]);
            (BoxDamage::Reconstruct, false, false)
        }
        9 => {
            if rng.chance(50) {
                s.float = rng.pick(&[Float::None, Float::Left, Float::Right]);
            } else {
                s.position = rng.pick(&[Position::Static, Position::Absolute, Position::Relative]);
            }
            (BoxDamage::Reconstruct, false, false)
        }
        10 => {
            s.text_decoration_line = if s.text_decoration_line.is_none() {
                TextDecorationLine::UNDERLINE
            } else {
                TextDecorationLine::NONE
            };
            (BoxDamage::Paint, false, true)
        }
        11 => {
            s.white_space_collapse = rng.pick(&[
                WhiteSpaceCollapse::Collapse,
                WhiteSpaceCollapse::Preserve,
                WhiteSpaceCollapse::PreserveBreaks,
            ]);
            (BoxDamage::Relayout, true, false)
        }
        _ => {
            s.list_style_type = if rng.chance(50) {
                ListStyleType::None
            } else {
                ListStyleType::Disc
            };
            s.list_style_position =
                rng.pick(&[ListStylePosition::Inside, ListStylePosition::Outside]);
            (BoxDamage::Relayout, true, false)
        }
    }
}

#[test]
fn random_changes_update_the_box_tree_like_a_fresh_build() {
    let only: Option<u64> = std::env::var("FUZZ_SEED").ok().and_then(|v| v.parse().ok());
    let count: u64 = std::env::var("FUZZ_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let verbose = std::env::var("FUZZ_VERBOSE").is_ok();
    // (seeds past the default count that found bugs: text turning into white space, kept in place)
    let found = [1501, 2080, 5405].into_iter().filter(|&s| s > count);
    for seed in (1..=count).chain(found) {
        if only.is_some_and(|o| o != seed) {
            continue;
        }
        let mut rng = Rng(seed.wrapping_mul(0xD1B54A32D192ED03) | 1);
        let mut t = T::new();
        let root = t.root();
        let mut budget = 30 + rng.below(150) as usize;
        grow(&mut t, &mut rng, root, 0, &mut budget);
        let size = Size::new(80.0 + rng.below(400) as f32, 300.0);
        t.run(size.width, size.height);
        for round in 0..6 {
            let elements: Vec<NodeId> = t
                .doc
                .dom
                .descendants(t.doc.root)
                .filter(|&n| t.doc.dom.is_element(n) && n != t.doc.root)
                .collect();
            if elements.is_empty() {
                break;
            }
            let mut changes = TreeChanges::default();
            for _ in 0..1 + rng.below(4) {
                let el = elements[rng.below(elements.len() as u64) as usize];
                if !t.doc.dom.is_connected(el) {
                    continue;
                }
                match rng.below(10) {
                    // A style change.
                    0..=5 => {
                        let mut s = (*t.doc.styles.elements[&el]).clone();
                        let (damage, contents, decorations) = mutate(&mut rng, &mut s);
                        t.doc.styles.elements.insert(el, std::sync::Arc::new(s));
                        if verbose {
                            eprintln!(
                                "seed {seed} round {round}: {el:?} {damage:?} {contents} {decorations}"
                            );
                        }
                        changes.restyled.push(
                            ElementChange::restyled(el, damage, contents).decorations(decorations),
                        );
                    }
                    // A ::before appearing, changing or disappearing.
                    6 => {
                        let key = (el, PseudoElement::Before);
                        if t.doc.styles.pseudos.contains_key(&key) && rng.chance(40) {
                            t.doc.styles.pseudos.remove(&key);
                        } else {
                            let w = rng.pick(WORDS);
                            let display = rng.pick(&[Display::INLINE, Display::BLOCK]);
                            t.doc.pseudo(el, PseudoElement::Before, |s| {
                                s.display = display;
                                s.content = Content::Items(vec![ContentItem::Text(w.into())]);
                            });
                        }
                        if verbose {
                            eprintln!("seed {seed} round {round}: {el:?} ::before");
                        }
                        changes.restyled.push(ElementChange::restyled(
                            el,
                            BoxDamage::Repaint,
                            true,
                        ));
                    }
                    // A child appended (with its first style), or the element removed.
                    7 => {
                        let child = if rng.chance(50) {
                            let w = rng.pick(WORDS);
                            t.doc.text(el, w)
                        } else {
                            let tag = rng.pick(&["div", "span", "li"]);
                            let c = t.doc.element(el, tag, |s| random_style(&mut rng, s, 3));
                            t.doc.text(c, "new");
                            changes.restyled.push(ElementChange::reconstruct(c));
                            c
                        };
                        if verbose {
                            eprintln!("seed {seed} round {round}: append {child:?} to {el:?}");
                        }
                        changes.children.push(el);
                    }
                    8 => {
                        let parent = t.doc.dom.parent(el).unwrap();
                        t.doc.dom.remove_node(el);
                        if verbose {
                            eprintln!("seed {seed} round {round}: remove {el:?}");
                        }
                        changes.children.push(parent);
                    }
                    // A text child's data.
                    _ => {
                        let text = t.doc.dom.children(el).find(|&c| t.doc.dom.is_text(c));
                        if let Some(c) = text {
                            let data = format!("{} {}", rng.pick(WORDS), rng.pick(WORDS));
                            t.doc.dom.set_data(c, &data);
                            if verbose {
                                eprintln!("seed {seed} round {round}: text {c:?} {data:?}");
                            }
                            changes.text.push(el);
                        }
                    }
                }
            }
            let stats = t
                .layout
                .update(&t.doc.dom, t.doc.document, &t.doc.styles, &changes);
            let inc = t.layout.layout(&t.text, size);
            check_finite(&inc);
            let mut fresh = LayoutTree::new();
            fresh.build(&t.doc.dom, t.doc.document, &t.doc.styles);
            let full = fresh.layout(&t.text, size);
            let what = format!("seed {seed} round {round}: incremental update differs ({stats:?})");
            assert_same(&inc.dump(), &full.dump(), &what);
            let (pi, pf) = (paint_dump(&inc), paint_dump(&full));
            if pi != pf {
                let diff: Vec<String> = pi
                    .lines()
                    .zip(pf.lines())
                    .filter(|(a, b)| a != b)
                    .take(4)
                    .map(|(a, b)| {
                        format!(
                            "incremental {a}
      fresh {b}"
                        )
                    })
                    .collect();
                panic!(
                    "{what}: painted styles differ
{}",
                    diff.join(
                        "
"
                    )
                );
            }
        }
    }
}
