// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Values and at-rules: custom properties, @property, var(), calc(), relative units, @media against the
//! Device, @container, @layer, @supports, @scope, @import, @font-face, adoptedStyleSheets, pseudo-elements.

mod common;

use std::sync::{Arc, Mutex};

use common::Page;
use snowlark_style::{
    computed, ColorScheme, DeviceConfig, FontFace, FontFaceSource, ImportLoader, ImportRequest,
    ImportResponse, MediaKind, PseudoElement,
};
use url::Url;

#[test]
fn custom_properties_and_var() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           :root { --gap: 4px; --c: rgb(1, 2, 3); --nested: var(--gap); }
           #a { margin-top: var(--gap); color: var(--c); padding-top: var(--missing, 9px);
                width: calc(var(--nested) * 3); }
           #b { --gap: 10px; margin-top: var(--gap); }
           #c { width: var(--missing); }
         </style><div id=a></div><div id=b><p id=bc></p></div><div id=c></div>",
    );
    assert_eq!(p.get("#a", "margin-top"), "4px");
    assert_eq!(p.get("#a", "color"), "rgb(1, 2, 3)");
    assert_eq!(p.get("#a", "padding-top"), "9px");
    assert_eq!(p.get("#a", "width"), "12px");
    assert_eq!(p.get("#b", "margin-top"), "10px");
    // Custom properties inherit.
    assert_eq!(p.get("#bc", "--gap"), "10px");
    // An invalid-at-computed-value-time var() makes the declaration unset.
    assert_eq!(p.get("#c", "width"), "auto");
}

#[test]
fn registered_custom_properties() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           @property --len { syntax: '<length>'; inherits: false; initial-value: 3px; }
           @property --col { syntax: '<color>'; inherits: true; initial-value: red; }
           #a { --len: 2em; font-size: 10px; width: var(--len); }
           #b { width: var(--len); }
           #c { --len: not-a-length; width: var(--len); }
         </style><div id=a><p id=b></p></div><div id=c></div>",
    );
    // The registered syntax computes the value: 2em at 10px is 20px.
    assert_eq!(p.get("#a", "--len"), "20px");
    assert_eq!(p.get("#a", "width"), "20px");
    // Not inherited: the child sees the initial value.
    assert_eq!(p.get("#b", "width"), "3px");
    assert_eq!(p.get("#b", "--col"), "rgb(255, 0, 0)");
    // Invalid for the syntax: the initial value.
    assert_eq!(p.get("#c", "width"), "3px");
}

#[test]
fn calc_and_relative_units() {
    let p = Page::with_device(
        "<!DOCTYPE html><style>
           html { font-size: 20px; }
           #a { font-size: 2em; width: 3rem; height: 10vw; margin-top: 50vh;
                padding-left: calc(1em + 10px); padding-top: calc(100% - 10px);
                margin-left: min(5px, 1vw); line-height: 1.5; }
           #b { font-size: 50%; }
         </style><div id=a><span id=b></span></div>",
        DeviceConfig {
            viewport: (800.0, 600.0),
            ..DeviceConfig::default()
        },
    );
    assert_eq!(p.get("#a", "font-size"), "40px");
    assert_eq!(p.get("#a", "width"), "60px");
    assert_eq!(p.get("#a", "height"), "80px");
    assert_eq!(p.get("#a", "margin-top"), "300px");
    assert_eq!(p.get("#a", "padding-left"), "50px");
    assert_eq!(p.get("#a", "padding-top"), "calc(100% - 10px)");
    assert_eq!(p.get("#a", "margin-left"), "5px");
    // CSSOM: the resolved line-height of a number is the used length.
    assert_eq!(p.get("#a", "line-height"), "60px");
    assert_eq!(p.get("#b", "font-size"), "20px");
}

const MEDIA_DOC: &str = "<!DOCTYPE html><style>
   p { width: 1px; }
   @media (min-width: 600px) { p { width: 2px; } }
   @media (max-width: 400px) { p { width: 3px; } }
   @media print { p { height: 4px; } }
   @media (prefers-color-scheme: dark) { p { color: rgb(9, 9, 9); } }
   @media (min-resolution: 2dppx) { p { padding-top: 5px; } }
   @media (orientation: portrait) { p { margin-left: 6px; } }
 </style><p>x</p>";

#[test]
fn media_queries_against_the_device() {
    let p = Page::new(MEDIA_DOC);
    assert_eq!(p.get("p", "width"), "2px");
    assert_eq!(p.get("p", "height"), "auto");
    assert_eq!(p.get("p", "color"), "rgb(0, 0, 0)");
    assert_eq!(p.get("p", "padding-top"), "0px");
    let p = Page::with_device(
        MEDIA_DOC,
        DeviceConfig {
            viewport: (300.0, 800.0),
            device_pixel_ratio: 2.0,
            color_scheme: ColorScheme::Dark,
            media: MediaKind::Print,
            ..DeviceConfig::default()
        },
    );
    assert_eq!(p.get("p", "width"), "3px");
    assert_eq!(p.get("p", "height"), "4px");
    assert_eq!(p.get("p", "color"), "rgb(9, 9, 9)");
    assert_eq!(p.get("p", "padding-top"), "5px");
    assert_eq!(p.get("p", "margin-left"), "6px");
}

#[test]
fn device_change_reevaluates_media_queries_and_viewport_units() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>
           p { width: 1px; height: 10vw; }
           @media (max-width: 400px) { p { width: 3px; } }
         </style><p>x</p>",
    );
    assert_eq!(p.get("p", "width"), "1px");
    assert_eq!(p.get("p", "height"), "102.4px");
    p.engine.set_device(
        &p.data,
        DeviceConfig {
            viewport: (300.0, 300.0),
            ..DeviceConfig::default()
        },
    );
    p.restyle();
    assert_eq!(p.get("p", "width"), "3px");
    assert_eq!(p.get("p", "height"), "30px");
}

/// CSS Values 4 §6.1.1: `rem` is relative to the root element's font size, whatever the device
/// change that restyled the element (a viewport resize restyles what uses viewport units, a media
/// query change everything; the root's font size is not changed by either). The device the media
/// queries and viewport units read is updated in place: a new one would forget the root's font
/// size (rem at the initial 16px), line height (rlh) and the body's color (the tables quirk).
#[test]
fn device_change_keeps_root_relative_units() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>
           html { font-size: 10px; line-height: 20px; }
           #v { font-size: 1.4rem; width: 50vw; height: 2rlh; }
           #v span { width: 2.4rem; }
           #m { font-size: 2rem; }
           @media (max-width: 400px) { #m { color: rgb(1, 2, 3); } }
         </style><div id=v><span>x</span></div><div id=m>y</div>",
    );
    assert_eq!(p.get("#v", "font-size"), "14px");
    assert_eq!(p.get("#v", "width"), "512px");
    // Several resizes, each restyling #v (it uses vw) but not the root.
    for (i, w) in [1000.0f32, 998.0, 640.0, 1004.0].into_iter().enumerate() {
        p.engine.set_device(
            &p.data,
            DeviceConfig {
                viewport: (w, 700.0),
                device_pixel_ratio: 1.5,
                ..DeviceConfig::default()
            },
        );
        p.restyle();
        assert_eq!(p.get("#v", "font-size"), "14px", "resize {i}");
        assert_eq!(p.get("#v", "width"), format!("{}px", w / 2.0), "resize {i}");
        assert_eq!(p.get("#v", "height"), "40px", "resize {i}");
        assert_eq!(p.get("#v span", "width"), "24px", "resize {i}");
    }
    // A media query change restyles everything; the root's font size is unchanged.
    p.engine.set_device(
        &p.data,
        DeviceConfig {
            viewport: (300.0, 700.0),
            ..DeviceConfig::default()
        },
    );
    p.restyle();
    assert_eq!(p.get("#m", "color"), "rgb(1, 2, 3)");
    assert_eq!(p.get("#m", "font-size"), "20px");
    assert_eq!(p.get("#v", "font-size"), "14px");
    // The device pixel ratio alone changes no length.
    p.engine.set_device(
        &p.data,
        DeviceConfig {
            viewport: (300.0, 700.0),
            device_pixel_ratio: 2.0,
            ..DeviceConfig::default()
        },
    );
    p.restyle();
    assert_eq!(p.get("#v", "font-size"), "14px");
    assert_eq!(p.get("#v", "width"), "150px");
}

/// A device change that replaces the device (the `medium` font size) restyles everything with the
/// new `medium`, and root-relative units still follow the root element's font size.
#[test]
fn default_font_size_change_keeps_root_relative_units() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>
           html { font-size: 10px; line-height: 20px; }
           #v { font-size: 1.4rem; width: 50vw; height: 2rlh; }
           #d { font-size: medium; }
         </style><div id=v>x</div><div id=d>y</div>",
    );
    assert_eq!(p.get("#d", "font-size"), "16px");
    p.engine.set_device(
        &p.data,
        DeviceConfig {
            viewport: (1000.0, 700.0),
            default_font_size: 20.0,
            ..DeviceConfig::default()
        },
    );
    p.restyle();
    assert_eq!(p.get("#d", "font-size"), "20px");
    assert_eq!(p.get("#v", "font-size"), "14px");
    assert_eq!(p.get("#v", "width"), "500px");
    assert_eq!(p.get("#v", "height"), "40px");
}

#[test]
fn container_queries() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>
           #box { container-type: inline-size; container-name: card; }
           p { width: 1px; }
           @container (min-width: 400px) { p { width: 2px; } }
           @container card (max-width: 200px) { p { width: 3px; } }
           @container nope (min-width: 0px) { p { height: 9px; } }
           p { --max: min(540px, 100cqw); max-height: var(--max); margin-left: 10cqi; }
         </style><div id=box><p>x</p></div>",
    );
    // No size from layout yet: queries do not match.
    assert_eq!(p.get("p", "width"), "1px");
    // CSS Contain 3 §6.1: container query length units parse (also through var()); with no
    // size from the query container they fall back to the small viewport size.
    assert_ne!(p.get("p", "max-height"), "none");
    let boxed = p.q("#box");
    p.data.set_query_container_size(boxed, Some(500.0), None);
    p.engine.invalidate_subtree(&p.dom, &mut p.data, boxed);
    p.restyle();
    assert_eq!(p.get("p", "width"), "2px");
    assert_eq!(p.get("p", "max-height"), "500px");
    assert_eq!(p.get("p", "margin-left"), "50px");
    p.data.set_query_container_size(boxed, Some(150.0), None);
    p.engine.invalidate_subtree(&p.dom, &mut p.data, boxed);
    p.restyle();
    assert_eq!(p.get("p", "width"), "3px");
    assert_eq!(p.get("p", "height"), "auto");
    assert_eq!(p.get("p", "max-height"), "150px");
    assert_eq!(p.get("p", "margin-left"), "15px");
}

#[test]
fn cascade_layers() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           @layer base, theme;
           @layer theme { p { color: rgb(2, 2, 2); } #x { width: 1px !important; } }
           @layer base { #x { color: rgb(1, 1, 1); } #x { width: 2px !important; } }
           p { height: 3px; }
           @layer theme { p { height: 4px; } }
         </style><p id=x>x</p>",
    );
    // Later layers win over earlier ones regardless of specificity...
    assert_eq!(p.get("p", "color"), "rgb(2, 2, 2)");
    // ...but for !important the order reverses.
    assert_eq!(p.get("p", "width"), "2px");
    // Unlayered styles beat layered ones.
    assert_eq!(p.get("p", "height"), "3px");
}

#[test]
fn supports_and_scope() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           @supports (display: grid) { #a { width: 1px; } }
           @supports (display: frobnicate) { #a { height: 2px; } }
           @supports not (display: frobnicate) { #a { margin-top: 3px; } }
           @supports selector(:has(a)) { #a { margin-left: 4px; } }
           @scope (.card) to (.content) { p { color: rgb(5, 5, 5); } }
         </style>
         <div id=a></div>
         <div class=card><p id=in>x</p><div class=content><p id=out>y</p></div></div><p id=no>z</p>",
    );
    assert_eq!(p.get("#a", "width"), "1px");
    assert_eq!(p.get("#a", "height"), "auto");
    assert_eq!(p.get("#a", "margin-top"), "3px");
    assert_eq!(p.get("#a", "margin-left"), "4px");
    assert_eq!(p.get("#in", "color"), "rgb(5, 5, 5)");
    assert_eq!(p.get("#out", "color"), "rgb(0, 0, 0)");
    assert_eq!(p.get("#no", "color"), "rgb(0, 0, 0)");
}

struct Imports {
    requests: Mutex<Vec<String>>,
}

impl ImportLoader for Imports {
    fn load(&self, request: &ImportRequest<'_>) -> ImportResponse {
        self.requests
            .lock()
            .unwrap()
            .push(request.url.as_str().to_owned());
        match request.url.path() {
            "/a.css" => ImportResponse::Css {
                text: "@import 'b.css'; p { width: 1px; }".into(),
                url: request.url.clone(),
            },
            "/b.css" => ImportResponse::Css {
                text: "p { height: 2px; }".into(),
                url: request.url.clone(),
            },
            "/print.css" => ImportResponse::Css {
                text: "p { padding-top: 3px; }".into(),
                url: request.url.clone(),
            },
            _ => ImportResponse::Failed,
        }
    }
}

#[test]
fn import_rules_go_through_the_loader() {
    let mut p = Page::new("<!DOCTYPE html><p>x</p>");
    let loader = Arc::new(Imports {
        requests: Mutex::new(Vec::new()),
    });
    p.engine.set_import_loader(Some(loader.clone()));
    let base = Url::parse("https://example.com/dir/page.html").unwrap();
    let sheet = p.engine.parse_sheet(
        &p.data,
        "@import url(/a.css); @import '/print.css' print; @import '/missing.css'; p { color: red; }",
        &base,
        "",
    );
    let owner = p.q("p");
    p.engine.add_sheet(&p.dom, &p.data, owner, sheet);
    p.restyle();
    assert_eq!(p.get("p", "width"), "1px");
    assert_eq!(p.get("p", "height"), "2px");
    assert_eq!(p.get("p", "padding-top"), "0px");
    assert_eq!(p.get("p", "color"), "rgb(255, 0, 0)");
    let requests = loader.requests.lock().unwrap().clone();
    assert_eq!(
        requests,
        [
            "https://example.com/a.css",
            "https://example.com/b.css",
            "https://example.com/print.css",
            "https://example.com/missing.css"
        ]
    );
}

#[test]
fn font_face_rules_reach_the_handler() {
    let mut p = Page::new("<!DOCTYPE html><p>x</p>");
    let seen: Arc<Mutex<Vec<FontFace>>> = Arc::default();
    let sink = seen.clone();
    p.engine
        .set_font_face_handler(Some(Arc::new(move |f: &FontFace| {
            sink.lock().unwrap().push(f.clone())
        })));
    let base = Url::parse("https://example.com/").unwrap();
    let sheet = p.engine.parse_sheet(
        &p.data,
        "@font-face { font-family: Foo; src: url(foo.woff2) format('woff2'), local(Bar);
                      font-weight: 100 900; font-display: swap; }
         @media print { @font-face { font-family: PrintOnly; src: url(p.woff); } }",
        &base,
        "",
    );
    let owner = p.q("p");
    p.engine.add_sheet(&p.dom, &p.data, owner, sheet);
    let faces = seen.lock().unwrap().clone();
    assert_eq!(faces.len(), 1);
    assert_eq!(faces[0].family, "Foo");
    assert_eq!(faces[0].weight, "100 900");
    assert_eq!(faces[0].display, "swap");
    assert_eq!(
        faces[0].sources,
        [
            FontFaceSource::Url {
                url: "https://example.com/foo.woff2".into(),
                format: Some("\"woff2\"".into())
            },
            FontFaceSource::Local("Bar".into())
        ]
    );
    assert_eq!(p.engine.font_faces(&p.data).len(), 1);
}

#[test]
fn font_faces_come_in_cascade_layer_order() {
    let mut p = Page::new("<!DOCTYPE html><p>x</p>");
    let base = Url::parse("https://example.com/").unwrap();
    let sheet = p.engine.parse_sheet(
        &p.data,
        "@font-face { font-family: Unlayered; src: local(a); }
         @layer second { @font-face { font-family: Second; src: local(a); } }
         @layer first { @font-face { font-family: First; src: local(a); } }
         @layer second { @font-face { font-family: Second2; src: local(a); } }
         @layer first.inner { @font-face { font-family: Inner; src: local(a); } }
         @layer first { @font-face { font-family: First2; src: local(a); } }",
        &base,
        "",
    );
    let owner = p.q("p");
    p.engine.add_sheet(&p.dom, &p.data, owner, sheet);
    let names: Vec<String> = p
        .engine
        .font_faces(&p.data)
        .into_iter()
        .map(|f| f.family)
        .collect();
    // `second` was named first, so it is the lower layer; a layer's own rules come after its
    // sublayers; unlayered rules last.
    assert_eq!(
        names,
        ["Second", "Second2", "Inner", "First", "First2", "Unlayered"]
    );
}

#[test]
fn font_face_descriptors_parse_and_serialize() {
    let base = Url::parse("https://example.com/").unwrap();
    let d = |n: &str, v: &str| snowlark_style::font_face_descriptor(n, v, &base);
    assert_eq!(d("font-weight", "bold").as_deref(), Some("bold"));
    assert_eq!(d("font-weight", "100 900").as_deref(), Some("100 900"));
    assert_eq!(d("font-weight", "heavy"), None);
    assert_eq!(d("unicode-range", "U+0-7F").as_deref(), Some("U+0-7F"));
    assert_eq!(d("font-display", "swap").as_deref(), Some("swap"));
    assert_eq!(d("font-display", "later"), None);
    assert_eq!(d("font-family", "'A b'").as_deref(), Some("A b"));
    // A name that is not a sequence of identifiers, or is a keyword, keeps its quotes.
    assert_eq!(
        d("font-family", "'21st Century'").as_deref(),
        Some("\"21st Century\"")
    );
    assert_eq!(d("font-family", "\"serif\"").as_deref(), Some("\"serif\""));
    let face = snowlark_style::parse_font_face(
        "font-family: Foo; src: local(Bar); font-style: italic",
        &base,
    )
    .unwrap();
    assert_eq!(
        (face.family.as_str(), face.style.as_str()),
        ("Foo", "italic")
    );
    assert!(snowlark_style::parse_font_face("src: local(Bar)", &base).is_none());
}

#[test]
fn adopted_style_sheets() {
    let mut p =
        Page::new("<!DOCTYPE html><style>p { color: rgb(1, 1, 1); width: 1px; }</style><p>x</p>");
    let base = Url::parse("about:blank").unwrap();
    let sheet = p
        .engine
        .parse_sheet(&p.data, "p { color: rgb(2, 2, 2); }", &base, "");
    let doc = p.doc;
    p.engine
        .set_adopted_sheets(&p.dom, &p.data, doc, vec![sheet.clone()]);
    p.restyle();
    // Adopted sheets come after the document's own.
    assert_eq!(p.get("p", "color"), "rgb(2, 2, 2)");
    assert_eq!(p.get("p", "width"), "1px");
    // `disabled` takes it out of the cascade.
    sheet.set_disabled(true);
    p.engine.sheets_changed();
    p.restyle();
    assert_eq!(p.get("p", "color"), "rgb(1, 1, 1)");
    p.engine
        .set_adopted_sheets(&p.dom, &p.data, doc, Vec::new());
    p.restyle();
    assert_eq!(p.get("p", "color"), "rgb(1, 1, 1)");
}

#[test]
fn style_element_changes() {
    let mut p = Page::new("<!DOCTYPE html><style id=s>p { width: 1px; }</style><p>x</p>");
    assert_eq!(p.get("p", "width"), "1px");
    let style = p.q("#s");
    p.dom.set_text_content(style, "p { width: 2px; }");
    p.engine.update_style_block(&p.dom, &p.data, style);
    p.restyle();
    assert_eq!(p.get("p", "width"), "2px");
    p.dom.set_attribute(style, "type", "text/plain").unwrap();
    p.engine.update_style_block(&p.dom, &p.data, style);
    p.restyle();
    assert_eq!(p.get("p", "width"), "auto");
}

#[test]
fn before_and_after_exist_only_when_their_rules_can_give_them_content() {
    // A `::before` none of whose rules declares `content` computes `content: normal` and is not
    // generated; one that inherits `display` or takes it from a variable keeps its style.
    let p = Page::new(
        "<!DOCTYPE html><style>
           *, ::before, ::after { box-sizing: border-box; }
           #a::before { content: 'a'; }
           #b { content: 'b'; }
           #b::before { all: inherit; }
           #c::before { content: var(--c); --c: 'c'; }
           #d::before { display: block; }
           #e::before { display: var(--d); --d: block; }
           #f::before { display: inherit; }
           #g::after { content: 'g' !important; }
           #h::after { content: none; }
         </style><p id=a></p><p id=b></p><p id=c></p><p id=d></p><p id=e></p><p id=f></p>
         <p id=g></p><p id=h></p><p id=i></p>",
    );
    let style = |sel: &str, pseudo: PseudoElement| {
        let el = p.q(sel);
        p.engine.pseudo_style(&p.dom, &p.data, el, &pseudo)
    };
    let before = style("#a", PseudoElement::Before).unwrap();
    assert_eq!(
        computed::computed_property_by_name(&before, "box-sizing").unwrap(),
        "border-box"
    );
    assert!(style("#a", PseudoElement::After).is_none());
    let b = style("#b", PseudoElement::Before).unwrap();
    assert_eq!(
        computed::computed_property_by_name(&b, "content").unwrap(),
        "\"b\""
    );
    assert!(style("#c", PseudoElement::Before).is_some());
    assert!(style("#d", PseudoElement::Before).is_none());
    // (kept with the element, as a `display` that depends on it, but never generated)
    assert!(style("#e", PseudoElement::Before).is_none());
    assert!(style("#f", PseudoElement::Before).is_none());
    assert!(style("#g", PseudoElement::After).is_some());
    assert!(style("#h", PseudoElement::After).is_none());
    assert!(style("#i", PseudoElement::Before).is_none());
}

#[test]
fn pseudo_element_styles() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           p::before { content: 'x'; color: rgb(1, 1, 1); }
           p::after { color: red; }
           li::marker { color: rgb(2, 2, 2); }
           input::placeholder { color: rgb(3, 3, 3); }
           p::first-letter { font-size: 30px; }
         </style><p>t</p><ul><li>l</li></ul><input placeholder=x>",
    );
    let style = |sel: &str, pseudo: PseudoElement| {
        let el = p.q(sel);
        p.engine
            .pseudo_style(&p.dom, &p.data, el, &pseudo)
            .map(|v| computed::computed_property_by_name(&v, "color").unwrap())
    };
    assert_eq!(
        style("p", PseudoElement::Before).as_deref(),
        Some("rgb(1, 1, 1)")
    );
    // ::after with `content: normal` does not exist.
    assert_eq!(style("p", PseudoElement::After), None);
    assert_eq!(
        style("li", PseudoElement::Marker).as_deref(),
        Some("rgb(2, 2, 2)")
    );
    assert_eq!(
        style("input", PseudoElement::Placeholder).as_deref(),
        Some("rgb(3, 3, 3)")
    );
    let el = p.q("p");
    let fl = p
        .engine
        .pseudo_style(&p.dom, &p.data, el, &PseudoElement::FirstLetter)
        .unwrap();
    assert_eq!(
        computed::computed_property_by_name(&fl, "font-size").unwrap(),
        "30px"
    );
}

/// Selectors 4 §compat: a `::-webkit-` pseudo-element is valid, so the rule it is in (and every
/// other selector of its list) stays; one this engine does not have matches nothing. The legacy
/// names of `::placeholder`, `::file-selector-button`, `:autofill` (HTML §4.16.3) and
/// `:fullscreen` select what the standard ones do.
#[test]
fn webkit_prefixed_pseudo_elements_and_classes() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           #a { color: red; }
           span::-webkit-something-invalid, #a, ::-webkit-inner-spin-button { color: rgb(0, 1, 0); }
           #b, ::-webkit-scrollbar-thumb:hover { color: rgb(0, 2, 0); }
           ::-webkitfoo, #a { color: red; }
           #c, input:-webkit-autofill { color: rgb(0, 3, 0); }
           #d, :-webkit-full-screen { color: rgb(0, 4, 0); }
           input::-webkit-input-placeholder { color: rgb(0, 5, 0); }
           #a::-webkit-inner-spin-button { color: red; }
         </style><p id=a>a</p><p id=b>b</p><p id=c>c</p><p id=d>d</p><input placeholder=x>",
    );
    assert_eq!(p.get("#a", "color"), "rgb(0, 1, 0)");
    assert_eq!(p.get("#b", "color"), "rgb(0, 2, 0)");
    assert_eq!(p.get("#c", "color"), "rgb(0, 3, 0)");
    assert_eq!(p.get("#d", "color"), "rgb(0, 4, 0)");
    let input = p.q("input");
    let placeholder = p
        .engine
        .pseudo_style(&p.dom, &p.data, input, &PseudoElement::Placeholder)
        .map(|v| computed::computed_property_by_name(&v, "color").unwrap());
    assert_eq!(placeholder.as_deref(), Some("rgb(0, 5, 0)"));
}

#[test]
fn display_none_subtrees_resolve_on_demand() {
    let p = Page::new(
        "<!DOCTYPE html><style>#h { display: none; color: rgb(4, 4, 4); } #h span { width: 5px; }</style>
         <div id=h><span>x</span></div>",
    );
    let span = p.q("#h span");
    // The traversal does not style inside display:none.
    assert!(p.data.styles(span).is_none());
    assert_eq!(p.get("#h span", "width"), "5px");
    assert_eq!(p.get("#h span", "color"), "rgb(4, 4, 4)");
    assert!(p.data.styles(p.q("#h")).is_some());
}

#[test]
fn a_transform_resolves_to_one_matrix_without_a_box_too() {
    let p = Page::new(
        "<!DOCTYPE html><style>#a { transform: scale(0); } #b { transform: translate(10px, 50%) rotate(90deg); }
         #c { transform: translateZ(0) translateX(3px); } #d { transform: perspective(100px); }</style>
         <div id=a></div><div id=b></div><div id=c></div><div id=d></div>",
    );
    let id = |n| computed::property_id(n).unwrap();
    // (no layout: the reference box is empty, so 50% is 0)
    let resolved = |sel| {
        let values = p.engine.computed_style(&p.dom, &p.data, p.q(sel)).unwrap();
        computed::resolved_property(&values, &id("transform"), None)
    };
    assert_eq!(resolved("#a"), "matrix(0, 0, 0, 0, 0, 0)");
    assert_eq!(resolved("#b"), "matrix(0, 1, -1, 0, 10, 0)");
    // A 3D function whose product is 2D is a matrix(); a real 3D one a matrix3d().
    assert_eq!(resolved("#c"), "matrix(1, 0, 0, 1, 3, 0)");
    assert_eq!(
        resolved("#d"),
        "matrix3d(1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, -0.01, 0, 0, 0, 1)"
    );
    // The computed value (Typed OM, animations) keeps the list.
    let values = p.engine.computed_style(&p.dom, &p.data, p.q("#a")).unwrap();
    assert_eq!(
        computed::computed_property(&values, &id("transform")),
        "scale(0)"
    );
}

struct Layout;

impl computed::UsedValues for Layout {
    fn used_value(&self, property: snowlark_style::LonghandId) -> Option<String> {
        (property == snowlark_style::LonghandId::Width).then(|| "123px".to_owned())
    }
}

#[test]
fn resolved_values_take_used_values_from_layout() {
    let p = Page::new(
        "<!DOCTYPE html><style>p { width: 50%; height: 10%; line-height: normal; }</style><p>x</p>",
    );
    let values = p.engine.computed_style(&p.dom, &p.data, p.q("p")).unwrap();
    let id = |n| computed::property_id(n).unwrap();
    assert_eq!(
        computed::resolved_property(&values, &id("width"), Some(&Layout)),
        "123px"
    );
    assert_eq!(
        computed::resolved_property(&values, &id("width"), None),
        "50%"
    );
    assert_eq!(
        computed::resolved_property(&values, &id("height"), Some(&Layout)),
        "10%"
    );
    assert_eq!(
        computed::resolved_property(&values, &id("line-height"), Some(&Layout)),
        "normal"
    );
    assert!(computed::is_layout_dependent(
        snowlark_style::LonghandId::MarginTop
    ));
    assert!(!computed::is_layout_dependent(
        snowlark_style::LonghandId::Color
    ));
    let names = computed::computed_property_names();
    assert!(names.windows(2).all(|w| w[0] < w[1]));
    assert!(names.contains(&"grid-template-columns"));
    assert!(computed::property_id("not-a-property").is_none());
}

#[test]
fn properties_servo_gates_as_unimplemented_parse() {
    // CSS Lists 3 counters (the UA sheet's list-item counter on lists), text-overflow, contain.
    let p = Page::new(
        "<!DOCTYPE html><style>#a { counter-reset: x 3; counter-increment: x; text-overflow: ellipsis;
           contain: paint }</style><ol id=o></ol><div id=a></div>",
    );
    assert_eq!(p.get("#o", "counter-reset"), "list-item 0");
    assert_eq!(p.get("#a", "counter-reset"), "x 3");
    assert_eq!(p.get("#a", "counter-increment"), "x 1");
    assert_eq!(p.get("#a", "text-overflow"), "ellipsis");
    assert_eq!(p.get("#a", "contain"), "paint");
}

/// CSS Multi-column Layout 1 §4: `column-rule-*`, the `column-rule` shorthand and `column-fill`.
/// The rule's width keeps its value when the style is `none` (unlike a border's), and its color
/// computes `currentcolor` to the element's color.
#[test]
fn column_rule_and_fill() {
    let p = Page::new(
        "<!DOCTYPE html><style>#a { color: rgb(0, 255, 0); column-rule: 10px dotted; column-fill: auto }
           #b { column-rule-width: 7px; column-rule-color: rgb(1, 2, 3) }</style>
         <div id=a></div><div id=b></div><div id=c></div>",
    );
    assert_eq!(p.get("#a", "column-rule-width"), "10px");
    assert_eq!(p.get("#a", "column-rule-style"), "dotted");
    assert_eq!(p.get("#a", "column-rule-color"), "rgb(0, 255, 0)");
    assert_eq!(p.get("#a", "column-rule"), "10px dotted rgb(0, 255, 0)");
    assert_eq!(p.get("#a", "column-fill"), "auto");
    assert_eq!(p.get("#b", "column-rule-style"), "none");
    assert_eq!(p.get("#b", "column-rule-width"), "7px");
    assert_eq!(p.get("#b", "column-rule-color"), "rgb(1, 2, 3)");
    assert_eq!(p.get("#c", "column-rule-width"), "3px");
    assert_eq!(p.get("#c", "column-fill"), "balance");
}

#[test]
fn markers_keep_their_spaces() {
    // CSS Lists 3 Appendix A's UA rules for ::marker.
    let p = Page::new("<!DOCTYPE html><ol><li id=i style='text-transform: uppercase'>x</li></ol>");
    let li = p.q("#i");
    let marker = p
        .engine
        .pseudo_style(&p.dom, &p.data, li, &PseudoElement::Marker)
        .unwrap();
    let get = |name| computed::computed_property_by_name(&marker, name).unwrap();
    assert_eq!(get("white-space-collapse"), "preserve");
    assert_eq!(get("text-wrap-mode"), "nowrap");
    assert_eq!(get("text-transform"), "none");
    assert_eq!(get("unicode-bidi"), "isolate");
}

/// SVG 2 §6.6: an SVG element's presentation attributes are declarations at the presentational
/// hints' level (author rules and `style` win; children inherit them), with unitless lengths as px.
#[test]
fn svg_presentation_attributes() {
    let p = Page::new(
        "<!DOCTYPE html><style>.css { fill: rgb(0, 128, 0) }</style>\
         <svg fill='rgb(255, 0, 0)' stroke-width='3' font-size='12'>\
           <rect id='a'/><rect id='b' class='css' fill='blue'/><rect id='c' style='fill: rgb(1, 2, 3)' fill='blue'/>\
           <rect id='d' fill='red;display:none'/>\
         </svg>",
    );
    assert_eq!(p.get("#a", "fill"), "rgb(255, 0, 0)");
    assert_eq!(p.get("#a", "stroke-width"), "3px");
    assert_eq!(p.get("svg", "font-size"), "12px");
    assert_eq!(p.get("#b", "fill"), "rgb(0, 128, 0)");
    assert_eq!(p.get("#c", "fill"), "rgb(1, 2, 3)");
    // (a value that would end the declaration is ignored, not injected)
    assert_eq!(p.get("#d", "fill"), "rgb(255, 0, 0)");
    assert_eq!(p.get("#d", "display"), "inline");
}

/// SVG 2 §6.6: the geometry presentation attributes on the elements they belong to (`width` and
/// `height` on the outermost `svg` only, svgwg#1057), the SVG spellings of `transform`, and the UA
/// sheet's `overflow: hidden` for inner viewports.
#[test]
fn svg_geometry_and_transform_presentation_attributes() {
    let p = Page::new(
        "<!DOCTYPE html><style>#css { width: 7px }</style>         <svg id=outer width=20 height='50%' x=3 transform='rotate(90)'>           <rect id=r x=1 y=2 width=3 height=4 rx=5 transform='translate(10 20) rotate(45 1 1)' />           <circle id=c cx=1 cy=2 r=3 /><g id=g x=1 width=3 transform='scale(2)' />           <svg id=inner width=5 x=6 /><rect id=css width=9 />           <linearGradient id=lg gradientTransform='scale(2)' transform='scale(3)' />         </svg>",
    );
    assert_eq!(p.get("#outer", "width"), "20px");
    assert_eq!(p.get("#outer", "height"), "50%");
    assert_eq!(p.get("#outer", "transform"), "none");
    assert_eq!(p.get("#outer", "overflow-x"), "hidden");
    assert_eq!(p.get("#r", "x"), "1px");
    assert_eq!(p.get("#r", "height"), "4px");
    assert_eq!(p.get("#r", "rx"), "5px");
    assert_eq!(
        p.get("#r", "transform"),
        "translate(10px, 20px) translate(1px, 1px) rotate(45deg) translate(-1px, -1px)"
    );
    assert_eq!(p.get("#c", "r"), "3px");
    assert_eq!(p.get("#g", "x"), "0px");
    assert_eq!(p.get("#g", "width"), "auto");
    assert_eq!(p.get("#g", "transform"), "scale(2)");
    assert_eq!(p.get("#inner", "width"), "auto");
    assert_eq!(p.get("#inner", "x"), "6px");
    assert_eq!(p.get("#inner", "overflow-x"), "hidden");
    assert_eq!(p.get("#css", "width"), "7px");
    assert_eq!(p.get("#lg", "transform"), "scale(2)");
}

/// Each vendored stylo patch of the CSS Images work round-trips through parse and serialization.
#[test]
fn css_images_stylo_patches_round_trip() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           #a { image-orientation: none; transform-box: fill-box;
                object-view-box: inset(10px 20% 30px 40%); image-rendering: smooth; }
           #b { image-rendering: high-quality; object-view-box: xywh(1px 2px 3px 4px);
                background-image: image(\"a.png\", red); }
           #c { image-rendering: optimizeSpeed; object-view-box: rect(1px 4px 5px 2px);
                background-image: cross-fade(50% red, blue 25%); }
           #d { background-image: image(ltr red); list-style-image: image(rtl \"b.png\"); }
           #e { background-image: linear-gradient(rgb(none 255 0), red); }
           #f { background-image: linear-gradient(in srgb, rgb(none 255 0), red); }
           #g { object-view-box: circle(1px); background-image: image(red, blue); }
         </style><div id=a></div><div id=b></div><div id=c></div><div id=d></div><div id=e></div>
         <div id=f></div><div id=g></div>",
    );
    assert_eq!(p.get("#a", "image-orientation"), "none");
    assert_eq!(p.get("#b", "image-orientation"), "from-image");
    assert_eq!(p.get("#a", "transform-box"), "fill-box");
    assert_eq!(p.get("#b", "transform-box"), "view-box");
    assert_eq!(p.get("#a", "object-view-box"), "inset(10px 20% 30px 40%)");
    assert_eq!(
        p.get("#b", "object-view-box"),
        "inset(2px calc(100% - 4px) calc(100% - 6px) 1px)"
    );
    assert_eq!(p.get("#a", "image-rendering"), "smooth");
    assert_eq!(p.get("#b", "image-rendering"), "high-quality");
    assert_eq!(p.get("#c", "image-rendering"), "optimizespeed");
    assert!(p.get("#b", "background-image").contains("a.png"));
    assert!(p
        .get("#b", "background-image")
        .ends_with(", rgb(255, 0, 0))"));
    assert_eq!(
        p.get("#c", "background-image"),
        "cross-fade(rgb(255, 0, 0) 50%, rgb(0, 0, 255) 25%)"
    );
    assert_eq!(p.get("#d", "background-image"), "image(ltr rgb(255, 0, 0))");
    assert!(p
        .get("#d", "list-style-image")
        .starts_with("image(rtl url("));
    // (a color with `none` is not a legacy color: Oklab by default, which serializes as omitted)
    assert!(!p.get("#e", "background-image").contains("in "));
    assert!(p.get("#f", "background-image").contains("in srgb"));
    assert_eq!(p.get("#g", "object-view-box"), "none");
    assert_eq!(p.get("#g", "background-image"), "none");
}

/// Appends a new `<p class=c>` to the element `host` matches and restyles; returns it.
fn append_classed_p(p: &mut Page, host: &str) -> snowlark_dom::NodeId {
    let doc = p.doc;
    let host = p.q(host);
    let el = p.dom.create_element(doc, "p", None).unwrap();
    p.dom.set_attribute(el, "class", "c").unwrap();
    p.dom.append_child(host, el).unwrap();
    p.restyle();
    el
}

#[test]
fn styles_are_kept_for_elements_styled_in_later_restyles() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>.c { width: 3px; }</style>
         <div id=a><p class=c>x</p></div><div id=b style='color: rgb(9, 9, 9)'></div>",
    );
    let first = p.data.styles_arc(p.q("#a > p")).unwrap();
    // The same rules under a parent that passes down the same values: the kept style, in an `Arc`
    // of its own (the struct the rule set is the same one; a cascade would have made another).
    let later = append_classed_p(&mut p, "#a");
    let later_style = p.data.styles_arc(later).unwrap();
    assert!(!servo_arc::Arc::ptr_eq(&first, &later_style));
    assert!(std::ptr::eq(
        first.get_position(),
        later_style.get_position()
    ));
    // Under a parent with another color: its own.
    let other = append_classed_p(&mut p, "#b");
    let style = p.data.styles_arc(other).unwrap();
    assert!(!servo_arc::Arc::ptr_eq(&first, &style));
    assert_eq!(
        computed::computed_property_by_name(&style, "color").unwrap(),
        "rgb(9, 9, 9)"
    );
}

#[test]
fn kept_styles_follow_rules_edited_in_place() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>.c { width: 3px; }</style><div id=a><p class=c>x</p></div>",
    );
    append_classed_p(&mut p, "#a");
    let doc = p.doc;
    let sheet = p.engine.tree_sheets(doc)[0].1.clone();
    let rule = sheet.rules().get(0).unwrap().declarations().unwrap();
    rule.with(|d| ((), d.set_property("width", "5px", "")));
    p.engine.sheets_changed();
    p.restyle();
    let new = append_classed_p(&mut p, "#a");
    assert_eq!(p.get("#a > p", "width"), "5px");
    let style = p.data.styles_arc(new).unwrap();
    assert_eq!(
        computed::computed_property_by_name(&style, "width").unwrap(),
        "5px"
    );
}

#[test]
fn kept_styles_follow_the_viewport() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>.c { width: 10vw; }</style><div id=a><p class=c>x</p></div>",
    );
    append_classed_p(&mut p, "#a");
    let config = DeviceConfig {
        viewport: (400.0, 300.0),
        ..DeviceConfig::default()
    };
    p.engine.set_device(&p.data, config);
    p.restyle();
    assert_eq!(p.get("#a > p", "width"), "40px");
    let new = append_classed_p(&mut p, "#a");
    let style = p.data.styles_arc(new).unwrap();
    assert_eq!(
        computed::computed_property_by_name(&style, "width").unwrap(),
        "40px"
    );
}

#[test]
fn kept_styles_follow_the_root_font_size() {
    // (body's own font size: only `rem` changes below it)
    let mut p = Page::new(
        "<!DOCTYPE html><html style='font-size: 10px'><style>body { font-size: 16px; }
         .c { width: 2rem; }</style><div id=a><p class=c>x</p></div>",
    );
    append_classed_p(&mut p, "#a");
    assert_eq!(p.get("#a > p", "width"), "20px");
    let html = p.q("html");
    p.dom
        .set_attribute(html, "style", "font-size: 20px")
        .unwrap();
    p.restyle();
    assert_eq!(p.get("#a > p", "width"), "40px");
    let new = append_classed_p(&mut p, "#a");
    let style = p.data.styles_arc(new).unwrap();
    assert_eq!(
        computed::computed_property_by_name(&style, "width").unwrap(),
        "40px"
    );
}
