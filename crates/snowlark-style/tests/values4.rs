// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! CSS Values 4/5 math functions and tree-counting functions, Media Queries 4/5 features and CSS
//! Conditional 5 container style queries, against stylo's computed values.

mod common;

use common::Page;
use snowlark_style::{supports_condition, DeviceConfig, MediaKind};

/// Media Queries 4/5: the features of a desktop browser window on a standard display.
#[test]
fn media_queries_4_and_5_features() {
    let doc = "<!DOCTYPE html><style>
           p { width: 1px; height: 1px; margin-left: 1px; padding-top: 1px; padding-left: 1px; }
           @media (prefers-reduced-motion: no-preference) and (prefers-contrast: no-preference)
             and (forced-colors: none) and (inverted-colors: none) { p { width: 2px; } }
           @media (color) and (min-color: 8) and (not (monochrome)) and (color-gamut: srgb)
             and (not (color-gamut: p3)) and (dynamic-range: standard) { p { height: 2px; } }
           @media (scripting: enabled) and (update: fast) and (overflow-block: scroll)
             and (display-mode: browser) { p { margin-left: 2px; } }
           @media (prefers-reduced-motion) or (forced-colors) or (inverted-colors)
             { p { padding-top: 9px; } }
           @media (device-aspect-ratio: 4/3) { p { padding-left: 2px; } }
         </style><p>x</p>";
    let p = Page::new(doc);
    assert_eq!(p.get("p", "width"), "2px");
    assert_eq!(p.get("p", "height"), "2px");
    assert_eq!(p.get("p", "margin-left"), "2px");
    // The boolean context of a feature whose value is its "none" value is false.
    assert_eq!(p.get("p", "padding-top"), "1px");
    // The default device is 1024x768.
    assert_eq!(p.get("p", "padding-left"), "2px");
    // Print pages and does not update; without scripting, `scripting: none`.
    let p = Page::with_device(
        "<!DOCTYPE html><style>
           p { width: 1px; height: 1px; }
           @media (overflow-block: paged) and (update: none) { p { width: 2px; } }
           @media (scripting: none) { p { height: 2px; } }
         </style><p>x</p>",
        DeviceConfig {
            media: MediaKind::Print,
            scripting: false,
            ..DeviceConfig::default()
        },
    );
    assert_eq!(p.get("p", "width"), "2px");
    assert_eq!(p.get("p", "height"), "2px");
}

/// `device-width` and `device-height` are the screen's, which a frame's document shares with the
/// top-level one.
#[test]
fn device_size_is_the_screen() {
    let p = Page::with_device(
        "<!DOCTYPE html><style>
           p { width: 1px; height: 1px; }
           @media (device-width: 800px) and (device-height: 600px) { p { width: 2px; } }
           @media (width: 100px) { p { height: 2px; } }
         </style><p>x</p>",
        DeviceConfig {
            viewport: (100.0, 80.0),
            screen: Some((800.0, 600.0)),
            ..DeviceConfig::default()
        },
    );
    assert_eq!(p.get("p", "width"), "2px");
    assert_eq!(p.get("p", "height"), "2px");
}

/// CSS Values 4 §10.9.2: NaN arguments and the argument ranges of `pow()`, `log()`, `round()`
/// and `mod()` give NaN, which a top-level calculation censors to 0.
#[test]
fn math_function_nan_rules() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           #a { z-index: calc(pow(1, infinity) + 5); }
           #b { z-index: calc(log(2, 1) + 6); }
           #c { z-index: calc(pow(NaN, 0) + 7); }
           #d { z-index: calc(round(infinity, NaN) + 8); }
           #e { z-index: calc(pow(-2, 3)); }
           #f { z-index: calc(mod(NaN, 3) + 9); }
         </style><p id=a></p><p id=b></p><p id=c></p><p id=d></p><p id=e></p><p id=f></p>",
    );
    assert_eq!(p.get("#a", "z-index"), "0");
    assert_eq!(p.get("#b", "z-index"), "0");
    assert_eq!(p.get("#c", "z-index"), "0");
    assert_eq!(p.get("#d", "z-index"), "0");
    assert_eq!(p.get("#e", "z-index"), "-8");
    assert_eq!(p.get("#f", "z-index"), "0");
}

/// Absolute lengths convert to px once, in double precision: `12cm` and `round(10cm, 6cm)` are
/// the same length.
#[test]
fn absolute_lengths_convert_in_double_precision() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           #a { margin-left: 12cm; } #b { margin-left: round(10cm, 6cm); }
           #c { margin-left: abs(-1cm); } #d { margin-left: 1cm; }
           #e { width: calc(60% - 50% + 3em); }
         </style><p id=a></p><p id=b></p><p id=c></p><p id=d></p><p id=e></p>",
    );
    assert_eq!(p.get("#a", "margin-left"), p.get("#b", "margin-left"));
    assert_eq!(p.get("#c", "margin-left"), p.get("#d", "margin-left"));
    assert_eq!(p.get("#e", "width"), "calc(10% + 48px)");
}

/// CSS Values 5 §7 tree-counting functions.
#[test]
fn sibling_index_and_count() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           li { z-index: sibling-index(); order: sibling-count(); }
           li { width: calc(10px * sibling-index()); }
         </style><ul><li id=a></li><li id=b></li><li id=c></li></ul>",
    );
    assert_eq!(p.get("#b", "z-index"), "2");
    assert_eq!(p.get("#c", "order"), "3");
    assert_eq!(p.get("#c", "width"), "30px");
}

/// CSS Conditional 5 §4: container style queries on custom properties.
#[test]
fn container_style_queries() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           #box { --theme: dark; }
           p { width: 1px; height: 1px; }
           @container style(--theme: dark) { p { width: 2px; } }
           @container style(--theme: light) { p { height: 2px; } }
         </style><div id=box><p>x</p></div>",
    );
    assert_eq!(p.get("p", "width"), "2px");
    assert_eq!(p.get("p", "height"), "1px");
}

/// CSS Conditional 4/5 `@supports` functions.
#[test]
fn supports_functions() {
    assert!(supports_condition("font-tech(color-COLRv0)"));
    assert!(supports_condition("font-format(woff2)"));
    assert!(!supports_condition("font-format(nonsense)"));
    assert!(supports_condition("at-rule(@container)"));
    assert!(supports_condition("selector(:has(a))"));
}

/// Media Queries 4 §2.4.2: a bare `(color-gamut)` or `(dynamic-range)` is true when a value other
/// than `none` would match, and both have one that does.
#[test]
fn range_keyword_media_features_in_a_boolean_context() {
    let p = Page::new(
        "<!DOCTYPE html><style>
           @media (color-gamut) { #a { z-index: 1; } }
           @media (dynamic-range) { #b { z-index: 2; } }
         </style><p id=a></p><p id=b></p>",
    );
    assert_eq!(p.get("#a", "z-index"), "1");
    assert_eq!(p.get("#b", "z-index"), "2");
}

/// CSS Conditional 5: the `container` shorthand sets the name and the type.
#[test]
fn container_shorthand() {
    let p = Page::new(
        "<!DOCTYPE html><style>#c { container: box / inline-size; }</style><div id=c></div>",
    );
    assert_eq!(p.get("#c", "container-name"), "box");
    assert_eq!(p.get("#c", "container-type"), "inline-size");
}
