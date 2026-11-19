// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark --layout-demo <png> [--runs N]`: builds a styled document by hand (the layout crate's test
//! `Style` struct standing in for stylo) covering block, inline, float, flex, grid, table and absolute
//! positioning; lays it out, paints it headless to `<png>`, and prints timings: box tree build, full
//! layout, relayout after one text change, display list, raster; then the same for a generated
//! 5,000-box document.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use snowlark_dom::NodeId;
use snowlark_layout::style::*;
use snowlark_layout::testing::{TestDocument, TestStyle};
use snowlark_layout::{BoxDamage, ElementChange, LayoutTree, TreeChanges};
use snowlark_paint::image::{bmp_from_rgba, decode};
use snowlark_paint::{
    Color, DisplayListBuilder, HeadlessRenderer, ImageStore, Matrix, Size, Sources, TextEngine,
    Viewport,
};

const WIDTH: u32 = 1000;
const HEIGHT: u32 = 1300;

fn median_ms(runs: usize, mut f: impl FnMut()) -> f64 {
    f();
    let mut times = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t = Instant::now();
        f();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

pub fn run(args: &[String]) -> i32 {
    let mut png = None;
    let mut runs = 20usize;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--runs" {
            runs = args
                .get(i + 1)
                .and_then(|v| v.parse().ok())
                .unwrap_or(runs)
                .max(1);
            i += 2;
        } else {
            png = Some(PathBuf::from(&args[i]));
            i += 1;
        }
    }
    let Some(png) = png else {
        eprintln!("usage: snowlark --layout-demo <png> [--runs N]");
        return 2;
    };
    match demo(&png, runs) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("layout-demo: {e}");
            1
        }
    }
}

fn px(v: f32) -> SizeValue {
    SizeValue::px(v)
}

fn lp(v: f32) -> LengthPercentage {
    LengthPercentage::px(v)
}

fn hex(v: u32) -> Color {
    Color::hex(v)
}

/// The demo page; returns the document and a text node to edit for the relayout timing.
fn page(images: &ImageStore) -> Result<(TestDocument, NodeId), String> {
    let mut d = TestDocument::with_root_style(|s| {
        s.display = Display::BLOCK;
        s.font_family = vec!["Segoe UI".into(), "sans-serif".into()];
        s.font_size = 15.0;
        s.color = hex(0x202124);
        s.background_color = hex(0xf6f7f9);
        s.line_height = LineHeight::Number(1.4);
    });
    let root = d.root;
    let body = d.element(root, "body", |s| {
        s.display = Display::BLOCK;
        s.margin_px(24.0);
    });

    // Header: rounded, bordered, a gradient strip and a shadow.
    let header = d.block(body, |s| {
        s.padding = [lp(16.0), lp(20.0), lp(16.0), lp(20.0)];
        s.background_color = hex(0x1a73e8);
        s.color = Color::WHITE;
        s.border_radius = [(lp(12.0), lp(12.0)); 4];
        s.box_shadows = vec![Shadow {
            offset_x: 0.0,
            offset_y: 4.0,
            blur: 12.0,
            spread: 0.0,
            color: Color::rgba(0, 0, 0, 70),
            inset: false,
        }];
        s.position = Position::Relative;
    });
    let h1 = d.element(header, "h1", |s| {
        s.display = Display::BLOCK;
        s.font_size = 28.0;
        s.font_weight = 700;
        s.margin_px(0.0);
    });
    d.text(h1, "Snowlark layout demo");
    let sub = d.block(header, |s| s.font_size = 14.0);
    d.text(sub, "Block, inline, floats, flex, grid, tables and positioning, painted by snowlark-paint");
    // An absolutely positioned badge in the header's corner.
    let badge = d.block(header, |s| {
        s.position = Position::Absolute;
        s.inset[0] = LengthPercentageAuto::px(14.0);
        s.inset[1] = LengthPercentageAuto::px(16.0);
        s.padding = [lp(4.0), lp(10.0), lp(4.0), lp(10.0)];
        s.background_color = hex(0xfbbc04);
        s.color = hex(0x202124);
        s.font_weight = 700;
        s.font_size = 12.0;
        s.border_radius = [(lp(10.0), lp(10.0)); 4];
        s.transform = Some(Matrix::rotation(0.08));
    });
    d.text(badge, "position: absolute");

    // A paragraph with a float, inline styles, an inline-block and an image.
    let para = d.block(body, |s| {
        s.margin = [
            LengthPercentageAuto::px(20.0),
            LengthPercentageAuto::ZERO,
            LengthPercentageAuto::px(12.0),
            LengthPercentageAuto::ZERO,
        ];
        s.text_align = TextAlign::Justify;
    });
    let fl = d.block(para, |s| {
        s.float = Float::Left;
        s.width = px(160.0);
        s.height = px(110.0);
        s.margin = [
            LengthPercentageAuto::ZERO,
            LengthPercentageAuto::px(14.0),
            LengthPercentageAuto::px(6.0),
            LengthPercentageAuto::ZERO,
        ];
        s.background_layers = vec![BackgroundLayer {
            image: Image::LinearGradient(Arc::new(LinearGradient {
                angle: 135.0,
                corner: None,
                stops: vec![
                    ColorStop::new(hex(0x34a853), None),
                    ColorStop::new(hex(0x4285f4), None),
                ],
                ramps: Vec::new(),
                repeating: false,
            })),
            ..BackgroundLayer::default()
        }];
        s.border_radius = [(lp(8.0), lp(8.0)); 4];
        s.color = Color::WHITE;
        s.padding_px(8.0);
        s.font_weight = 600;
    });
    d.text(fl, "float: left with a linear-gradient background");
    let edit = d.text(
        para,
        "Text flows around the float, line by line, and is justified across the full width once \
         it clears it. Inline formatting handles ",
    );
    let b = d.span(para, |s| s.font_weight = 700);
    d.text(b, "bold spans");
    d.text(para, ", ");
    let u = d.span(para, |s| {
        s.text_decoration_line = TextDecorationLine::UNDERLINE;
        s.color = hex(0x1a73e8);
    });
    d.text(u, "underlined links");
    d.text(para, ", ");
    let hl = d.span(para, |s| {
        s.background_color = hex(0xfff3bf);
        s.padding = [lp(1.0), lp(4.0), lp(1.0), lp(4.0)];
        s.border_radius = [(lp(4.0), lp(4.0)); 4];
        s.border_width = [1.0; 4];
        s.border_style = [snowlark_paint::BorderStyle::Solid; 4];
        s.border_color = [Some(hex(0xf0c000)); 4];
    });
    d.text(hl, "inline boxes with padding and borders");
    d.text(para, ", an inline-block ");
    let ib = d.span(para, |s| {
        s.display = Display::INLINE_BLOCK;
        s.width = px(70.0);
        s.padding_px(4.0);
        s.background_color = hex(0xe8f0fe);
        s.border(1.0, hex(0x1a73e8));
        s.font_size = 11.0;
        s.text_align = TextAlign::Center;
        s.vertical_align = VerticalAlign::Middle;
    });
    d.text(ib, "inline-block, two lines");
    d.text(para, " aligned to the middle, an image ");
    let img_id = images.add(
        decode(&bmp_from_rgba(32, 32, &checker(32))).map_err(|e| e.to_string())?,
    );
    let img = d.element(para, "img", |s| s.width = px(24.0));
    d.replace(img, ReplacedContent::image(img_id, 32.0, 32.0));
    d.text(
        para,
        " on the baseline, and long words break at their opportunities: \
         supercalifragilisticexpialidocious, internationalization, \
         and Arabic مرحبا بالعالم or 日本語のテキスト through font fallback.",
    );

    // Flex row.
    let flex = d.block(body, |s| {
        s.display = Display::FLEX;
        s.column_gap = Some(lp(12.0));
        s.align_items = Align::Center;
        s.margin[2] = LengthPercentageAuto::px(14.0);
        s.padding_px(12.0);
        s.background_color = Color::WHITE;
        s.border(1.0, hex(0xdadce0));
        s.border_radius = [(lp(8.0), lp(8.0)); 4];
    });
    let colors = [0xea4335, 0xfbbc04, 0x34a853, 0x4285f4];
    for (i, c) in colors.iter().enumerate() {
        let item = d.block(flex, |s| {
            s.flex_grow = (i + 1) as f32;
            s.flex_basis = FlexBasis::LengthPercentage(lp(0.0));
            s.padding_px(10.0);
            s.height = px(30.0 + 12.0 * i as f32);
            s.background_color = hex(*c);
            s.color = Color::WHITE;
            s.border_radius = [(lp(6.0), lp(6.0)); 4];
            s.font_weight = 600;
        });
        d.text(item, &format!("flex-grow: {}", i + 1));
    }

    // Grid with areas.
    let grid = d.block(body, |s| {
        s.display = Display::GRID;
        s.grid_template_columns = TrackList {
            items: vec![
                TrackListItem::Track(TrackSize::Breadth(TrackBreadth::LengthPercentage(lp(
                    180.0,
                )))),
                TrackListItem::Repeat {
                    count: RepeatCount::Count(2),
                    tracks: vec![TrackSize::Breadth(TrackBreadth::Fr(1.0))],
                    line_names: Vec::new(),
                },
            ],
            line_names: Vec::new(),
        };
        s.grid_auto_rows = vec![TrackSize::MinMax(
            TrackBreadth::LengthPercentage(lp(44.0)),
            TrackBreadth::Auto,
        )];
        s.row_gap = Some(lp(8.0));
        s.column_gap = Some(lp(8.0));
        s.margin[2] = LengthPercentageAuto::px(14.0);
        s.grid_template_areas = Some(GridTemplateAreas {
            areas: vec![GridArea {
                name: "side".into(),
                row_start: 1,
                row_end: 3,
                column_start: 1,
                column_end: 2,
            }],
            rows: 2,
            columns: 3,
        });
    });
    let side = d.block(grid, |s| {
        s.grid_row_start = GridLine::Named("side".into(), 0);
        s.grid_row_end = GridLine::Named("side".into(), 0);
        s.grid_column_start = GridLine::Named("side".into(), 0);
        s.grid_column_end = GridLine::Named("side".into(), 0);
        s.background_color = hex(0x202124);
        s.color = Color::WHITE;
        s.padding_px(10.0);
        s.border_radius = [(lp(6.0), lp(6.0)); 4];
    });
    d.text(side, "grid-area: side (two rows)");
    for i in 0..4 {
        let cell = d.block(grid, |s| {
            s.background_color = hex(if i % 2 == 0 { 0xe6f4ea } else { 0xfce8e6 });
            s.padding_px(10.0);
            s.border_radius = [(lp(6.0), lp(6.0)); 4];
        });
        d.text(cell, &format!("auto-placed item {}", i + 1));
    }

    // Table with collapsed borders, a caption, colspan and alignment.
    let table = d.element(body, "table", |s| {
        s.display = Display::TABLE;
        s.border_collapse = BorderCollapse::Collapse;
        s.border(2.0, hex(0x5f6368));
        s.background_color = Color::WHITE;
        s.width = SizeValue::percent(100.0);
        s.margin[2] = LengthPercentageAuto::px(14.0);
    });
    let cap = d.element(table, "caption", |s| {
        s.display = Display::TABLE_CAPTION;
        s.font_weight = 700;
        s.padding_px(6.0);
    });
    d.text(cap, "table: auto layout, collapsing borders, colspan");
    let rows = [
        ["Algorithm", "Spec", "Status"],
        ["Block and floats", "CSS 2 §9–10", "ours"],
        ["Inline and bidi", "css-text-3, css-inline-3", "ours"],
        ["Flex and grid", "css-flexbox-1, css-grid-2", "taffy"],
    ];
    for (ri, row) in rows.iter().enumerate() {
        let tr = d.element(table, "tr", |s| {
            s.display = Display::TABLE_ROW;
            if ri == 0 {
                s.background_color = hex(0xe8eaed);
                s.font_weight = 700;
            }
        });
        for (ci, text) in row.iter().enumerate() {
            let td = d.element(tr, "td", |s| {
                s.display = Display::TABLE_CELL;
                s.padding = [lp(6.0), lp(10.0), lp(6.0), lp(10.0)];
                s.border(1.0, hex(0xbdc1c6));
                if ci == 2 {
                    s.text_align = TextAlign::Center;
                }
            });
            d.text(td, text);
        }
    }
    let tr = d.element(table, "tr", |s| s.display = Display::TABLE_ROW);
    let wide = d.element(tr, "td", |s| {
        s.display = Display::TABLE_CELL;
        s.padding_px(6.0);
        s.border(1.0, hex(0xbdc1c6));
        s.background_color = hex(0xfef7e0);
        s.text_align = TextAlign::Center;
    });
    d.dom.set_attribute(wide, "colspan", "3").map_err(|e| format!("{e:?}"))?;
    d.text(wide, "colspan=3");

    // A list, and a relatively positioned box with z-index stacking.
    let ul = d.element(body, "ul", |s| {
        s.display = Display::BLOCK;
        s.padding[3] = lp(28.0);
        s.margin[2] = LengthPercentageAuto::px(10.0);
    });
    for (i, t) in ["list-item with an outside ::marker", "ordered by HTML's rules", "and wrapped like any block"].iter().enumerate() {
        let li = d.element(ul, "li", |s| {
            s.display = Display::LIST_ITEM;
            s.list_style_type = if i == 1 {
                ListStyleType::Square
            } else {
                ListStyleType::Disc
            };
        });
        d.text(li, t);
    }
    let stack = d.block(body, |s| {
        s.position = Position::Relative;
        s.height = px(90.0);
    });
    for (i, c) in [0xea4335u32, 0x34a853, 0x4285f4].iter().enumerate() {
        let b = d.block(stack, |s| {
            s.position = Position::Absolute;
            s.inset[0] = LengthPercentageAuto::px(10.0 * i as f32);
            s.inset[3] = LengthPercentageAuto::px(40.0 * i as f32);
            s.width = px(120.0);
            s.height = px(60.0);
            s.background_color = hex(*c).with_alpha(220);
            s.z_index = Some(3 - i as i32);
            s.color = Color::WHITE;
            s.padding_px(6.0);
            s.border_radius = [(lp(6.0), lp(6.0)); 4];
        });
        d.text(b, &format!("z-index: {}", 3 - i));
    }
    let op = d.block(stack, |s| {
        s.position = Position::Absolute;
        s.inset[0] = LengthPercentageAuto::px(5.0);
        s.inset[3] = LengthPercentageAuto::px(240.0);
        s.width = px(220.0);
        s.opacity = 0.6;
        s.background_color = hex(0x9334e6);
        s.color = Color::WHITE;
        s.padding_px(8.0);
        s.border_radius = [(lp(6.0), lp(6.0)); 4];
    });
    d.text(op, "opacity: 0.6 makes a stacking context");
    let rtl = d.block(body, |s| {
        s.direction = snowlark_paint::Direction::Rtl;
        s.margin[0] = LengthPercentageAuto::px(8.0);
        s.padding_px(6.0);
        s.background_color = Color::WHITE;
        s.border(1.0, hex(0xdadce0));
    });
    d.text(rtl, "direction: rtl — שלום עולם, text aligns to the start edge on the right.");
    Ok((d, edit))
}

fn checker(n: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let on = ((x / 8) + (y / 8)) % 2 == 0;
            let (r, g, b) = if on { (234, 67, 53) } else { (255, 255, 255) };
            v.extend_from_slice(&[r, g, b, 255]);
        }
    }
    v
}

/// A 5,000-box document: sections of paragraphs with inline spans, nested blocks and a few floats.
fn big_document() -> (TestDocument, NodeId, usize) {
    let mut d = TestDocument::with_root_style(|s| {
        s.display = Display::BLOCK;
        s.font_family = vec!["Segoe UI".into()];
        s.font_size = 14.0;
        s.line_height = LineHeight::Number(1.3);
    });
    let root = d.root;
    let body = d.block(root, |s| s.margin_px(8.0));
    let mut edit = None;
    let mut elements = 2usize;
    let mut section = 0;
    while elements < 5000 {
        let sec = d.block(body, |s| {
            s.margin[2] = LengthPercentageAuto::px(10.0);
            s.padding_px(4.0);
        });
        elements += 1;
        let h = d.block(sec, |s| {
            s.font_size = 18.0;
            s.font_weight = 700;
        });
        d.text(h, &format!("Section {section}"));
        elements += 1;
        for p in 0..6 {
            let para = d.block(sec, |s| s.margin[2] = LengthPercentageAuto::px(6.0));
            elements += 1;
            if p == 0 && section % 3 == 0 {
                let f = d.block(para, |s| {
                    s.float = Float::Right;
                    s.width = px(120.0);
                    s.height = px(40.0);
                    s.background_color = hex(0xe8f0fe);
                });
                d.text(f, "aside");
                elements += 1;
            }
            let t = d.text(
                para,
                "Layout performance is measured, not assumed: this paragraph has some text, ",
            );
            if edit.is_none() && section == 40 {
                edit = Some(t);
            }
            let s = d.span(para, |s| s.font_weight = 700);
            d.text(s, "a bold span");
            elements += 1;
            d.text(para, " and more words so that it wraps across a few lines at this width.");
            let inner = d.block(para, |s| s.padding[3] = lp(10.0));
            elements += 1;
            d.text(inner, "A nested block.");
        }
        section += 1;
    }
    (d, edit.expect("edit target"), elements)
}

fn demo(png: &std::path::Path, runs: usize) -> Result<(), String> {
    let text = TextEngine::new().map_err(|e| e.to_string())?;
    let images = ImageStore::new();
    let (mut doc, edit) = page(&images)?;
    let viewport = Size::new(WIDTH as f32, HEIGHT as f32);

    let t = Instant::now();
    let mut layout: LayoutTree<Arc<TestStyle>> = LayoutTree::new();
    layout.build(&doc.dom, doc.document, &doc.styles);
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    let fragments = layout.layout(&text, viewport);
    let first_ms = t.elapsed().as_secs_f64() * 1000.0;
    let boxes = layout.boxes().len();
    let frags = fragments.count();
    let full_ms = median_ms(runs, || {
        let mut l: LayoutTree<Arc<TestStyle>> = LayoutTree::new();
        l.build(&doc.dom, doc.document, &doc.styles);
        l.layout(&text, viewport);
    });
    // Relayout after editing one text node, alternating between two strings.
    let mut flip = false;
    let relayout_ms = median_ms(runs, || {
        flip = !flip;
        let s = if flip {
            "Text flows around the float, line by line (edited), and is justified across the full width once it clears it. Inline formatting handles "
        } else {
            "Text flows around the float, line by line, and is justified across the full width once it clears it. Inline formatting handles "
        };
        doc.dom.set_data(edit, s);
        layout.text_changed(&doc.dom, edit);
        layout.layout(&text, viewport);
    });
    let stats = layout.stats();
    // Paint.
    let fragments = layout.layout(&text, viewport);
    let mut builder = DisplayListBuilder::new();
    snowlark_layout::paint(&fragments, &text, viewport, &mut builder);
    let list = builder.build();
    let paint_ms = median_ms(runs, || {
        let mut b = DisplayListBuilder::new();
        snowlark_layout::paint(&fragments, &text, viewport, &mut b);
        b.build();
    });
    let mut renderer = HeadlessRenderer::new().map_err(|e| e.to_string())?;
    let sources = Sources {
        text: &text,
        images: &images,
    };
    let view = Viewport {
        width: WIDTH,
        height: HEIGHT,
        scale: 1.0,
        background: Color::WHITE,
    };
    let t = Instant::now();
    let pixmap = renderer
        .render(&list, sources, &view)
        .map_err(|e| e.to_string())?;
    let raster_ms = t.elapsed().as_secs_f64() * 1000.0;
    pixmap.write_png(png).map_err(|e| e.to_string())?;
    println!(
        "demo page: {boxes} boxes, {frags} fragments; box tree {build_ms:.2} ms; first layout {first_ms:.2} ms (font matching, shaping)"
    );
    println!(
        "  full build+layout {full_ms:.2} ms, relayout after a text edit {relayout_ms:.3} ms ({} boxes laid out, {} reused, {} items shaped)",
        stats.laid_out, stats.reused, stats.shaped
    );
    println!(
        "  display list {} items in {paint_ms:.2} ms; raster {raster_ms:.1} ms -> {}",
        list.len(),
        png.display()
    );

    // The 5,000-box document.
    let (mut big, edit, elements) = big_document();
    let viewport = Size::new(1000.0, 800.0);
    let mut l: LayoutTree<Arc<TestStyle>> = LayoutTree::new();
    let t = Instant::now();
    l.build(&big.dom, big.document, &big.styles);
    let build_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    let f = l.layout(&text, viewport);
    let cold_ms = t.elapsed().as_secs_f64() * 1000.0;
    let cold_prepare_ms = l.stats().prepare_us as f64 / 1000.0;
    let boxes = l.boxes().len();
    let height = f.overflow.height;
    let full_ms = median_ms(runs.min(10), || {
        let mut l2: LayoutTree<Arc<TestStyle>> = LayoutTree::new();
        l2.build(&big.dom, big.document, &big.styles);
        l2.layout(&text, viewport);
    });
    let mut flip = false;
    let relayout_ms = median_ms(runs, || {
        flip = !flip;
        let s = if flip {
            "Layout performance is measured, not assumed: this edited paragraph has some text, "
        } else {
            "Layout performance is measured, not assumed: this paragraph has some text, "
        };
        big.dom.set_data(edit, s);
        l.text_changed(&big.dom, edit);
        l.layout(&text, viewport);
    });
    let stats = l.stats();
    let resize_ms = median_ms(runs.min(10), || {
        flip = !flip;
        l.layout(&text, Size::new(if flip { 900.0 } else { 1000.0 }, 800.0));
    });
    // A style change on one element (the edited paragraph): a colour, then a padding, through the
    // incremental update (the element's boxes take the new style in place) and through
    // `style_changed` (its parent's contents regenerated).
    let para = big.dom.parent(edit).unwrap();
    let style_ms = |l: &mut LayoutTree<Arc<TestStyle>>,
                    big: &mut TestDocument,
                    flip: &mut bool,
                    relayout: bool,
                    in_place: bool| {
        median_ms(runs, || {
            *flip = !*flip;
            let f = *flip;
            big.restyle(para, |s| {
                if relayout {
                    s.padding[3] = lp(if f { 12.0 } else { 0.0 });
                } else {
                    s.color = if f { hex(0xd93025) } else { Color::BLACK };
                }
            });
            if in_place {
                let damage = if relayout {
                    BoxDamage::Relayout
                } else {
                    BoxDamage::Repaint
                };
                let changes = TreeChanges {
                    restyled: vec![ElementChange::restyled(para, damage, false)],
                    ..Default::default()
                };
                l.update(&big.dom, big.document, &big.styles, &changes);
            } else {
                l.style_changed(&big.dom, para, &big.styles);
            }
            l.layout(&text, viewport);
        })
    };
    let repaint_ms = style_ms(&mut l, &mut big, &mut flip, false, true);
    let restyle_stats = l.stats();
    let relayout_style_ms = style_ms(&mut l, &mut big, &mut flip, true, true);
    let old_repaint_ms = style_ms(&mut l, &mut big, &mut flip, false, false);
    let old_stats = l.stats();
    let old_relayout_ms = style_ms(&mut l, &mut big, &mut flip, true, false);
    println!(
        "  style change on one element, in place: colour {repaint_ms:.3} ms ({} laid out, {} shaped), padding {relayout_style_ms:.3} ms",
        restyle_stats.laid_out, restyle_stats.shaped
    );
    println!(
        "  the same through style_changed (parent's contents regenerated): colour {old_repaint_ms:.3} ms ({} laid out, {} shaped, {} from the shaping cache), padding {old_relayout_ms:.3} ms",
        old_stats.laid_out, old_stats.shaped, old_stats.shape_cache_hits
    );
    println!(
        "{elements} elements, {boxes} boxes ({height:.0} px tall): box tree {build_ms:.2} ms, first layout {cold_ms:.2} ms, of which {cold_prepare_ms:.2} ms text preparation (white space, fonts, shaping)"
    );
    println!(
        "  full build+layout {full_ms:.2} ms; relayout after one text change {relayout_ms:.3} ms ({} laid out, {} reused, {} shaped); relayout at a new width {resize_ms:.2} ms",
        stats.laid_out, stats.reused, stats.shaped
    );
    Ok(())
}
