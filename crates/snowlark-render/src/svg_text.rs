// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SVG text layout for the DOM (SVG 2 §11.8 "text layout"): the addressable characters of a `text`
//! element with white space handled (§11.7.1), their positions from the `x`, `y`, `dx`, `dy` and
//! `rotate` attributes of the `text` and `tspan` elements (§11.8.2 "resolve character
//! positioning"), advances from the text engine, text chunks anchored per `text-anchor`, vertical
//! writing modes, and `textPath` (§11.8.3). The glyph cells it gives are what
//! `SVGTextContentElement`'s methods, `getBBox` and hit testing read.
//!
//! Not modelled: bidi reordering, shaping across characters (each character's advance is its own),
//! `letter-spacing`, `word-spacing`, `textLength`, baseline alignment other than the alphabetic one.

use std::collections::HashMap;

use kurbo::{BezPath, Point, Rect};
use snowlark_dom::{NodeData, NodeId};
use snowlark_layout::svg::{geom, Length, LengthDir, Matrix, PathData};

use crate::svg_geometry::SvgQuery;

/// One addressable character (SVG 2 §11.8.1) in layout.
#[derive(Clone, Debug)]
pub struct TextChar {
    /// The innermost element whose child text the character is in.
    pub owner: NodeId,
    pub ch: char,
    /// The glyph origin: on the baseline at the start of the glyph (the centre line for vertical
    /// text).
    pub x: f32,
    pub y: f32,
    pub advance: f32,
    /// The rotation of the glyph, in degrees (`rotate`, plus a `textPath`'s direction).
    pub rotate: f32,
    pub vertical: bool,
    pub ascent: f32,
    pub descent: f32,
    /// The font size (the cell's thickness in vertical text).
    pub size: f32,
    /// Off a `textPath`'s path: not rendered.
    pub hidden: bool,
}

impl TextChar {
    /// The glyph's direction of progression.
    fn dir(&self) -> (f32, f32) {
        let a = self.rotate.to_radians();
        let (s, c) = a.sin_cos();
        if self.vertical {
            (-s, c)
        } else {
            (c, s)
        }
    }

    /// The point at the end of the glyph's advance.
    pub fn end(&self) -> (f32, f32) {
        let (dx, dy) = self.dir();
        (self.x + dx * self.advance, self.y + dy * self.advance)
    }

    /// The glyph cell: the advance by the font's ascent and descent (by the font size, centred on
    /// the glyph origin, in vertical text), rotated by the glyph's rotation.
    pub fn cell(&self) -> BezPath {
        let r = if self.vertical {
            Rect::new(
                (self.x - self.size / 2.0) as f64,
                self.y as f64,
                (self.x + self.size / 2.0) as f64,
                (self.y + self.advance) as f64,
            )
        } else {
            Rect::new(
                self.x as f64,
                (self.y - self.ascent) as f64,
                (self.x + self.advance) as f64,
                (self.y + self.descent) as f64,
            )
        };
        let m = Matrix::translate((self.x as f64, self.y as f64))
            * Matrix::rotate((self.rotate as f64).to_radians())
            * Matrix::translate((-self.x as f64, -self.y as f64));
        m * kurbo::Shape::to_path(&r, 0.1)
    }

    /// The bounding box of the glyph cell.
    pub fn extent(&self) -> Rect {
        kurbo::Shape::bounding_box(&self.cell())
    }
}

/// The characters of a `text` element in layout.
#[derive(Clone, Debug, Default)]
pub struct TextLayout {
    pub chars: Vec<TextChar>,
}

/// Per-character positioning values (SVG 2 §11.8.2).
#[derive(Clone, Copy, Default)]
struct Positioning {
    x: Option<f32>,
    y: Option<f32>,
    dx: f32,
    dy: f32,
    rotate: Option<f32>,
}

fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

impl SvgQuery<'_> {
    /// The `text` element a text content element belongs to (itself for a `text` element).
    pub fn text_root(&self, n: NodeId) -> Option<NodeId> {
        let dom = &self.dom;
        let mut cur = Some(n);
        while let Some(c) = cur {
            let e = dom.element(c)?;
            if e.name().ns != snowlark_dom::ns!(svg) {
                return None;
            }
            if &*e.name().local == "text" {
                return Some(c);
            }
            cur = dom.parent_element(c);
        }
        None
    }

    /// Whether white space is preserved in `el` (`white-space` other than `normal`, or
    /// `xml:space="preserve"` when not rendered).
    fn preserves_space(&self, el: NodeId) -> bool {
        let dom = &self.dom;
        let mut cur = Some(el);
        while let Some(c) = cur {
            if let Some(v) = dom.attr(c, &snowlark_dom::ns!(xml), &"space".into()) {
                return v == "preserve";
            }
            cur = dom.parent_element(c);
        }
        self.prop(el, "white-space")
            .is_some_and(|v| v != "normal" && v != "nowrap")
    }

    /// The addressable characters of a `text` element and the element each is in, white space
    /// collapsed (SVG 2 §11.7.1).
    pub(crate) fn text_characters(&self, text_el: NodeId) -> Vec<(NodeId, char)> {
        let mut out: Vec<(NodeId, char)> = Vec::new();
        // (a pre-order walk that skips undisplayed and non-text-content children)
        fn walk(q: &SvgQuery, n: NodeId, out: &mut Vec<(NodeId, char)>) {
            let dom = &q.dom;
            for c in dom.children(n) {
                match dom.data(c) {
                    NodeData::Text(t) => {
                        let preserve = q.preserves_space(n);
                        for ch in t.chars() {
                            if ch == '\n' && !preserve {
                                continue;
                            }
                            let ch = if is_space(ch) { ' ' } else { ch };
                            if ch == ' ' && !preserve && out.last().is_none_or(|l| l.1 == ' ') {
                                continue;
                            }
                            out.push((n, ch));
                        }
                    }
                    NodeData::Element(e) => {
                        let ok = e.name().ns == snowlark_dom::ns!(svg)
                            && matches!(&*e.name().local, "tspan" | "textPath" | "a")
                            && q.prop(c, "display").as_deref() != Some("none");
                        if ok {
                            walk(q, c, out);
                        }
                    }
                    _ => {}
                }
            }
        }
        walk(self, text_el, &mut out);
        // (a trailing collapsible space goes)
        if out
            .last()
            .is_some_and(|l| l.1 == ' ' && !self.preserves_space(l.0))
        {
            out.pop();
        }
        out
    }

    /// A length list attribute in user units.
    pub(crate) fn length_list(&self, el: NodeId, name: &str, dir: LengthDir) -> Vec<f32> {
        let Some(v) = self.attr(el, name) else {
            return Vec::new();
        };
        let mut lengths: Vec<Length> = Vec::new();
        for t in v
            .split(|c: char| c == ',' || c.is_ascii_whitespace())
            .filter(|t| !t.is_empty())
        {
            match Length::parse(t) {
                Some(l) => lengths.push(l),
                None => return Vec::new(),
            }
        }
        let ctx = self.length_context_for(el, lengths.iter().map(|l| &l.unit));
        lengths.iter().map(|l| l.to_user(dir, &ctx)).collect()
    }

    /// Lays out a `text` element's characters.
    pub fn text_layout(&self, text_el: NodeId) -> Option<TextLayout> {
        let chars = self.text_characters(text_el);
        let n = chars.len();
        let mut pos = vec![Positioning::default(); n];
        // §11.8.2: each `text`/`tspan` element's lists apply to its characters, inner elements
        // overriding outer ones (a pre-order walk assigns outer first).
        let dom = &self.dom;
        let mut elements = vec![text_el];
        for d in dom.descendants(text_el) {
            if let Some(e) = dom.element(d) {
                if e.name().ns == snowlark_dom::ns!(svg) && &*e.name().local == "tspan" {
                    elements.push(d);
                }
            }
        }
        for &el in &elements {
            // The character range of `el`.
            let first = chars
                .iter()
                .position(|c| dom.is_inclusive_ancestor_of(el, c.0));
            let Some(first) = first else { continue };
            let count = chars[first..]
                .iter()
                .take_while(|c| dom.is_inclusive_ancestor_of(el, c.0))
                .count();
            let xs = self.length_list(el, "x", LengthDir::Horizontal);
            let ys = self.length_list(el, "y", LengthDir::Vertical);
            let dxs = self.length_list(el, "dx", LengthDir::Horizontal);
            let dys = self.length_list(el, "dy", LengthDir::Vertical);
            let rotates = self
                .attr(el, "rotate")
                .and_then(snowlark_layout::svg::parse_number_list)
                .unwrap_or_default();
            for i in 0..count {
                let p = &mut pos[first + i];
                if let Some(v) = xs.get(i) {
                    p.x = Some(*v);
                }
                if let Some(v) = ys.get(i) {
                    p.y = Some(*v);
                }
                if let Some(v) = dxs.get(i) {
                    p.dx = *v;
                }
                if let Some(v) = dys.get(i) {
                    p.dy = *v;
                }
                // (the last rotate value applies to the remaining characters)
                if let Some(v) = rotates.get(i).or(rotates.last()) {
                    p.rotate = Some(*v as f32);
                }
            }
        }
        // Advances and fonts.
        let mut fonts: HashMap<NodeId, Option<(snowlark_paint::FontListId, f32, f32, f32)>> =
            HashMap::new();
        let mut out = Vec::with_capacity(n);
        let vertical = self.prop(text_el, "writing-mode").is_some_and(|w| {
            w.starts_with("vertical") || w.starts_with("sideways") || w.starts_with("tb")
        });
        let (mut cx, mut cy) = (0.0f32, 0.0f32);
        let mut chunks: Vec<usize> = Vec::new();
        for (i, &(owner, ch)) in chars.iter().enumerate() {
            let font = *fonts.entry(owner).or_insert_with(|| {
                let (text, list, size) = self.font_of(owner)?;
                let m = text.metrics(text.primary_font(list), size);
                Some((list, size, m.ascent, m.descent))
            });
            let (advance, size, ascent, descent) = match font {
                Some((list, size, ascent, descent)) => {
                    let text = self.style.as_ref().map(|s| s.text);
                    let adv = text
                        .and_then(|t| t.measure(&ch.to_string(), list, size).ok())
                        .unwrap_or(size * 0.5);
                    (adv, size, ascent, descent)
                }
                None => (8.0, 16.0, 12.8, 3.2),
            };
            let p = pos[i];
            let new_chunk = i == 0 || p.x.is_some() || p.y.is_some();
            if let Some(x) = p.x {
                cx = x;
            }
            if let Some(y) = p.y {
                cy = y;
            }
            cx += p.dx;
            cy += p.dy;
            if new_chunk {
                chunks.push(i);
            }
            out.push(TextChar {
                owner,
                ch,
                x: cx,
                y: cy,
                advance,
                rotate: p.rotate.unwrap_or(0.0),
                vertical,
                ascent,
                descent,
                size,
                hidden: false,
            });
            if vertical {
                cy += advance;
            } else {
                cx += advance;
            }
        }
        // Text chunks anchored per `text-anchor` (§11.8.2 step "apply anchoring").
        chunks.push(n);
        for w in chunks.windows(2) {
            let (a, b) = (w[0], w[1]);
            if a >= b {
                continue;
            }
            let anchor = self.prop(out[a].owner, "text-anchor");
            let total: f32 = out[a..b].iter().map(|c| c.advance).sum();
            let shift = match anchor.as_deref() {
                Some("middle") => -total / 2.0,
                Some("end") => -total,
                _ => 0.0,
            };
            if shift != 0.0 {
                for c in &mut out[a..b] {
                    if vertical {
                        c.y += shift;
                    } else {
                        c.x += shift;
                    }
                }
            }
        }
        self.apply_text_paths(text_el, &mut out);
        Some(TextLayout { chars: out })
    }

    /// The path a `textPath` element follows: its `path` attribute, else the `path` element its
    /// `href` references, in the `textPath`'s user space.
    pub fn text_path_geometry(&self, tp: NodeId) -> Option<BezPath> {
        if let Some(d) = self.attr(tp, "path") {
            return Some(PathData::parse(d).to_bezpath());
        }
        let target = self.use_target(tp)?;
        let path = self.shape_path(target)?;
        Some(self.local_transform(target) * path)
    }

    /// Positions the characters in `textPath` elements along their paths (SVG 2 §11.8.3).
    fn apply_text_paths(&self, text_el: NodeId, chars: &mut [TextChar]) {
        let dom = &self.dom;
        let paths: Vec<NodeId> = dom
            .descendants(text_el)
            .filter(|&d| {
                dom.element(d).is_some_and(|e| {
                    e.name().ns == snowlark_dom::ns!(svg) && &*e.name().local == "textPath"
                })
            })
            .collect();
        for tp in paths {
            let idx: Vec<usize> = (0..chars.len())
                .filter(|&i| dom.is_inclusive_ancestor_of(tp, chars[i].owner))
                .collect();
            if idx.is_empty() {
                continue;
            }
            let Some(path) = self.text_path_geometry(tp) else {
                for &i in &idx {
                    chars[i].hidden = true;
                }
                continue;
            };
            let length = geom::total_length(&path) as f32;
            let start =
                self.attr(tp, "startOffset")
                    .and_then(Length::parse)
                    .map_or(0.0, |l| match l.unit {
                        snowlark_layout::svg::LengthUnit::Percent => l.value * length / 100.0,
                        _ => self.to_user(tp, &l, LengthDir::Horizontal),
                    });
            // The inline offset of each character from the first one, kept from the layout above.
            let origin = chars[idx[0]].x;
            for &i in &idx {
                let c = &mut chars[i];
                let along = start + (c.x - origin);
                let mid = along + c.advance / 2.0;
                if mid < 0.0 || mid > length || length <= 0.0 {
                    c.hidden = true;
                    continue;
                }
                let p = geom::point_at_length(&path, mid as f64);
                let angle = geom::angle_at_length(&path, mid as f64) as f32;
                let (s, co) = angle.to_radians().sin_cos();
                let half = c.advance / 2.0;
                c.x = p.x as f32 - co * half;
                c.y = p.y as f32 - s * half;
                c.rotate += angle;
            }
        }
    }

    /// The characters of a text content element `n` in its `text` element's layout (the index of
    /// the first one, and the slice).
    pub fn text_chars(&self, n: NodeId) -> Option<(TextLayout, std::ops::Range<usize>)> {
        let root = self.text_root(n)?;
        let layout = self.text_layout(root)?;
        let dom = &self.dom;
        let first = layout
            .chars
            .iter()
            .position(|c| dom.is_inclusive_ancestor_of(n, c.owner));
        let range = match first {
            Some(f) => {
                let count = layout.chars[f..]
                    .iter()
                    .take_while(|c| dom.is_inclusive_ancestor_of(n, c.owner))
                    .count();
                f..f + count
            }
            None => 0..0,
        };
        Some((layout, range))
    }

    /// The union of the glyph cells of a text content element's rendered characters, as a path.
    pub fn text_cells(&self, n: NodeId) -> Option<BezPath> {
        let (layout, range) = self.text_chars(n)?;
        let mut p = BezPath::new();
        for c in &layout.chars[range] {
            if !c.hidden {
                p.extend(c.cell());
            }
        }
        (!p.elements().is_empty()).then_some(p)
    }

    /// The bounding box of a text content element's glyph cells.
    pub fn text_bbox(&self, n: NodeId, m: Matrix) -> Option<Rect> {
        let cells = self.text_cells(n)?;
        geom::bounds(&cells, m)
    }

    /// The character of a text content element whose glyph cell holds `p` (SVG 2 §11.10.1
    /// `getCharNumAtPosition`), as an index among its characters.
    pub fn char_at(&self, n: NodeId, p: Point) -> Option<usize> {
        let (layout, range) = self.text_chars(n)?;
        layout.chars[range]
            .iter()
            .enumerate()
            .rev()
            .find(|(_, c)| !c.hidden && kurbo::Shape::winding(&c.cell(), p) != 0)
            .map(|(i, _)| i)
    }
}
