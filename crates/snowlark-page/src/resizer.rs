// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The resizer of a box whose `resize` lets the user change its size (CSS UI 4 §5.1): a primary
//! press on it and a drag set the element's `width` and `height` in its style attribute, in
//! pixels and without `!important`, as the platform resizers do.

use snowlark_bindings::rt::Cx;
use snowlark_dom::NodeId;
use snowlark_layout::style::Resize;
use snowlark_paint::Point;

use crate::engine::view::scroll_shift;
use crate::engine::Engine;

/// A resize in progress, from a press on an element's resizer to the button's release.
#[derive(Clone, Copy, Debug)]
pub struct Resizing {
    element: NodeId,
    axes: Resize,
    /// Where the press was, in the top-level viewport (moves are measured from it).
    start: Point,
    /// The `width` and `height` the element had then (its content or border box, as
    /// `box-sizing` says), and the last ones written.
    size: (f32, f32),
    last: (f32, f32),
    /// The smallest `width` and `height`: the border box stays as large as the resizer.
    min: (f32, f32),
}

impl Engine {
    /// The element whose resizer is under `p` (CSS px in the viewport of `el`'s document), `el`
    /// or one of its ancestors there, with the sizes its resizer changes and its geometry then.
    fn resizer_under(&self, el: NodeId, p: Point) -> Option<(NodeId, snowlark_layout::Resizer)> {
        self.with_layout(el, |r, st| {
            let dom = self.dom();
            let mut cur = Some(el);
            while let Some(n) = cur {
                if dom.is_element(n) {
                    if let Some(z) = r.resizer(n) {
                        let shift = scroll_shift(r, st, &dom, n);
                        let sticky = r.sticky_offset(n, st.viewport, st.scroll, &st.element_scroll);
                        let corner = z.corner.translate(sticky.x - shift.x, sticky.y - shift.y);
                        if p.x >= corner.x
                            && p.x < corner.right()
                            && p.y >= corner.y
                            && p.y < corner.bottom()
                        {
                            return Some((n, z));
                        }
                    }
                }
                cur = r.flat_parent(&dom, n).filter(|&a| dom.is_element(a));
            }
            None
        })
        .flatten()
    }

    /// The cursor over the resizer under the pointer, or of the resize in progress: the resize
    /// cursor of the sizes it changes.
    pub(crate) fn resizer_cursor(&self, el: Option<NodeId>, p: Point) -> Option<&'static str> {
        let axes = match self.pointer.borrow().resizing {
            Some(r) => Some(r.axes),
            None => None,
        };
        let axes = axes.or_else(|| self.resizer_under(el?, p).map(|(_, z)| z.axes))?;
        Some(match axes {
            Resize::Horizontal => "ew-resize",
            Resize::Vertical => "ns-resize",
            _ => "nwse-resize",
        })
    }

    /// A primary press on `el` at `p` (its document's viewport) that its `mousedown` did not
    /// cancel: when it is on a resizer, the resize starts (`top` is the press in the top-level
    /// viewport) and nothing else happens by default (no caret, no selection, no drag).
    pub(crate) fn resizer_pressed(&self, el: NodeId, p: Point, top: Point) -> bool {
        let Some((element, z)) = self.resizer_under(el, p) else {
            return false;
        };
        let edges = (
            z.border_box.width - z.content_box.width,
            z.border_box.height - z.content_box.height,
        );
        let (size, own) = if z.border_box_sizing {
            ((z.border_box.width, z.border_box.height), (0.0, 0.0))
        } else {
            ((z.content_box.width, z.content_box.height), edges)
        };
        let min = (
            (z.corner.width - own.0).max(0.0),
            (z.corner.height - own.1).max(0.0),
        );
        self.pointer.borrow_mut().resizing = Some(Resizing {
            element,
            axes: z.axes,
            start: top,
            size,
            last: size,
            min,
        });
        true
    }

    /// The pointer moved to `top` (the top-level viewport) during a resize: the element's sizes
    /// follow it. Whether a resize is in progress (a move then selects nothing).
    pub(crate) fn resizer_moved(&self, cx: &mut Cx, top: Point) -> bool {
        let Some(mut z) = self.pointer.borrow().resizing else {
            return false;
        };
        let here = {
            let dom = self.dom();
            dom.is_alive(z.element) && dom.is_connected(z.element)
        };
        if !here {
            self.pointer.borrow_mut().resizing = None;
            return false;
        }
        let to = |size: f32, d: f32, min: f32| (size + d).max(min).round();
        let mut props = Vec::new();
        if z.axes.horizontal() {
            let w = to(z.size.0, top.x - z.start.x, z.min.0);
            if w != z.last.0 {
                props.push(("width".to_string(), format!("{w}px")));
                z.last.0 = w;
            }
        }
        if z.axes.vertical() {
            let h = to(z.size.1, top.y - z.start.y, z.min.1);
            if h != z.last.1 {
                props.push(("height".to_string(), format!("{h}px")));
                z.last.1 = h;
            }
        }
        self.pointer.borrow_mut().resizing = Some(z);
        if !props.is_empty() {
            self.set_inline_properties(cx, z.element, &props);
        }
        true
    }

    /// The primary button went up: a resize in progress ends.
    pub(crate) fn resizer_released(&self) {
        self.pointer.borrow_mut().resizing = None;
    }
}
