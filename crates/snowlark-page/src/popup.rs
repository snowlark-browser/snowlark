// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The UI a page asks its embedder to show over it: a drop-down `select`'s list (HTML §4.10.7: "the
//! user agent may show a list of the options"; the embedder draws it outside the page's scroll area
//! and reports the user's choice back), and the list box selection a click makes in a `select` with
//! `multiple` or a `size` above 1 (with Ctrl and Shift as on Windows).

use serde::{Deserialize, Serialize};
use snowlark_bindings::rt::Cx;
use snowlark_dom::{local_name, NodeId};
use snowlark_paint::{Point, Rect};

use crate::engine::html::is_disabled;
use crate::engine::{node_handle, Engine};
use crate::input::Modifiers;
use crate::Page;

/// A row of a drop-down list: an option or an optgroup's label.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PopupItem {
    pub label: String,
    pub disabled: bool,
    /// An optgroup label (not an option).
    pub group: bool,
    /// Inside an optgroup.
    pub indented: bool,
}

/// A drop-down `select`'s list, for the embedder to show.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SelectPopup {
    pub select: NodeId,
    /// The select's border box in the top-level viewport (CSS px).
    pub anchor: Rect,
    pub items: Vec<PopupItem>,
    /// The selected option's row in `items`.
    pub selected: Option<usize>,
    /// The select's font size (CSS px).
    pub font_size: f32,
}

impl SelectPopup {
    /// The option index (among the select's options) of a row, `None` for a group label.
    pub fn option_of_row(&self, row: usize) -> Option<usize> {
        let item = self.items.get(row)?;
        if item.group {
            return None;
        }
        Some(self.items[..row].iter().filter(|i| !i.group).count())
    }
}

/// Whether a select is a drop-down box (HTML §4.10.7: no `multiple`, display size 1).
pub(crate) fn is_drop_down(dom: &snowlark_dom::Dom, select: NodeId) -> bool {
    dom.get_attribute(select, "multiple").is_none()
        && dom
            .get_attribute(select, "size")
            .and_then(snowlark_bindings::rt::parse_non_negative_integer)
            .is_none_or(|n| n <= 1)
}

impl Engine {
    /// `el`'s border box in the top-level viewport (CSS px), through the frames it is in.
    pub(crate) fn top_viewport_rect(&self, el: NodeId) -> Option<Rect> {
        self.top_viewport_box(el, false)
    }

    /// `el`'s border box (or content box) in the top-level viewport (CSS px).
    pub(crate) fn top_viewport_box(&self, el: NodeId, content: bool) -> Option<Rect> {
        let mut rect = self
            .with_layout(el, |r, st| {
                let g = r.geometry(el)?;
                let s = crate::engine::view::scroll_shift(r, st, &self.dom(), el);
                let b = if content { g.content_box } else { g.border_box };
                Some(b.translate(-s.x, -s.y))
            })
            .flatten()?;
        let mut doc = self.dom().node_document(el);
        while let Some(c) = self.rendered_container(doc) {
            let origin = self
                .with_layout(c, |r, st| {
                    let g = r.geometry(c)?;
                    let s = crate::engine::view::scroll_shift(r, st, &self.dom(), c);
                    Some(Point::new(g.content_box.x - s.x, g.content_box.y - s.y))
                })
                .flatten()?;
            rect = rect.translate(origin.x, origin.y);
            doc = self.dom().node_document(c);
        }
        Some(rect)
    }

    /// The rows of a select's list: its options in tree order with its optgroups' labels.
    fn popup_items(&self, select: NodeId) -> (Vec<PopupItem>, Option<usize>) {
        let selected = self.select_selected_index(select);
        let dom = self.dom();
        let mut items = Vec::new();
        let mut row_of_selected = None;
        let mut index = 0i32;
        let mut push = |items: &mut Vec<PopupItem>, o: NodeId, indented: bool| {
            if index == selected {
                row_of_selected = Some(items.len());
            }
            index += 1;
            items.push(PopupItem {
                label: snowlark_render::controls::option_label(&dom, o),
                disabled: is_disabled(&dom, o),
                group: false,
                indented,
            });
        };
        for c in dom.element_children(select) {
            if dom.is_html_element_named(c, &local_name!("option")) {
                push(&mut items, c, false);
            } else if dom.is_html_element_named(c, &local_name!("optgroup")) {
                items.push(PopupItem {
                    label: dom.get_attribute(c, "label").unwrap_or("").to_owned(),
                    disabled: dom.get_attribute(c, "disabled").is_some(),
                    group: true,
                    indented: false,
                });
                for o in dom.element_children(c) {
                    if dom.is_html_element_named(o, &local_name!("option")) {
                        push(&mut items, o, true);
                    }
                }
            }
        }
        (items, row_of_selected)
    }

    /// Shows a drop-down select's list (a press on it, Alt+Down, F4, Space).
    pub(crate) fn open_select_popup(&self, select: NodeId) {
        {
            let dom = self.dom();
            if !dom.is_alive(select) || is_disabled(&dom, select) || !is_drop_down(&dom, select) {
                return;
            }
        }
        let Some(anchor) = self.top_viewport_rect(select) else {
            return;
        };
        let font_size = self
            .with_style(select, |r| r.resolved_property(select, "font-size"))
            .flatten()
            .and_then(|v| v.trim_end_matches("px").parse::<f32>().ok())
            .unwrap_or(13.333);
        let (items, selected) = self.popup_items(select);
        *self.select_popup.borrow_mut() = Some(SelectPopup {
            select,
            anchor,
            items,
            selected,
            font_size,
        });
    }

    /// The drop-down list the page asked to show since the last call.
    pub fn take_select_popup(&self) -> Option<SelectPopup> {
        self.select_popup.borrow_mut().take()
    }

    /// The user chose option `index` of a drop-down select's list: its selectedness, then HTML's
    /// "send select update notifications" (`input`, then `change`) when it changed.
    pub fn select_popup_choose(&self, cx: &mut Cx, select: NodeId, index: usize) {
        let options = {
            let dom = self.dom();
            if !dom.is_alive(select) || is_disabled(&dom, select) {
                return;
            }
            drop(dom);
            self.options_of(select)
        };
        let Some(&option) = options.get(index) else {
            return;
        };
        if is_disabled(&self.dom(), option) {
            return;
        }
        if self.select_selected_index(select) == index as i32 {
            return;
        }
        self.select_set_selected_index(cx, select, index as i32);
        self.control_edited(select);
        self.select_update_notifications(cx, select);
        self.microtask_checkpoint(cx);
    }

    fn select_update_notifications(&self, cx: &mut Cx, select: NodeId) {
        self.set_user_validity(select, true);
        self.fire_event(cx, node_handle(select), "input", true, false);
        self.fire_event(cx, node_handle(select), "change", true, false);
    }

    /// A primary press at `y` (from the top of the content box) in a list box: the option there is
    /// selected (alone; toggled with Ctrl, a range from the last clicked option with Shift, in a
    /// `multiple` select), with `input` and `change` when the selection changed.
    pub(crate) fn list_box_press(&self, cx: &mut Cx, select: NodeId, y: f32, mods: Modifiers) {
        if is_disabled(&self.dom(), select) {
            return;
        }
        let index = self
            .with_layout(select, |r, _| {
                r.with_box_fragment(select, |f| {
                    snowlark_layout::controls::list_box_option_at(f, y)
                })
            })
            .flatten()
            .flatten();
        let Some(index) = index else {
            return;
        };
        let options = self.options_of(select);
        let Some(&clicked) = options.get(index) else {
            return;
        };
        if is_disabled(&self.dom(), clicked) {
            return;
        }
        let multiple = self.dom().get_attribute(select, "multiple").is_some();
        let before: Vec<bool> = {
            let dom = self.dom();
            options
                .iter()
                .map(|&o| self.option_selected_in(&dom, o))
                .collect()
        };
        let mut after = vec![false; options.len()];
        let anchor = self.list_anchor.borrow().get(&select).copied();
        if multiple && mods.shift {
            let a = anchor.filter(|&a| a < options.len()).unwrap_or(index);
            let (lo, hi) = (a.min(index), a.max(index));
            let dom = self.dom();
            for (i, s) in after.iter_mut().enumerate() {
                *s = (lo..=hi).contains(&i) && !is_disabled(&dom, options[i]);
            }
        } else if multiple && mods.ctrl {
            after = before.clone();
            after[index] = !after[index];
            self.list_anchor.borrow_mut().insert(select, index);
        } else {
            after[index] = true;
            self.list_anchor.borrow_mut().insert(select, index);
        }
        if after == before {
            return;
        }
        for (&o, &s) in options.iter().zip(&after) {
            self.ctl(o, |c| c.selected = Some(s));
            self.form_state_changed(o);
        }
        self.control_edited(select);
        self.select_update_notifications(cx, select);
    }
}

impl Page {
    /// The user chose option `index` in the drop-down list of `select`.
    pub fn select_popup_choose(&mut self, select: NodeId, index: usize) {
        self.with(|e, cx| e.select_popup_choose(cx, select, index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_map_to_options() {
        let item = |group| PopupItem {
            label: String::new(),
            disabled: false,
            group,
            indented: false,
        };
        let p = SelectPopup {
            select: NodeId::from_bits(1 << 32).unwrap(),
            anchor: Rect::ZERO,
            items: vec![item(false), item(true), item(false), item(false)],
            selected: None,
            font_size: 13.0,
        };
        assert_eq!(p.option_of_row(0), Some(0));
        assert_eq!(p.option_of_row(1), None);
        assert_eq!(p.option_of_row(3), Some(2));
        assert_eq!(p.option_of_row(4), None);
    }
}
