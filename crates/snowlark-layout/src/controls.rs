// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Form controls (HTML §15.5): the content of a replaced box whose [`ReplacedContent::control`] is
//! set. A control is "replaced-ish": its border box is sized like a replaced element's, so `width`,
//! `height`, `padding`, `border` and `box-sizing` from CSS apply as to any atomic inline, with the
//! natural content size coming from the control's font ([`natural_size`]: `size` x the average
//! character width for text fields, `cols`/`rows` for textareas, the widest option for selects). Its
//! text (value, placeholder, label, options) is shaped during layout into an anonymous text layout
//! ([`ControlLayout`]) placed in the content box and baseline-aligned like an inline-block's line box;
//! painting draws that text and, for `appearance: auto`, the widget's own parts (check marks, radio
//! dots, the drop-down arrow, range track and thumb, progress and meter bars) with plain display-list
//! primitives in the Windows look (flat light-grey borders, the #0075ff accent). The box's background
//! and border are CSS: the UA sheet gives the native look, so author-styled controls look
//! author-styled.

use std::ops::Range;

use smallvec::SmallVec;
use snowlark_paint::{
    Color, DisplayListBuilder, FontQuery, Matrix, Point, Radii, Rect, ShapeOptions, ShapedText,
    TextEngine,
};

use crate::boxes::BoxTree;
use crate::inline::{line_height, tree_font_info, FontInfo};
use crate::style::*;

/// The accent color of checked boxes, radio dots, range and progress fills.
pub const ACCENT: Color = Color::rgb(0x00, 0x75, 0xff);
/// The border of unchecked boxes and radios and of tracks.
const BORDER: Color = Color::rgb(0x76, 0x76, 0x76);
const TRACK: Color = Color::rgb(0xef, 0xef, 0xef);
const TRACK_BORDER: Color = Color::rgb(0xb2, 0xb2, 0xb2);
/// A list box's selected rows (without focus).
const SELECTED_ROW: Color = Color::rgb(0xce, 0xce, 0xce);
/// The width the drop-down arrow takes at the end of a select's content box.
const ARROW_AREA: f32 = 20.0;
/// What a textarea's and an overflowing list box's width includes for a scroll bar.
const SCROLLBAR: f32 = 17.0;

/// A run of shaped text and the lines it is broken into, each with its baseline origin in the box
/// fragment's coordinates.
#[derive(Debug)]
pub struct ControlText {
    pub shaped: ShapedText,
    pub lines: Vec<(Range<usize>, Point)>,
    pub color: Color,
}

/// A control's laid-out text, in its box fragment's coordinates.
#[derive(Debug, Default)]
pub struct ControlLayout {
    pub texts: Vec<ControlText>,
    /// Text is clipped to this rectangle.
    pub clip: Rect,
    /// The first line's baseline, from the border-box top: the box's baseline in a line box.
    pub baseline: Option<f32>,
    /// A file input's button.
    pub button: Option<Rect>,
    /// A textarea's lines, however tall its box: at least the line an empty one has for its caret.
    pub text_height: f32,
}

impl PartialEq for ControlLayout {
    /// Layouts are compared by identity (a relayout makes a new one).
    fn eq(&self, other: &ControlLayout) -> bool {
        std::ptr::eq(self, other)
    }
}

fn shape(text: &TextEngine, s: &impl Style, font: &FontInfo, value: &str) -> ShapedText {
    let mut out = ShapedText::default();
    let features = s.font_features();
    let _ = text.shape(
        value,
        font.list,
        &ShapeOptions {
            size: font.size,
            locale: s.locale(),
            direction: s.direction(),
            features: &features,
            vertical: Default::default(),
            levels: None,
        },
        &mut out,
    );
    out
}

fn measure(text: &TextEngine, s: &impl Style, font: &FontInfo, value: &str) -> f32 {
    if value.is_empty() {
        return 0.0;
    }
    shape(text, s, font, value).width()
}

/// The average character width: the advance of "0" (what HTML §15.5.x's "average character width"
/// falls back to without a font's own figure).
fn char_width(text: &TextEngine, s: &impl Style, font: &FontInfo) -> f32 {
    measure(text, s, font, "0").max(1.0)
}

fn bold_font(text: &TextEngine, s: &impl Style) -> Option<FontInfo> {
    let families: SmallVec<[&str; 8]> = s.font_family().collect();
    let list = text
        .font_list(&FontQuery {
            families: &families,
            weight: 700,
            style: s.font_style(),
            stretch: s.font_stretch(),
        })
        .ok()?;
    let primary = text.primary_font(list);
    Some(FontInfo {
        list,
        primary,
        metrics: crate::inline::line_metrics(text.metrics(primary, s.font_size())),
        size: s.font_size(),
        vertical: Default::default(),
        alphabetic: 0.0,
    })
}

/// The file input's button: its label width plus padding and border.
fn file_button_width(text: &TextEngine, s: &impl Style, font: &FontInfo, label: &str) -> f32 {
    measure(text, s, font, label) + 2.0 * 7.0 + 2.0
}

/// The natural content-box size of a control (HTML §15.5.x: what a widget takes with `width` and
/// `height` auto).
pub(crate) fn natural_size<S: Style>(
    tree: &BoxTree<S>,
    text: &TextEngine,
    s: &S,
    c: &Control,
) -> (f32, f32) {
    let font = tree_font_info(tree, text, s);
    let line = line_height(s, &font);
    match &c.kind {
        // §15.5.x: `size` times the average character width.
        ControlKind::TextField { size, .. } => (*size as f32 * char_width(text, s, &font), line),
        ControlKind::DateTime { text: t } => (measure(text, s, &font, t) + 4.0 + 16.0, line),
        // §15.5.16 textarea: `cols` characters plus a scroll bar, `rows` lines.
        ControlKind::TextArea { rows, cols, .. } => (
            *cols as f32 * char_width(text, s, &font) + SCROLLBAR,
            *rows as f32 * line,
        ),
        ControlKind::Checkbox { .. } | ControlKind::Radio { .. } if c.native => (13.0, 13.0),
        ControlKind::Checkbox { .. } | ControlKind::Radio { .. } => (0.0, 0.0),
        ControlKind::Button { label } => (measure(text, s, &font, label), line),
        ControlKind::File { button, label } => (
            file_button_width(text, s, &font, button) + 4.0 + measure(text, s, &font, label),
            line,
        ),
        ControlKind::Color(_) => (44.0, 23.0),
        ControlKind::Range { .. } => (129.0, 16.0),
        ControlKind::Select { options, list_rows } => {
            let bold = bold_font(text, s);
            let widest = options
                .iter()
                .map(|o| {
                    let f = if o.group { bold.as_ref() } else { None }.unwrap_or(&font);
                    measure(text, s, f, &o.label) + if o.indented { 8.0 } else { 0.0 }
                })
                .fold(0.0f32, f32::max);
            match list_rows {
                None => (widest + 4.0 + ARROW_AREA, line),
                Some(rows) => {
                    let bar = if options.len() > *rows as usize {
                        SCROLLBAR
                    } else {
                        0.0
                    };
                    (widest + 8.0 + bar, *rows as f32 * line)
                }
            }
        }
        // §15.5.14-15: the UA sheet sizes them (10em and 5em by 1em).
        ControlKind::Progress { .. } => (10.0 * s.font_size(), s.font_size()),
        ControlKind::Meter { .. } => (5.0 * s.font_size(), s.font_size()),
    }
}

/// Greedy line breaking of shaped text into `width` (break opportunities, hard breaks; a cluster
/// that doesn't fit alone gets its own line).
fn break_lines(shaped: &ShapedText, width: f32) -> Vec<Range<usize>> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut x = 0.0;
    let mut last_break: Option<usize> = None;
    let mut i = 0;
    let n = shaped.clusters.len();
    while i < n {
        let c = &shaped.clusters[i];
        if x + c.advance > width && i > start && !c.whitespace {
            let end = last_break.filter(|&b| b > start).unwrap_or(i);
            lines.push(start..end);
            start = end;
            x = shaped.clusters_width(start..i);
            last_break = None;
            continue;
        }
        x += c.advance;
        match c.break_after {
            snowlark_paint::BreakAfter::Mandatory => {
                // (The newline itself draws nothing.)
                lines.push(start..i);
                start = i + 1;
                x = 0.0;
                last_break = None;
            }
            snowlark_paint::BreakAfter::Allowed => last_break = Some(i + 1),
            snowlark_paint::BreakAfter::Never => {}
        }
        i += 1;
    }
    if start < n {
        lines.push(start..n);
    }
    lines
}

/// Lays a control's text out in its content box (`content`, `padding` in fragment coordinates).
pub(crate) fn layout_text<S: Style>(
    tree: &BoxTree<S>,
    text: &TextEngine,
    s: &S,
    c: &Control,
    content: Rect,
    padding: Rect,
) -> ControlLayout {
    let font = tree_font_info(tree, text, s);
    let line = line_height(s, &font);
    let m = font.metrics;
    // The baseline of a line of `line` height whose top is at `top` (half-leading above).
    let baseline_at = |top: f32| top + (line - (m.ascent + m.descent)) / 2.0 + m.ascent;
    let centered = baseline_at(content.y + (content.height - line) / 2.0);
    let color = s.color();
    let mut out = ControlLayout {
        texts: Vec::new(),
        clip: Rect::new(content.x, padding.y, content.width, padding.height),
        baseline: None,
        button: None,
        text_height: 0.0,
    };
    let one_line = |out: &mut ControlLayout, value: &str, x: f32, color: Color| {
        if value.is_empty() {
            return;
        }
        let shaped = shape(text, s, &font, value);
        let n = shaped.clusters.len();
        out.texts.push(ControlText {
            shaped,
            lines: vec![(0..n, Point::new(x, centered))],
            color,
        });
    };
    match &c.kind {
        ControlKind::TextField {
            text: t,
            placeholder,
            ..
        } => {
            let color = if *placeholder {
                c.placeholder_color
            } else {
                color
            };
            let scroll = if *placeholder {
                0.0
            } else {
                c.editing.as_ref().map_or(0.0, |e| e.scroll)
            };
            one_line(&mut out, t, content.x - scroll, color);
            out.baseline = Some(centered);
        }
        ControlKind::DateTime { text: t } => {
            out.clip.width = (content.width - 20.0).max(0.0);
            one_line(&mut out, t, content.x, color);
            out.baseline = Some(centered);
        }
        ControlKind::Button { label } => {
            let w = measure(text, s, &font, label);
            let rtl = s.direction() == snowlark_paint::Direction::Rtl;
            let x = match s.text_align() {
                TextAlign::Center => content.x + (content.width - w) / 2.0,
                TextAlign::Right => content.right() - w,
                TextAlign::End if !rtl => content.right() - w,
                TextAlign::Start | TextAlign::Justify | TextAlign::MatchParent if rtl => {
                    content.right() - w
                }
                _ => content.x,
            };
            out.clip = padding;
            one_line(&mut out, label, x, color);
            out.baseline = Some(centered);
        }
        ControlKind::File { button, label } => {
            let bw = file_button_width(text, s, &font, button);
            let bx = content.x + (bw - measure(text, s, &font, button)) / 2.0;
            let top = (content.y - 1.0).max(padding.y);
            let bottom = (content.bottom() + 1.0).min(padding.bottom());
            out.button = Some(Rect::new(content.x, top, bw, bottom - top));
            one_line(&mut out, button, bx, color);
            one_line(&mut out, label, content.x + bw + 4.0, color);
            out.baseline = Some(centered);
        }
        ControlKind::TextArea {
            text: t,
            placeholder,
            ..
        } => {
            out.clip = padding;
            let color = if *placeholder {
                c.placeholder_color
            } else {
                color
            };
            let shaped = shape(text, s, &font, t);
            let lines: Vec<_> = break_lines(&shaped, content.width)
                .into_iter()
                .enumerate()
                .map(|(i, r)| {
                    (
                        r,
                        Point::new(content.x, baseline_at(content.y + i as f32 * line)),
                    )
                })
                .collect();
            // (a placeholder is not content: it is one line, as an empty field is)
            let rows = if *placeholder { 1 } else { lines.len().max(1) };
            out.text_height = rows as f32 * line;
            out.texts.push(ControlText {
                shaped,
                lines,
                color,
            });
            // A scrolling multi-line box: its baseline is the bottom margin edge (no baseline), as
            // in Chrome.
        }
        ControlKind::Select {
            options,
            list_rows: None,
        } => {
            out.clip.width = (content.width - ARROW_AREA).max(0.0);
            if let Some(o) = options.iter().find(|o| o.selected) {
                one_line(&mut out, &o.label, content.x + 4.0, color);
            }
            out.baseline = Some(centered);
        }
        ControlKind::Select {
            options,
            list_rows: Some(_),
        } => {
            out.clip = content;
            let bold = bold_font(text, s);
            for (i, o) in options.iter().enumerate() {
                let f = if o.group { bold.as_ref() } else { None }.unwrap_or(&font);
                let shaped = shape(text, s, f, &o.label);
                let n = shaped.clusters.len();
                let x = content.x + 2.0 + if o.indented { 8.0 } else { 0.0 };
                let y = baseline_at(content.y + i as f32 * line);
                let color = if o.disabled {
                    Color::rgba(color.r, color.g, color.b, color.a / 2)
                } else {
                    color
                };
                out.texts.push(ControlText {
                    shaped,
                    lines: vec![(0..n, Point::new(x, y))],
                    color,
                });
            }
        }
        _ => {}
    }
    out
}

/// The option row of a list box (`select` with `multiple` or a `size` above 1) at `y` from the top
/// of its content box (as [`crate::HitTestResult::content_point`] gives it): the index among its options (optgroup label rows are not options), or
/// `None` over a label row or below the last row.
pub fn list_box_option_at<S: Style>(f: &crate::Fragment<S>, y: f32) -> Option<usize> {
    let b = f.as_box()?;
    let crate::BoxContent::Replaced {
        content,
        clip,
        control: layout,
        ..
    } = &b.content
    else {
        return None;
    };
    let c = content.control.as_ref()?;
    let ControlKind::Select {
        options,
        list_rows: Some(_),
    } = &c.kind
    else {
        return None;
    };
    // Rows are a line apart (the distance between the first two rows' baselines).
    let row = layout
        .as_deref()
        .and_then(|l| l.texts.get(1).zip(l.texts.first()))
        .map(|(b, a)| b.lines[0].1.y - a.lines[0].1.y)
        .filter(|r| *r > 0.0)
        .unwrap_or(clip.height / options.len().max(1) as f32);
    if y < 0.0 || row <= 0.0 {
        return None;
    }
    let i = (y / row).floor() as usize;
    let o = options.get(i)?;
    if o.group {
        return None;
    }
    Some(options[..i].iter().filter(|o| !o.group).count())
}

/// The text control (text field or textarea) of a box fragment and its laid-out text.
fn text_control<S: Style>(
    f: &crate::Fragment<S>,
) -> Option<(&Control, Option<&ControlLayout>, Rect)> {
    let b = f.as_box()?;
    let crate::BoxContent::Replaced {
        content,
        clip,
        control: layout,
        ..
    } = &b.content
    else {
        return None;
    };
    let c = content.control.as_deref()?;
    matches!(
        c.kind,
        ControlKind::TextField { .. } | ControlKind::TextArea { .. }
    )
    .then_some((c, layout.as_deref(), *clip))
}

/// The height of what a textarea scrolls: its lines and its padding (CSSOM View's scrolling area, for
/// `scrollHeight`). Its box may be any height, and the text is not in the fragment tree.
pub fn textarea_scroll_height<S: Style>(f: &crate::Fragment<S>) -> Option<f32> {
    let (c, layout, _) = text_control(f)?;
    if !matches!(c.kind, ControlKind::TextArea { .. }) {
        return None;
    }
    let padding = f.as_box()?.padding;
    Some(layout?.text_height + padding.top + padding.bottom)
}

/// The bytes of a line of `t` (its clusters `range`).
fn line_bytes(t: &ControlText, range: &Range<usize>) -> (usize, usize) {
    let clusters = &t.shaped.clusters;
    let lo = clusters.get(range.start).map_or_else(
        || clusters.last().map_or(0, |c| c.text.1 as usize),
        |c| c.text.0 as usize,
    );
    let hi = if range.is_empty() {
        lo
    } else {
        clusters[range.end - 1].text.1 as usize
    };
    (lo, hi)
}

/// The caret position (a byte offset into the displayed text) nearest to `p`, a point from the top
/// left of a text field's or textarea's content box: the line under it (the first or last beyond
/// the text), then the cluster boundary nearest along it. 0 for an empty field or one showing its
/// placeholder.
pub fn control_text_offset_at<S: Style>(f: &crate::Fragment<S>, p: Point) -> Option<usize> {
    let (c, layout, clip) = text_control(f)?;
    let placeholder = matches!(
        c.kind,
        ControlKind::TextField {
            placeholder: true,
            ..
        } | ControlKind::TextArea {
            placeholder: true,
            ..
        }
    );
    let Some(t) = layout
        .and_then(|l| l.texts.first())
        .filter(|_| !placeholder)
    else {
        return Some(0);
    };
    let (x, y) = (p.x + clip.x, p.y + clip.y);
    // The line: the last one whose top (three quarters of a line pitch above its baseline) is above
    // the point.
    let pitch = match (t.lines.first(), t.lines.get(1)) {
        (Some(a), Some(b)) => b.1.y - a.1.y,
        _ => f.size.height,
    };
    let mut line = 0;
    for (i, (_, origin)) in t.lines.iter().enumerate() {
        if origin.y - pitch * 0.75 <= y {
            line = i;
        }
    }
    let Some((range, origin)) = t.lines.get(line) else {
        return Some(0);
    };
    let (lo, hi) = line_bytes(t, range);
    let mut cx = origin.x;
    for c in &t.shaped.clusters[range.clone()] {
        if x < cx + c.advance / 2.0 {
            return Some((c.text.0 as usize).max(lo));
        }
        cx += c.advance;
    }
    Some(hi)
}

/// For a one-line text field: the x of byte offset `byte` from the start of its text (unscrolled),
/// the text's width and the content box's width, to keep the caret in view.
pub fn control_caret_metrics<S: Style>(
    f: &crate::Fragment<S>,
    byte: usize,
) -> Option<(f32, f32, f32)> {
    let (c, layout, clip) = text_control(f)?;
    if !matches!(c.kind, ControlKind::TextField { .. }) {
        return None;
    }
    let Some(t) = layout.and_then(|l| l.texts.first()) else {
        return Some((0.0, 0.0, clip.width));
    };
    let mut x = 0.0;
    for cl in &t.shaped.clusters {
        if cl.text.0 as usize >= byte {
            break;
        }
        x += cl.advance;
    }
    Some((x, t.shaped.width(), clip.width))
}

fn dim(c: Color, disabled: bool) -> Color {
    if disabled {
        Color::rgba(c.r, c.g, c.b, (c.a as u32 * 2 / 5) as u8)
    } else {
        c
    }
}

/// A straight stroke from `a` to `b` with square caps half a width beyond each end.
fn stroke(out: &mut DisplayListBuilder, a: Point, b: Point, width: f32, color: Color) {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len = dx.hypot(dy);
    if len <= 0.0 {
        return;
    }
    let angle = dy.atan2(dx);
    out.push_transform(Matrix::rotation(angle).then(&Matrix::translation(a.x, a.y)));
    out.fill_rect(
        Rect::new(-width / 2.0, -width / 2.0, len + width, width),
        color,
    );
    out.pop_transform();
}

fn circle(out: &mut DisplayListBuilder, r: Rect, color: Color) {
    let radius = r.width.min(r.height) / 2.0;
    out.fill_rounded_rect(r, Radii::uniform(radius), color);
}

/// The largest square centered in `r`.
fn square(r: Rect) -> Rect {
    let side = r.width.min(r.height);
    Rect::new(
        r.x + (r.width - side) / 2.0,
        r.y + (r.height - side) / 2.0,
        side,
        side,
    )
}

/// Paints a control's native parts and text. `content` is the box's content box, already translated
/// to where the fragment is painted (`at`).
pub(crate) fn paint<S: Style>(
    out: &mut DisplayListBuilder,
    s: &S,
    c: &Control,
    layout: Option<&ControlLayout>,
    content: Rect,
    at: Point,
) {
    let off = c.disabled;
    let text_color = s.color();
    if c.native {
        match &c.kind {
            ControlKind::Checkbox {
                checked,
                indeterminate,
            } => {
                let b = square(content);
                let k = b.width / 13.0;
                let radii = Radii::uniform(2.0 * k);
                if *checked || *indeterminate {
                    out.fill_rounded_rect(b, radii, dim(ACCENT, off));
                    if *indeterminate {
                        out.fill_rect(
                            Rect::new(b.x + 3.0 * k, b.y + 5.5 * k, 7.0 * k, 2.0 * k),
                            Color::WHITE,
                        );
                    } else {
                        let p = |x: f32, y: f32| Point::new(b.x + x * k, b.y + y * k);
                        let w = 1.8 * k;
                        stroke(out, p(3.2, 6.6), p(5.4, 8.8), w, Color::WHITE);
                        stroke(out, p(5.4, 8.8), p(9.9, 4.3), w, Color::WHITE);
                    }
                } else {
                    out.fill_rounded_rect(b, radii, dim(BORDER, off));
                    let inner = b.inflate(-k, -k);
                    out.fill_rounded_rect(inner, Radii::uniform(1.0 * k), Color::WHITE);
                }
            }
            ControlKind::Radio { checked } => {
                let b = square(content);
                let k = b.width / 13.0;
                if *checked {
                    circle(out, b, dim(ACCENT, off));
                    circle(out, b.inflate(-k, -k), Color::WHITE);
                    circle(out, b.inflate(-3.0 * k, -3.0 * k), dim(ACCENT, off));
                } else {
                    circle(out, b, dim(BORDER, off));
                    circle(out, b.inflate(-k, -k), Color::WHITE);
                }
            }
            ControlKind::Range { fraction } => {
                let thumb = content.height.clamp(0.0, 16.0);
                let cy = content.y + content.height / 2.0;
                let track = Rect::new(content.x, cy - 2.0, content.width, 4.0);
                let x = content.x + thumb / 2.0 + (content.width - thumb).max(0.0) * fraction;
                let radii = Radii::uniform(2.0);
                out.fill_rounded_rect(track, radii, dim(TRACK_BORDER, off));
                out.fill_rounded_rect(track.inflate(-1.0, -1.0), Radii::uniform(1.0), TRACK);
                out.fill_rounded_rect(
                    Rect::new(track.x, track.y, x - track.x, track.height),
                    radii,
                    dim(ACCENT, off),
                );
                circle(
                    out,
                    Rect::new(x - thumb / 2.0, cy - thumb / 2.0, thumb, thumb),
                    dim(ACCENT, off),
                );
            }
            ControlKind::Color(color) => {
                let swatch = content.inflate(-2.0, -4.0);
                out.fill_rect(swatch, dim(Color::rgb(0x77, 0x77, 0x77), off));
                out.fill_rect(swatch.inflate(-1.0, -1.0), dim(*color, off));
            }
            ControlKind::Select {
                list_rows: None, ..
            } => {
                // The drop-down arrow: a chevron at the end of the content box.
                let cx = content.right() - ARROW_AREA / 2.0;
                let cy = content.y + content.height / 2.0;
                let color = text_color;
                stroke(
                    out,
                    Point::new(cx - 3.5, cy - 1.75),
                    Point::new(cx, cy + 1.75),
                    1.4,
                    color,
                );
                stroke(
                    out,
                    Point::new(cx, cy + 1.75),
                    Point::new(cx + 3.5, cy - 1.75),
                    1.4,
                    color,
                );
            }
            ControlKind::Select {
                options,
                list_rows: Some(_),
            } => {
                let font_line = layout
                    .and_then(|l| l.texts.get(1).zip(l.texts.first()))
                    .map(|(b, a)| b.lines[0].1.y - a.lines[0].1.y);
                if let Some(row) = font_line.or(Some(content.height / options.len().max(1) as f32))
                {
                    for (i, o) in options.iter().enumerate() {
                        if o.selected {
                            out.fill_rect(
                                Rect::new(
                                    content.x,
                                    content.y + i as f32 * row,
                                    content.width,
                                    row,
                                ),
                                SELECTED_ROW,
                            );
                        }
                    }
                }
            }
            ControlKind::Progress { fraction } => {
                let radii = Radii::uniform((content.height / 2.0).min(4.0));
                out.fill_rounded_rect(content, radii, dim(TRACK_BORDER, off));
                out.fill_rounded_rect(content.inflate(-1.0, -1.0), radii, TRACK);
                let (from, to) = match fraction {
                    Some(f) => (0.0, *f),
                    // Indeterminate: a segment at the start.
                    None => (0.0, 0.25),
                };
                if to > from {
                    out.fill_rounded_rect(
                        Rect::new(
                            content.x + content.width * from,
                            content.y,
                            content.width * (to - from),
                            content.height,
                        ),
                        radii,
                        dim(ACCENT, off),
                    );
                }
            }
            ControlKind::Meter { fraction, level } => {
                let radii = Radii::uniform((content.height / 2.0).min(4.0));
                out.fill_rounded_rect(content, radii, dim(TRACK_BORDER, off));
                out.fill_rounded_rect(content.inflate(-1.0, -1.0), radii, TRACK);
                let color = match level {
                    MeterLevel::Optimum => Color::rgb(0x10, 0x7c, 0x10),
                    MeterLevel::Suboptimum => Color::rgb(0xff, 0xb9, 0x00),
                    MeterLevel::EvenLessGood => Color::rgb(0xd1, 0x34, 0x38),
                };
                if *fraction > 0.0 {
                    out.fill_rounded_rect(
                        Rect::new(
                            content.x,
                            content.y,
                            content.width * fraction,
                            content.height,
                        ),
                        radii,
                        dim(color, off),
                    );
                }
            }
            ControlKind::DateTime { .. } => {
                // A calendar icon at the end.
                let icon = Rect::new(
                    content.right() - 14.0,
                    content.y + (content.height - 12.0) / 2.0,
                    12.0,
                    12.0,
                );
                let color = dim(text_color, off);
                out.fill_rect(Rect::new(icon.x, icon.y + 1.0, icon.width, 3.0), color);
                out.stroke_rect(icon.inflate(-0.5, -0.5).translate(0.0, 0.5), 1.0, color);
                for (cx, cy) in [(3.0, 6.0), (6.0, 6.0), (9.0, 6.0), (3.0, 9.0), (6.0, 9.0)] {
                    out.fill_rect(Rect::new(icon.x + cx - 1.0, icon.y + cy, 2.0, 2.0), color);
                }
            }
            _ => {}
        }
    }
    // A file input's button (painted for any appearance, as `::file-selector-button` is styled).
    if let Some(b) = layout.and_then(|l| l.button) {
        let b = b.translate(at.x, at.y);
        out.fill_rounded_rect(b, Radii::uniform(2.0), dim(BORDER, off));
        out.fill_rounded_rect(b.inflate(-1.0, -1.0), Radii::uniform(1.0), dim(TRACK, off));
    }
    let Some(layout) = layout else {
        return;
    };
    let empty = matches!(
        c.kind,
        ControlKind::TextField {
            placeholder: true,
            ..
        } | ControlKind::TextArea {
            placeholder: true,
            ..
        }
    ) || layout.texts.is_empty();
    if let (true, Some(_)) = (empty, &c.editing) {
        if matches!(
            c.kind,
            ControlKind::TextField { .. } | ControlKind::TextArea { .. }
        ) {
            let size = s.font_size();
            let baseline = layout
                .baseline
                .map(|b| b + at.y)
                .unwrap_or(content.y + size * 1.05);
            out.fill_rect(
                Rect::new(content.x, baseline - size * 0.95, 1.0, size * 1.25),
                text_color,
            );
        }
    }
    if layout.texts.is_empty() {
        return;
    }
    out.push_clip_rect(layout.clip.translate(at.x, at.y));
    let editing = c.editing.as_ref().filter(|_| {
        matches!(
            c.kind,
            ControlKind::TextField {
                placeholder: false,
                ..
            } | ControlKind::TextArea {
                placeholder: false,
                ..
            }
        )
    });
    if let (Some(e), Some(t)) = (editing, layout.texts.first()) {
        paint_selection(out, s, t, e, at);
    }
    for t in &layout.texts {
        for (range, origin) in &t.lines {
            t.shaped.paint(
                range.clone(),
                Point::new(origin.x + at.x, origin.y + at.y),
                t.color,
                out,
            );
        }
    }
    out.pop_clip();
}

/// The x of byte offset `byte` on a line (clusters `range` of `t`, starting at `origin`).
fn x_at(t: &ControlText, range: &Range<usize>, origin: Point, byte: usize) -> f32 {
    let mut x = origin.x;
    for c in &t.shaped.clusters[range.clone()] {
        if c.text.0 as usize >= byte {
            break;
        }
        x += c.advance;
    }
    x
}

/// A focused text field's selection highlight and caret, under and over its text.
fn paint_selection<S: Style>(
    out: &mut DisplayListBuilder,
    s: &S,
    t: &ControlText,
    e: &TextEditing,
    at: Point,
) {
    let size = s.font_size();
    let (above, below) = (size * 0.95, size * 0.3);
    let nlines = t.lines.len();
    for (i, (range, origin)) in t.lines.iter().enumerate() {
        let (lo, hi) = match (
            t.shaped.clusters.get(range.start),
            t.shaped.clusters[..range.end].last(),
        ) {
            (Some(a), Some(b)) => (a.text.0 as usize, b.text.1 as usize),
            _ => continue,
        };
        // The composition's underline on this line.
        if let Some(c) = &e.composition {
            let (a, b) = (c.start.max(lo), c.end.min(hi));
            if a < b {
                let x0 = x_at(t, range, *origin, a);
                let x1 = x_at(t, range, *origin, b);
                let thickness = (size / 14.0).max(1.0);
                out.fill_rect(
                    Rect::new(x0 + at.x, origin.y + below * 0.5 + at.y, x1 - x0, thickness),
                    s.color(),
                );
            }
        }
        // The selection part on this line.
        let (a, b) = (e.selection.start.max(lo), e.selection.end.min(hi));
        if a < b {
            let x0 = x_at(t, range, *origin, a);
            let x1 = x_at(t, range, *origin, b);
            out.fill_rect(
                Rect::new(x0 + at.x, origin.y - above + at.y, x1 - x0, above + below),
                Color::rgba(0x00, 0x78, 0xd7, 90),
            );
        }
        // The caret, on the line it falls in (the end of the last line included).
        if e.selection.is_empty()
            && e.caret >= lo
            && (e.caret < hi || i + 1 == nlines || e.caret == hi && t.lines.get(i + 1).is_none())
        {
            let x = x_at(t, range, *origin, e.caret);
            out.fill_rect(
                Rect::new(x + at.x, origin.y - above + at.y, 1.0, above + below),
                s.color(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_paint::{BreakAfter, Cluster};

    #[test]
    fn breaks_at_opportunities() {
        let mut shaped = ShapedText::default();
        // "ab cd\nef": 10 px per cluster; break opportunities after the space, a newline.
        for (i, ch) in "ab cd\nef".char_indices() {
            shaped.clusters.push(Cluster {
                text: (i as u32, i as u32 + 1),
                glyphs: (i as u32, i as u32 + 1),
                advance: 10.0,
                break_after: match ch {
                    ' ' => BreakAfter::Allowed,
                    '\n' => BreakAfter::Mandatory,
                    _ => BreakAfter::Never,
                },
                whitespace: ch == ' ' || ch == '\n',
                soft_hyphen: false,
            });
        }
        assert_eq!(break_lines(&shaped, 100.0), vec![0..5, 6..8]);
        assert_eq!(break_lines(&shaped, 35.0), vec![0..3, 3..5, 6..8]);
        // Narrower than one word: the word breaks between clusters.
        assert_eq!(
            break_lines(&shaped, 15.0),
            vec![0..1, 1..3, 3..4, 4..5, 6..7, 7..8]
        );
    }
}
