// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Linux text engine's tests: the Windows engine's, with the fonts of a Linux system
//! (Liberation, DejaVu, Noto Sans CJK and Noto Color Emoji; `docs/linux.md` lists the packages).

use super::*;
use crate::display_list::DisplayListBuilder;
use crate::geom::{Color, Point};

fn engine() -> TextEngine {
    TextEngine::new().expect("fontconfig")
}

fn list(e: &TextEngine, families: &[&str], weight: u16) -> FontListId {
    e.font_list(&FontQuery {
        families,
        weight,
        style: FontStyle::Normal,
        stretch: FontStretch::NORMAL,
    })
    .unwrap()
}

fn shape(e: &TextEngine, text: &str, l: FontListId, size: f32) -> ShapedText {
    let mut out = ShapedText::default();
    e.shape(
        text,
        l,
        &ShapeOptions {
            size,
            ..Default::default()
        },
        &mut out,
    )
    .unwrap();
    out
}

/// Splitting the shaping of "AVA" after "A" keeps the "AV" kerning in the first part, and loses
/// no advance.
#[test]
fn a_shaping_splits_into_the_parts_of_its_text() {
    let e = engine();
    let sans = list(&e, &["Liberation Sans"], 400);
    let options = ShapeOptions {
        size: 100.0,
        ..Default::default()
    };
    let mut whole = ShapedText::default();
    e.shape("AVA", sans, &options, &mut whole).unwrap();
    let mut alone = ShapedText::default();
    e.shape("A", sans, &options, &mut alone).unwrap();
    let parts = whole.split(&[1, 3]).unwrap();
    assert_eq!(parts.len(), 2);
    assert!(
        parts[0].width() < alone.width() - 1.0,
        "{} {}",
        parts[0].width(),
        alone.width()
    );
    assert!((parts[0].width() + parts[1].width() - whole.width()).abs() < 0.001);
    assert_eq!(parts[1].clusters.len(), 2);
    assert_eq!(parts[1].clusters[0].text, (0, 1));
    assert_eq!(parts[1].runs[0].text, (0, 2));
    assert_eq!(parts[0].clusters[0].break_after, BreakAfter::Allowed);
}

/// A query resolves to the same list whatever the case of its family names (CSS Fonts 4 §5.1), and
/// to another list when a family, its order or a descriptor differs.
#[test]
fn font_lists_are_found_by_their_query() {
    let e = engine();
    let a = list(&e, &["Nonexistent Family", "Arial", "DejaVu Serif"], 400);
    assert_eq!(
        list(&e, &["NONEXISTENT family", "arial", "DEJAVU serif"], 400),
        a
    );
    assert_ne!(
        list(&e, &["Nonexistent Family", "DejaVu Serif", "Arial"], 400),
        a
    );
    assert_ne!(
        list(&e, &["Nonexistent Family", "Arial", "DejaVu Serif"], 700),
        a
    );
}

/// A missing family is skipped (fontconfig always matches something, so the match must be the
/// family asked for); a metric-compatible alias fontconfig binds strongly (Liberation Sans for
/// Arial) counts as the family; the weight is matched; generic families resolve.
#[test]
fn family_list_skips_missing_families_and_matches_weight() {
    let e = engine();
    assert!(!e.has_family("No Such Font 123"));
    assert!(e.has_family("Arial"));
    let l = list(&e, &["No Such Font 123", "DejaVu Serif", "serif"], 700);
    let font = e.primary_font(l);
    assert_eq!(e.family_name(font).as_deref(), Some("DejaVu Serif"));
    assert_eq!(e.weight(font), Some(700));
    let arial = e.primary_font(list(&e, &["Arial"], 400));
    assert_eq!(e.family_name(arial).as_deref(), Some("Liberation Sans"));
    let mono = e.primary_font(list(&e, &["monospace"], 400));
    assert!(e.family_name(mono).is_some());
    assert_ne!(mono, arial);
}

/// A local font is found by its family name, as an installed one is (and a web font is not).
#[test]
fn local_fonts_are_found_by_family_name() {
    let e = engine();
    let ahem = include_bytes!("../../../snowlark-layout/tests/fixtures/Ahem.ttf");
    let web = e.add_font_data(ahem).unwrap();
    assert_ne!(web, "Ahem");
    assert!(!e.has_family("Ahem"));
    assert_eq!(e.add_local_font(ahem).unwrap(), "Ahem");
    let l = list(&e, &["ahem", "Arial"], 400);
    assert_eq!(e.family_name(e.primary_font(l)).as_deref(), Some("Ahem"));
    // Ahem's glyphs are 1em squares.
    let shaped = shape(&e, "XX", l, 20.0);
    assert!((shaped.width() - 40.0).abs() < 0.01, "{}", shaped.width());
    let m = e.metrics(e.primary_font(l), 20.0);
    assert!(
        (m.ascent - 16.0).abs() < 0.01 && (m.descent - 4.0).abs() < 0.01,
        "{m:?}"
    );
}

/// Bidi isolates and other default-ignorable characters draw nothing and take no room.
#[test]
fn default_ignorables_are_invisible() {
    let e = engine();
    let l = list(&e, &["DejaVu Sans"], 400);
    let plain = shape(&e, "in r/videos", l, 16.0);
    let isolated = shape(&e, "in \u{2068}r/videos\u{2069}\u{200B}", l, 16.0);
    assert!(
        (plain.width() - isolated.width()).abs() < 0.01,
        "{} {}",
        plain.width(),
        isolated.width()
    );
    let hidden = isolated
        .clusters
        .iter()
        .filter(|c| c.advance == 0.0)
        .count();
    assert_eq!(hidden, 3);
}

#[test]
fn metrics_scale_with_size() {
    let e = engine();
    let font = e.primary_font(list(&e, &["DejaVu Sans"], 400));
    let m = e.metrics(font, 20.0);
    let m2 = e.metrics(font, 40.0);
    assert!(m.ascent > 15.0 && m.ascent < 25.0, "{m:?}");
    assert!((m2.ascent - 2.0 * m.ascent).abs() < 1e-3);
    assert!(m.x_height > 0.0 && m.x_height < m.cap_height && m.cap_height < m.ascent);
    assert!(
        m.underline_offset > 0.0 && m.underline_thickness > 0.0,
        "{m:?}"
    );
    assert!(m.strikeout_offset < 0.0, "{m:?}");
    assert!(e.space_advance(font, 20.0) > 4.0);
}

#[test]
fn shaping_gives_clusters_breaks_and_widths() {
    let e = engine();
    let l = list(&e, &["DejaVu Sans"], 400);
    let s = shape(&e, "Hello wide world", l, 16.0);
    assert_eq!(s.runs.len(), 1);
    assert_eq!(s.clusters.len(), 16);
    let total: f32 = s.clusters.iter().map(|c| c.advance).sum();
    assert!((total - s.width()).abs() < 1e-3 && s.width() > 80.0);
    // breaks after the spaces, not inside words
    assert_eq!(s.clusters[5].break_after, BreakAfter::Allowed);
    assert!(s.clusters[5].whitespace);
    assert_eq!(s.clusters[2].break_after, BreakAfter::Never);
    assert_eq!(s.clusters[0].text, (0, 1));
    assert_eq!(s.clusters[15].break_after, BreakAfter::Allowed);
    let newline = shape(&e, "a\nb", l, 16.0);
    assert_eq!(newline.clusters[1].break_after, BreakAfter::Mandatory);
    let soft = shape(&e, "co\u{AD}op", l, 16.0);
    assert!(soft.clusters[2].soft_hyphen);
    assert_eq!(soft.clusters[2].break_after, BreakAfter::Allowed);
}

#[test]
fn ligatures_and_kerning_follow_features() {
    let e = engine();
    let l = list(&e, &["DejaVu Sans"], 400);
    let mut with = ShapedText::default();
    e.shape("ffi AV", l, &ShapeOptions::default(), &mut with)
        .unwrap();
    let mut without = ShapedText::default();
    let off = [FontFeature::new(b"liga", 0), FontFeature::new(b"kern", 0)];
    e.shape(
        "ffi AV",
        l,
        &ShapeOptions {
            features: &off,
            ..Default::default()
        },
        &mut without,
    )
    .unwrap();
    // DejaVu Sans ligates "ffi" into one glyph: one cluster for three characters
    assert!(with.glyphs.len() < without.glyphs.len());
    assert_eq!(with.clusters[0].text, (0, 3));
    // kerning pulls "AV" together
    assert!(with.width() < without.width());
}

#[test]
fn fallback_covers_cjk_arabic_and_emoji() {
    let e = engine();
    let l = list(&e, &["Liberation Sans"], 400);
    let text = "Hi 你好 مرحبا 😀";
    let s = shape(&e, text, l, 16.0);
    let families: Vec<String> = s
        .runs
        .iter()
        .map(|r| e.family_name(r.font).unwrap())
        .collect();
    assert!(
        families.iter().any(|f| f != "Liberation Sans"),
        "{families:?}"
    );
    assert!(s.runs.iter().any(|r| r.bidi_level == 1), "Arabic is RTL");
    let emoji = s.runs.last().unwrap();
    assert!(emoji.color, "{families:?}");
    // no .notdef glyphs
    assert!(
        s.glyphs.iter().all(|g| *g != 0),
        "{families:?} {:?}",
        s.glyphs
    );
    // the clusters cover the text in logical order
    assert_eq!(s.clusters.last().unwrap().text.1 as usize, text.len());
    assert!(s.clusters.windows(2).all(|c| c[0].text.1 == c[1].text.0));
}

#[test]
fn family_list_fallback_prefers_later_families() {
    let e = engine();
    // Liberation Mono has no CJK; the list's next family must supply it before system fallback
    let l = list(&e, &["Liberation Mono", "Noto Sans CJK JP"], 400);
    let s = shape(&e, "ab日本", l, 16.0);
    let names: Vec<String> = s
        .runs
        .iter()
        .map(|r| e.family_name(r.font).unwrap())
        .collect();
    assert_eq!(names, vec!["Liberation Mono", "Noto Sans CJK JP"]);
}

/// Right-to-left runs keep their glyphs in logical order with positive advances, and paint in
/// visual order.
#[test]
fn paint_emits_runs_in_visual_order() {
    let e = engine();
    let l = list(&e, &["DejaVu Sans"], 400);
    let s = shape(&e, "abc مرحبا def", l, 16.0);
    let rtl = s.runs.iter().find(|r| r.bidi_level == 1).unwrap();
    let cl = &s.clusters[rtl.clusters.0 as usize..rtl.clusters.1 as usize];
    assert!(cl.windows(2).all(|c| c[0].text.1 <= c[1].text.0), "{cl:?}");
    assert!(s.advances.iter().all(|a| *a >= 0.0));
    let mut b = DisplayListBuilder::new();
    let w = s.paint(
        0..s.clusters.len(),
        Point::new(10.0, 50.0),
        Color::BLACK,
        &mut b,
    );
    assert!((w - s.width()).abs() < 1e-3);
    let list = b.build();
    let xs: Vec<f32> = list
        .items
        .iter()
        .filter_map(|i| match i {
            crate::Item::Text(t) => Some(t.origin.x),
            _ => None,
        })
        .collect();
    assert!(xs.windows(2).all(|p| p[0] < p[1]), "{xs:?}");
    assert_eq!(xs[0], 10.0);
}

#[test]
fn web_fonts_get_unique_names() {
    let e = engine();
    let font = e.primary_font(list(&e, &["DejaVu Serif"], 400));
    let data = e.font_data(font).unwrap().bytes.to_vec();
    let a = e.add_font_data(&data).unwrap();
    let b = e.add_font_data(&data).unwrap();
    assert_ne!(a, b);
    assert!(a.starts_with(WEB_PREFIX));
    let font = e.primary_font(list(&e, &[a.as_str()], 400));
    assert_eq!(e.family_name(font).as_deref(), Some("DejaVu Serif"));
    let s = shape(&e, "Web font", list(&e, &[a.as_str()], 400), 20.0);
    assert!(s.width() > 50.0);
    assert!(e.add_font_data(b"not a font").is_err());
}

/// `text-decoration-skip-ink`: a descender reaches into a band below the baseline, an "x" does not.
#[test]
fn glyph_intercepts_follow_the_outline() {
    let e = engine();
    let l = list(&e, &["DejaVu Sans"], 400);
    let font = e.primary_font(l);
    let p = shape(&e, "p", l, 20.0).glyphs[0];
    let x = shape(&e, "x", l, 20.0).glyphs[0];
    let (left, right) = e.glyph_intercept(font, 20.0, p, 2.0, 3.0).unwrap();
    assert!(
        left >= 0.0 && right > left && right < 20.0,
        "{left} {right}"
    );
    assert!(e.glyph_intercept(font, 20.0, x, 2.0, 3.0).is_none());
}
