// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Color schemes (CSS Color Adjust 1 §2, HTML `<meta name=color-scheme>`) and the scroll bar
//! properties (CSS Scrollbars 1).

mod common;

use common::Page;
use snowlark_style::color_scheme::used_color_scheme;
use snowlark_style::{ColorScheme, DeviceConfig};

fn scheme(p: &Page, selector: &str) -> ColorScheme {
    let values = p
        .engine
        .computed_style(&p.dom, &p.data, p.q(selector))
        .expect("styled");
    used_color_scheme(&values)
}

const BLACK: &str = "rgb(0, 0, 0)";
const WHITE: &str = "rgb(255, 255, 255)";

#[test]
fn the_used_color_scheme_decides_canvas_text_and_initial_color() {
    let p = Page::new(
        "<!DOCTYPE html><style>.dark { color-scheme: dark } .light { color-scheme: light }
           .both { color-scheme: light dark } .initial { color: initial }
           .canvas { background-color: Canvas }</style>
         <div id=n class='initial canvas'></div><div id=d class='dark initial canvas'></div>
         <div id=l class='light initial'></div><div id=b class=both></div>",
    );
    // `normal` without a page color scheme: light, whatever the preference.
    assert_eq!(scheme(&p, ":root"), ColorScheme::Light);
    assert_eq!(p.get(":root", "color"), BLACK);
    assert_eq!(p.get("#n", "color"), BLACK);
    assert_eq!(p.get("#n", "background-color"), WHITE);
    // CSS Color 4: the initial `color` is CanvasText of the element's scheme.
    assert_eq!(scheme(&p, "#d"), ColorScheme::Dark);
    assert_eq!(p.get("#d", "color"), WHITE);
    assert_eq!(p.get("#d", "background-color"), "rgb(18, 18, 18)");
    assert_eq!(p.get("#l", "color"), BLACK);
    assert_eq!(scheme(&p, "#b"), ColorScheme::Light);

    // The preferred scheme picks between two listed ones only.
    let dark = DeviceConfig {
        color_scheme: ColorScheme::Dark,
        ..DeviceConfig::default()
    };
    let p = Page::with_device(
        "<!DOCTYPE html><div id=b style='color-scheme: light dark'></div><div id=n></div>",
        dark,
    );
    assert_eq!(scheme(&p, "#b"), ColorScheme::Dark);
    assert_eq!(scheme(&p, "#n"), ColorScheme::Light);
    assert_eq!(p.get("#n", "color"), BLACK);
}

#[test]
fn links_follow_the_used_color_scheme() {
    // HTML's rendering section: `:link { color: LinkText }`. On a dark canvas the light blue is
    // 1.99:1 against the background; LinkText of the dark scheme is 7.84:1.
    let p = Page::new(
        "<!DOCTYPE html><a id=l href=x>a</a><div style='color-scheme: dark'><a id=d href=x>b</a></div>",
    );
    assert_eq!(p.get("#l", "color"), "rgb(0, 0, 238)");
    assert_eq!(p.get("#d", "color"), "rgb(158, 158, 255)");
}

#[test]
fn the_root_color_scheme_makes_text_light() {
    let p = Page::new("<!DOCTYPE html><style>:root { color-scheme: dark }</style><p>x</p>");
    assert_eq!(scheme(&p, ":root"), ColorScheme::Dark);
    assert_eq!(p.get("p", "color"), WHITE);
    // The root's own background stays transparent (the canvas surface is the embedder's).
    assert_eq!(p.get(":root", "background-color"), "rgba(0, 0, 0, 0)");
}

#[test]
fn meta_color_scheme_sets_the_page_color_scheme() {
    let mut p = Page::new(
        "<!DOCTYPE html><meta id=m name=color-scheme content=dark>
         <div id=d style='color-scheme: light'><div id=n style='color-scheme: normal'></div></div>",
    );
    // `normal` elements take the page color scheme; the root's computed value stays `normal`.
    assert_eq!(scheme(&p, ":root"), ColorScheme::Dark);
    assert_eq!(p.get(":root", "color-scheme"), "normal");
    assert_eq!(p.get(":root", "color"), WHITE);
    assert_eq!(scheme(&p, "#d"), ColorScheme::Light);
    assert_eq!(scheme(&p, "#n"), ColorScheme::Dark);

    // HTML: determined again when the `content` or `name` changes or the element goes.
    let m = p.q("#m");
    p.dom.set_attribute(m, "content", ",,invalid").unwrap();
    p.restyle();
    assert_eq!(scheme(&p, ":root"), ColorScheme::Light);
    assert_eq!(p.get(":root", "color"), BLACK);
    p.dom.set_attribute(m, "content", "only dark").unwrap();
    p.restyle();
    assert_eq!(scheme(&p, "#n"), ColorScheme::Dark);
    p.dom.remove_attribute(m, "name");
    p.restyle();
    assert_eq!(scheme(&p, "#n"), ColorScheme::Light);
    p.dom.set_attribute(m, "name", "COLOR-SCHEME").unwrap();
    p.restyle();
    assert_eq!(scheme(&p, "#n"), ColorScheme::Dark);
    p.dom.remove(m, false);
    p.restyle();
    assert_eq!(scheme(&p, ":root"), ColorScheme::Light);

    // A `meta` inserted later, anywhere in the document tree, counts; the first valid one wins.
    let body = p.q("body");
    let a = p.dom.create_element(p.doc, "meta", None).unwrap();
    p.dom.set_attribute(a, "name", "color-scheme").unwrap();
    p.dom.set_attribute(a, "content", "").unwrap();
    let b = p.dom.create_element(p.doc, "meta", None).unwrap();
    p.dom.set_attribute(b, "name", "color-scheme").unwrap();
    p.dom.set_attribute(b, "content", "dark").unwrap();
    p.dom.append_child(body, a).unwrap();
    p.dom.append_child(body, b).unwrap();
    p.restyle();
    assert_eq!(scheme(&p, ":root"), ColorScheme::Dark);
    assert_eq!(p.get(":root", "color"), WHITE);
}

#[test]
fn scrollbar_properties_compute() {
    let p = Page::new(
        "<!DOCTYPE html><div id=a style='scrollbar-color: red blue; scrollbar-width: thin'>
         <div id=b></div></div>",
    );
    assert_eq!(
        p.get("#a", "scrollbar-color"),
        "rgb(255, 0, 0) rgb(0, 0, 255)"
    );
    assert_eq!(p.get("#a", "scrollbar-width"), "thin");
    // `scrollbar-color` inherits, `scrollbar-width` does not.
    assert_eq!(
        p.get("#b", "scrollbar-color"),
        "rgb(255, 0, 0) rgb(0, 0, 255)"
    );
    assert_eq!(p.get("#b", "scrollbar-width"), "auto");
}
