// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests of the API the page crate uses: scroll positions in painting and hit testing, geometry
//! queries, and a few painting rules (flex item stacking, tab stops).

use std::collections::HashMap;
use std::sync::Arc;

use snowlark_paint::{DisplayListBuilder, Item, Point, Size};

use crate::style::*;
use crate::tests::*;
use crate::*;

#[test]
fn scroll_positions_move_content_but_not_fixed_boxes() {
    let mut t = T::new();
    let r0 = t.root();
    let scroller = t.doc.block(r0, |s| {
        s.height = SizeValue::px(50.0);
        s.overflow_x = Overflow::Auto;
        s.overflow_y = Overflow::Auto;
    });
    let inner = t.doc.block(scroller, |s| s.height = SizeValue::px(300.0));
    let deep = t.doc.block(inner, |s| {
        s.margin[0] = LengthPercentageAuto::px(100.0);
        s.height = SizeValue::px(10.0);
    });
    let fixed = t.doc.block(r0, |s| {
        s.position = Position::Fixed;
        s.inset[0] = LengthPercentageAuto::px(0.0);
        s.inset[1] = LengthPercentageAuto::px(0.0);
        s.width = SizeValue::px(20.0);
        s.height = SizeValue::px(20.0);
    });
    let f = t.run(200.0, 200.0);
    let size = Size::new(200.0, 200.0);
    // Unscrolled, `deep` (at y 100) is clipped away by the 50px scroller.
    assert_ne!(
        hit_test(&f, Point::new(10.0, 25.0), size).map(|h| h.node),
        Some(deep)
    );
    let mut offsets = HashMap::new();
    offsets.insert(scroller, Point::new(0.0, 80.0));
    let opts = PaintOptions {
        viewport: size,
        viewport_scroll: Point::ZERO,
        scroll: Some(&offsets),
        embeds: None,
        selection: None,
        find: None,
        layers: false,
        device_scale: 1.0,
        composited: None,
        animations: None,
    };
    // Scrolled by 80, `deep` is at 20..30.
    assert_eq!(
        hit_test_with(&f, Point::new(10.0, 25.0), &opts).map(|h| h.node),
        Some(deep)
    );
    // The viewport scrolled by 30: content moves up, the fixed box doesn't.
    let opts = PaintOptions {
        viewport: size,
        viewport_scroll: Point::new(0.0, 30.0),
        scroll: None,
        embeds: None,
        selection: None,
        find: None,
        layers: false,
        device_scale: 1.0,
        composited: None,
        animations: None,
    };
    assert_eq!(
        hit_test_with(&f, Point::new(190.0, 10.0), &opts).map(|h| h.node),
        Some(fixed)
    );
    let mut b = DisplayListBuilder::new();
    paint_with(&f, &t.text, &opts, &mut b);
    assert_eq!(b.build().validate(), Ok(()));
}

#[test]
fn geometry_and_client_rects() {
    let mut t = T::new();
    let r0 = t.root();
    let d = t.doc.block(r0, |s| {
        s.margin_px(5.0);
        s.padding_px(3.0);
        s.border(2.0, snowlark_paint::Color::BLACK);
        s.width = SizeValue::px(40.0);
    });
    let span = t.doc.span(d, |_| {});
    let text = t.doc.text(span, "aaa bbb");
    t.run(200.0, 200.0);
    let g = t.layout.geometry(d).unwrap();
    assert_eq!(g.border_box, r(5.0, 5.0, 50.0, 30.0));
    assert_eq!(g.padding_box, r(7.0, 7.0, 46.0, 26.0));
    assert_eq!(g.content_box, r(10.0, 10.0, 40.0, 20.0));
    // The span wraps onto two lines: two client rects.
    assert_eq!(t.layout.client_rects(span).len(), 2);
    assert_eq!(t.layout.client_rects(text)[1], r(10.0, 20.0, 30.0, 10.0));
}

#[test]
fn flex_items_with_z_index_stack_without_positioning() {
    let mut t = T::new();
    let r0 = t.root();
    let red = snowlark_paint::Color::rgb(255, 0, 0);
    let blue = snowlark_paint::Color::rgb(0, 0, 255);
    let flex = t.doc.block(r0, |s| s.display = Display::FLEX);
    t.doc.block(flex, |s| {
        s.width = SizeValue::px(10.0);
        s.height = SizeValue::px(10.0);
        s.background_color = red;
        s.z_index = Some(1);
    });
    t.doc.block(flex, |s| {
        s.width = SizeValue::px(10.0);
        s.height = SizeValue::px(10.0);
        s.background_color = blue;
        s.margin[3] = LengthPercentageAuto::px(-5.0);
    });
    let f = t.run(100.0, 100.0);
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, t.viewport, &mut b);
    let colors: Vec<_> = b
        .build()
        .items
        .iter()
        .filter_map(|i| match i {
            Item::FillRect { color, .. } => Some(*color),
            _ => None,
        })
        .collect();
    // The z-index item paints above its later sibling.
    assert_eq!(colors, vec![blue, red]);
}

#[test]
fn tab_stops_are_multiples_of_the_space_advance() {
    let mut t = T::new();
    let r0 = t.root();
    let pre = t.doc.block(r0, |s| {
        s.white_space_collapse = WhiteSpaceCollapse::Preserve;
        s.tab_size = TabSize::Spaces(4.0);
    });
    let text = t.doc.text(pre, "a\tb");
    let f = t.run(300.0, 100.0);
    let tx = texts(&f, text);
    // Ahem's space is 10px wide: tab stops every 40px.
    assert_eq!(tx[1].x, 40.0);
}

#[test]
fn fixed_and_sticky_boxes_are_marked_as_layers() {
    use snowlark_paint::{Color, LayerKind, Rect};
    let mut t = T::new();
    let r0 = t.root();
    let container = t.doc.block(r0, |s| {
        s.height = SizeValue::px(1000.0);
        s.background_color = Color::hex(0xeeeeee);
    });
    t.doc.block(container, |s| s.height = SizeValue::px(50.0));
    t.doc.block(container, |s| {
        s.position = Position::Sticky;
        s.inset[0] = LengthPercentageAuto::px(10.0);
        s.height = SizeValue::px(20.0);
        s.background_color = Color::hex(0x0000ff);
    });
    // A sticky box inside a scroll container: its scrollport is the container's, so it moves with
    // the document (stuck to the container's top as that scrolls).
    let scroller = t.doc.block(r0, |s| {
        s.height = SizeValue::px(50.0);
        s.overflow_x = Overflow::Auto;
        s.overflow_y = Overflow::Auto;
    });
    t.doc.block(scroller, |s| {
        s.position = Position::Sticky;
        s.inset[0] = LengthPercentageAuto::px(0.0);
        s.height = SizeValue::px(10.0);
        s.background_color = Color::hex(0x00ff00);
    });
    t.doc.block(scroller, |s| s.height = SizeValue::px(300.0));
    t.doc.block(r0, |s| {
        s.position = Position::Fixed;
        s.inset[0] = LengthPercentageAuto::px(0.0);
        s.width = SizeValue::px(200.0);
        s.height = SizeValue::px(30.0);
        s.z_index = Some(5);
        s.background_color = Color::hex(0xff0000);
    });
    let f = t.run(200.0, 200.0);
    let mut offsets = HashMap::new();
    offsets.insert(scroller, Point::new(0.0, 20.0));
    let paint_at = |scroll: f32, layers: bool| {
        let opts = PaintOptions {
            viewport: Size::new(200.0, 200.0),
            viewport_scroll: Point::new(0.0, scroll),
            scroll: Some(&offsets),
            layers,
            ..Default::default()
        };
        let mut b = DisplayListBuilder::new();
        paint_with(&f, &t.text, &opts, &mut b);
        b.build()
    };
    // Without layers, nothing is marked.
    assert!(!paint_at(100.0, false)
        .items
        .iter()
        .any(|i| matches!(i, Item::PushLayer { .. })));
    let split = paint_at(100.0, true).split_layers();
    assert!(!split.pinned);
    let kinds: Vec<_> = split.layers.iter().map(|l| l.kind).collect();
    let sticky = kinds.iter().find_map(|k| match k {
        Some(LayerKind::Sticky(c)) => Some(*c),
        _ => None,
    });
    let c = sticky.expect("a sticky layer");
    // Unshifted at 50 in the document, 100 scrolled: at -50 in the list, inside its container.
    assert_eq!(c.rect, Rect::new(0.0, -50.0, 200.0, 20.0));
    assert_eq!(c.container.y, -100.0);
    assert_eq!(c.insets, [Some(10.0), None, None, None]);
    // Painted stuck at 10 for the viewport it was painted for.
    let fill_y = |kind: &dyn Fn(&Option<LayerKind>) -> bool| {
        split
            .layers
            .iter()
            .filter(|p| kind(&p.kind))
            .flat_map(|p| p.list.items.iter())
            .find_map(|i| match i {
                Item::FillRect { rect, .. } => Some(rect.y),
                _ => None,
            })
    };
    assert_eq!(
        fill_y(&|k| matches!(k, Some(LayerKind::Sticky(_)))),
        Some(10.0)
    );
    // The fixed box is a layer of its own, painted last (z-index 5), at the viewport's top.
    assert_eq!(kinds.last(), Some(&Some(LayerKind::Fixed)));
    assert_eq!(fill_y(&|k| matches!(k, Some(LayerKind::Fixed))), Some(0.0));
    // The document's own content comes first, scrolling with it, with the sticky box in the
    // scroller (at 1000 in the document, 900 in the list) stuck to the scroller's top although the
    // scroller is scrolled by 20.
    assert_eq!(kinds.first(), Some(&None));
    let green = split.layers.iter().find_map(|p| {
        let (k, l) = (&p.kind, &p.list);
        l.items.iter().find_map(|i| match i {
            Item::FillRect { rect, color } if *color == Color::hex(0x00ff00) => Some((*k, rect.y)),
            _ => None,
        })
    });
    assert_eq!(green, Some((None, 900.0)));
}

#[test]
fn sticky_boxes_in_groups_pin_the_list() {
    use snowlark_paint::Color;
    let mut t = T::new();
    let r0 = t.root();
    let group = t.doc.block(r0, |s| {
        s.height = SizeValue::px(1000.0);
        s.opacity = 0.5;
    });
    t.doc.block(group, |s| {
        s.position = Position::Sticky;
        s.inset[0] = LengthPercentageAuto::px(0.0);
        s.height = SizeValue::px(10.0);
        s.background_color = Color::BLACK;
    });
    let f = t.run(200.0, 200.0);
    let opts = PaintOptions {
        viewport: Size::new(200.0, 200.0),
        viewport_scroll: Point::new(0.0, 100.0),
        layers: true,
        ..Default::default()
    };
    let mut b = DisplayListBuilder::new();
    paint_with(&f, &t.text, &opts, &mut b);
    // Inside the opacity group, a compositor can't move it: the list must be painted again as the
    // viewport scrolls.
    assert!(b.build().split_layers().pinned);
}

#[test]
fn a_sticky_box_that_fills_its_containing_block_never_moves() {
    // css-position-3 §3.4: a sticky box is shifted only as far as it stays inside its containing
    // block. One that fills it (a sticky bar in a sticky header of its own height) is never
    // shifted against it: it moves with what holds it, and does not pin the list.
    use snowlark_paint::{Color, LayerKind};
    let mut t = T::new();
    let r0 = t.root();
    let header = t.doc.block(r0, |s| {
        s.position = Position::Sticky;
        s.inset[0] = LengthPercentageAuto::px(0.0);
        s.z_index = Some(100);
        s.height = SizeValue::px(64.0);
    });
    let bar = t.doc.block(header, |s| {
        s.position = Position::Sticky;
        s.inset[0] = LengthPercentageAuto::px(0.0);
        s.z_index = Some(100);
        s.height = SizeValue::px(64.0);
        s.background_color = Color::BLACK;
    });
    t.doc.block(r0, |s| s.height = SizeValue::px(2000.0));
    let paint = |t: &mut T| {
        let f = t.run(200.0, 200.0);
        let opts = PaintOptions {
            viewport: Size::new(200.0, 200.0),
            viewport_scroll: Point::new(0.0, 500.0),
            layers: true,
            ..Default::default()
        };
        let mut b = DisplayListBuilder::new();
        paint_with(&f, &t.text, &opts, &mut b);
        b.build().split_layers()
    };
    let split = paint(&mut t);
    assert!(!split.pinned);
    // The bar is painted in the header's layer, stuck at the viewport's top with it.
    let black = split.layers.iter().find_map(|p| {
        p.list.items.iter().find_map(|i| match i {
            Item::FillRect { rect, color } if *color == Color::BLACK => Some((p.kind, rect.y)),
            _ => None,
        })
    });
    assert!(
        matches!(black, Some((Some(LayerKind::Sticky(_)), y)) if y == 0.0),
        "{black:?}"
    );
    // With room to move in the header, the bar's own shift depends on the scroll position in a
    // way the header's layer does not describe: still pinned.
    t.doc.restyle(bar, |s| s.height = SizeValue::px(54.0));
    assert!(paint(&mut t).pinned);
}

#[test]
fn the_viewport_is_the_scrollport_under_a_root_or_body_with_overflow() {
    // CSS Overflow 3 §3.3: the root's overflow (`html { overflow-x: hidden }`), and with a visible
    // root the body's, apply to the viewport: a sticky box under them sticks to the viewport and
    // is a layer.
    use snowlark_paint::{Color, LayerKind};
    for on_body in [false, true] {
        let mut t = T::new();
        let r0 = t.root();
        let body = t.doc.element(r0, "body", |s| {
            s.display = Display::BLOCK;
            if on_body {
                s.overflow_x = Overflow::Hidden;
                s.overflow_y = Overflow::Auto;
            }
        });
        if !on_body {
            t.doc.restyle(r0, |s| {
                s.overflow_x = Overflow::Hidden;
                s.overflow_y = Overflow::Scroll;
            });
        }
        t.doc.block(body, |s| s.height = SizeValue::px(100.0));
        t.doc.block(body, |s| {
            s.position = Position::Sticky;
            s.inset[0] = LengthPercentageAuto::px(0.0);
            s.height = SizeValue::px(10.0);
            s.background_color = Color::BLACK;
        });
        t.doc.block(body, |s| s.height = SizeValue::px(1000.0));
        let f = t.run(200.0, 200.0);
        let opts = PaintOptions {
            viewport: Size::new(200.0, 200.0),
            viewport_scroll: Point::new(0.0, 300.0),
            layers: true,
            ..Default::default()
        };
        let mut b = DisplayListBuilder::new();
        paint_with(&f, &t.text, &opts, &mut b);
        let split = b.build().split_layers();
        assert!(!split.pinned, "overflow on body: {on_body}");
        let sticky = split.layers.iter().find_map(|p| match &p.kind {
            Some(LayerKind::Sticky(_)) => p.list.items.iter().find_map(|i| match i {
                Item::FillRect { rect, .. } => Some(rect.y),
                _ => None,
            }),
            _ => None,
        });
        // Stuck at the viewport's top (not scrolled away with the root or body).
        assert_eq!(sticky, Some(0.0), "overflow on body: {on_body}");
    }
}

#[test]
fn positioned_content_of_a_sticky_scroller_moves_with_it() {
    // A sticky sidebar that scrolls itself (`overflow: auto`): its positioned descendants, clipped
    // by it, are layers with its constraint.
    use snowlark_paint::{Color, LayerKind};
    let mut t = T::new();
    let r0 = t.root();
    let rail = t.doc.block(r0, |s| {
        s.position = Position::Sticky;
        s.inset[0] = LengthPercentageAuto::px(0.0);
        s.height = SizeValue::px(100.0);
        s.overflow_x = Overflow::Auto;
        s.overflow_y = Overflow::Auto;
    });
    t.doc.block(rail, |s| {
        s.position = Position::Relative;
        s.height = SizeValue::px(300.0);
        s.background_color = Color::BLACK;
    });
    t.doc.block(r0, |s| s.height = SizeValue::px(2000.0));
    let f = t.run(200.0, 200.0);
    let opts = PaintOptions {
        viewport: Size::new(200.0, 200.0),
        viewport_scroll: Point::new(0.0, 500.0),
        layers: true,
        ..Default::default()
    };
    let mut b = DisplayListBuilder::new();
    paint_with(&f, &t.text, &opts, &mut b);
    let split = b.build().split_layers();
    assert!(!split.pinned);
    let black = split.layers.iter().find_map(|p| {
        let (k, l) = (&p.kind, &p.list);
        l.items
            .iter()
            .any(|i| matches!(i, Item::FillRect { color, .. } if *color == Color::BLACK))
            .then_some(*k)
    });
    assert!(matches!(black, Some(Some(LayerKind::Sticky(_)))));
}

#[test]
fn an_inline_box_on_two_lines_positions_its_background_once_for_all_its_pieces() {
    // CSS Backgrounds 3 §3.9, CSS Fragmentation 3 §5.4 (`box-decoration-break: slice`): the
    // pieces are positioned as one box side by side. A link's icon at `right`, after its
    // `padding-right`, is drawn once at the end of the last piece, not at the end of each piece.
    use snowlark_paint::{ImageId, Rect};
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| s.width = SizeValue::px(70.0));
    let a = t.doc.span(p, |s| {
        s.padding[1] = LengthPercentage::px(10.0);
        s.background_layers = vec![BackgroundLayer {
            image: Image::raster(ImageId(4), 8.0, 8.0),
            position: (
                LengthPercentage::percent(100.0),
                LengthPercentage::percent(0.0),
            ),
            repeat: (BackgroundRepeat::NoRepeat, BackgroundRepeat::NoRepeat),
            origin: BoxArea::BorderBox,
            ..BackgroundLayer::default()
        }];
    });
    t.doc.text(a, "aaaa bbbb");
    let f = t.run(200.0, 200.0);
    let pieces = t.layout.client_rects(a);
    assert_eq!(pieces.len(), 2, "{pieces:?}");
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, Size::new(200.0, 200.0), &mut b);
    let list = b.build();
    let images: Vec<Rect> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Image { image, dest, .. } if *image == ImageId(4) => Some(*dest),
            _ => None,
        })
        .collect();
    let right = |r: &Rect| r.x + r.width;
    // (the second piece's end, where the icon is; never over the first piece's last letters)
    assert!(
        images
            .iter()
            .any(|d| right(d) == right(&pieces[1]) && d.y == pieces[1].y),
        "{images:?} {pieces:?}"
    );
    assert!(
        !images
            .iter()
            .any(|d| right(d) == right(&pieces[0]) && d.y == pieces[0].y),
        "{images:?} {pieces:?}"
    );
}

#[test]
fn a_mask_group_holds_the_mask_layers_then_the_masked_content() {
    use snowlark_paint::{Color, ImageId};
    let mut t = T::new();
    let r0 = t.root();
    let icon = t.doc.block(r0, |s| {
        s.width = SizeValue::px(20.0);
        s.height = SizeValue::px(20.0);
        s.background_color = Color::BLACK;
        s.mask_layers = vec![MaskLayer {
            layer: BackgroundLayer {
                image: Image::raster(ImageId(3), 10.0, 10.0),
                position: (
                    LengthPercentage::percent(50.0),
                    LengthPercentage::percent(50.0),
                ),
                repeat: (BackgroundRepeat::NoRepeat, BackgroundRepeat::NoRepeat),
                origin: BoxArea::BorderBox,
                ..BackgroundLayer::default()
            },
            luminance: false,
            ..MaskLayer::default()
        }];
    });
    t.doc.block(icon, |s| s.height = SizeValue::px(5.0));
    let f = t.run(200.0, 200.0);
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, Size::new(200.0, 200.0), &mut b);
    let list = b.build();
    let at = |pred: &dyn Fn(&Item) -> bool| list.items.iter().position(pred).unwrap();
    let push = at(&|i| matches!(i, Item::PushMask { .. }));
    let content = at(&|i| matches!(i, Item::MaskContent));
    let pop = at(&|i| matches!(i, Item::PopMask));
    // The mask: the image centered in the border box; then the box's own painting.
    assert_eq!(
        list.items[push],
        Item::PushMask {
            bounds: snowlark_paint::Rect::new(0.0, 0.0, 20.0, 20.0),
            luminance: false
        }
    );
    assert!(list.items[push..content].iter().any(|i| matches!(
        i,
        Item::Image { image, dest, .. } if *image == ImageId(3) && *dest == snowlark_paint::Rect::new(5.0, 5.0, 10.0, 10.0)
    )));
    assert!(list.items[content..pop]
        .iter()
        .any(|i| matches!(i, Item::FillRect { color, .. } if *color == Color::BLACK)));
}

/// CSS Masking 1 §7.7: the bottom layer (the list's last) is painted first; each layer above it
/// follows a mask layer item with its own `mask-composite` and `mask-mode` (the gradient border of
/// `mask: linear-gradient(#000 0 0) content-box, linear-gradient(#000 0 0); mask-composite:
/// exclude`).
#[test]
fn each_mask_layer_above_the_bottom_one_carries_its_composite_operator() {
    use snowlark_paint::{Color, ImageId, MaskComposite};
    let mut t = T::new();
    let r0 = t.root();
    let layer = |id: u32, origin: BoxArea| BackgroundLayer {
        image: Image::raster(ImageId(id), 10.0, 10.0),
        origin,
        clip: origin,
        ..BackgroundLayer::default()
    };
    t.doc.block(r0, |s| {
        s.width = SizeValue::px(20.0);
        s.height = SizeValue::px(20.0);
        s.padding = [LengthPercentage::px(2.0); 4];
        s.background_color = Color::BLACK;
        s.mask_layers = vec![
            MaskLayer {
                layer: layer(1, BoxArea::ContentBox),
                luminance: true,
                composite: MaskComposite::Xor,
            },
            MaskLayer {
                layer: layer(2, BoxArea::BorderBox),
                luminance: false,
                // (the bottom layer's operator composites with nothing)
                composite: MaskComposite::SourceIn,
            },
        ];
    });
    let f = t.run(200.0, 200.0);
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, Size::new(200.0, 200.0), &mut b);
    let list = b.build();
    let at = |pred: &dyn Fn(&Item) -> bool| list.items.iter().position(pred).unwrap();
    let push = at(&|i| matches!(i, Item::PushMask { .. }));
    let content = at(&|i| matches!(i, Item::MaskContent));
    let image = |id: u32| at(&|i| matches!(i, Item::Image { image, .. } if *image == ImageId(id)));
    let layers: Vec<usize> = (push..content)
        .filter(|&i| matches!(list.items[i], Item::MaskLayer { .. }))
        .collect();
    assert!(matches!(
        list.items[push],
        Item::PushMask {
            luminance: false,
            ..
        }
    ));
    assert_eq!(layers.len(), 1, "{:?}", &list.items[push..content]);
    assert_eq!(
        list.items[layers[0]],
        Item::MaskLayer {
            composite: MaskComposite::Xor,
            luminance: true
        }
    );
    assert!(push < image(2) && image(2) < layers[0] && layers[0] < image(1) && image(1) < content);
}

#[test]
fn scroll_bars_show_for_user_scrollable_overflow_only() {
    // CSS Overflow 3 §3: `hidden` has no scroll bar; overflow under a device pixel is not
    // scrollable; an `auto` container with overflow has one, moved by its scroll position.
    use snowlark_paint::Color;
    let bar = Color::rgba(0, 0, 0, 110);
    let mut t = T::new();
    let r0 = t.root();
    let make = |t: &mut T, overflow: Overflow, inner: f32, hidden: bool| {
        let c = t.doc.block(r0, |s| {
            s.height = SizeValue::px(100.0);
            s.overflow_x = overflow;
            s.overflow_y = overflow;
            if hidden {
                s.visibility = Visibility::Hidden;
            }
        });
        t.doc.block(c, |s| s.height = SizeValue::px(inner));
        c
    };
    let auto = make(&mut t, Overflow::Auto, 300.0, false);
    make(&mut t, Overflow::Hidden, 300.0, false);
    make(&mut t, Overflow::Auto, 100.4, false);
    make(&mut t, Overflow::Auto, 300.0, true);
    // A bordered `auto` box whose content fits: its border is not scrollable overflow (CSS
    // Overflow 3 §2.2: the padding box and what descendants contribute), so it has no bar.
    let bordered = t.doc.block(r0, |s| {
        s.height = SizeValue::px(100.0);
        s.overflow_x = Overflow::Auto;
        s.overflow_y = Overflow::Auto;
        s.border_width = [3.0; 4];
        s.border_style = [BorderStyle::Solid; 4];
    });
    t.doc.block(bordered, |s| s.height = SizeValue::px(100.0));
    let f = t.run(200.0, 600.0);
    let g = t.layout.geometry(bordered).unwrap();
    assert_eq!(g.scrollable_overflow, g.padding_box);
    let bars = |scale: f32, offset: f32| {
        let mut scroll = HashMap::new();
        scroll.insert(auto, Point::new(0.0, offset));
        let opts = PaintOptions {
            viewport: Size::new(200.0, 600.0),
            scroll: Some(&scroll),
            device_scale: scale,
            ..Default::default()
        };
        let mut b = DisplayListBuilder::new();
        paint_with(&f, &t.text, &opts, &mut b);
        b.build()
            .items
            .iter()
            .filter_map(|i| match i {
                Item::FillRoundedRect { rect, color, .. } if *color == bar => Some(*rect),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    // One bar, in the `auto` box (0..100): a third of the track, at its top, then at its bottom.
    let at_top = bars(1.0, 0.0);
    assert_eq!(at_top.len(), 1);
    assert_eq!((at_top[0].x, at_top[0].y), (192.0, 2.0));
    assert!((at_top[0].height - 32.0).abs() < 0.01);
    let at_end = bars(1.0, 200.0);
    assert!((at_end[0].bottom() - 98.0).abs() < 0.01);
    // At 3 device pixels per CSS px, 0.4 px of overflow is over a device pixel: it scrolls.
    assert_eq!(bars(3.0, 0.0).len(), 2);
}

#[test]
fn scroll_bars_follow_color_scheme_scrollbar_color_and_scrollbar_width() {
    // CSS Color Adjust 1 §2.4: scroll bars follow the used color scheme; CSS Scrollbars 1:
    // `scrollbar-color` gives the thumb and track, `scrollbar-width: thin` a thinner bar and
    // `none` none at all.
    use snowlark_paint::Color;
    let red = Color::rgb(255, 0, 0);
    let blue = Color::rgb(0, 0, 255);
    let mut t = T::new();
    let r0 = t.root();
    let make = |t: &mut T, f: &dyn Fn(&mut TestStyle)| {
        let c = t.doc.block(r0, |s| {
            s.height = SizeValue::px(100.0);
            s.overflow_x = Overflow::Auto;
            s.overflow_y = Overflow::Auto;
            f(s);
        });
        t.doc.block(c, |s| s.height = SizeValue::px(300.0));
    };
    make(&mut t, &|s| s.color_scheme = ColorScheme::Dark);
    make(&mut t, &|s| {
        s.scrollbar_color = Some(ScrollbarColors {
            thumb: red,
            track: blue,
        })
    });
    make(&mut t, &|s| s.scrollbar_width = ScrollbarWidth::Thin);
    make(&mut t, &|s| s.scrollbar_width = ScrollbarWidth::None);
    let f = t.run(200.0, 600.0);
    let mut b = DisplayListBuilder::new();
    let opts = PaintOptions {
        viewport: Size::new(200.0, 600.0),
        ..Default::default()
    };
    paint_with(&f, &t.text, &opts, &mut b);
    let list = b.build();
    let thumbs: Vec<_> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::FillRoundedRect { rect, color, .. } => Some((*rect, *color)),
            _ => None,
        })
        .collect();
    // Three bars: the fourth box has `scrollbar-width: none`.
    assert_eq!(thumbs.len(), 3);
    // A light thumb on the dark scheme.
    assert_eq!(thumbs[0].1, Color::rgba(255, 255, 255, 120));
    assert_eq!(thumbs[0].0.width, 6.0);
    // `scrollbar-color: red blue`: a red thumb over a blue track along the edge.
    assert_eq!(thumbs[1].1, red);
    assert!(
        list.items.iter().any(|i| matches!(i,
        Item::FillRect { rect, color } if *color == blue && rect.x == 190.0 && rect.width == 10.0))
    );
    // Thin: a thinner thumb in the light scheme's color.
    assert_eq!(thumbs[2].1, Color::rgba(0, 0, 0, 110));
    assert_eq!(thumbs[2].0.width, 4.0);
}

/// Hit testing and caret placement skip the subtrees whose paint bounds miss the point; a
/// transform that moves a box out of its parent's overflow keeps it findable (paint bounds go
/// through transforms), and the display list of a part of the page leaves out what lies outside.
#[test]
fn point_queries_and_partial_paints_skip_only_what_cannot_matter() {
    let mut t = T::new();
    let r0 = t.root();
    let wrapper = t.doc.block(r0, |s| s.height = SizeValue::px(50.0));
    let mover = t.doc.block(wrapper, |s| {
        s.width = SizeValue::px(20.0);
        s.height = SizeValue::px(20.0);
        s.background_color = snowlark_paint::Color::rgb(255, 0, 0);
        s.transform = Some(snowlark_paint::Matrix::translation(0.0, 300.0));
    });
    let far = t.doc.block(r0, |s| {
        s.margin[0] = LengthPercentageAuto::px(1000.0);
        s.height = SizeValue::px(30.0);
        s.background_color = snowlark_paint::Color::rgb(0, 0, 255);
    });
    let near = t.doc.block(r0, |_| {});
    let near_text = t.doc.text(near, "hello");
    let f = t.run(200.0, 2000.0);
    let size = Size::new(200.0, 2000.0);
    assert_eq!(
        hit_test(&f, Point::new(10.0, 310.0), size).map(|h| h.node),
        Some(mover)
    );
    assert_eq!(
        hit_test(&f, Point::new(10.0, 1060.0), size).map(|h| h.node),
        Some(far)
    );
    // The caret: on the line under the point, else the nearest line (the text is below `far`).
    let opts = PaintOptions::viewport(size);
    let line = texts(&f, near_text)[0];
    let on = Point::new(line.x + 1.0, line.y + line.height / 2.0);
    assert_eq!(
        caret_position(&f, on, &opts, &|_| true).map(|c| c.0),
        Some(near_text)
    );
    assert_eq!(
        caret_position(&f, Point::new(5.0, 10.0), &opts, &|_| true).map(|c| c.0),
        Some(near_text)
    );
    // A part around y 300: the moved box is in it, the far one is not.
    let fills = |cull: Option<Rect>| {
        let mut b = DisplayListBuilder::new();
        paint_part(&f, &t.text, &opts, cull, &mut b);
        b.build()
            .items
            .iter()
            .filter_map(|i| match i {
                Item::FillRect { color, .. } => Some(*color),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let red = snowlark_paint::Color::rgb(255, 0, 0);
    let blue = snowlark_paint::Color::rgb(0, 0, 255);
    let part = fills(Some(Rect::new(0.0, 250.0, 200.0, 200.0)));
    assert!(part.contains(&red) && !part.contains(&blue), "{part:?}");
    let all = fills(None);
    assert!(all.contains(&red) && all.contains(&blue), "{all:?}");
}

/// Every node's fragments as a full walk of the tree finds them (what the lookup must return).
fn walked(f: &Fragment<Arc<TestStyle>>, node: snowlark_dom::NodeId) -> Vec<snowlark_paint::Rect> {
    let mut out = Vec::new();
    f.walk(&mut |fr, at| {
        let backdrop = fr
            .as_box()
            .is_some_and(|b| b.pseudo == Some(PseudoElement::Backdrop));
        if fr.node() == Some(node) && !backdrop {
            out.push(r(at.x, at.y, fr.size.width, fr.size.height));
        }
    });
    out
}

/// A node's fragments are found along its path (and through an index once reads
/// repeat), with the same answer as a walk of the whole tree: boxes placed by containing blocks
/// other than their parents (absolute, fixed), floats, inline boxes split over lines, text, and
/// nodes without boxes.
#[test]
fn fragment_lookup_finds_what_a_full_walk_finds() {
    let mut t = T::new();
    let r0 = t.root();
    let rel = t.doc.block(r0, |s| {
        s.position = Position::Relative;
        s.inset[0] = LengthPercentageAuto::px(7.0);
        s.padding_px(4.0);
    });
    let mid = t.doc.block(rel, |s| s.margin_px(3.0));
    let abs = t.doc.block(mid, |s| {
        s.position = Position::Absolute;
        s.inset[0] = LengthPercentageAuto::px(11.0);
        s.inset[3] = LengthPercentageAuto::px(13.0);
        s.width = SizeValue::px(20.0);
        s.height = SizeValue::px(20.0);
    });
    let abs_child = t.doc.block(abs, |s| s.height = SizeValue::px(5.0));
    let fixed = t.doc.block(mid, |s| {
        s.position = Position::Fixed;
        s.inset[2] = LengthPercentageAuto::px(0.0);
        s.width = SizeValue::px(10.0);
        s.height = SizeValue::px(10.0);
    });
    let float = t.doc.block(mid, |s| {
        s.float = Float::Left;
        s.width = SizeValue::px(15.0);
        s.height = SizeValue::px(15.0);
    });
    let p = t.doc.block(mid, |s| s.width = SizeValue::px(60.0));
    let span = t.doc.span(p, |_| {});
    let text = t.doc.text(span, "aaa bbb ccc ddd");
    let hidden = t.doc.block(mid, |s| s.display = Display::None);
    let hidden_child = t.doc.block(hidden, |_| {});
    let mut nodes = vec![
        r0,
        rel,
        mid,
        abs,
        abs_child,
        fixed,
        float,
        p,
        span,
        text,
        hidden,
        hidden_child,
    ];
    for i in 0..40 {
        let b = t
            .doc
            .block(mid, |s| s.height = SizeValue::px(1.0 + i as f32));
        nodes.push(b);
    }
    let f = t.run(200.0, 200.0);
    let check = |t: &T| {
        for &n in &nodes {
            assert_eq!(t.layout.client_rects(n), walked(&f, n), "node {n:?}");
            let first = walked(&f, n).first().copied();
            if n != text {
                assert_eq!(t.layout.geometry(n).map(|g| g.border_box), first);
            }
        }
    };
    // The first reads search along each node's path; repeated reads build the index.
    check(&t);
    for _ in 0..10 {
        check(&t);
    }
    assert!(t.layout.client_rects(span).len() > 1);
    assert!(t.layout.client_rects(hidden_child).is_empty());
    assert_eq!(t.layout.geometry(abs).unwrap().border_box.y, 7.0 + 11.0);
}

/// Every node's border boxes through the transforms of its fragment and those it is in, by a walk
/// of the whole tree (what the transformed lookup must return).
fn walked_transformed(
    f: &Fragment<Arc<TestStyle>>,
    node: snowlark_dom::NodeId,
) -> Vec<snowlark_paint::Rect> {
    use snowlark_paint::Matrix;
    fn go(
        f: &Fragment<Arc<TestStyle>>,
        m: Matrix,
        any: bool,
        node: snowlark_dom::NodeId,
        out: &mut Vec<snowlark_paint::Rect>,
    ) {
        if f.node() == Some(node) {
            let own = r(0.0, 0.0, f.size.width, f.size.height);
            out.push(if any {
                m.apply_rect(&own)
            } else {
                own.translate(m.dx, m.dy)
            });
        }
        for c in &f.children {
            let offset = Matrix::translation(c.offset.x, c.offset.y);
            match c
                .fragment
                .as_box()
                .and_then(|b| b.used_transform(c.fragment.size))
            {
                Some(t) => go(&c.fragment, t.then(&offset).then(&m), true, node, out),
                None => go(&c.fragment, offset.then(&m), any, node, out),
            }
        }
    }
    let mut out = Vec::new();
    go(f, Matrix::IDENTITY, false, node, &mut out);
    out
}

/// Under a transformed box, `getClientRects()` finds a node's fragments along its path
/// with the transforms composed on the way (Blink's `LayoutObject::MapLocalToAncestor`), not by a
/// walk of the whole tree per read (which made each IntersectionObserver target cost the tree, every
/// frame); the answer is the full walk's, through the search and through the index.
#[test]
fn transformed_client_rects_cost_the_path_not_the_tree() {
    let mut t = T::new();
    let r0 = t.root();
    let moved = t.doc.block(r0, |s| {
        s.transform = Some(snowlark_paint::Matrix::translation(10.0, 20.0));
        s.padding_px(3.0);
    });
    let mut nodes = vec![moved];
    let mut sections = Vec::new();
    for i in 0..30 {
        let section = t.doc.block(moved, |s| {
            if i == 2 {
                s.transform = Some(snowlark_paint::Matrix::rotation(0.3));
            }
        });
        sections.push(section);
        nodes.push(section);
        for j in 0..10 {
            let b = t
                .doc
                .block(section, |s| s.height = SizeValue::px(1.0 + j as f32));
            nodes.push(b);
        }
    }
    let p = t.doc.block(sections[2], |s| s.width = SizeValue::px(60.0));
    let span = t.doc.span(p, |_| {});
    let text = t.doc.text(span, "aaa bbb ccc ddd");
    nodes.extend([p, span, text]);
    let f = t.run(200.0, 200.0);
    // One read of a box deep in the tree visits its path and the children along it (about 50
    // fragments here), not the tree (over 330).
    let _ = t.layout.client_rects(p);
    let cost = t.layout.fragments_searched();
    assert!(cost < 100, "one read visited {cost} fragments");
    for _ in 0..12 {
        for &n in &nodes {
            assert_eq!(
                t.layout.client_rects(n),
                walked_transformed(&f, n),
                "node {n:?}"
            );
        }
    }
    assert_eq!(t.layout.client_rects(moved)[0].x, 10.0);
    assert!(t.layout.client_rects(span).len() > 1);
}

/// An inline split around a block (CSS 2 §9.2.1.1; custom elements are inline by default and
/// often hold blocks) finds its pieces and that block along their paths, not by a walk of the
/// whole tree per read: an IntersectionObserver target that is one is read at every rendering
/// update. The pieces and the block come in tree order, transformed ancestors applied.
#[test]
fn split_inline_client_rects_cost_the_path_not_the_tree() {
    let mut t = T::new();
    let r0 = t.root();
    let moved = t.doc.block(r0, |s| {
        s.transform = Some(snowlark_paint::Matrix::translation(10.0, 20.0));
    });
    let mut sections = Vec::new();
    for _ in 0..30 {
        let section = t.doc.block(moved, |_| {});
        sections.push(section);
        for j in 0..10 {
            t.doc
                .block(section, |s| s.height = SizeValue::px(1.0 + j as f32));
        }
    }
    let p = t.doc.block(sections[20], |s| s.width = SizeValue::px(60.0));
    let a = t.doc.span(p, |_| {});
    t.doc.text(a, "before");
    let d = t.doc.block(a, |s| s.height = SizeValue::px(15.0));
    t.doc.text(a, "after");
    t.run(200.0, 200.0);
    for transforms in [true, false] {
        let before = t.layout.fragments_searched();
        let rects = if transforms {
            t.layout.client_rects(a)
        } else {
            t.layout.layout_rects(a)
        };
        let cost = t.layout.fragments_searched() - before;
        assert!(cost < 100, "one read visited {cost} fragments");
        // (the piece before, the block, the piece after)
        assert_eq!(rects.len(), 3, "{rects:?}");
        let block = if transforms {
            t.layout.client_rects(d)
        } else {
            t.layout.layout_rects(d)
        };
        assert_eq!(rects[1], block[0], "transforms {transforms}");
        assert!(
            rects[0].y < rects[1].y && rects[1].y < rects[2].y,
            "{rects:?}"
        );
        assert_eq!(rects[1].x, if transforms { 10.0 } else { 0.0 });
    }
}

/// A page of many split inlines (a comment thread of custom elements, each observed): reading
/// each of them costs, in all, about the tree a few times (the reads' paths until the index is
/// built, then the index), not the tree per read, nor every split per read; the index's answers
/// are the search's.
#[test]
fn many_split_inlines_cost_the_tree_once() {
    let mut t = T::new();
    let r0 = t.root();
    let main = t.doc.block(r0, |s| {
        s.transform = Some(snowlark_paint::Matrix::translation(5.0, 0.0));
    });
    let mut inlines = Vec::new();
    for _ in 0..300 {
        let comment = t.doc.span(main, |_| {});
        t.doc.text(comment, "by someone");
        let body = t.doc.block(comment, |s| s.height = SizeValue::px(12.0));
        t.doc.text(body, "a comment");
        let actions = t.doc.span(comment, |_| {});
        t.doc.block(actions, |s| s.height = SizeValue::px(4.0));
        inlines.extend([comment, actions]);
    }
    t.run(400.0, 400.0);
    let read = |t: &T, transforms: bool| -> Vec<Vec<Rect>> {
        inlines
            .iter()
            .map(|&n| {
                if transforms {
                    t.layout.client_rects(n)
                } else {
                    t.layout.layout_rects(n)
                }
            })
            .collect()
    };
    for transforms in [true, false] {
        let before = t.layout.fragments_searched();
        let first = read(&t, transforms);
        let again = read(&t, transforms);
        let cost = t.layout.fragments_searched() - before;
        // (about 3000 fragments: building the index and the searches before it)
        assert!(
            cost < 20_000,
            "reading 600 split inlines twice visited {cost} fragments"
        );
        assert_eq!(first, again, "the index answers as the search did");
        assert!(first.iter().all(|r| r.len() >= 2), "{:?}", first[0]);
        assert_eq!(first[0][0].x, if transforms { 5.0 } else { 0.0 });
    }
}

/// A layout with nothing changed since the last one in the same viewport keeps its fragment
/// tree (so a script's forced layout per read costs nothing); a change or another viewport lays
/// out again.
#[test]
fn layout_without_changes_keeps_the_fragment_tree() {
    let mut t = T::new();
    let r0 = t.root();
    let a = t.doc.block(r0, |s| s.height = SizeValue::px(10.0));
    let first = t.run(200.0, 200.0);
    let again = t.relayout();
    assert!(Arc::ptr_eq(&first, &again));
    assert_eq!(t.layout.stats().laid_out, 0);
    t.doc.restyle(a, |s| s.height = SizeValue::px(30.0));
    t.layout.update(
        &t.doc.dom,
        t.doc.document,
        &t.doc.styles,
        &TreeChanges {
            restyled: vec![ElementChange::restyled(a, BoxDamage::Relayout, false)],
            ..Default::default()
        },
    );
    let changed = t.relayout();
    assert!(!Arc::ptr_eq(&first, &changed));
    assert_eq!(t.layout.geometry(a).unwrap().border_box.height, 30.0);
    let other = t.layout.layout(&t.text, Size::new(300.0, 200.0));
    assert!(!Arc::ptr_eq(&changed, &other));
    assert_eq!(t.layout.geometry(a).unwrap().border_box.width, 300.0);
}

/// The fill colors of a display list, with `(` and `)` for clip groups and `[` `]` for opacity
/// groups around them.
fn fills_and_groups(list: &snowlark_paint::DisplayList) -> String {
    list.items
        .iter()
        .filter_map(|i| match i {
            Item::FillRect { color, .. } => {
                Some(format!("{:02x}{:02x}{:02x}", color.r, color.g, color.b))
            }
            Item::PushClipRect { .. } => Some("(".into()),
            Item::PopClip => Some(")".into()),
            Item::PushOpacity { .. } => Some("[".into()),
            Item::PopOpacity => Some("]".into()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn positioned_boxes_paint_in_their_nearest_ancestor_stacking_context() {
    use snowlark_paint::Color;
    // CSS 2 Appendix E: a fixed box inside a `z-index: 0` wrapper is painted by the wrapper's
    // stacking context (below a later `z-index: 0` sibling), although its containing block is the
    // initial containing block; and a fixed box in the root's context is painted before later
    // positioned boxes of that context.
    let mut t = T::new();
    let r0 = t.root();
    let wrapper = t.doc.block(r0, |s| {
        s.position = Position::Relative;
        s.z_index = Some(0);
        s.height = SizeValue::px(10.0);
    });
    let overlay = t.doc.block(wrapper, |s| {
        s.position = Position::Fixed;
        s.inset = [LengthPercentageAuto::px(0.0); 4];
        s.background_color = Color::hex(0x111111);
    });
    t.doc.block(r0, |s| {
        s.position = Position::Fixed;
        s.inset = [LengthPercentageAuto::px(0.0); 4];
        s.background_color = Color::hex(0x222222);
    });
    let dialog = t.doc.block(r0, |s| {
        s.position = Position::Relative;
        s.z_index = Some(0);
        s.width = SizeValue::px(50.0);
        s.height = SizeValue::px(50.0);
        s.background_color = Color::hex(0xffffff);
    });
    let f = t.run(200.0, 200.0);
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, t.viewport, &mut b);
    assert_eq!(fills_and_groups(&b.build()), "111111 222222 ffffff");
    // Hit testing follows: the dialog is on top, the overlay outside the wrapper's own box is hit.
    assert_eq!(
        hit_test(&f, Point::new(20.0, 40.0), t.viewport).map(|h| h.node),
        Some(dialog)
    );
    let _ = overlay;
}

#[test]
fn adopted_positioned_boxes_keep_their_containing_block_clip_and_their_owner_effects() {
    use snowlark_paint::{Color, Rect};
    // A fixed box inside a small `overflow: hidden` box and a `z-index` wrapper: the wrapper's
    // stacking context paints it, but the overflow clip (whose box is not in its containing block
    // chain) does not clip it (CSS Overflow 3 §3).
    let mut t = T::new();
    let r0 = t.root();
    let clipper = t.doc.block(r0, |s| {
        s.overflow_x = Overflow::Hidden;
        s.overflow_y = Overflow::Hidden;
        s.height = SizeValue::px(10.0);
        s.width = SizeValue::px(10.0);
    });
    let wrapper = t.doc.block(clipper, |s| {
        s.position = Position::Relative;
        s.z_index = Some(1);
        s.height = SizeValue::px(20.0);
        s.background_color = Color::hex(0x0000ff);
    });
    let modal = t.doc.block(wrapper, |s| {
        s.position = Position::Fixed;
        s.inset = [LengthPercentageAuto::px(0.0); 4];
        s.background_color = Color::hex(0xff0000);
    });
    // An absolutely positioned box whose containing block is the initial containing block, in a
    // translucent box: it is painted inside the opacity group.
    let faded = t.doc.block(r0, |s| {
        s.opacity = 0.5;
        s.height = SizeValue::px(10.0);
    });
    t.doc.block(faded, |s| {
        s.position = Position::Absolute;
        s.inset[0] = LengthPercentageAuto::px(150.0);
        s.width = SizeValue::px(10.0);
        s.height = SizeValue::px(10.0);
        s.background_color = Color::hex(0x00ff00);
    });
    let f = t.run(200.0, 200.0);
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, t.viewport, &mut b);
    // (the empty groups are the clipper's own phases; the wrapper's clip is around its own
    // content only, the modal in its zero list is outside it)
    assert_eq!(
        fills_and_groups(&b.build()),
        "( ) ( ) ( ) [ 00ff00 ] ( 0000ff ) ( ) ff0000 ( )"
    );
    // Found by hit testing far outside the clipper and the wrapper.
    assert_eq!(
        hit_test(&f, Point::new(100.0, 100.0), t.viewport).map(|h| h.node),
        Some(modal)
    );
    // A partial paint away from the wrapper still paints the modal.
    let opts = PaintOptions::viewport(t.viewport);
    let mut b = DisplayListBuilder::new();
    paint_part(
        &f,
        &t.text,
        &opts,
        Some(Rect::new(100.0, 100.0, 50.0, 50.0)),
        &mut b,
    );
    assert!(fills_and_groups(&b.build()).contains("ff0000"));
}

#[test]
fn scroll_bars_paint_after_the_scroller_descendants_and_under_later_positioned_boxes() {
    use snowlark_paint::Color;
    let mut t = T::new();
    let r0 = t.root();
    let scroller = t.doc.block(r0, |s| {
        s.height = SizeValue::px(50.0);
        s.overflow_x = Overflow::Auto;
        s.overflow_y = Overflow::Auto;
    });
    // A positioned descendant (painted in the root's lists, after the scroller's flow content).
    t.doc.block(scroller, |s| {
        s.position = Position::Relative;
        s.height = SizeValue::px(300.0);
        s.background_color = Color::hex(0x00ff00);
    });
    // A later positioned box with a positive z-index over the scroller.
    t.doc.block(r0, |s| {
        s.position = Position::Relative;
        s.z_index = Some(1);
        s.margin[0] = LengthPercentageAuto::px(-50.0);
        s.height = SizeValue::px(50.0);
        s.background_color = Color::hex(0xff0000);
    });
    let f = t.run(200.0, 200.0);
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, t.viewport, &mut b);
    let order: Vec<&str> = b
        .build()
        .items
        .iter()
        .filter_map(|i| match i {
            Item::FillRect { color, .. } if *color == Color::hex(0x00ff00) => Some("content"),
            Item::FillRect { color, .. } if *color == Color::hex(0xff0000) => Some("cover"),
            Item::FillRoundedRect { .. } => Some("thumb"),
            _ => None,
        })
        .collect();
    assert_eq!(order, ["content", "thumb", "cover"]);
}

#[test]
fn overflow_clips_follow_the_padding_box_curve() {
    // CSS Backgrounds 3 §5.3: a box with rounded corners whose overflow clips clips its content
    // (in flow and positioned) to its padding box curve: the border radii less the border widths.
    use snowlark_paint::{Color, Radii, Rect};
    let mut t = T::new();
    let r0 = t.root();
    let pill = t.doc.block(r0, |s| {
        s.position = Position::Relative;
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(40.0);
        s.overflow_x = Overflow::Hidden;
        s.overflow_y = Overflow::Hidden;
        s.border_width = [2.0; 4];
        s.border_style = [BorderStyle::Solid; 4];
        // (radii larger than half the height: CSS Backgrounds 3 §5.5 scales them down to 22)
        s.border_radius = [(LengthPercentage::px(50.0), LengthPercentage::px(50.0)); 4];
    });
    t.doc.block(pill, |s| {
        s.height = SizeValue::px(40.0);
        s.background_color = Color::hex(0x00ff00);
    });
    t.doc.block(pill, |s| {
        s.position = Position::Absolute;
        s.inset[0] = LengthPercentageAuto::px(-20.0);
        s.width = SizeValue::px(100.0);
        s.height = SizeValue::px(40.0);
        s.background_color = Color::hex(0xff0000);
    });
    let f = t.run(200.0, 200.0);
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, t.viewport, &mut b);
    let list = b.build();
    let rounded: Vec<(Rect, Radii)> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::PushClipRoundedRect { rect, radii } => Some((*rect, *radii)),
            _ => None,
        })
        .collect();
    // The flow content's clip and the positioned child's: the padding box, radius 22 - 2 = 20.
    let expected = (Rect::new(2.0, 2.0, 100.0, 40.0), Radii::uniform(20.0));
    assert!(rounded.len() >= 2, "{rounded:?}");
    assert!(rounded.iter().all(|r| *r == expected), "{rounded:?}");
}

#[test]
fn align_content_aligns_a_block_containers_content() {
    // CSS Box Alignment 3 §5.4: in a box that establishes an independent formatting context, the
    // content moves as a whole in the block axis (buttons: HTML's `align-content: center`); not
    // when it does not fit, and not under `normal`.
    let mut t = T::new();
    let r0 = t.root();
    let make = |t: &mut T, align: Align, inner: f32| {
        let b = t.doc.block(r0, |s| {
            s.display = Display::FLOW_ROOT;
            s.height = SizeValue::px(40.0);
            s.padding = [LengthPercentage::px(2.0); 4];
            s.align_content = align;
        });
        let c = t.doc.block(b, |s| s.height = SizeValue::px(inner));
        (b, c)
    };
    let (center, center_child) = make(&mut t, Align::Center, 20.0);
    let (end, end_child) = make(&mut t, Align::End, 20.0);
    let (normal, normal_child) = make(&mut t, Align::Normal, 20.0);
    let (over, over_child) = make(&mut t, Align::Center, 60.0);
    let f = t.run(200.0, 400.0);
    let dy = |b, c| rect(&f, c).y - rect(&f, b).y;
    assert_eq!(dy(center, center_child), 12.0);
    assert_eq!(dy(end, end_child), 22.0);
    assert_eq!(dy(normal, normal_child), 2.0);
    assert_eq!(dy(over, over_child), 2.0);
}

#[test]
fn empty_inline_boxes_have_no_outline() {
    // An inline box with no content has no width, so no outline rectangle (CSS UI 4 §5.1 as
    // Chrome draws it); one with text has its outline.
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |_| {});
    t.doc.text(p, "a ");
    t.doc.span(p, |s| {
        s.outline_width = 2.0;
        s.outline_style = BorderStyle::Solid;
    });
    let full = t.doc.span(p, |s| {
        s.outline_width = 3.0;
        s.outline_style = BorderStyle::Solid;
    });
    t.doc.text(full, "b");
    let f = t.run(200.0, 200.0);
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, Size::new(200.0, 200.0), &mut b);
    let outlines: Vec<f32> = b
        .build()
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Border(b) if b.styles[0] == BorderStyle::Solid => Some(b.widths.top),
            _ => None,
        })
        .collect();
    assert_eq!(outlines, vec![3.0]);
}

#[test]
fn scroll_offsets_paint_in_whole_device_pixels() {
    // A scroll position of 10.4 CSS px paints as 10 device px at scale 1 and as 10.666 CSS px
    // (16 device px) at 1.5: what is scrolled stays on pixel edges.
    let mut t = T::new();
    let r0 = t.root();
    let d = t.doc.block(r0, |s| {
        s.height = SizeValue::px(20.0);
        s.background_color = Color::BLACK;
    });
    let _ = d;
    t.doc.block(r0, |s| s.height = SizeValue::px(1000.0));
    let f = t.run(200.0, 200.0);
    for (scale, expected) in [(1.0f32, -10.0f32), (1.5, -16.0 / 1.5)] {
        let opts = PaintOptions {
            viewport: Size::new(200.0, 200.0),
            viewport_scroll: Point::new(0.0, 10.4),
            scroll: None,
            embeds: None,
            selection: None,
            find: None,
            layers: false,
            device_scale: scale,
            composited: None,
            animations: None,
        };
        let mut b = DisplayListBuilder::new();
        paint_with(&f, &t.text, &opts, &mut b);
        let y = b
            .build()
            .items
            .iter()
            .find_map(|i| match i {
                Item::FillRoundedRect { rect, color, .. } if *color == Color::BLACK => Some(rect.y),
                Item::FillRect { rect, color } if *color == Color::BLACK => Some(rect.y),
                _ => None,
            })
            .unwrap();
        assert!((y - expected).abs() < 1e-4, "scale {scale}: {y}");
    }
}

#[test]
fn selected_text_is_drawn_in_its_highlight_colors() {
    // CSS Pseudo-Elements 4 §3.6: the highlight's background is behind the selected part of the
    // text, and that part is drawn in the highlight's `color`. A partly selected fragment is drawn
    // once per part, clipped at the selection's edges; the text keeps its color where the
    // highlight gives none (§3.6.3), and a highlight with only a color has no background (§3.3.1).
    let mut t = T::new();
    let r0 = t.root();
    let p = t.doc.block(r0, |s| s.color = Color::WHITE);
    let text = t.doc.text(p, "abcdef");
    let f = t.run(200.0, 200.0);
    let (yellow, red) = (Color::rgb(255, 255, 0), Color::rgb(255, 0, 0));
    // What is painted: the highlight's rectangles, and each text run's color and clip.
    let painted = |start: u32, end: u32, highlight: Option<(Color, Option<Color>)>| {
        let selection = TextSelection {
            ranges: HashMap::from([(
                text,
                SelectedText {
                    start,
                    end,
                    highlight,
                    continues: false,
                    shadows: 0,
                },
            )]),
            background: Color::rgb(0, 0, 255),
            ..Default::default()
        };
        let opts = PaintOptions {
            selection: Some(&selection),
            ..PaintOptions::viewport(Size::new(200.0, 200.0))
        };
        let mut b = DisplayListBuilder::new();
        paint_with(&f, &t.text, &opts, &mut b);
        let (mut fills, mut runs, mut clips) = (Vec::new(), Vec::new(), Vec::new());
        for i in &b.build().items {
            match i {
                Item::FillRect { rect, color } => fills.push((rect.x, rect.right(), *color)),
                Item::PushClipRect { rect } => clips.push((rect.x, rect.right())),
                Item::PopClip => {
                    clips.pop();
                }
                Item::Text(run) => runs.push((run.color, clips.last().copied())),
                _ => {}
            }
        }
        (fills, runs)
    };
    let x = rect(&f, p).x;
    // Partly selected (Ahem at 10px: `cd` is 20px to 40px): three parts, the middle one recolored.
    let (fills, runs) = painted(2, 4, Some((yellow, Some(red))));
    assert_eq!(fills, vec![(x + 20.0, x + 40.0, yellow)]);
    let colors: Vec<Color> = runs.iter().map(|r| r.0).collect();
    assert_eq!(colors, vec![Color::WHITE, red, Color::WHITE]);
    let clips: Vec<(f32, f32)> = runs.iter().map(|r| r.1.unwrap()).collect();
    assert!(clips[0].0 < x && clips[0].1 == x + 20.0, "{clips:?}");
    assert_eq!(clips[1], (x + 20.0, x + 40.0));
    assert!(clips[2].0 == x + 40.0 && clips[2].1 > x + 60.0, "{clips:?}");
    // Selected from its start: no part before, and the selected part is open at the start.
    let (_, runs) = painted(0, 4, Some((yellow, Some(red))));
    assert_eq!(runs.len(), 2);
    let first = runs[0].1.unwrap();
    assert!(runs[0].0 == red && first.0 < x && first.1 == x + 40.0);
    // All of it: one run in the highlight's color, not clipped.
    let (_, runs) = painted(0, 6, Some((yellow, Some(red))));
    assert_eq!(runs, vec![(red, None)]);
    // No `color` from the highlight: the text is drawn as it is.
    let (fills, runs) = painted(2, 4, Some((yellow, None)));
    assert_eq!(fills.len(), 1);
    assert_eq!(runs, vec![(Color::WHITE, None)]);
    // Only a color: no background.
    let (fills, runs) = painted(2, 4, Some((Color::TRANSPARENT, Some(red))));
    assert!(fills.is_empty(), "{fills:?}");
    assert_eq!(runs.len(), 3);
    // No `::selection` style: the user agent's background, the text unchanged.
    let (fills, runs) = painted(2, 4, None);
    assert_eq!(fills, vec![(x + 20.0, x + 40.0, Color::rgb(0, 0, 255))]);
    assert_eq!(runs, vec![(Color::WHITE, None)]);
}

/// The destination rectangles of the images a painted list draws.
fn image_dests(f: &F, t: &T, opts: &PaintOptions) -> Vec<snowlark_paint::Rect> {
    let mut b = DisplayListBuilder::new();
    paint_with(f, &t.text, opts, &mut b);
    b.build()
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Image { dest, .. } => Some(*dest),
            _ => None,
        })
        .collect()
}

fn one_image(attachment: BackgroundAttachment, y: f32) -> BackgroundLayer {
    BackgroundLayer {
        image: Image::raster(snowlark_paint::ImageId(1), 10.0, 10.0),
        position: (LengthPercentage::ZERO, LengthPercentage::percent(y)),
        repeat: (BackgroundRepeat::NoRepeat, BackgroundRepeat::NoRepeat),
        attachment,
        ..BackgroundLayer::default()
    }
}

#[test]
fn fixed_backgrounds_are_positioned_in_the_viewport() {
    // CSS Backgrounds 3 §3.5: `fixed` positions the image in the viewport, which the page
    // scrolls under; `scroll` keeps it on the box.
    let mut t = T::new();
    let r0 = t.root();
    t.doc.block(r0, |s| {
        s.height = SizeValue::px(400.0);
        s.background_layers = vec![one_image(BackgroundAttachment::Fixed, 100.0)];
    });
    t.doc.block(r0, |s| {
        s.height = SizeValue::px(400.0);
        s.background_layers = vec![one_image(BackgroundAttachment::Scroll, 0.0)];
    });
    let f = t.run(200.0, 200.0);
    let mut opts = PaintOptions::viewport(Size::new(200.0, 200.0));
    opts.viewport_scroll = Point::new(0.0, 250.0);
    let dests = image_dests(&f, &t, &opts);
    // fixed: at the viewport's bottom (100% of 200 - 10); scroll: at its box's top, 400 - 250
    assert_eq!(dests.len(), 2);
    assert_eq!(dests[0].y, 190.0);
    assert_eq!(dests[1].y, 150.0);
}

#[test]
fn a_gradient_repeated_along_an_axis_it_does_not_change_on_is_one_item() {
    // CSS Backgrounds 3 §3.4: `repeat-x` tiles a 1px-wide vertical gradient across the box. Its
    // tiles meet edge to edge and each is the same at every x, so together they are the gradient
    // over the whole row: one item, not one a pixel. A horizontal gradient differs along x: its
    // 200 tiles are one pattern fill of the gradient in a 1px tile.
    let gradient = |angle: f32| {
        Image::LinearGradient(Arc::new(LinearGradient {
            angle,
            corner: None,
            stops: [Color::rgb(255, 0, 0), Color::rgb(0, 0, 255)]
                .into_iter()
                .map(|color| ColorStop {
                    color,
                    position: None,
                    hint: false,
                })
                .collect(),
            ramps: Vec::new(),
            repeating: false,
        }))
    };
    let layer = |angle: f32| BackgroundLayer {
        image: gradient(angle),
        size: BackgroundSize::Explicit(
            Some(LengthPercentage::px(1.0)),
            Some(LengthPercentage::percent(100.0)),
        ),
        repeat: (BackgroundRepeat::Repeat, BackgroundRepeat::NoRepeat),
        ..BackgroundLayer::default()
    };
    let mut t = T::new();
    let r0 = t.root();
    t.doc.block(r0, |s| {
        s.height = SizeValue::px(50.0);
        s.background_layers = vec![layer(180.0)];
    });
    t.doc.block(r0, |s| {
        s.height = SizeValue::px(50.0);
        s.background_layers = vec![layer(90.0)];
    });
    let f = t.run(200.0, 200.0);
    let mut b = DisplayListBuilder::new();
    paint_with(
        &f,
        &t.text,
        &PaintOptions::viewport(Size::new(200.0, 200.0)),
        &mut b,
    );
    let list = b.build();
    let gradients = |items: &[Item]| -> Vec<snowlark_paint::Rect> {
        items
            .iter()
            .filter_map(|i| match i {
                Item::LinearGradient { rect, .. } => Some(*rect),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        gradients(&list.items),
        [snowlark_paint::Rect::new(0.0, 0.0, 200.0, 50.0)]
    );
    let fills: Vec<_> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::FillPattern { rect, pattern, .. } => Some((*rect, *pattern)),
            _ => None,
        })
        .collect();
    assert_eq!(fills.len(), 1, "{fills:?}");
    assert_eq!(
        fills[0].0,
        snowlark_paint::Rect::new(0.0, 50.0, 200.0, 50.0)
    );
    let p = &list.patterns[fills[0].1 as usize];
    assert_eq!(p.tile, Size::new(1.0, 50.0));
    assert_eq!(
        gradients(&p.list.items),
        [snowlark_paint::Rect::new(0.0, 0.0, 1.0, 50.0)]
    );
}

#[test]
fn a_small_image_repeated_over_a_large_box_is_one_pattern_fill() {
    // CSS Backgrounds 3 §3.4: a 4px image repeated over a 400×300 box is 7500 tiles edge to
    // edge: one pattern fill of the image in a 4px tile over the box, not an item per tile.
    let mut t = T::new();
    let r0 = t.root();
    t.doc.block(r0, |s| {
        s.width = SizeValue::px(400.0);
        s.height = SizeValue::px(300.0);
        s.background_layers = vec![BackgroundLayer {
            image: Image::raster(snowlark_paint::ImageId(7), 4.0, 4.0),
            ..BackgroundLayer::default()
        }];
    });
    let f = t.run(800.0, 600.0);
    let mut b = DisplayListBuilder::new();
    paint_with(
        &f,
        &t.text,
        &PaintOptions::viewport(Size::new(800.0, 600.0)),
        &mut b,
    );
    let list = b.build();
    assert!(!list.items.iter().any(|i| matches!(i, Item::Image { .. })));
    let fills: Vec<_> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::FillPattern { rect, pattern, .. } => Some((*rect, *pattern)),
            _ => None,
        })
        .collect();
    assert_eq!(fills.len(), 1);
    assert_eq!(
        fills[0].0,
        snowlark_paint::Rect::new(0.0, 0.0, 400.0, 300.0)
    );
    let p = &list.patterns[fills[0].1 as usize];
    assert_eq!(p.tile, Size::new(4.0, 4.0));
    assert_eq!(
        p.list
            .items
            .iter()
            .filter(|i| matches!(i, Item::Image { .. }))
            .count(),
        1
    );
}

#[test]
fn an_empty_positioning_area_still_places_an_image() {
    // CSS Backgrounds 3 §3.7: the area only positions the image; §3.10's clip is what shows it.
    let mut t = T::new();
    let r0 = t.root();
    t.doc.block(r0, |s| {
        s.height = SizeValue::px(0.0);
        s.padding_px(20.0);
        s.background_layers = vec![BackgroundLayer {
            origin: crate::style::BoxArea::ContentBox,
            clip: crate::style::BoxArea::BorderBox,
            ..one_image(BackgroundAttachment::Scroll, 0.0)
        }];
    });
    let f = t.run(200.0, 200.0);
    let opts = PaintOptions::viewport(Size::new(200.0, 200.0));
    let dests = image_dests(&f, &t, &opts);
    assert_eq!(dests.len(), 1);
    assert_eq!((dests[0].x, dests[0].y), (20.0, 20.0));
}

#[test]
fn local_backgrounds_scroll_with_the_content() {
    // `local` positions the image in the scroll container's scrollable area (content and
    // padding) and moves it with the scroll position; `scroll` stays on the padding box.
    for (attachment, expected) in [
        (BackgroundAttachment::Local, 300.0 - 10.0 - 120.0),
        (BackgroundAttachment::Scroll, 50.0 - 10.0),
    ] {
        let mut t = T::new();
        let r0 = t.root();
        let scroller = t.doc.block(r0, |s| {
            s.height = SizeValue::px(50.0);
            s.overflow_x = Overflow::Auto;
            s.overflow_y = Overflow::Auto;
            s.background_layers = vec![one_image(attachment, 100.0)];
        });
        t.doc.block(scroller, |s| s.height = SizeValue::px(300.0));
        let f = t.run(200.0, 200.0);
        let mut offsets = HashMap::new();
        offsets.insert(scroller, Point::new(0.0, 120.0));
        let mut opts = PaintOptions::viewport(Size::new(200.0, 200.0));
        opts.scroll = Some(&offsets);
        let dests = image_dests(&f, &t, &opts);
        assert_eq!(dests.len(), 1, "{attachment:?}");
        assert_eq!(dests[0].y, expected, "{attachment:?}");
    }
}

#[test]
fn space_and_round_repeat_fit_whole_images() {
    // CSS Backgrounds 3 §3.4 on a 105 px wide box with a 20 px image: `round` makes five
    // images of 21 px, `space` five of 20 px with 1.25 px between them.
    let xs = |repeat: BackgroundRepeat| {
        let mut t = T::new();
        let r0 = t.root();
        t.doc.block(r0, |s| {
            s.width = SizeValue::px(105.0);
            s.height = SizeValue::px(20.0);
            s.background_layers = vec![BackgroundLayer {
                image: Image::raster(snowlark_paint::ImageId(1), 20.0, 20.0),
                repeat: (repeat, BackgroundRepeat::NoRepeat),
                ..BackgroundLayer::default()
            }];
        });
        let f = t.run(200.0, 200.0);
        image_dests(&f, &t, &PaintOptions::viewport(Size::new(200.0, 200.0)))
    };
    let round = xs(BackgroundRepeat::Round);
    assert_eq!(round.len(), 5);
    assert_eq!(
        (round[1].x, round[1].width, round[1].height),
        (21.0, 21.0, 21.0)
    );
    let space = xs(BackgroundRepeat::Space);
    assert_eq!(space.len(), 5);
    assert_eq!(
        (space[1].x, space[4].x, space[4].width),
        (21.25, 85.0, 20.0)
    );
}

#[test]
fn flex_containers_paint_their_borders() {
    // A flex container laid out in block flow keeps its border (and padding) on its fragment.
    let mut t = T::new();
    let r0 = t.root();
    let flex = t.doc.block(r0, |s| {
        s.display = Display::FLEX;
        s.width = SizeValue::px(50.0);
        s.height = SizeValue::px(20.0);
        s.border_width = [1.0; 4];
        s.border_style = [BorderStyle::Solid; 4];
    });
    t.doc.block(flex, |s| s.width = SizeValue::px(10.0));
    let f = t.run(200.0, 200.0);
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, Size::new(200.0, 200.0), &mut b);
    let borders = b
        .build()
        .items
        .iter()
        .filter(|i| matches!(i, Item::Border(_)))
        .count();
    assert_eq!(borders, 1, "{}", f.dump());
}

#[test]
fn a_native_button_paints_its_own_frame_until_the_page_restyles_it() {
    // CSS UI 4 §6.2.1: a devolvable widget keeps its native look (a 1px border with 2px corners
    // over the border box) while the page sets none of its border or background properties; then
    // its CSS border (the UA sheet's 2px outset) and background paint.
    let mut t = T::new();
    let r0 = t.root();
    let button = |t: &mut T, native: bool, author: bool| {
        t.doc.element(r0, "button", |s| {
            s.display = Display::BLOCK;
            s.width = SizeValue::px(50.0);
            s.height = SizeValue::px(20.0);
            s.border_width = [2.0; 4];
            s.border_style = [BorderStyle::Outset; 4];
            s.native_appearance = native;
            s.author_border_background = author;
        })
    };
    button(&mut t, true, false);
    button(&mut t, true, true);
    button(&mut t, false, false);
    let f = t.run(200.0, 200.0);
    let mut b = DisplayListBuilder::new();
    paint(&f, &t.text, Size::new(200.0, 200.0), &mut b);
    let borders: Vec<_> = b
        .build()
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Border(b) => Some((b.widths.top, b.styles[0], b.radii.top_left.width)),
            _ => None,
        })
        .collect();
    assert_eq!(
        borders,
        [
            (1.0, BorderStyle::Solid, 2.0),
            (2.0, BorderStyle::Outset, 0.0),
            (2.0, BorderStyle::Outset, 0.0),
        ],
        "{}",
        f.dump()
    );
}

#[test]
fn an_empty_box_casts_its_spread_shadow() {
    // CSS Backgrounds 3 §7.1: a 0x0 box with a 50px spread casts a 100x100 shadow.
    let mut t = T::new();
    let r0 = t.root();
    t.doc.block(r0, |s| {
        s.width = SizeValue::px(0.0);
        s.height = SizeValue::px(0.0);
        s.box_shadows = vec![Shadow {
            offset_x: 50.0,
            offset_y: 50.0,
            blur: 0.0,
            spread: 50.0,
            color: Color::rgb(0, 128, 0),
            inset: false,
        }];
    });
    let f = t.run(200.0, 200.0);
    // painted whole, and painted for a part of the page (the window's band), which must not
    // skip the box for having no area
    for cull in [
        None,
        Some(snowlark_paint::Rect::new(-100.0, -100.0, 400.0, 400.0)),
    ] {
        let mut b = DisplayListBuilder::new();
        paint_part(
            &f,
            &t.text,
            &PaintOptions::viewport(Size::new(200.0, 200.0)),
            cull,
            &mut b,
        );
        let shadows = b
            .build()
            .items
            .iter()
            .filter(|i| matches!(i, Item::BoxShadow(_)))
            .count();
        assert_eq!(shadows, 1, "{cull:?} {}", f.dump());
    }
}

/// A fixed box painted by a fixed box's stacking context stays in place with it, in its
/// layer: nothing of it depends on the scroll position. (It was marked as content no layer
/// describes, and the page was painted again at every scroll position.)
#[test]
fn a_fixed_box_in_a_fixed_boxes_layer_does_not_pin_the_list() {
    use snowlark_paint::{Color, LayerKind};
    let mut t = T::new();
    let r0 = t.root();
    t.doc.block(r0, |s| s.height = SizeValue::px(3000.0));
    let outer = t.doc.block(r0, |s| {
        s.position = Position::Fixed;
        s.inset[0] = LengthPercentageAuto::px(0.0);
        s.height = SizeValue::px(40.0);
        s.width = SizeValue::px(200.0);
        s.background_color = Color::hex(0x0000ff);
    });
    t.doc.block(outer, |s| {
        s.position = Position::Fixed;
        s.inset[0] = LengthPercentageAuto::px(50.0);
        s.height = SizeValue::px(10.0);
        s.width = SizeValue::px(20.0);
        s.background_color = Color::hex(0x00ff00);
    });
    let f = t.run(200.0, 200.0);
    let opts = PaintOptions {
        viewport: Size::new(200.0, 200.0),
        viewport_scroll: Point::new(0.0, 100.0),
        layers: true,
        ..Default::default()
    };
    let mut b = DisplayListBuilder::new();
    paint_with(&f, &t.text, &opts, &mut b);
    let split = b.build().split_layers();
    assert!(!split.pinned);
    // Both boxes are in the one layer, where the viewport has them.
    let fixed: Vec<_> = split
        .layers
        .iter()
        .filter(|l| l.kind == Some(LayerKind::Fixed))
        .collect();
    assert_eq!(fixed.len(), 1);
    let ys: Vec<f32> = fixed[0]
        .list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::FillRect { rect, .. } => Some(rect.y),
            _ => None,
        })
        .collect();
    assert_eq!(ys, [0.0, 50.0]);
}

/// A canvas background whose images are all attached to the viewport
/// (`background-attachment: fixed`) is a layer that stays in place under the page; with an image
/// that scrolls too, it is painted again at each scroll position as before.
#[test]
fn a_canvas_background_attached_to_the_viewport_is_a_layer_under_the_page() {
    use snowlark_paint::{Color, LayerKind};
    let paint = |attachments: &[BackgroundAttachment]| {
        let mut t = T::new();
        let r0 = t.root();
        let layers: Vec<BackgroundLayer> = attachments
            .iter()
            .map(|&attachment| one_image(attachment, 0.0))
            .collect();
        t.doc.restyle(r0, |s| {
            s.background_color = Color::hex(0x050b1a);
            s.background_layers = layers;
        });
        t.doc.block(r0, |s| {
            s.height = SizeValue::px(3000.0);
            s.width = SizeValue::px(50.0);
            s.background_color = Color::hex(0x00ff00);
        });
        let f = t.run(200.0, 200.0);
        let opts = PaintOptions {
            viewport: Size::new(200.0, 200.0),
            viewport_scroll: Point::new(0.0, 100.0),
            layers: true,
            ..Default::default()
        };
        let mut b = DisplayListBuilder::new();
        paint_with(&f, &t.text, &opts, &mut b);
        b.build().split_layers()
    };
    let split = paint(&[BackgroundAttachment::Fixed]);
    assert!(!split.pinned);
    let kinds: Vec<_> = split.layers.iter().map(|l| l.kind).collect();
    assert_eq!(kinds, [Some(LayerKind::Fixed), None]);
    // (the canvas's color is in the layer, under the page's own content)
    assert!(split.layers[0]
        .list
        .items
        .iter()
        .any(|i| matches!(i, Item::FillRect { color, .. } if *color == Color::hex(0x050b1a))));
    assert!(paint(&[BackgroundAttachment::Fixed, BackgroundAttachment::Scroll]).pinned);
    assert!(!paint(&[BackgroundAttachment::Scroll]).pinned);
}
