// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the style adapter needs besides stylo's computed values: the decoded images CSS `url()`s refer
//! to, and the web fonts `@font-face` rules registered with the text engine under internal names.
//!
//! [`StyleEnv`] is shared by every [`crate::RenderStyle`] handle of a document. The image table is live
//! (behind a lock: a background image that arrives is painted at the next paint without rebuilding
//! boxes); the font table is a snapshot taken when the box tree is built, because `font-family` is lent
//! out as `&str` and a new web font changes shaping anyway (the renderer rebuilds the boxes then).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use snowlark_layout::style::NaturalSize;
use snowlark_paint::{FontStyle, ImageId};

/// A loaded image (a decoded bitmap, or an SVG image's placeholder: see [`crate::svg`]) with its
/// natural dimensions in CSS px (a bitmap's pixels divided by the density the URL was chosen for: a
/// `2x` `srcset` candidate is half its pixel size).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoadedImage {
    pub id: ImageId,
    pub natural: NaturalSize,
    /// The image with its EXIF orientation not applied, for `image-orientation: none` (CSS Images 3
    /// §5.3), when it has one and is same-origin or shared by CORS (a cross-origin image is
    /// otherwise always shown oriented).
    pub unoriented: Option<(ImageId, NaturalSize)>,
    /// The image is cross-origin, shared by its response's CORS headers: [`LoadedImage::unoriented`]
    /// holds only for elements that fetch in CORS mode.
    pub cors_only: bool,
    /// An SVG image's variant for an embedding element of a dark used color scheme, whose document
    /// answers `prefers-color-scheme` so (CSS Color Adjust 1 §2.3).
    pub dark: Option<ImageId>,
    /// Who may read the image's pixels (HTML's origin-clean rules: WebGL texture uploads).
    pub access: PixelAccess,
}

/// Whether script may read an image's pixels: a same-origin image (or a `data:` one), or a
/// cross-origin image whose response shares it by its CORS headers, to an element that fetches in
/// CORS mode (HTML "CORS-same-origin"). Unknown is never readable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PixelAccess {
    #[default]
    None,
    SameOrigin,
    CorsShared,
}

impl PixelAccess {
    /// Whether an element that fetches in CORS mode (`crossorigin` set) or not may read it.
    pub fn readable(self, cors_mode: bool) -> bool {
        match self {
            PixelAccess::SameOrigin => true,
            PixelAccess::CorsShared => cors_mode,
            PixelAccess::None => false,
        }
    }
}

impl LoadedImage {
    /// A bitmap of `width` x `height` CSS px.
    pub fn bitmap(id: ImageId, width: f32, height: f32) -> LoadedImage {
        LoadedImage {
            id,
            natural: NaturalSize::fixed(width, height),
            unoriented: None,
            cors_only: false,
            dark: None,
            access: PixelAccess::None,
        }
    }

    /// An image with these natural dimensions.
    pub fn new(id: ImageId, natural: NaturalSize) -> LoadedImage {
        LoadedImage {
            id,
            natural,
            unoriented: None,
            cors_only: false,
            dark: None,
            access: PixelAccess::None,
        }
    }

    /// The image as `image-orientation` shows it: oriented (`from_image`), else as stored when that
    /// differs and is allowed: for a same-origin image, or one fetched in CORS mode (`cors_mode`;
    /// CSS images are not).
    pub fn oriented(self, from_image: bool, cors_mode: bool) -> LoadedImage {
        match self.unoriented {
            Some((id, natural)) if !from_image && (!self.cors_only || cors_mode) => LoadedImage {
                id,
                natural,
                unoriented: None,
                cors_only: false,
                dark: None,
                access: self.access,
            },
            _ => self,
        }
    }

    /// The image as an element of that used color scheme shows it.
    pub fn for_scheme(self, dark: bool) -> LoadedImage {
        match self.dark {
            Some(id) if dark => LoadedImage { id, ..self },
            _ => self,
        }
    }

    /// The image at a `srcset` density: its natural sizes divided by it (HTML §4.8.4.3.1 "density-
    /// corrected natural width and height").
    pub fn at_density(self, density: f32) -> LoadedImage {
        if density == 1.0 || density <= 0.0 || density.is_nan() {
            return self;
        }
        let scaled = |n: NaturalSize| NaturalSize {
            width: n.width.map(|w| w / density),
            height: n.height.map(|h| h / density),
            ratio: n.ratio,
        };
        LoadedImage {
            id: self.id,
            natural: scaled(self.natural),
            unoriented: self.unoriented.map(|(id, n)| (id, scaled(n))),
            cors_only: self.cors_only,
            dark: self.dark,
            access: self.access,
        }
    }
}

/// Images by absolute URL.
#[derive(Default)]
pub struct ImageTable {
    map: RwLock<HashMap<String, LoadedImage>>,
    /// Where the images' decoded pixels are, for what layout reads of them (`shape-outside`).
    store: std::sync::OnceLock<Arc<snowlark_paint::ImageStore>>,
    /// The SVG images among them (the store has a placeholder for each until it is painted at a
    /// size), and their rasters at their natural sizes once read.
    svgs: RwLock<HashMap<ImageId, Arc<crate::svg::SvgImage>>>,
    svg_rasters: RwLock<HashMap<ImageId, Arc<snowlark_paint::DecodedImage>>>,
}

impl std::fmt::Debug for ImageTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageTable")
            .field("map", &self.map)
            .finish()
    }
}

impl ImageTable {
    /// An image table whose images' pixels are in `store`.
    pub fn with_store(store: Arc<snowlark_paint::ImageStore>) -> ImageTable {
        let table = ImageTable::default();
        let _ = table.store.set(store);
        table
    }

    /// Records that image `id` is the SVG image `image`.
    pub(crate) fn add_svg(&self, id: ImageId, image: Arc<crate::svg::SvgImage>) {
        self.svgs.write().unwrap().insert(id, image);
    }

    /// The pixels of `image`, decoded (an SVG image drawn at its natural size, rounded): its first
    /// frame has four bytes a pixel, alpha last. `None` when the table has no store or the store
    /// no such image.
    pub fn pixels(&self, image: &LoadedImage) -> Option<Arc<snowlark_paint::DecodedImage>> {
        let id = image.id;
        let svg = self.svgs.read().unwrap().get(&id).cloned();
        let Some(svg) = svg else {
            return self.store.get()?.get(id);
        };
        if let Some(r) = self.svg_rasters.read().unwrap().get(&id) {
            return Some(r.clone());
        }
        let (w, h) = (
            image
                .natural
                .width
                .unwrap_or(300.0)
                .round()
                .clamp(1.0, 4096.0),
            image
                .natural
                .height
                .unwrap_or(150.0)
                .round()
                .clamp(1.0, 4096.0),
        );
        let raster = Arc::new(svg.rasterize(
            (w, h),
            (w as u32, h as u32),
            snowlark_paint::ImageRendering::Smooth,
            None,
        )?);
        self.svg_rasters.write().unwrap().insert(id, raster.clone());
        Some(raster)
    }

    pub fn get(&self, url: &str) -> Option<LoadedImage> {
        self.map.read().unwrap().get(url).copied()
    }

    pub fn insert(&self, url: String, image: LoadedImage) {
        self.map.write().unwrap().insert(url, image);
    }

    pub fn len(&self) -> usize {
        self.map.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One loaded `@font-face` face: the internal family name the text engine gave it and the descriptors
/// font matching compares (CSS Fonts 4 §5.2).
#[derive(Clone, Debug, PartialEq)]
pub struct WebFace {
    pub internal: String,
    /// `font-weight` descriptor range (a single weight is `(w, w)`).
    pub weight: (u16, u16),
    /// `font-style` descriptor.
    pub slope: FaceSlope,
    /// `font-width` (`font-stretch`) descriptor range in percent.
    pub width: (f32, f32),
    /// Declaration order among the family's faces.
    pub order: usize,
}

/// The faces of one `@font-face` family name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebFamily {
    /// As written in the rule (matched ASCII case-insensitively).
    pub name: String,
    /// The loaded faces; a family whose faces all failed (or are still loading) has none, and is then
    /// skipped in `font-family` lists so the next family applies.
    pub faces: Vec<WebFace>,
}

/// The `@font-face` families of a document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FontAliases {
    pub families: Vec<WebFamily>,
}

impl FontAliases {
    /// The family named `name` (ASCII case-insensitive), if a `@font-face` rule declares it.
    pub fn family(&self, name: &str) -> Option<&WebFamily> {
        self.families
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case(name))
    }

    pub fn family_mut(&mut self, name: &str) -> &mut WebFamily {
        if let Some(i) = self
            .families
            .iter()
            .position(|f| f.name.eq_ignore_ascii_case(name))
        {
            return &mut self.families[i];
        }
        self.families.push(WebFamily {
            name: name.to_owned(),
            faces: Vec::new(),
        });
        self.families.last_mut().unwrap()
    }
}

/// The shared environment of a document's style handles.
#[derive(Debug, Default)]
pub struct StyleEnv {
    pub fonts: FontAliases,
    pub images: Arc<ImageTable>,
    /// The path data of the shapes `offset-path: url()` names in the document, by id: the DOM is not
    /// at hand where a style's transform is worked out.
    pub motion_paths: Arc<RwLock<HashMap<String, String>>>,
}

/// A face's `font-style` descriptor as font matching sees it (CSS Fonts 4 §5.2): an italic face, or a
/// range of oblique angles in degrees (`normal` is `oblique 0deg`, plain `oblique` is 14deg).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FaceSlope {
    Italic,
    Oblique(f32, f32),
}

/// The `font-style` an element asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WantStyle {
    Normal,
    Italic,
    /// An angle in degrees.
    Oblique(f32),
}

/// What an element's `font-weight`, `font-style` and `font-width` ask of a family's faces.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Wanted {
    pub weight: u16,
    pub style: WantStyle,
    /// In percent.
    pub width: f32,
}

impl WantStyle {
    /// A computed `font-style` (`oblique 0deg` computes to `normal`).
    pub fn of(style: &style::values::computed::FontStyle) -> WantStyle {
        use style::values::computed::FontStyle as S;
        if *style == S::NORMAL {
            WantStyle::Normal
        } else if *style == S::ITALIC {
            WantStyle::Italic
        } else {
            WantStyle::Oblique(style.oblique_degrees())
        }
    }

    /// `font-style` text as a `font` shorthand gives it (`normal`, `italic`, `oblique [<angle>]`).
    pub fn from_text(text: &str) -> WantStyle {
        match parse_style_descriptor(text) {
            FaceSlope::Italic => WantStyle::Italic,
            FaceSlope::Oblique(0.0, _) => WantStyle::Normal,
            FaceSlope::Oblique(a, _) => WantStyle::Oblique(a),
        }
    }
}

impl Wanted {
    /// What an element's computed font properties ask for.
    pub fn of(font: &style::properties::style_structs::Font) -> Wanted {
        Wanted {
            weight: font.font_weight.value().round().clamp(1.0, 1000.0) as u16,
            style: WantStyle::of(&font.font_style),
            width: font.font_width.to_percentage().0 * 100.0,
        }
    }

    /// A normal-width request of one weight and style (`oblique` at its default 14deg).
    pub fn new(weight: u16, style: FontStyle) -> Wanted {
        Wanted {
            weight,
            style: match style {
                FontStyle::Normal => WantStyle::Normal,
                FontStyle::Italic => WantStyle::Italic,
                FontStyle::Oblique => WantStyle::Oblique(14.0),
            },
            width: 100.0,
        }
    }
}

impl WebFace {
    /// A face whose `font-weight`, `font-style` and `font-width` descriptors are given as CSS text (as
    /// `@font-face` rules and `FontFace` objects hold them; empty is the initial value).
    pub fn described(internal: String, weight: &str, style: &str, width: &str) -> WebFace {
        WebFace {
            internal,
            weight: parse_weight_descriptor(weight),
            slope: parse_style_descriptor(style),
            width: parse_width_descriptor(width),
            order: 0,
        }
    }
}

/// `font-weight` descriptor text: one weight or a range (`auto` and the initial value are 400).
pub fn parse_weight_descriptor(text: &str) -> (u16, u16) {
    let one = |t: &str| -> Option<u16> {
        match t.to_ascii_lowercase().as_str() {
            "normal" => Some(400),
            "bold" => Some(700),
            v => v
                .parse::<f32>()
                .ok()
                .map(|w| w.round().clamp(1.0, 1000.0) as u16),
        }
    };
    let parts: Vec<&str> = text.split_ascii_whitespace().collect();
    match parts.as_slice() {
        [a] => one(a).map_or((400, 400), |w| (w, w)),
        // CSS Fonts 4 §4.4: a reversed range is swapped.
        [a, b] => match (one(a), one(b)) {
            (Some(a), Some(b)) => (a.min(b), a.max(b)),
            _ => (400, 400),
        },
        _ => (400, 400),
    }
}

/// An `<angle>` in degrees.
fn parse_angle(text: &str) -> Option<f32> {
    let t = text.to_ascii_lowercase();
    let split = t.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(t.len());
    let (number, unit) = t.split_at(split);
    let v: f32 = number.parse().ok()?;
    Some(match unit {
        "deg" => v,
        "grad" => v * 0.9,
        "rad" => v.to_degrees(),
        "turn" => v * 360.0,
        // (a unitless zero is an angle)
        "" if v == 0.0 => 0.0,
        _ => return None,
    })
}

/// `font-style` descriptor text (CSS Fonts 4 §4.4): `normal`, `italic`, or `oblique` with one angle or
/// a range (14deg without one). The initial value, `auto` and anything unparsable are `normal`.
pub fn parse_style_descriptor(text: &str) -> FaceSlope {
    let normal = FaceSlope::Oblique(0.0, 0.0);
    let mut parts = text.split_ascii_whitespace();
    match parts.next().map(str::to_ascii_lowercase).as_deref() {
        Some("italic") => FaceSlope::Italic,
        Some("oblique") => {
            let angles: Vec<Option<f32>> = parts.map(parse_angle).collect();
            match angles.as_slice() {
                [] => FaceSlope::Oblique(14.0, 14.0),
                [Some(a)] => FaceSlope::Oblique(*a, *a),
                [Some(a), Some(b)] => FaceSlope::Oblique(a.min(*b), a.max(*b)),
                _ => normal,
            }
        }
        _ => normal,
    }
}

/// `font-width` (`font-stretch`) descriptor text in percent: a keyword, a percentage or a range of
/// them. The initial value, `auto` and anything unparsable are 100%.
pub fn parse_width_descriptor(text: &str) -> (f32, f32) {
    let one = |t: &str| -> Option<f32> {
        Some(match t.to_ascii_lowercase().as_str() {
            "ultra-condensed" => 50.0,
            "extra-condensed" => 62.5,
            "condensed" => 75.0,
            "semi-condensed" => 87.5,
            "normal" => 100.0,
            "semi-expanded" => 112.5,
            "expanded" => 125.0,
            "extra-expanded" => 150.0,
            "ultra-expanded" => 200.0,
            v => v.strip_suffix('%')?.parse().ok()?,
        })
    };
    let parts: Vec<&str> = text.split_ascii_whitespace().collect();
    match parts.as_slice() {
        [a] => one(a).map_or((100.0, 100.0), |w| (w, w)),
        [a, b] => match (one(a), one(b)) {
            (Some(a), Some(b)) => (a.min(b), a.max(b)),
            _ => (100.0, 100.0),
        },
        _ => (100.0, 100.0),
    }
}

/// How a face ranks for a request (lower is better): the width first, then the style, then the
/// weight, each a group (the stage of the search that reaches the face) and a distance in that stage.
pub(crate) type FaceRank = ((u8, u32), (u8, u32), u32);

/// A distance in a search stage, as an integer key.
fn dist(d: f32) -> u32 {
    (d.max(0.0) * 1000.0).round() as u32
}

/// CSS Fonts 4 §5.2 step 4, `font-width`: at or below 100%, narrower widths in descending order, then
/// wider ones in ascending order; above 100%, the other way round.
fn width_rank((lo, hi): (f32, f32), want: f32) -> (u8, u32) {
    if lo <= want && want <= hi {
        return (0, 0);
    }
    let narrower = hi < want;
    match (want <= 100.0, narrower) {
        (true, true) => (1, dist(want - hi)),
        (true, false) => (2, dist(lo - want)),
        (false, false) => (1, dist(lo - want)),
        (false, true) => (2, dist(want - hi)),
    }
}

/// CSS Fonts 4 §5.2 step 4, `font-style`, among oblique angles, for a requested angle `a`: from 11deg
/// up, larger angles ascending, then smaller positive ones descending; from 0 below 11deg, smaller
/// positive angles descending, then larger ones ascending; then the angles at or below 0 descending.
/// A negative request is the same with the signs and directions turned round.
fn oblique_rank(lo: f32, hi: f32, a: f32) -> (u8, u32) {
    if a < 0.0 {
        return oblique_rank(-hi, -lo, -a);
    }
    if lo <= a && a <= hi {
        return (0, 0);
    }
    let above = lo > a;
    let positive_below = !above && hi > 0.0;
    if a >= 11.0 {
        if above {
            (1, dist(lo - a))
        } else if positive_below {
            (2, dist(a - hi))
        } else {
            (3, dist(-hi))
        }
    } else if positive_below {
        (1, dist(a - hi))
    } else if above {
        (2, dist(lo - a))
    } else {
        (3, dist(-hi))
    }
}

/// CSS Fonts 4 §5.2 step 4, `font-style`. `italic` takes italic faces, then the oblique ones as for an
/// 11deg request. `normal` takes oblique angles from 0 up, then italic faces, then negative angles.
/// An oblique request never falls back to an italic face before every oblique and normal one (the
/// CSS Working Group's resolution on issue 9389); `oblique 0deg` is `normal`.
fn style_rank(slope: FaceSlope, want: WantStyle) -> (u8, u32) {
    match (want, slope) {
        (WantStyle::Italic, FaceSlope::Italic) => (0, 0),
        (WantStyle::Italic, FaceSlope::Oblique(lo, hi)) => {
            let (g, d) = oblique_rank(lo, hi, 11.0);
            (g + 1, d)
        }
        (WantStyle::Normal, FaceSlope::Italic) => (3, 0),
        (WantStyle::Normal, FaceSlope::Oblique(lo, hi)) => match oblique_rank(lo, hi, 0.0) {
            (3, d) => (4, d),
            r => r,
        },
        (WantStyle::Oblique(_), FaceSlope::Italic) => (5, 0),
        (WantStyle::Oblique(a), FaceSlope::Oblique(lo, hi)) => oblique_rank(lo, hi, a),
    }
}

/// CSS Fonts 4 §5.2 step 4, `font-weight`: from 400 to 500, heavier weights up to 500 ascending, then
/// lighter ones descending, then heavier than 500 ascending; below 400 lighter ones first; above
/// 500 heavier ones first.
fn weight_rank((lo, hi): (u16, u16), weight: u16) -> u32 {
    let w = weight as i32;
    if (lo as i32..=hi as i32).contains(&w) {
        return 0;
    }
    let (below, above) = (w - hi as i32, lo as i32 - w);
    if (400..=500).contains(&weight) {
        if above > 0 && (lo as i32) <= 500 {
            above as u32
        } else if below > 0 {
            1000 + below as u32
        } else {
            2000 + above as u32
        }
    } else if weight < 400 {
        if below > 0 {
            below as u32
        } else {
            1000 + above as u32
        }
    } else if above > 0 {
        above as u32
    } else {
        1000 + below as u32
    }
}

/// CSS Fonts 4 §5.2 step 4: the rank of a face for a request (lower is better). Each property narrows
/// the faces to those closest to the request before the next is looked at, so comparing the three in
/// turn picks the face the algorithm picks.
pub(crate) fn face_rank(face: &WebFace, want: &Wanted) -> FaceRank {
    (
        width_rank(face.width, want.width),
        style_rank(face.slope, want.style),
        weight_rank(face.weight, want.weight),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(weight: &str, style: &str, width: &str) -> WebFace {
        WebFace::described(String::new(), weight, style, width)
    }

    fn want(weight: u16, style: WantStyle, width: f32) -> Wanted {
        Wanted {
            weight,
            style,
            width,
        }
    }

    /// Each descriptor value in `order` ranks before the next one for the request.
    fn assert_order(request: Wanted, order: &[&str], make: impl Fn(&str) -> WebFace) {
        for pair in order.windows(2) {
            let (a, b) = (make(pair[0]), make(pair[1]));
            assert!(
                face_rank(&a, &request) < face_rank(&b, &request),
                "{request:?}: {:?} should rank before {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn weight_matching_follows_css_fonts() {
        let by_weight = |w: &str| face(w, "", "");
        let normal = |w| want(w, WantStyle::Normal, 100.0);
        assert_order(
            normal(400),
            &[
                "400", "450 460", "500", "350 399", "351 398", "501 550", "502 560",
            ],
            by_weight,
        );
        assert_order(
            normal(430),
            &[
                "420 440", "450 460", "500", "400 425", "350 399", "340 398", "501 550", "502 560",
            ],
            by_weight,
        );
        assert_order(
            normal(501),
            &[
                "501", "502 510", "503 520", "500", "450 460", "390 410", "300 350",
            ],
            by_weight,
        );
        assert_order(
            normal(399),
            &[
                "350 399", "340 360", "200 300", "400", "450 460", "500 501", "502 510",
            ],
            by_weight,
        );
    }

    #[test]
    fn width_is_matched_first_narrower_or_wider_by_the_request() {
        let by_width = |w: &str| face("", "", w);
        let width = |w| want(400, WantStyle::Normal, w);
        assert_order(width(100.0), &["100%", "110% 120%", "115% 116%"], by_width);
        assert_order(
            width(110.0),
            &[
                "110% 120%",
                "115% 116%",
                "105%",
                "100%",
                "50% 80%",
                "60% 70%",
            ],
            by_width,
        );
        assert_order(
            width(90.0),
            &["90% 100%", "50% 80%", "60% 70%", "110% 140%", "120% 130%"],
            by_width,
        );
        assert_order(
            width(75.0),
            &["condensed", "semi-condensed", "normal"],
            by_width,
        );
    }

    #[test]
    fn style_is_matched_by_angle_and_oblique_never_prefers_italic() {
        let by_style = |s: &str| face("", s, "");
        let style = |s| want(400, s, 100.0);
        use WantStyle::*;
        assert_order(
            style(Normal),
            &[
                "normal",
                "oblique 10deg 40deg",
                "oblique 20deg 30deg",
                "italic",
                "oblique -50deg -20deg",
                "oblique -40deg -30deg",
            ],
            by_style,
        );
        assert_order(
            style(Italic),
            &[
                "italic",
                "oblique 20deg",
                "oblique 30deg 60deg",
                "oblique 40deg 50deg",
                "oblique 5deg 10deg",
                "oblique 5deg",
                "normal",
                "oblique -60deg -30deg",
                "oblique -50deg -40deg",
            ],
            by_style,
        );
        assert_order(
            style(Oblique(20.0)),
            &[
                "oblique 20deg",
                "oblique 30deg 60deg",
                "oblique 40deg 50deg",
                "oblique 10deg",
                "oblique 0deg",
                "oblique -50deg -20deg",
                "oblique -40deg -30deg",
                "italic",
            ],
            by_style,
        );
        assert_order(
            style(Oblique(10.0)),
            &[
                "oblique 10deg",
                "oblique 5deg",
                "oblique 15deg 20deg",
                "oblique 30deg 60deg",
                "oblique 40deg 50deg",
                "oblique 0deg",
                "oblique -50deg -20deg",
                "oblique -40deg -30deg",
                "italic",
            ],
            by_style,
        );
        assert_order(
            style(Oblique(-10.0)),
            &[
                "oblique -10deg",
                "oblique -5deg",
                "oblique -1deg 0deg",
                "oblique -20deg -15deg",
                "oblique -60deg -30deg",
                "oblique -50deg -40deg",
                "oblique 0deg 10deg",
                "oblique 40deg 50deg",
                "italic",
            ],
            by_style,
        );
        assert_order(
            style(Oblique(-21.0)),
            &[
                "oblique -21deg",
                "oblique -60deg -40deg",
                "oblique -10deg",
                "oblique 0deg",
                "oblique 30deg 60deg",
                "oblique 40deg 50deg",
                "italic",
            ],
            by_style,
        );
        // Plain `oblique` is 14deg; other angle units are converted.
        assert_eq!(
            parse_style_descriptor("oblique"),
            FaceSlope::Oblique(14.0, 14.0)
        );
        assert_eq!(
            parse_style_descriptor("oblique 0.25turn 100grad"),
            FaceSlope::Oblique(90.0, 90.0)
        );
    }

    #[test]
    fn width_outranks_style_and_style_outranks_weight() {
        // The three faces of one family: 125% wide, italic, and weight 350.
        let faces = [
            face("", "", "125%"),
            face("", "italic", ""),
            face("350", "", ""),
        ];
        let best = |w: Wanted| {
            (0..faces.len())
                .min_by_key(|&i| face_rank(&faces[i], &w))
                .unwrap()
        };
        assert_eq!(best(want(400, WantStyle::Italic, 125.0)), 0);
        assert_eq!(best(want(350, WantStyle::Normal, 125.0)), 0);
        assert_eq!(best(want(350, WantStyle::Italic, 100.0)), 1);
    }
}
