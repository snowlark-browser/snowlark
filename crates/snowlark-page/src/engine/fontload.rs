// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Font Loading 3: `FontFace` and `document.fonts` (`FontFaceSet`) over the renderer's faces.
//!
//! A `FontFace` a script makes is registered with its document's renderer once it is added to
//! `document.fonts` or loaded, and takes part in font matching while in the set. The faces the
//! document's `@font-face` rules declare are the set's CSS-connected members (a wrapper object is made
//! for each when the set is read). The renderer fetches the fonts; the page polls their state in its
//! loop and settles the promises (`FontFace.loaded`, `FontFaceSet.load()`).

use snowlark_bindings::generated::{
    self as g, FontFaceImpl, FontFaceLoadStatus, FontFaceSetImpl, FontFaceSetLoadStatus,
};
use snowlark_bindings::rt::{self, Cx, DomStr, ExceptionName, Handle, JsObject, JsPromise};
use snowlark_bindings::InterfaceId;
use snowlark_dom::NodeId;
use snowlark_render::{FaceInfo, FaceState};

use super::cssom::FontFaceData;
use super::{exception, Engine, Object};

/// A `FontFaceSet.load()` waiting for its faces.
pub struct LoadWaiter {
    pub promise: JsPromise,
    pub faces: Vec<Handle>,
}

type BufferOrString =
    g::StringOrArrayBufferOrInt8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataView;

fn buffer_of(source: &BufferOrString) -> Option<&JsObject> {
    use BufferOrString as B;
    match source {
        B::String(_) => None,
        B::ArrayBuffer(o)
        | B::Int8Array(o)
        | B::Int16Array(o)
        | B::Int32Array(o)
        | B::Uint8Array(o)
        | B::Uint16Array(o)
        | B::Uint32Array(o)
        | B::Uint8ClampedArray(o)
        | B::BigInt64Array(o)
        | B::BigUint64Array(o)
        | B::Float16Array(o)
        | B::Float32Array(o)
        | B::Float64Array(o)
        | B::DataView(o) => Some(o),
    }
}

/// What a CSS `font` shorthand value (`"bold condensed 16px Foo, serif"`) asks of a font face set.
#[derive(Debug, PartialEq)]
pub(crate) struct FontQuery {
    pub families: Vec<String>,
    pub weight: u16,
    /// `normal`, `italic`, `oblique` or `oblique <angle>`.
    pub style: String,
    /// The `font-width` in percent.
    pub width: f32,
}

/// The `<font-width-css3>` keywords of the `font` shorthand, in percent (CSS Fonts 4 §2.3).
fn width_keyword(keyword: &str) -> Option<f32> {
    Some(match keyword {
        "ultra-condensed" => 50.0,
        "extra-condensed" => 62.5,
        "condensed" => 75.0,
        "semi-condensed" => 87.5,
        "semi-expanded" => 112.5,
        "expanded" => 125.0,
        "extra-expanded" => 150.0,
        "ultra-expanded" => 200.0,
        _ => return None,
    })
}

/// An `<angle>` token in degrees.
fn angle_degrees(token: &str) -> Option<f32> {
    let split = token.find(|c: char| c.is_ascii_alphabetic())?;
    let (number, unit) = token.split_at(split);
    let v: f32 = number.parse().ok()?;
    Some(match unit {
        "deg" => v,
        "grad" => v * 0.9,
        "rad" => v.to_degrees(),
        "turn" => v * 360.0,
        _ => return None,
    })
}

/// The font a CSS `font` shorthand value asks for (CSS Font Loading 3 "find the matching font
/// faces": the shorthand's family list, `font-weight`, `font-style` and `font-width`); `None` when
/// it is not one.
pub(crate) fn parse_font_query(font: &str) -> Option<FontQuery> {
    let font = font.trim();
    if font.is_empty() {
        return None;
    }
    // Tokens up to the size (with an optional `/line-height`), then the family list.
    let mut weight = 400u16;
    let mut style = String::from("normal");
    let mut width = 100.0f32;
    let mut rest = font;
    loop {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let tok = &rest[..end];
        if tok.is_empty() {
            return None;
        }
        let lower = tok.to_ascii_lowercase();
        let size_part = lower.split('/').next().unwrap_or("");
        let is_size = size_part
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() || c == '.' || c == '+')
            && size_part
                .chars()
                .any(|c| c.is_ascii_alphabetic() || c == '%')
            || size_part.starts_with("calc(")
            || matches!(
                size_part,
                "xx-small"
                    | "x-small"
                    | "small"
                    | "medium"
                    | "large"
                    | "x-large"
                    | "xx-large"
                    | "xxx-large"
                    | "larger"
                    | "smaller"
            );
        if is_size {
            let after = rest[end..].trim_start();
            let after = after.strip_prefix('/').map_or(after, |a| {
                // `16px / 1.5 Foo`: the line height is a token of its own.
                let a = a.trim_start();
                a.find(char::is_whitespace).map_or("", |i| &a[i..])
            });
            // (`16px/1.5 Foo`: the slash was part of the token)
            let families = after.trim();
            if families.is_empty() {
                return None;
            }
            let families = split_families(families)?;
            return Some(FontQuery {
                families,
                weight,
                style,
                width,
            });
        }
        if let Some(w) = width_keyword(&lower) {
            width = w;
            rest = &rest[end..];
            continue;
        }
        match lower.as_str() {
            "normal" | "small-caps" => {}
            "italic" => style = lower.clone(),
            "oblique" => {
                style = lower.clone();
                // `oblique <angle [-90deg,90deg]>?`
                let after = rest[end..].trim_start();
                let next_end = after.find(char::is_whitespace).unwrap_or(after.len());
                let next = after[..next_end].to_ascii_lowercase();
                if let Some(a) = angle_degrees(&next) {
                    if !(-90.0..=90.0).contains(&a) {
                        return None;
                    }
                    style = format!("oblique {a}deg");
                    rest = &after[next_end..];
                    continue;
                }
            }
            "bold" => weight = 700,
            "bolder" => weight = 700,
            "lighter" => weight = 100,
            n => match n.parse::<f32>() {
                Ok(w) if (1.0..=1000.0).contains(&w) => weight = w as u16,
                _ => return None,
            },
        }
        rest = &rest[end..];
    }
}

fn split_families(list: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut had_quote = false;
    for c in list.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                had_quote = true;
            }
            (None, ',') => {
                let name = cur.trim().to_owned();
                if name.is_empty() && !had_quote {
                    return None;
                }
                out.push(name);
                cur.clear();
                had_quote = false;
            }
            (None, c) => cur.push(c),
        }
    }
    let name = cur.trim().to_owned();
    if (name.is_empty() && !had_quote) || quote.is_some() {
        return None;
    }
    // CSS-wide keywords are not family names here.
    if out.is_empty()
        && matches!(
            name.to_ascii_lowercase().as_str(),
            "inherit" | "initial" | "unset" | "revert" | "revert-layer"
        )
    {
        return None;
    }
    out.push(name);
    Some(out)
}

fn syntax_error() -> rt::Error {
    exception(
        ExceptionName::SyntaxError,
        "The value is not valid for the descriptor",
    )
}

impl Engine {
    fn font_set_parts(&self, set: Handle) -> NodeId {
        match &*self.obj(set) {
            Object::FontFaceSet { document, .. } => *document,
            _ => panic!("not a FontFaceSet"),
        }
    }

    fn font_face_doc(&self, face: Handle) -> Option<NodeId> {
        match &*self.obj(face) {
            Object::FontFace(d) => d.doc,
            _ => None,
        }
    }

    fn face_data<R>(&self, h: Handle, f: impl FnOnce(&mut FontFaceData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::FontFace(d) => f(d),
            _ => panic!("not a FontFace"),
        }
    }

    fn doc_base(&self, doc: Option<NodeId>) -> url::Url {
        doc.map(|d| self.dom().document_url(d).to_owned())
            .and_then(|u| url::Url::parse(u.as_str()).ok())
            .unwrap_or_else(|| url::Url::parse("about:blank").expect("a URL"))
    }

    /// The descriptors of `face` as an `@font-face` rule's declarations.
    fn face_declarations(d: &FontFaceData) -> String {
        let mut s = format!("font-family: {}; ", d.family);
        if !d.src.is_empty() {
            s.push_str(&format!("src: {}; ", d.src));
        } else {
            s.push_str("src: local(a); ");
        }
        for (name, value) in [
            ("font-style", &d.style),
            ("font-weight", &d.weight),
            ("font-width", &d.stretch),
            ("unicode-range", &d.unicode_range),
            ("font-feature-settings", &d.feature_settings),
            ("font-variation-settings", &d.variation_settings),
            ("font-display", &d.display),
            ("ascent-override", &d.ascent_override),
            ("descent-override", &d.descent_override),
            ("line-gap-override", &d.line_gap_override),
        ] {
            s.push_str(&format!("{name}: {value}; "));
        }
        s
    }

    /// The renderer's id of a script's face, registering it first. `None` without a renderer.
    fn ensure_renderer_face(&self, face: Handle) -> Option<(NodeId, usize)> {
        let (doc, existing) = {
            let doc = self.font_face_doc(face)?;
            (doc, self.face_data(face, |d| d.rid))
        };
        if let Some(rid) = existing {
            return Some((doc, rid));
        }
        let (decls, bytes, src_empty) = self.face_data(face, |d| {
            (
                Self::face_declarations(d),
                d.bytes.clone(),
                d.src.is_empty(),
            )
        });
        let base = self.doc_base(Some(doc));
        let mut parsed = snowlark_style::parse_font_face(&decls, &base)?;
        if src_empty {
            parsed.sources.clear();
        }
        let rid = self
            .with_doc_renderer(doc, |r, _| r.add_script_face(parsed, bytes.as_deref()))
            .flatten()?;
        self.face_data(face, |d| {
            d.rid = Some(rid);
            d.doc = Some(doc);
        });
        Some((doc, rid))
    }

    /// The load state of a face from the renderer, when it has a renderer face.
    fn face_load_status(&self, face: Handle) -> FontFaceLoadStatus {
        let (doc, rid, status) = self.face_data(face, |d| (d.doc, d.rid, d.status));
        let (Some(doc), Some(rid)) = (doc, rid) else {
            return status;
        };
        match self
            .with_doc_renderer(doc, |r, _| r.face_state(rid))
            .unwrap_or(FaceState::Error)
        {
            FaceState::Loaded => FontFaceLoadStatus::Loaded,
            FaceState::Error => FontFaceLoadStatus::Error,
            FaceState::Loading => FontFaceLoadStatus::Loading,
            // (a script's face asked to load that has not begun: loading)
            FaceState::Unloaded if status == FontFaceLoadStatus::Loading => {
                FontFaceLoadStatus::Loading
            }
            FaceState::Unloaded => FontFaceLoadStatus::Unloaded,
        }
    }

    /// The set's members in order: the faces its document's style sheets declare, then those a
    /// script added.
    fn font_set_members(&self, set: Handle) -> Vec<Handle> {
        let doc = self.font_set_parts(set);
        let infos: Option<Vec<FaceInfo>> = self.with_doc_renderer(doc, |r, _| r.faces());
        let Some(infos) = infos else {
            return match &*self.obj(set) {
                Object::FontFaceSet { script, .. } => script.clone(),
                _ => Vec::new(),
            };
        };
        let mut members = Vec::new();
        let mut css_next: Vec<(usize, Handle)> = Vec::new();
        for info in infos {
            if info.script {
                let scripts = match &*self.obj(set) {
                    Object::FontFaceSet { script, .. } => script.clone(),
                    _ => Vec::new(),
                };
                let found = scripts
                    .into_iter()
                    .find(|&h| self.face_data(h, |d| d.rid) == Some(info.id));
                members.extend(found);
                continue;
            }
            let known = match &*self.obj(set) {
                Object::FontFaceSet { css, .. } => css.iter().find(|(i, _)| *i == info.id).copied(),
                _ => None,
            };
            let h = match known {
                Some((_, h)) => h,
                None => self.alloc(Object::FontFace(Box::new(FontFaceData {
                    doc: Some(doc),
                    family: info.family.clone(),
                    style: or_default(&info.style, "normal"),
                    weight: or_default(&info.weight, "normal"),
                    stretch: or_default(&info.stretch, "normal"),
                    unicode_range: or_default(&info.unicode_range, "U+0-10FFFF"),
                    feature_settings: "normal".into(),
                    variation_settings: "normal".into(),
                    display: "auto".into(),
                    ascent_override: "normal".into(),
                    descent_override: "normal".into(),
                    line_gap_override: "normal".into(),
                    src: String::new(),
                    bytes: None,
                    status: FontFaceLoadStatus::Unloaded,
                    rid: Some(info.id),
                    css: true,
                    promise: None,
                    error: None,
                }))),
            };
            css_next.push((info.id, h));
            members.push(h);
        }
        if let Object::FontFaceSet { css, .. } = &mut *self.obj_mut(set) {
            *css = css_next;
        }
        // The CSS-connected ones come first (the renderer lists them so), then the script's.
        members
    }

    /// Settles the promises of faces and `FontFaceSet.load()`s whose fonts have arrived or failed.
    pub(crate) fn update_font_waiters(&self, cx: &mut Cx) {
        // FontFace.loaded / load().
        let pending: Vec<Handle> = self.cssom.borrow().face_promises.clone();
        let mut still = Vec::new();
        for face in pending {
            let status = self.face_load_status(face);
            match status {
                FontFaceLoadStatus::Loaded => {
                    self.face_data(face, |d| d.status = status);
                    if let Some(p) = self.face_data(face, |d| d.promise.take()) {
                        p.resolve(cx, face);
                    }
                }
                FontFaceLoadStatus::Error => {
                    self.face_data(face, |d| d.status = status);
                    if let Some(p) = self.face_data(face, |d| d.promise.take()) {
                        p.reject(
                            cx,
                            exception(ExceptionName::NetworkError, "The font failed to load"),
                        );
                    }
                }
                _ => still.push(face),
            }
        }
        self.cssom.borrow_mut().face_promises = still;
        // FontFaceSet.load().
        let waiters = std::mem::take(&mut self.cssom.borrow_mut().load_waiters);
        let mut keep = Vec::new();
        for w in waiters {
            let states: Vec<FontFaceLoadStatus> =
                w.faces.iter().map(|&f| self.face_load_status(f)).collect();
            if states.contains(&FontFaceLoadStatus::Error) {
                w.promise.reject(
                    cx,
                    exception(ExceptionName::NetworkError, "A font failed to load"),
                );
            } else if states.iter().all(|s| *s == FontFaceLoadStatus::Loaded) {
                let faces = w.faces.clone();
                w.promise.resolve(cx, faces);
            } else {
                keep.push(w);
            }
        }
        self.cssom.borrow_mut().load_waiters.extend(keep);
    }

    pub(crate) fn font_waiters_pending(&self) -> bool {
        let c = self.cssom.borrow();
        !c.face_promises.is_empty() || !c.load_waiters.is_empty()
    }

    fn start_face_load(&self, face: Handle) {
        if self.face_data(face, |d| d.css) {
            return;
        }
        let Some((doc, rid)) = self.ensure_renderer_face(face) else {
            self.face_data(face, |d| d.status = FontFaceLoadStatus::Error);
            return;
        };
        self.face_data(face, |d| {
            if d.status == FontFaceLoadStatus::Unloaded {
                d.status = FontFaceLoadStatus::Loading;
            }
        });
        self.with_doc_renderer(doc, |r, _| r.load_script_face(rid));
    }

    /// The faces `font` and `text` select among the set's members, or `None` when `font` is not a
    /// font shorthand.
    fn matching_members(&self, set: Handle, font: &str, text: &str) -> Option<Vec<Handle>> {
        let query = parse_font_query(font)?;
        let doc = self.font_set_parts(set);
        let members = self.font_set_members(set);
        let ids = self
            .with_doc_renderer(doc, |r, _| {
                r.matching_faces(
                    &query.families,
                    query.weight,
                    &query.style,
                    query.width,
                    text,
                )
            })
            .unwrap_or_default();
        Some(
            members
                .into_iter()
                .filter(|&h| {
                    self.face_data(h, |d| d.rid)
                        .is_some_and(|r| ids.contains(&r))
                })
                .collect(),
        )
    }

    fn face_descriptor_set(
        &self,
        this: Handle,
        css_name: &str,
        value: &str,
        store: impl FnOnce(&mut FontFaceData, String),
    ) -> rt::Result<()> {
        if self.face_data(this, |d| d.css) {
            return Err(exception(
                ExceptionName::NoModificationAllowedError,
                "A CSS-connected FontFace's descriptors cannot be changed",
            ));
        }
        let base = self.doc_base(self.font_face_doc(this));
        let Some(canonical) = snowlark_style::font_face_descriptor(css_name, value, &base) else {
            return Err(syntax_error());
        };
        self.face_data(this, |d| store(d, canonical));
        // The renderer's copy of the descriptors follows.
        if let Some((doc, rid)) = self.face_data(this, |d| d.doc.zip(d.rid)) {
            let decls = self.face_data(this, |d| Self::face_declarations(d));
            if let Some(mut parsed) = snowlark_style::parse_font_face(&decls, &base) {
                parsed.sources.clear();
                self.with_doc_renderer(doc, |r, _| r.set_script_face_descriptors(rid, parsed));
            }
        }
        Ok(())
    }
}

fn or_default(v: &str, default: &str) -> String {
    if v.is_empty() {
        default.to_owned()
    } else {
        v.to_owned()
    }
}

macro_rules! descriptor {
    ($get:ident, $set:ident, $field:ident, $css:literal) => {
        fn $get(&self, this: Handle) -> DomStr<'_> {
            DomStr::Owned(self.face_data(this, |f| f.$field.clone()))
        }
        fn $set(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
            self.face_descriptor_set(this, $css, value, |d, v| d.$field = v)
        }
    };
}

impl FontFaceImpl for Engine {
    descriptor!(family, set_family, family, "font-family");
    descriptor!(style, set_style, style, "font-style");
    descriptor!(weight, set_weight, weight, "font-weight");
    descriptor!(stretch, set_stretch, stretch, "font-width");
    descriptor!(width, set_width, stretch, "font-width");
    descriptor!(
        unicode_range,
        set_unicode_range,
        unicode_range,
        "unicode-range"
    );
    descriptor!(
        feature_settings,
        set_feature_settings,
        feature_settings,
        "font-feature-settings"
    );
    descriptor!(
        variation_settings,
        set_variation_settings,
        variation_settings,
        "font-variation-settings"
    );
    descriptor!(display, set_display, display, "font-display");
    descriptor!(
        ascent_override,
        set_ascent_override,
        ascent_override,
        "ascent-override"
    );
    descriptor!(
        descent_override,
        set_descent_override,
        descent_override,
        "descent-override"
    );
    descriptor!(
        line_gap_override,
        set_line_gap_override,
        line_gap_override,
        "line-gap-override"
    );
    fn status(&self, this: Handle) -> FontFaceLoadStatus {
        self.face_load_status(this)
    }
    fn loaded(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let status = self.face_load_status(this);
        if let Some(p) = self.face_data(this, |d| d.promise.clone()) {
            if status != FontFaceLoadStatus::Loaded && status != FontFaceLoadStatus::Error {
                return Ok(p);
            }
        }
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        match status {
            FontFaceLoadStatus::Loaded => p.resolve(cx, this),
            FontFaceLoadStatus::Error => {
                let error = self.face_data(this, |d| d.error);
                p.reject(
                    cx,
                    match error {
                        Some((name, msg)) => exception(name, msg),
                        None => exception(ExceptionName::NetworkError, "The font failed to load"),
                    },
                );
            }
            _ => {
                self.face_data(this, |d| d.promise = Some(p.clone()));
                self.cssom.borrow_mut().face_promises.push(this);
            }
        }
        Ok(p)
    }
    fn load(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        if self.face_load_status(this) == FontFaceLoadStatus::Unloaded
            && self.face_data(this, |d| d.error.is_none())
        {
            self.start_face_load(this);
        }
        FontFaceImpl::loaded(self, cx, this)
    }
    fn constructor(
        &self,
        cx: &mut Cx,
        family: &str,
        source: BufferOrString,
        d: g::FontFaceDescriptors,
    ) -> rt::Result<Handle> {
        let doc = Some(self.current_document(cx)?);
        let base = self.doc_base(doc);
        let valid = |name: &str, value: &str| {
            snowlark_style::font_face_descriptor(name, value, &base).ok_or_else(syntax_error)
        };
        let mut data = FontFaceData {
            doc,
            family: valid("font-family", family)?,
            style: valid("font-style", &d.style)?,
            weight: valid("font-weight", &d.weight)?,
            stretch: valid("font-width", d.width.as_deref().unwrap_or(&d.stretch))?,
            unicode_range: valid("unicode-range", &d.unicode_range)?,
            feature_settings: valid("font-feature-settings", &d.feature_settings)?,
            variation_settings: valid("font-variation-settings", &d.variation_settings)?,
            display: valid("font-display", &d.display)?,
            ascent_override: valid("ascent-override", &d.ascent_override)?,
            descent_override: valid("descent-override", &d.descent_override)?,
            line_gap_override: valid("line-gap-override", &d.line_gap_override)?,
            src: String::new(),
            bytes: None,
            status: FontFaceLoadStatus::Unloaded,
            rid: None,
            css: false,
            promise: None,
            error: None,
        };
        match &source {
            BufferOrString::String(s) => {
                match snowlark_style::font_face_descriptor("src", s, &base) {
                    Some(src) => data.src = src,
                    None => {
                        data.status = FontFaceLoadStatus::Error;
                        data.error = Some((
                            ExceptionName::SyntaxError,
                            "The source is not a valid src descriptor",
                        ));
                    }
                }
            }
            other => {
                if let Some(o) = buffer_of(other) {
                    data.bytes = Some(super::encoding::buffer_bytes(cx, o));
                }
            }
        }
        let has_bytes = data.bytes.is_some();
        let h = self.alloc(Object::FontFace(Box::new(data)));
        if has_bytes {
            // A font from memory is parsed at once: the face is loading, then loaded or in error.
            self.face_data(h, |d| d.status = FontFaceLoadStatus::Loading);
            self.start_face_load(h);
            if self.face_load_status(h) == FontFaceLoadStatus::Error {
                self.face_data(h, |d| {
                    d.error = Some((ExceptionName::SyntaxError, "The buffer is not a font"))
                });
            }
        }
        Ok(h)
    }
}

impl FontFaceSetImpl for Engine {
    fn ready(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        let doc = self.font_set_parts(this);
        if let Some(p) = self.cssom.borrow().fonts_ready.get(&doc) {
            return Ok(p.clone());
        }
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        if self.fonts_loading(doc) {
            self.cssom.borrow_mut().fonts_ready.insert(doc, p.clone());
        } else {
            p.resolve(cx, this);
        }
        Ok(p)
    }
    fn status(&self, this: Handle) -> FontFaceSetLoadStatus {
        if self.fonts_loading(self.font_set_parts(this)) {
            FontFaceSetLoadStatus::Loading
        } else {
            FontFaceSetLoadStatus::Loaded
        }
    }
    fn add(&self, _cx: &mut Cx, this: Handle, font: Handle) -> rt::Result<Handle> {
        if self.face_data(font, |d| d.css) {
            return Ok(this);
        }
        let already = matches!(&*self.obj(this),
            Object::FontFaceSet { script, .. } if script.contains(&font));
        if already {
            return Ok(this);
        }
        if let Some((doc, rid)) = self.ensure_renderer_face(font) {
            self.with_doc_renderer(doc, |r, _| r.use_script_face(rid, true));
        }
        if let Object::FontFaceSet { script, .. } = &mut *self.obj_mut(this) {
            script.push(font);
        }
        Ok(this)
    }
    fn delete(&self, _cx: &mut Cx, this: Handle, font: Handle) -> rt::Result<bool> {
        if self.face_data(font, |d| d.css) {
            return Ok(false);
        }
        let removed = match &mut *self.obj_mut(this) {
            Object::FontFaceSet { script, .. } => {
                let before = script.len();
                script.retain(|&h| h != font);
                script.len() != before
            }
            _ => false,
        };
        if removed {
            if let Some((doc, rid)) = self.face_data(font, |d| d.doc.zip(d.rid)) {
                self.with_doc_renderer(doc, |r, _| r.use_script_face(rid, false));
            }
        }
        Ok(removed)
    }
    fn clear(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let faces = match &mut *self.obj_mut(this) {
            Object::FontFaceSet { script, .. } => std::mem::take(script),
            _ => Vec::new(),
        };
        for font in faces {
            if let Some((doc, rid)) = self.face_data(font, |d| d.doc.zip(d.rid)) {
                self.with_doc_renderer(doc, |r, _| r.use_script_face(rid, false));
            }
        }
        Ok(())
    }
    fn setlike_at(&self, this: Handle, index: u32) -> Option<Handle> {
        self.font_set_members(this).get(index as usize).copied()
    }
    fn setlike_size(&self, this: Handle) -> u32 {
        self.font_set_members(this).len() as u32
    }
    fn setlike_has(&self, this: Handle, value: Handle) -> bool {
        self.font_set_members(this).contains(&value)
    }
    fn load(&self, cx: &mut Cx, this: Handle, font: &str, text: &str) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let Some(faces) = self.matching_members(this, font, text) else {
            p.reject(cx, syntax_error());
            return Ok(p);
        };
        for &f in &faces {
            if self.face_load_status(f) == FontFaceLoadStatus::Unloaded {
                self.start_face_load(f);
            }
        }
        self.cssom.borrow_mut().load_waiters.push(LoadWaiter {
            promise: p.clone(),
            faces,
        });
        // (settled by the page's next poll: a face already loaded resolves it then)
        self.update_font_waiters(cx);
        Ok(p)
    }
    fn check(&self, _cx: &mut Cx, this: Handle, font: &str, text: &str) -> rt::Result<bool> {
        let Some(faces) = self.matching_members(this, font, text) else {
            return Err(syntax_error());
        };
        Ok(faces
            .iter()
            .all(|&f| self.face_load_status(f) == FontFaceLoadStatus::Loaded))
    }
}

#[allow(dead_code)]
const _: InterfaceId = InterfaceId::FontFace;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_shorthand_gives_families_weight_style_and_width() {
        assert_eq!(
            parse_font_query("bold 16px Foo, 'Bar Baz', serif"),
            Some(FontQuery {
                families: vec!["Foo".into(), "Bar Baz".into(), "serif".into()],
                weight: 700,
                style: "normal".into(),
                width: 100.0,
            })
        );
        assert_eq!(
            parse_font_query("italic 300 1.5em/2 \"A\""),
            Some(FontQuery {
                families: vec!["A".into()],
                weight: 300,
                style: "italic".into(),
                width: 100.0,
            })
        );
        assert_eq!(
            parse_font_query("20px/1 ahem").map(|r| r.families),
            Some(vec!["ahem".into()])
        );
        // `<font-width-css3>` and `oblique <angle>` (CSS Fonts 4 §2.10).
        assert_eq!(
            parse_font_query("condensed bold 16px A").map(|q| (q.width, q.weight)),
            Some((75.0, 700))
        );
        assert_eq!(
            parse_font_query("ultra-expanded 16px A").map(|q| q.width),
            Some(200.0)
        );
        assert_eq!(
            parse_font_query("oblique 20deg semi-condensed 16px A").map(|q| (q.style, q.width)),
            Some(("oblique 20deg".into(), 87.5))
        );
        assert_eq!(
            parse_font_query("oblique 16px A").map(|q| q.style),
            Some("oblique".into())
        );
        assert_eq!(parse_font_query("oblique 91deg 16px A"), None);
        assert_eq!(parse_font_query("16px"), None);
        assert_eq!(parse_font_query("inherit"), None);
        assert_eq!(parse_font_query("16px inherit"), None);
        assert_eq!(parse_font_query(""), None);
    }
}
