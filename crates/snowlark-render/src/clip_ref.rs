// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `clip-path: url(#id)` (CSS Masking 1 §5.1, §6): a `<clipPath>` element as layout's
//! [`ClipReference`]. The element is written out as an SVG document with a shape that uses it
//! ([`crate::inline_svg::clip_path_markup`]) and usvg resolves its content (`use`, transforms,
//! text, `clip-rule`), so the geometry here is the geometry SVG images get. A `<clipPath>` with a
//! `clip-path` of its own is another level, clipping inside the first (its reference box the same).
//!
//! The clip of a shape inside a `clipPath` (`<rect clip-path=...>`) is not applied: the union of
//! the shapes takes the whole shape.

use resvg::usvg;
use snowlark_dom::{ns, Dom, LocalName, NodeId};
use snowlark_layout::style::{ClipLevel, ClipReference};
use snowlark_paint::{Matrix, Point};
use usvg::tiny_skia_path::{PathSegment, Transform};

/// Line segments per curve. Curves are flattened in the `clipPath`'s own units (which may be
/// fractions of a box), so by a fixed count and not by length.
const CURVE_SEGMENTS: u32 = 24;

/// How many `<clipPath>`s may clip one another (`clip-path` on a `clipPath`).
pub(crate) const MAX_LEVELS: usize = 4;

/// The id a computed `url()` names: its fragment, whatever document the URL names (as for SVG
/// content, `crate::svg_paint`; a style sheet's `url(#id)` computes to the sheet's URL with the
/// fragment, and references to other documents are not followed).
pub(crate) fn local_id(url: &str) -> Option<&str> {
    let (_, id) = url.rsplit_once('#')?;
    (!id.is_empty()).then_some(id)
}

/// The `clipPath` element `id` names for `user`: in `user`'s tree, then the document's. Anything
/// else, or nothing, is an invalid reference: no clipping (CSS Masking 1 §5.1).
pub(crate) fn find_clip_path(dom: &Dom, user: NodeId, id: &str) -> Option<NodeId> {
    let named = |root: NodeId| {
        dom.descendants(root)
            .find(|&n| dom.is_element(n) && dom.attr(n, &ns!(), &LocalName::from("id")) == Some(id))
    };
    let root = dom.root(user);
    let found = named(root).or_else(|| {
        let doc = dom.node_document(user);
        (doc != root).then(|| named(doc)).flatten()
    })?;
    let e = dom.element(found)?;
    (e.name().ns == ns!(svg) && &*e.name().local == "clipPath").then_some(found)
}

/// Whether `clip` has `clipPathUnits="objectBoundingBox"`.
pub(crate) fn bounding_box_units(dom: &Dom, clip: NodeId) -> bool {
    let name = LocalName::from("clipPathUnits");
    dom.attr(clip, &ns!(), &name)
        .is_some_and(|v| v.trim() == "objectBoundingBox")
}

/// Resolves the levels of a reference, each from its `clipPath`'s markup (as
/// [`crate::inline_svg::clip_path_markup`] wrote it) and units. A level whose markup usvg cannot
/// read, or whose `clipPath` has no content, clips everything away (usvg drops a shape whose clip
/// path is empty: it shows nothing).
pub(crate) fn resolve(
    levels: &[(String, bool)],
    fonts: Option<std::sync::Arc<usvg::fontdb::Database>>,
) -> ClipReference {
    let mut out = ClipReference::default();
    for (markup, bounding_box_units) in levels {
        let mut options = usvg::Options {
            font_size: 16.0,
            ..usvg::Options::default()
        };
        if let Some(fonts) = &fonts {
            options.fontdb = fonts.clone();
        }
        let tree = usvg::Tree::from_str(markup, &options).ok();
        let clip = tree.as_ref().and_then(|t| clip_of(t.root()));
        let level = match clip {
            Some(c) => {
                let mut shapes = Vec::new();
                collect(c.root(), Transform::identity(), &mut shapes);
                let (contours, even_odd) = snowlark_layout::clip::union_of_shapes(shapes);
                let t = c.transform();
                ClipLevel {
                    bounding_box_units: *bounding_box_units,
                    transform: Matrix::new(t.sx, t.ky, t.kx, t.sy, t.tx, t.ty),
                    contours,
                    even_odd,
                }
            }
            None => ClipLevel {
                bounding_box_units: *bounding_box_units,
                ..ClipLevel::default()
            },
        };
        out.levels.push(level);
    }
    out
}

/// The clip path of the one shape in the document.
fn clip_of(group: &usvg::Group) -> Option<&usvg::ClipPath> {
    group.clip_path().or_else(|| {
        group.children().iter().find_map(|n| match n {
            usvg::Node::Group(g) => clip_of(g),
            _ => None,
        })
    })
}

/// The shapes of a `clipPath`'s content, as the renderer of SVG images composes their transforms
/// (each group's onto its parent's; the `clipPath`'s own is kept apart).
fn collect(group: &usvg::Group, base: Transform, out: &mut Vec<(Vec<Vec<Point>>, bool)>) {
    for node in group.children() {
        match node {
            usvg::Node::Group(g) => collect(g, base.pre_concat(g.transform()), out),
            usvg::Node::Path(p) if p.is_visible() => {
                let even_odd = p
                    .fill()
                    .is_some_and(|f| f.rule() == usvg::FillRule::EvenOdd);
                out.push((contours(p.data(), base), even_odd));
            }
            usvg::Node::Text(t) => collect(t.flattened(), base, out),
            usvg::Node::Path(_) | usvg::Node::Image(_) => {}
        }
    }
}

fn contours(path: &usvg::tiny_skia_path::Path, m: Transform) -> Vec<Vec<Point>> {
    let map = |p: usvg::tiny_skia_path::Point| {
        Point::new(
            m.sx * p.x + m.kx * p.y + m.tx,
            m.ky * p.x + m.sy * p.y + m.ty,
        )
    };
    let mut out = Vec::new();
    let mut contour: Vec<Point> = Vec::new();
    let mut at = Point::ZERO;
    for segment in path.segments() {
        match segment {
            PathSegment::MoveTo(p) => {
                if contour.len() > 2 {
                    out.push(std::mem::take(&mut contour));
                }
                contour.clear();
                at = map(p);
                contour.push(at);
            }
            PathSegment::LineTo(p) => {
                at = map(p);
                contour.push(at);
            }
            PathSegment::QuadTo(c, p) => {
                let (c, e) = (map(c), map(p));
                for i in 1..=CURVE_SEGMENTS {
                    let t = i as f32 / CURVE_SEGMENTS as f32;
                    let u = 1.0 - t;
                    let (a, b, d) = (u * u, 2.0 * u * t, t * t);
                    contour.push(Point::new(
                        a * at.x + b * c.x + d * e.x,
                        a * at.y + b * c.y + d * e.y,
                    ));
                }
                at = e;
            }
            PathSegment::CubicTo(c1, c2, p) => {
                let (c1, c2, e) = (map(c1), map(c2), map(p));
                for i in 1..=CURVE_SEGMENTS {
                    let t = i as f32 / CURVE_SEGMENTS as f32;
                    let u = 1.0 - t;
                    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
                    contour.push(Point::new(
                        a * at.x + b * c1.x + c * c2.x + d * e.x,
                        a * at.y + b * c1.y + c * c2.y + d * e.y,
                    ));
                }
                at = e;
            }
            PathSegment::Close => {}
        }
    }
    if contour.len() > 2 {
        out.push(contour);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_id_is_the_fragment() {
        assert_eq!(local_id("#c"), Some("c"));
        assert_eq!(local_id("http://a.test/p#c"), Some("c"));
        assert_eq!(local_id("http://a.test/sheet.css#c"), Some("c"));
        assert_eq!(local_id("#"), None);
        assert_eq!(local_id("c"), None);
    }

    /// A `clipPath` of two shapes, one turned, in user units: one path, the union of both.
    #[test]
    fn a_clip_path_element_resolves_to_one_path_per_level() {
        let markup = "<svg xmlns='http://www.w3.org/2000/svg' width='100' height='100'>\
             <defs><clipPath id='c' transform='translate(5 0)'>\
             <rect width='10' height='10'/><rect x='20' width='10' height='10' transform='rotate(45 25 5)'/>\
             </clipPath></defs><rect width='1' height='1' clip-path='url(#c)'/></svg>";
        let r = resolve(&[(markup.to_owned(), false)], None);
        assert_eq!(r.levels.len(), 1);
        let level = &r.levels[0];
        assert_eq!(level.transform, Matrix::new(1.0, 0.0, 0.0, 1.0, 5.0, 0.0));
        assert_eq!(level.contours.len(), 2);
        assert!(!level.even_odd);
        let inside =
            |x, y| snowlark_layout::clip::path_contains(&level.contours, false, Point::new(x, y));
        assert!(inside(5.0, 5.0));
        assert!(inside(25.0, 5.0));
        // (the turned square's corner is at 25 ± 7.07 on its middle line, not at its old corner)
        assert!(inside(31.0, 5.0));
        assert!(!inside(30.0, 1.0));
        assert!(!inside(15.0, 5.0));
    }

    #[test]
    fn an_empty_clip_path_clips_everything() {
        let markup = "<svg xmlns='http://www.w3.org/2000/svg' width='100' height='100'>\
             <defs><clipPath id='c'></clipPath></defs><rect width='1' height='1' clip-path='url(#c)'/></svg>";
        let r = resolve(&[(markup.to_owned(), true)], None);
        assert_eq!(r.levels.len(), 1);
        assert!(r.levels[0].contours.is_empty());
        assert!(r.levels[0].bounding_box_units);
    }
}
