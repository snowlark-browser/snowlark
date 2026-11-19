// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Font metrics for stylo's font-relative units (`ex`, `ch`, `cap`, `ic`; CSS Values 4 §6.1.1) from the
//! paint crate's text engine, with the document's web fonts.

use std::sync::{Arc, RwLock};

use smallvec::SmallVec;
use snowlark_paint::{FontQuery, TextEngine};
use style::device::servo::FontMetricsProvider;
use style::font_metrics::FontMetrics;
use style::properties::style_structs::Font;
use style::values::computed::font::{GenericFontFamily, QueryFontMetricsFlags, SingleFontFamily};
use style::values::computed::{CSSPixelLength, Length};

use crate::adapter::{font_stretch, font_style, generic_name};
use crate::env::{face_rank, FontAliases, Wanted};

/// The metrics provider handed to the style engine's device.
pub struct TextMetrics {
    text: Arc<TextEngine>,
    fonts: Arc<RwLock<FontAliases>>,
    default_size: f32,
    monospace_size: f32,
}

impl std::fmt::Debug for TextMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextMetrics").finish_non_exhaustive()
    }
}

impl TextMetrics {
    pub fn new(
        text: Arc<TextEngine>,
        fonts: Arc<RwLock<FontAliases>>,
        default_size: f32,
        monospace_size: f32,
    ) -> TextMetrics {
        TextMetrics {
            text,
            fonts,
            default_size,
            monospace_size,
        }
    }
}

impl FontMetricsProvider for TextMetrics {
    fn query_font_metrics(
        &self,
        vertical: bool,
        font: &Font,
        base_size: CSSPixelLength,
        flags: QueryFontMetricsFlags,
    ) -> FontMetrics {
        let weight = font.font_weight.value().round().clamp(1.0, 1000.0) as u16;
        let style = font_style(&font.font_style);
        let aliases = self.fonts.read().unwrap();
        let mut names: SmallVec<[String; 8]> = SmallVec::new();
        for f in font.font_family.families.iter() {
            match f {
                SingleFontFamily::Generic(g) => names.push(generic_name(*g).to_owned()),
                SingleFontFamily::FamilyName(n) => match aliases.family(&n.name) {
                    Some(family) => {
                        let mut faces: Vec<_> = family.faces.iter().collect();
                        let want = Wanted::of(font);
                        faces.sort_by_key(|f| (face_rank(f, &want), std::cmp::Reverse(f.order)));
                        names.extend(faces.iter().map(|f| f.internal.clone()));
                    }
                    None => names.push(n.name.to_string()),
                },
            }
        }
        drop(aliases);
        let families: SmallVec<[&str; 8]> = names.iter().map(String::as_str).collect();
        let query = FontQuery {
            families: &families,
            weight,
            style,
            stretch: font_stretch(&font.font_width),
        };
        let Ok(list) = self.text.font_list(&query) else {
            return FontMetrics::default();
        };
        let size = base_size.px();
        let m = self.text.metrics(self.text.primary_font(list), size);
        // (the advance in the inline axis: vertical for upright text, CSS Values 4 §6.1.1)
        let advance = |s: &str| {
            if vertical {
                self.text.measure_upright(s, list, size)
            } else {
                self.text.measure(s, list, size)
            }
        };
        let zero = if flags.contains(QueryFontMetricsFlags::NEEDS_CH) {
            advance("0").ok().map(Length::new)
        } else {
            None
        };
        let ic = if flags.contains(QueryFontMetricsFlags::NEEDS_IC) {
            advance("\u{6C34}").ok().map(Length::new)
        } else {
            None
        };
        FontMetrics {
            x_height: (m.x_height > 0.0).then(|| Length::new(m.x_height)),
            zero_advance_measure: zero,
            cap_height: (m.cap_height > 0.0).then(|| Length::new(m.cap_height)),
            ic_width: ic,
            ascent: Length::new(m.ascent),
            script_percent_scale_down: None,
            script_script_percent_scale_down: None,
        }
    }

    fn base_size_for_generic(&self, generic: GenericFontFamily) -> Length {
        Length::new(match generic {
            GenericFontFamily::Monospace => self.monospace_size,
            _ => self.default_size,
        })
    }
}
