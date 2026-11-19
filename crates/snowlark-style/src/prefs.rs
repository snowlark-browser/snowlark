// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! stylo's feature switches. stylo gates CSS features that Firefox had not shipped when it was cut (grid,
//! multi-column, container queries, `:has()`, `@scope`, writing modes...) behind process-wide preferences
//! that default to off; they decide what parses, so they are set once, before the first sheet is parsed.

use std::sync::Once;

/// Turns on the CSS features Snowlark supports. Idempotent; every entry point of the crate that parses CSS
/// calls it first.
pub fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // CSS Grid Layout 1 and Multi-column Layout 1 (layout lays them out through taffy and our own
        // block layout).
        stylo_static_prefs::set_pref!("layout.grid.enabled", true);
        stylo_static_prefs::set_pref!("layout.columns.enabled", true);
        // CSS Writing Modes 4: `writing-mode`, `text-orientation`.
        stylo_static_prefs::set_pref!("layout.writing-mode.enabled", true);
        // CSS Containment 3 container size queries.
        stylo_static_prefs::set_pref!("layout.container-queries.enabled", true);
        // Selectors 4 `:has()` and `:nth-child(An+B of S)`.
        stylo_static_prefs::set_pref!("layout.css.has-selector.enabled", true);
        stylo_static_prefs::set_pref!("layout.css.nth-child-of.enabled", true);
        // CSS Cascade 6 `@scope`.
        stylo_static_prefs::set_pref!("layout.css.at-scope.enabled", true);
        // CSS Transitions 2 `@starting-style`.
        stylo_static_prefs::set_pref!("layout.css.starting-style-at-rules.enabled", true);
        // CSS Overflow 4 `line-clamp`, the longhand behind the legacy `-webkit-line-clamp` (layout
        // clamps the legacy form: a vertical `-webkit-box`).
        stylo_static_prefs::set_pref!("layout.css.line-clamp.enabled", true);
        // CSS Fonts 4 variable fonts (`font-variation-settings`, `font-optical-sizing`).
        stylo_static_prefs::set_pref!("layout.variable_fonts.enabled", true);
        // CSS Values 4 §10.9 typed arithmetic (`calc(1px * 2em / 1rem)`, `calc(4px / round(100% /
        // 25px))`): products and quotients of dimensions, valid when the result has the property's
        // type.
        stylo_static_prefs::set_pref!("layout.css.calc-typed-arithmetic.enabled", true);
        // CSS Values 5 tree-counting functions `sibling-index()`/`sibling-count()` and `progress()`.
        stylo_static_prefs::set_pref!("layout.css.tree-counting-functions.enabled", true);
        stylo_static_prefs::set_pref!("layout.css.progress-function.enabled", true);
        // CSS Conditional 5 container style queries (`@container style(--x: 1)`).
        stylo_static_prefs::set_pref!("layout.css.style-queries.enabled", true);
        // CSS Conditional 5 `@supports at-rule(@x)`, CSS Conditional 4/5 `font-tech()` and
        // `font-format()`.
        stylo_static_prefs::set_pref!("layout.css.supports.at-rule.enabled", true);
        stylo_static_prefs::set_pref!("layout.css.font-tech.enabled", true);
        // CSS Fonts 4 `@font-palette-values` (the `font-palette` property is not gated).
        stylo_static_prefs::set_pref!("layout.css.font-palette.enabled", true);
        // Media Queries 5 `@custom-media`.
        stylo_static_prefs::set_pref!("layout.css.custom-media.enabled", true);
        // The properties Servo's own layout lacks, which stylo's servo build gates together:
        // `counter-reset`/`counter-increment` (CSS Lists 3 counters; the UA sheet's list-item
        // counters), `text-overflow`, `contain`, `mask-*`, `backdrop-filter`, `zoom`,
        // `appearance`, `text-orientation`, `scrollbar-*`, `overscroll-behavior-*`,
        // `text-underline-*`, ... Browsers parse and compute all of them; Snowlark's layout and
        // renderer read the ones they implement.
        stylo_static_prefs::set_pref!("layout.unimplemented", true);
        // CSS Images 4 `cross-fade()`, CSS Color 5 `light-dark()` of images.
        stylo_static_prefs::set_pref!("layout.css.cross-fade.enabled", true);
        stylo_static_prefs::set_pref!("layout.css.light-dark.images.enabled", true);
        // CSS Motion Path 1 `offset-path: url()`, and CSS Shapes 2 `shape()` it can follow.
        stylo_static_prefs::set_pref!("layout.css.motion-path-url.enabled", true);
        stylo_static_prefs::set_pref!("layout.css.basic-shape-shape.enabled", true);
        // CSS Values 5 tree-counting functions, `sibling-index()` and `sibling-count()`.
        stylo_static_prefs::set_pref!("layout.css.tree-counting-functions.enabled", true);
    });
}
