// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Cascade, specificity, importance, inheritance, blockification, presentational hints, quirks and the
//! user-agent sheet.

mod common;

use common::Page;

#[test]
fn form_control_frames_compute_as_the_rendering_section_expects() {
    // Text fields: `padding: 1px 2px; border: 2px inset`, buttons `padding: 1px 6px; border: 2px
    // outset`, no rounded corners (the widget's own frame is painted while the page leaves border and
    // background alone); a page that restyles only the border keeps the padding.
    let p = Page::new(
        "<!DOCTYPE html><input id=t><input id=s type=submit><button id=b></button><textarea></textarea>\
         <select id=sel></select><input id=c type=color><input id=x type=checkbox>\
         <input id=styled style='border: 1px solid #ccc'>",
    );
    for (sel, padding, border, style) in [
        ("#t", ("1px", "2px"), "2px", "inset"),
        ("#s", ("1px", "6px"), "2px", "outset"),
        ("#b", ("1px", "6px"), "2px", "outset"),
        ("textarea", ("2px", "2px"), "1px", "solid"),
        ("#sel", ("1px", "0px"), "1px", "solid"),
        ("#c", ("1px", "2px"), "1px", "solid"),
        ("#x", ("0px", "0px"), "0px", "none"),
        ("#styled", ("1px", "2px"), "1px", "solid"),
    ] {
        assert_eq!(p.get(sel, "padding-top"), padding.0, "{sel}");
        assert_eq!(p.get(sel, "padding-left"), padding.1, "{sel}");
        assert_eq!(p.get(sel, "border-top-width"), border, "{sel}");
        assert_eq!(p.get(sel, "border-top-style"), style, "{sel}");
        assert_eq!(p.get(sel, "border-top-left-radius"), "0px", "{sel}");
    }
}

#[test]
fn ua_sheet_defaults() {
    let p = Page::new(
        "<!DOCTYPE html><html><head><title>t</title></head><body><div id=d></div><span id=s></span>\
         <h1>h</h1><ul><li>x</li></ul><table><tr><td>c</td><th>h</th></tr></table><a href=x>l</a>\
         <b>b</b><pre>p</pre><input id=i><dialog id=dg></dialog><p hidden id=hid></p>\
         <noscript id=ns>n</noscript><details><summary id=sm>s</summary></details></body></html>",
    );
    assert_eq!(p.get("head", "display"), "none");
    assert_eq!(p.get("body", "display"), "block");
    assert_eq!(p.get("#d", "display"), "block");
    assert_eq!(p.get("#s", "display"), "inline");
    assert_eq!(p.get("body", "margin-top"), "8px");
    assert_eq!(p.get("body", "margin"), "8px");
    assert_eq!(p.get("h1", "font-size"), "32px");
    assert_eq!(p.get("h1", "font-weight"), "700");
    assert_eq!(p.get("h1", "margin-top"), "21.44px");
    assert_eq!(p.get("li", "display"), "list-item");
    assert_eq!(p.get("ul", "list-style-type"), "disc");
    assert_eq!(p.get("ul", "padding-left"), "40px");
    assert_eq!(p.get("table", "display"), "table");
    assert_eq!(p.get("table", "border-spacing"), "2px");
    assert_eq!(p.get("td", "display"), "table-cell");
    assert_eq!(p.get("td", "padding-top"), "1px");
    assert_eq!(p.get("th", "text-align"), "center");
    assert_eq!(p.get("a", "color"), "rgb(0, 0, 238)");
    assert_eq!(p.get("a", "text-decoration-line"), "underline");
    assert_eq!(p.get("b", "font-weight"), "700");
    assert_eq!(p.get("pre", "font-family"), "monospace");
    assert_eq!(p.get("pre", "white-space-collapse"), "preserve");
    assert_eq!(p.get("#i", "display"), "inline-block");
    assert_eq!(p.get("#dg", "display"), "none");
    assert_eq!(p.get("#hid", "display"), "none");
    assert_eq!(p.get("#ns", "display"), "none");
    assert_eq!(p.get("#sm", "display"), "list-item");
}

/// CSS Display 3 §2.5 "Unboxing": `display: contents` on an element that cannot be unboxed (a
/// replaced element, a form control, a line break; an SVG element other than a nested `svg`, `a`,
/// `g`, `use`, `tspan`, `textPath`) computes to `none`; on any other element, and on a nested `svg`,
/// it stays `contents`.
#[test]
fn display_contents_on_unusual_elements_computes_to_none() {
    let p = Page::new(
        "<!DOCTYPE html><style>.c { display: contents }</style>         <div class=c id=div></div><span class=c id=span></span><button class=c id=button></button>         <input class=c id=input><textarea class=c id=textarea></textarea><select class=c id=select></select>         <img class=c id=img><video class=c id=video></video><canvas class=c id=canvas></canvas>         <iframe class=c id=iframe></iframe><br class=c id=br><progress class=c id=progress></progress>         <svg class=c id=top><g class=c id=g></g><rect class=c id=rect></rect>         <svg class=c id=nested></svg></svg>",
    );
    for id in ["div", "span", "button", "g", "nested"] {
        assert_eq!(p.get(&format!("#{id}"), "display"), "contents", "{id}");
    }
    for id in [
        "input", "textarea", "select", "img", "video", "canvas", "iframe", "br", "progress", "top",
        "rect",
    ] {
        assert_eq!(p.get(&format!("#{id}"), "display"), "none", "{id}");
    }
}

#[test]
fn specificity_order_and_importance() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           #x { color: rgb(1, 1, 1); }
           .c { color: rgb(2, 2, 2); }
           div { color: rgb(3, 3, 3); }
           .later { width: 10px; }
           .later { width: 20px; }
           .imp { height: 5px !important; }
           #y { height: 6px; }
           .styled { margin-left: 1px !important; }
         </style>
         <div id=x class=c></div>
         <div class=c></div>
         <div class=later></div>
         <div id=y class=imp style='height: 7px'></div>
         <div id=z class=styled style='margin-left: 2px; margin-right: 3px'></div>",
    );
    assert_eq!(p.get("#x", "color"), "rgb(1, 1, 1)");
    assert_eq!(p.get("div.c:not(#x)", "color"), "rgb(2, 2, 2)");
    assert_eq!(p.get(".later", "width"), "20px");
    // !important in a sheet beats the style attribute and a more specific normal rule.
    assert_eq!(p.get("#y", "height"), "5px");
    assert_eq!(p.get("#z", "margin-left"), "1px");
    assert_eq!(p.get("#z", "margin-right"), "3px");
}

#[test]
fn author_sheets_apply_in_document_order() {
    let p = Page::new(
        "<!DOCTYPE html><head><style>p { color: rgb(1, 0, 0); }</style></head>
         <body><p>x</p><style>p { color: rgb(0, 2, 0); }</style></body>",
    );
    assert_eq!(p.get("p", "color"), "rgb(0, 2, 0)");
}

#[test]
fn style_media_attribute() {
    let p = Page::new(
        "<!DOCTYPE html><style media='print'>p { color: rgb(1, 0, 0); }</style>
         <style media='screen and (min-width: 500px)'>p { width: 3px; }</style><p>x</p>",
    );
    assert_eq!(p.get("p", "color"), "rgb(0, 0, 0)");
    assert_eq!(p.get("p", "width"), "3px");
}

#[test]
fn inheritance() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           #outer { color: rgb(9, 8, 7); font-size: 20px; border: 1px solid; padding: 3px;
                    visibility: hidden; }
           #explicit { padding: inherit; color: initial; }
           #unset { color: unset; padding: unset; }
         </style>
         <div id=outer><span id=inner><em id=deep>x</em></span><span id=explicit></span>
         <span id=unset></span></div>",
    );
    assert_eq!(p.get("#deep", "color"), "rgb(9, 8, 7)");
    assert_eq!(p.get("#deep", "font-size"), "20px");
    assert_eq!(p.get("#deep", "visibility"), "hidden");
    // Non-inherited properties take their initial value.
    assert_eq!(p.get("#inner", "padding-top"), "0px");
    assert_eq!(p.get("#inner", "border-top-style"), "none");
    assert_eq!(p.get("#explicit", "padding-top"), "3px");
    assert_eq!(p.get("#explicit", "color"), "rgb(0, 0, 0)");
    assert_eq!(p.get("#unset", "color"), "rgb(9, 8, 7)");
    assert_eq!(p.get("#unset", "padding-top"), "0px");
}

#[test]
fn blockification() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           .float { float: left; }
           .abs { position: absolute; }
           .flex { display: flex; }
           .grid { display: grid; }
           .inline-table { display: inline-table; }
         </style>
         <span class=float id=f></span>
         <span class=abs id=a></span>
         <span class=inline-table id=it></span>
         <div class=flex><span id=fi></span></div>
         <div class=grid><span id=gi></span></div>",
    );
    assert_eq!(p.get("#f", "display"), "block");
    assert_eq!(p.get("#a", "display"), "block");
    assert_eq!(p.get("#fi", "display"), "block");
    assert_eq!(p.get("#gi", "display"), "block");
    assert_eq!(p.get(".grid", "display"), "grid");
    // A floated inline-table becomes a table; absolutely positioned leaves it table too.
    assert_eq!(p.get("#it", "display"), "inline-table");
    // The root element is blockified.
    let p = Page::new("<!DOCTYPE html><style>html { display: inline; }</style>");
    assert_eq!(p.get("html", "display"), "block");
}

#[test]
fn presentational_hints_rank_below_author_rules() {
    let p = Page::new(
        "<!DOCTYPE html><style>.w { width: 9px; }</style>
         <body bgcolor=red text='#00ff00' marginwidth=3>
         <table border=2 width=50% cellspacing=4 id=t><tr><td align=center id=c>x</td></tr></table>
         <img width=100 height=50 id=im><img width=100 height=50 class=w id=im2>
         <font color=blue size=5 face=Arial id=fo>f</font><hr size=1 id=hr>
         <p align=right id=p>r</p><center id=ce><span id=cs>c</span></center></body>",
    );
    assert_eq!(p.get("body", "background-color"), "rgb(255, 0, 0)");
    assert_eq!(p.get("body", "color"), "rgb(0, 255, 0)");
    assert_eq!(p.get("body", "margin-left"), "3px");
    assert_eq!(p.get("body", "margin-top"), "8px");
    assert_eq!(p.get("#t", "width"), "50%");
    assert_eq!(p.get("#t", "border-top-width"), "2px");
    assert_eq!(p.get("#t", "border-top-style"), "outset");
    assert_eq!(p.get("#t", "border-spacing"), "4px");
    assert_eq!(p.get("#c", "border-top-style"), "inset");
    assert_eq!(p.get("#c", "text-align"), "-webkit-center");
    assert_eq!(p.get("#im", "width"), "100px");
    assert_eq!(p.get("#im", "aspect-ratio"), "auto 100 / 50");
    assert_eq!(p.get("#im2", "width"), "9px");
    assert_eq!(p.get("#fo", "color"), "rgb(0, 0, 255)");
    assert_eq!(p.get("#fo", "font-size"), "24px");
    assert_eq!(p.get("#fo", "font-family"), "Arial");
    assert_eq!(p.get("#hr", "border-bottom-width"), "0px");
    assert_eq!(p.get("#p", "text-align"), "right");
    // HTML §15.3.3: `center` centers and aligns its descendants (the legacy keyword, inherited).
    assert_eq!(p.get("#ce", "text-align"), "-webkit-center");
    assert_eq!(p.get("#cs", "text-align"), "-webkit-center");
}

#[test]
fn table_border_zero_has_no_outset_style() {
    let p = Page::new("<!DOCTYPE html><table border=0><tr><td>x</td></tr></table>");
    assert_eq!(p.get("table", "border-top-style"), "none");
    assert_eq!(p.get("td", "border-top-style"), "none");
}

#[test]
fn quirks_mode_rules() {
    // No doctype: quirks mode. Tables reset inherited font properties, and class selectors match
    // ASCII case-insensitively.
    let p = Page::new(
        "<style>body { font-size: 30px; } .Foo { color: rgb(1, 2, 3); }</style>
         <body><table><tr><td id=c>x</td></tr></table><p class=foo>q</p>
         <li id=li>x</li><ul><li id=nested>y</li></ul></body>",
    );
    assert_eq!(p.get("#c", "font-size"), "16px");
    assert_eq!(p.get("p", "color"), "rgb(1, 2, 3)");
    assert_eq!(p.get("#li", "list-style-position"), "inside");
    // `:is(dir, menu, ol, ul) :is(..., li) { list-style-position: unset; }` wins inside a list.
    assert_eq!(p.get("#nested", "list-style-position"), "outside");
    let p = Page::new(
        "<!DOCTYPE html><style>body { font-size: 30px; } .Foo { color: rgb(1, 2, 3); }</style>
         <body><table><tr><td id=c>x</td></tr></table><p class=foo>q</p></body>",
    );
    assert_eq!(p.get("#c", "font-size"), "30px");
    assert_eq!(p.get("p", "color"), "rgb(0, 0, 0)");
}

#[test]
fn computed_values_of_shorthands_and_custom_properties() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           #a { margin: 1px 2px 3px 4px; border: 2px dashed rgb(1, 2, 3); --x: 5px; --empty:; }
         </style><div id=a></div>",
    );
    assert_eq!(p.get("#a", "margin"), "1px 2px 3px 4px");
    assert_eq!(p.get("#a", "border-top"), "2px dashed rgb(1, 2, 3)");
    assert_eq!(p.get("#a", "--x"), "5px");
    assert_eq!(p.get("#a", "--undefined"), "");
    assert_eq!(p.get("#a", "MARGIN-TOP"), "1px");
}

#[test]
fn font_palette_computes_and_interpolates() {
    // CSS Fonts 4 §9.1-9.2: font-palette is inherited; a palette-mix()'s calc() percentages
    // resolve and its omitted ones are filled in at computed-value time.
    let p = Page::new(
        "<!DOCTYPE html><style>
           :root { font-size: 16px }
           #a { font-palette: --x }
           #b { font-palette: palette-mix(in srgb, dark calc(10%), light) }
           #c { font-palette: palette-mix(in srgb, dark calc(50%), light) }
           #d { font-palette: palette-mix(light calc(sign(1rem - 1px) * 10%), dark calc(sign(1rem - 1px) * 40%)) }
         </style><div id=a><span id=i></span></div><div id=b></div><div id=c></div><div id=d></div>",
    );
    assert_eq!(p.get("#i", "font-palette"), "--x");
    assert_eq!(
        p.get("#b", "font-palette"),
        "palette-mix(in srgb, dark 10%, light 90%)"
    );
    assert_eq!(
        p.get("#c", "font-palette"),
        "palette-mix(in srgb, dark, light)"
    );
    assert_eq!(
        p.get("#d", "font-palette"),
        "palette-mix(light 10%, dark 40%)"
    );
    // Interpolation is palette-mix(in oklab, A, B p) between the end points (§9.1).
    use style::values::animated::{Animate, Procedure};
    use style::values::computed::font::FontPalette;
    use style_traits::ToCss;
    let at = |from: &FontPalette, to: &FontPalette, progress: f64| {
        from.animate(to, Procedure::Interpolate { progress })
            .unwrap()
            .to_css_string()
    };
    let (light, dark) = (FontPalette::light(), FontPalette::dark());
    assert_eq!(at(&light, &dark, 0.3), "palette-mix(light 70%, dark 30%)");
    assert_eq!(at(&light, &dark, 0.0), "light");
    assert_eq!(at(&light, &dark, -2.0), "light");
    assert_eq!(at(&light, &dark, 1.5), "dark");
    assert_eq!(at(&light, &light, 0.5), "light");
    assert!(light.animate(&dark, Procedure::Add).is_err());
}

#[test]
fn table_cellpadding_pads_its_cells() {
    // HTML §15.3.8: a table's cellpadding is a pixel padding hint on the cells whose table it is
    // (through row groups), not on a nested table's cells; changing it restyles the cells.
    let mut p = Page::new(
        "<!DOCTYPE html><table cellpadding=0 id=t><tr><td id=a>x
           <table><tr><td id=n>y</td></tr></table></td></tr>
           <tbody><tr><th id=b>z</th></tr></tbody></table>",
    );
    assert_eq!(p.get("#a", "padding-top"), "0px");
    assert_eq!(p.get("#b", "padding-left"), "0px");
    assert_eq!(p.get("#n", "padding-top"), "1px");
    let t = p.q("#t");
    p.dom.set_attribute(t, "cellpadding", "5").unwrap();
    p.restyle();
    assert_eq!(p.get("#a", "padding-right"), "5px");
    assert_eq!(p.get("#b", "padding-bottom"), "5px");
    assert_eq!(p.get("#n", "padding-top"), "1px");
}
