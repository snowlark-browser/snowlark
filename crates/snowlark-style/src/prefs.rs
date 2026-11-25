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
        use stylo_static_prefs::set_pref;
        // CSS Grid Layout 2 §9 subgrids: `grid-template-columns`/`-rows: subgrid [<line-names>]*`.
        set_pref!("layout.css.grid-template-subgrid-value.enabled", true);
        // Multi-column Layout 1 (layout lays it out in our own block layout).
        set_pref!("layout.css.columns.enabled", true);
        // CSS Writing Modes 4: `writing-mode`, `text-orientation`, `sideways-*`.
        set_pref!("layout.css.writing-mode.enabled", true);
        set_pref!("layout.css.writing-mode.text-orientation.enabled", true);
        set_pref!("layout.css.writing-mode.sideways.enabled", true);
        // CSS Containment 3 container size queries, and `contain`.
        set_pref!("layout.css.container-queries.enabled", true);
        set_pref!("layout.css.contain.enabled", true);
        // Selectors 4 `:has()` and `:nth-child(An+B of S)`; CSS Pseudo 4 `::first-line`.
        set_pref!("layout.css.has-selector.enabled", true);
        set_pref!("layout.css.nth-child-of.enabled", true);
        set_pref!("layout.css.first-line.enabled", true);
        // CSS Cascade 6 `@scope`.
        set_pref!("layout.css.at-scope.enabled", true);
        // CSS Transitions 2 `@starting-style`.
        set_pref!("layout.css.starting-style-at-rules.enabled", true);
        // CSS Cascade 5 §6.2: the transition origin is above every other, important user-agent
        // declarations included.
        set_pref!("layout.css.transitions-override-ua", true);
        // CSS Overflow 4 `line-clamp`, the longhand behind the legacy `-webkit-line-clamp` (layout
        // clamps the legacy form: a vertical `-webkit-box`), and `text-overflow`.
        set_pref!("layout.css.line-clamp.enabled", true);
        set_pref!("layout.css.text-overflow.enabled", true);
        // CSS Fonts 4 variable fonts (`font-variation-settings`, `font-optical-sizing`), and the
        // other font properties stylo gates for servo.
        set_pref!("layout.css.font-variations.enabled", true);
        set_pref!("layout.css.font-size-adjust.enabled", true);
        set_pref!("layout.css.font-synthesis.enabled", true);
        set_pref!("layout.css.font-variant-emoji.enabled", true);
        set_pref!("layout.css.font-variant-caps.all-values.enabled", true);
        // CSS Values 4 §10.9 typed arithmetic (`calc(1px * 2em / 1rem)`, `calc(4px / round(100% /
        // 25px))`): products and quotients of dimensions, valid when the result has the property's
        // type.
        set_pref!("layout.css.calc-typed-arithmetic.enabled", true);
        // CSS Values 5 tree-counting functions `sibling-index()`/`sibling-count()` and `progress()`.
        set_pref!("layout.css.tree-counting-functions.enabled", true);
        set_pref!("layout.css.progress-function.enabled", true);
        // CSS Conditional 5 container style queries (`@container style(--x: 1)`).
        set_pref!("layout.css.style-queries.enabled", true);
        // CSS Conditional 5 `@supports at-rule(@x)`, CSS Conditional 4/5 `font-tech()` and
        // `font-format()`.
        set_pref!("layout.css.supports.at-rule.enabled", true);
        set_pref!("layout.css.font-tech.enabled", true);
        // CSS Fonts 4 `@font-palette-values` (the `font-palette` property is not gated).
        set_pref!("layout.css.font-palette.enabled", true);
        // Media Queries 5 `@custom-media`.
        set_pref!("layout.css.custom-media.enabled", true);
        // The properties Servo's own layout lacks, which stylo's servo build gates: CSS Lists 3
        // counters (the UA sheet's list-item counters), `mask-*`, `backdrop-filter`, `zoom`,
        // `appearance`, `scrollbar-*`, `overscroll-behavior-*`, `text-underline-position`,
        // `scroll-behavior`, ... Browsers parse and compute all of them; Snowlark's layout and
        // renderer read the ones they implement.
        set_pref!("layout.css.counters.enabled", true);
        set_pref!("layout.css.masking.enabled", true);
        set_pref!("layout.css.backdrop-filter.enabled", true);
        set_pref!("layout.css.zoom.enabled", true);
        set_pref!("layout.css.appearance.enabled", true);
        set_pref!("layout.css.scrollbar-styling.enabled", true);
        set_pref!("layout.css.overscroll-behavior.enabled", true);
        set_pref!("layout.css.scroll-behavior.enabled", true);
        set_pref!("layout.css.text-underline-position.enabled", true);
        set_pref!("layout.css.text-decoration-inset.enabled", true);
        set_pref!("layout.css.text-combine-upright.enabled", true);
        set_pref!("layout.css.forced-color-adjust.enabled", true);
        set_pref!("layout.css.dominant-baseline.enabled", true);
        set_pref!("layout.css.math-properties.enabled", true);
        set_pref!("layout.css.motion-path.enabled", true);
        set_pref!("layout.css.corner-shape.enabled", true);
        set_pref!("layout.css.scroll-driven-animations.enabled", true);
        set_pref!("layout.css.view-transitions.enabled", true);
        // CSS Text 3/4 and CSS Inline 3 values and properties stylo gates for servo.
        set_pref!("layout.css.hyphens.enabled", true);
        set_pref!("layout.css.text-box.enabled", true);
        set_pref!("layout.css.text-indent-keywords.enabled", true);
        set_pref!("layout.css.text-wrap-style.balance.enabled", true);
        set_pref!("layout.css.text-wrap-style.pretty.enabled", true);
        set_pref!("layout.css.word-break.break-word.enabled", true);
        set_pref!("layout.css.webkit-text-fill-color.enabled", true);
        set_pref!("layout.css.background-clip.text.enabled", true);
        // CSS Generated Content 3 alternative text after `content` (`content: "x" / "alt"`).
        set_pref!("layout.css.content.alt-text.enabled", true);
        // CSS Images 4 `cross-fade()`, CSS Color 5 `light-dark()` of images.
        set_pref!("layout.css.cross-fade.enabled", true);
        set_pref!("layout.css.light-dark.images.enabled", true);
        // CSS Motion Path 1 `offset-path: url()`, and CSS Shapes 2 `shape()` it can follow.
        set_pref!("layout.css.motion-path-url.enabled", true);
        set_pref!("layout.css.basic-shape-shape.enabled", true);
        // CSS Anchor Positioning 1: `anchor-name`, `position-anchor`, `anchor()`, `anchor-size()`,
        // `@position-try` (layout resolves them where it places absolutely positioned boxes).
        set_pref!("layout.css.anchor-positioning.enabled", true);
    });
}
