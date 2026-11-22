// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The document timeline driving CSS transitions and animations: `restyle_at` moves the timeline and
//! the animated values follow it; `restyle` (a script-driven style read) does not move it.

mod common;

use common::Page;

impl Page {
    fn restyle_at(&mut self, now: f64) {
        let reactions = self.dom.take_reactions();
        self.engine
            .process_reactions(&self.dom, &mut self.data, &reactions);
        self.engine.restyle_at(&self.dom, &mut self.data, now);
    }
}

const TRANSITION: &str = "<!DOCTYPE html><style>
    #t { width: 100px; transition: width 1s linear; }
    #t.wide { width: 200px; }
    #c { color: rgb(0, 0, 0); transition: color 2s linear 1s; }
    #c.red { color: rgb(200, 0, 0); }
  </style><div id=t></div><div id=c><span id=inner>x</span></div>";

#[test]
fn transition_values_follow_the_timeline() {
    let mut p = Page::new(TRANSITION);
    p.restyle_at(1.0);
    assert_eq!(p.get("#t", "width"), "100px");
    assert!(!p.engine.animations_active());

    let t = p.q("#t");
    p.dom.set_attribute(t, "class", "wide").unwrap();
    // The style change event at timeline time 1 starts the transition there.
    p.restyle_at(1.0);
    assert_eq!(p.get("#t", "width"), "100px");
    assert!(p.engine.animations_active());
    p.restyle_at(1.25);
    assert_eq!(p.get("#t", "width"), "125px");
    // A script-driven restyle in the same frame keeps the frame's time.
    p.restyle();
    assert_eq!(p.get("#t", "width"), "125px");
    p.restyle_at(1.5);
    assert_eq!(p.get("#t", "width"), "150px");
    p.restyle_at(2.0);
    assert_eq!(p.get("#t", "width"), "200px");
    assert!(!p.engine.animations_active());
    p.restyle_at(5.0);
    assert_eq!(p.get("#t", "width"), "200px");
}

#[test]
fn transition_delay_and_inheritance() {
    let mut p = Page::new(TRANSITION);
    p.restyle_at(0.0);
    let c = p.q("#c");
    p.dom.set_attribute(c, "class", "red").unwrap();
    p.restyle_at(0.0);
    // In the delay the before-change value holds.
    p.restyle_at(0.5);
    assert_eq!(p.get("#c", "color"), "rgb(0, 0, 0)");
    assert!(p.engine.animations_active());
    p.restyle_at(2.0);
    assert_eq!(p.get("#c", "color"), "rgb(100, 0, 0)");
    // Descendants inherit the animated value.
    assert_eq!(p.get("#inner", "color"), "rgb(100, 0, 0)");
    p.restyle_at(3.5);
    assert_eq!(p.get("#c", "color"), "rgb(200, 0, 0)");
    assert_eq!(p.get("#inner", "color"), "rgb(200, 0, 0)");
}

#[test]
fn the_first_style_starts_no_transition() {
    // CSS Transitions 1 §3: an element without a before-change style does not transition.
    let mut p = Page::new(TRANSITION);
    p.restyle_at(0.5);
    assert_eq!(p.get("#t", "width"), "100px");
    assert_eq!(p.get("#c", "color"), "rgb(0, 0, 0)");
    assert!(!p.engine.animations_active());
}

#[test]
fn the_timeline_never_moves_back() {
    let mut p = Page::new(TRANSITION);
    p.restyle_at(2.0);
    p.restyle_at(1.0);
    assert_eq!(p.engine.timeline_time(), 2.0);
}

#[test]
fn keyframes_animation_sampled_over_time() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>
           @keyframes grow { from { width: 0px } 50% { width: 100px } to { width: 300px } }
           #a { width: 7px; animation: grow 2s linear 1s 2 alternate; }
           #f { width: 7px; animation: grow 1s linear forwards; }
         </style><div id=a></div><div id=f></div>",
    );
    // The animations start at the first style, at timeline time 0.
    p.restyle_at(0.0);
    assert!(p.engine.animations_active());
    // In the delay (no backwards fill) the underlying value shows.
    p.restyle_at(0.5);
    assert_eq!(p.get("#a", "width"), "7px");
    // First iteration, a quarter in: halfway to the 50% keyframe.
    p.restyle_at(1.5);
    assert_eq!(p.get("#a", "width"), "50px");
    // Second iteration (alternate: reversed), a quarter in: back between 100% and 50%.
    p.restyle_at(3.5);
    assert_eq!(p.get("#a", "width"), "200px");
    // After the end, no forwards fill: the underlying value again. `#f` keeps its last keyframe.
    p.restyle_at(6.0);
    assert_eq!(p.get("#a", "width"), "7px");
    assert_eq!(p.get("#f", "width"), "300px");
    assert!(!p.engine.animations_active());
    p.restyle_at(7.0);
    assert_eq!(p.get("#f", "width"), "300px");
}

#[test]
fn an_infinite_animation_skips_whole_iterations() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>
           @keyframes grow { from { width: 0px } to { width: 100px } }
           #a { animation: grow 1s linear infinite; }
         </style><div id=a></div>",
    );
    p.restyle_at(0.0);
    p.restyle_at(1000.25);
    assert_eq!(p.get("#a", "width"), "25px");
    assert!(p.engine.animations_active());
}

#[test]
fn removed_elements_lose_their_animations() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>
           @keyframes grow { from { width: 0px } to { width: 100px } }
           #a { animation: grow 1s linear infinite; }
         </style><div id=a></div>",
    );
    p.restyle_at(0.0);
    assert!(p.engine.animations_active());
    let a = p.q("#a");
    p.dom.remove(a, false);
    p.restyle_at(0.5);
    assert!(!p.engine.animations_active());
}

const TRANSITION_ALL: &str = "<!DOCTYPE html><style>
    #t { width: 100px; color: rgb(0, 0, 0); transition: all 1s linear; }
    #t.wide { width: 200px; }
    #t.wide.red { color: rgb(200, 0, 0); }
  </style><div id=t></div>";

/// `transition: all` lists every animatable longhand: the changed ones transition, the others start
/// nothing, and a running transition keeps going through a change that leaves its property alone.
#[test]
fn transition_all_starts_only_what_changed() {
    let mut p = Page::new(TRANSITION_ALL);
    p.restyle_at(1.0);
    let t = p.q("#t");
    p.dom.set_attribute(t, "class", "wide").unwrap();
    p.restyle_at(1.0);
    assert!(p.engine.animations_active());
    p.restyle_at(1.5);
    assert_eq!(p.get("#t", "width"), "150px");
    assert_eq!(p.get("#t", "color"), "rgb(0, 0, 0)");
    // (width's after-change value is still 200px: its transition goes on; color starts its own)
    p.dom.set_attribute(t, "class", "wide red").unwrap();
    p.restyle_at(1.5);
    p.restyle_at(1.75);
    assert_eq!(p.get("#t", "width"), "175px");
    assert_eq!(p.get("#t", "color"), "rgb(50, 0, 0)");
    p.restyle_at(3.0);
    assert_eq!(p.get("#t", "width"), "200px");
    assert_eq!(p.get("#t", "color"), "rgb(200, 0, 0)");
    assert!(!p.engine.animations_active());
}

/// An element with a running transition gets the style of its rules without the transition (kept
/// from its after-change style) with the transition's value applied over it, as the cascade ends
/// with the transitions origin (CSS Cascade 5 §6.1): its other values are not cascaded again.
#[test]
fn a_running_transition_is_applied_over_the_kept_base_style() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>
            #t { width: 100px; color: rgb(1, 2, 3); transition: width 1s linear; }
            #t.wide { width: 200px; }
          </style><div id=t></div>",
    );
    p.restyle_at(1.0);
    let t = p.q("#t");
    p.dom.set_attribute(t, "class", "wide").unwrap();
    p.restyle_at(1.0);
    p.restyle_at(1.25);
    assert_eq!(p.get("#t", "width"), "125px");
    let first = p.data.styles_arc(t).unwrap();
    p.restyle_at(1.5);
    assert_eq!(p.get("#t", "width"), "150px");
    assert_eq!(p.get("#t", "color"), "rgb(1, 2, 3)");
    let second = p.data.styles_arc(t).unwrap();
    // (a cascade makes the struct the color rule sets anew; both frames have the base style's)
    assert!(std::ptr::eq(
        first.get_inherited_text(),
        second.get_inherited_text()
    ));
    p.restyle_at(2.0);
    assert_eq!(p.get("#t", "width"), "200px");
}

/// A transition of a property other values are computed from (here `font-size`, which `em` lengths
/// read) is cascaded with the element's rules: those values follow the transition.
#[test]
fn a_transition_other_values_depend_on_is_cascaded() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>
            #t { font-size: 10px; padding-left: 2em; transition: font-size 1s linear; }
            #t.big { font-size: 20px; }
          </style><div id=t></div>",
    );
    p.restyle_at(1.0);
    let t = p.q("#t");
    p.dom.set_attribute(t, "class", "big").unwrap();
    p.restyle_at(1.0);
    p.restyle_at(1.5);
    assert_eq!(p.get("#t", "font-size"), "15px");
    assert_eq!(p.get("#t", "padding-left"), "30px");
}

/// The transition steps skip a property whose before-change and after-change computed values are
/// equal (stylo patch 39) only when its animated values must be equal too: same zoom and font size.
#[test]
fn transition_values_compare_equal_only_with_the_same_zoom_and_font_size() {
    use style::properties::animated_properties::AnimationValue;
    use style::properties::{LonghandId, PropertyDeclarationId};
    let p = Page::new(
        "<!DOCTYPE html><style>
          div { width: 100px; color: rgb(0, 0, 0); --x: 1; }
          #wide { width: 200px; }
          #big { font-size: 30px; }
          #zoomed { width: 50px; zoom: 2; }
        </style><div id=a></div><div id=b></div><div id=wide></div><div id=big></div>
        <div id=zoomed></div>",
    );
    let style = |s: &str| p.data.styles_arc(p.q(s)).expect("styled");
    let (a, b) = (style("#a"), style("#b"));
    let width = PropertyDeclarationId::Longhand(LonghandId::Width);
    let color = PropertyDeclarationId::Longhand(LonghandId::Color);
    assert!(AnimationValue::computed_values_equal(width, &a, &b));
    assert!(AnimationValue::computed_values_equal(color, &a, &b));
    assert!(!AnimationValue::computed_values_equal(
        width,
        &a,
        &style("#wide")
    ));
    // (equal widths, but `text-decoration-inset: auto` and the like follow the font size)
    assert!(!AnimationValue::computed_values_equal(
        width,
        &a,
        &style("#big")
    ));
    // (the same zoomed computed width, but lengths animate unzoomed)
    let zoomed = style("#zoomed");
    assert_eq!(zoomed.get_position().width, a.get_position().width);
    assert!(!AnimationValue::computed_values_equal(width, &a, &zoomed));
    let custom = style::custom_properties::Name::from("x");
    assert!(!AnimationValue::computed_values_equal(
        PropertyDeclarationId::Custom(&custom),
        &a,
        &b
    ));
}

/// CSS Animations 2 §3.3: `animation-duration: auto` parses, alone and in the shorthand, and
/// resolves to 0s on the document timeline.
#[test]
fn an_auto_duration_parses_and_resolves_to_zero_seconds() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>
        #a { animation: grow auto linear forwards; }
        #b { animation-name: grow; animation-duration: auto; }
      </style><div id=a></div><div id=b></div>",
    );
    p.restyle_at(0.0);
    assert_eq!(p.get("#a", "animation-name"), "grow");
    assert_eq!(p.get("#a", "animation-fill-mode"), "forwards");
    assert_eq!(p.get("#a", "animation-duration"), "0s");
    assert_eq!(p.get("#b", "animation-name"), "grow");
    assert_eq!(p.get("#b", "animation-duration"), "0s");
}
