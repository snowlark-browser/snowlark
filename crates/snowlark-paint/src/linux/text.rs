// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Text on Linux: font matching and fallback through fontconfig, shaping, metrics and glyph
//! outlines through HarfBuzz, bidi levels by UAX #9 (`unicode-bidi`), scripts by UAX #24
//! (`unicode-script`) and line-break opportunities by UAX #14 (`icu_segmenter`). It fills the same
//! types as the Windows engine ([`snowlark_graphics::text`]), so layout measures and paints the
//! same way on both. The raster side draws the glyphs from the same font bytes ([`FontData`]).
//!
//! Font matching (CSS Fonts 4 §5): a family is resolved through fontconfig's substitution rules and
//! accepted only when the face it matches belongs to the family asked for or to one fontconfig binds
//! strongly to it (a metric-compatible alias such as Liberation Sans for Arial); otherwise the
//! family counts as missing and the next one of the list is tried. A character none of the list's
//! faces has falls back to the first font of fontconfig's sorted list for the text's language that
//! has it (cached per language).
//!
//! Thread safety: a `TextEngine` is `Send + Sync`. HarfBuzz faces and fonts are made immutable
//! before they are shared; every fontconfig call is made under one lock.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

#[path = "ffi.rs"]
mod ffi;

use ffi::*;

use crate::display_list::{FontId, GlyphOffset};
use crate::text_cache::{SharedTextCaches, WordCache};
use crate::web_font::FontLoadError;
pub use snowlark_graphics::text::*;
use snowlark_graphics::{Error, Result};

/// The error of the text engine's calls.
pub type TextEngineError = Error;

/// The prefix of the internal family names given to web fonts.
const WEB_PREFIX: &str = "__snowlark_web_";

/// A font file's bytes (mapped from disk, or a copy of a font loaded from memory), shared by the
/// text engine and the raster side.
pub struct FontBytes {
    blob: *mut hb_blob_t,
    data: *const u8,
    len: usize,
}

// SAFETY: an hb_blob_t is reference counted atomically and the engine never makes one writable;
// its data is immutable for its whole life.
unsafe impl Send for FontBytes {}
// SAFETY: as above: shared reads of immutable bytes.
unsafe impl Sync for FontBytes {}

impl FontBytes {
    /// Maps a font file.
    fn from_file(path: &CStr) -> Option<FontBytes> {
        // SAFETY: a NUL-terminated path; the blob returned is owned (null on failure).
        let blob = unsafe { hb_blob_create_from_file_or_fail(path.as_ptr()) };
        FontBytes::from_blob(blob)
    }

    /// Takes ownership of `bytes`.
    fn from_vec(bytes: Vec<u8>) -> Option<FontBytes> {
        unsafe extern "C" fn release(user: *mut c_void) {
            // SAFETY: `user` is the box made below, released once by HarfBuzz.
            drop(unsafe { Box::from_raw(user.cast::<Vec<u8>>()) });
        }
        let len = c_uint::try_from(bytes.len()).ok()?;
        let boxed = Box::new(bytes);
        let data = boxed.as_ptr().cast::<c_char>();
        let user = Box::into_raw(boxed).cast::<c_void>();
        // SAFETY: the data stays valid until HarfBuzz calls `release`, which it does exactly once
        // (also when the creation fails).
        let blob = unsafe {
            hb_blob_create_or_fail(data, len, HB_MEMORY_MODE_READONLY, user, Some(release))
        };
        FontBytes::from_blob(blob)
    }

    fn from_blob(blob: *mut hb_blob_t) -> Option<FontBytes> {
        if blob.is_null() {
            return None;
        }
        let mut len = 0;
        // SAFETY: an owned, non-null blob.
        let data = unsafe { hb_blob_get_data(blob, &mut len) };
        Some(FontBytes {
            blob,
            data: data.cast(),
            len: len as usize,
        })
    }
}

impl std::ops::Deref for FontBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: the blob's data, valid and unchanged while the blob is held.
        unsafe { std::slice::from_raw_parts(self.data, self.len) }
    }
}

impl Drop for FontBytes {
    fn drop(&mut self) {
        // SAFETY: the reference taken when this was made.
        unsafe { hb_blob_destroy(self.blob) }
    }
}

/// What the raster side needs to draw a face's glyphs as the engine shaped them.
#[derive(Clone)]
pub struct FontData {
    pub bytes: Arc<FontBytes>,
    /// The face's index in its file.
    pub index: u32,
    /// The variation axes' values (a variable font's instance).
    pub variations: Vec<([u8; 4], f32)>,
    /// Synthesized bold (fontconfig asks for it when no bold face exists).
    pub embolden: bool,
    /// Synthesized oblique.
    pub oblique: bool,
}

/// A face the engine handed out an id for.
struct Face {
    data: FontData,
    hb_face: *mut hb_face_t,
    /// Immutable, at the face's design units.
    font: *mut hb_font_t,
    upem: f32,
    /// Metrics in design units.
    metrics: FontMetrics,
    color: bool,
    family: String,
    weight: u16,
    vertical_variants: bool,
}

// SAFETY: the face's HarfBuzz face and font are made immutable before the `Face` is shared, and
// HarfBuzz documents immutable objects as safe to use from several threads; they are destroyed only
// when the last reference goes.
unsafe impl Send for Face {}
// SAFETY: as above.
unsafe impl Sync for Face {}

impl Drop for Face {
    fn drop(&mut self) {
        // SAFETY: the references taken when the face was made.
        unsafe {
            hb_font_destroy(self.font);
            hb_face_destroy(self.hb_face);
        }
    }
}

impl Face {
    fn glyph(&self, c: char) -> Option<u32> {
        let mut g = 0;
        // SAFETY: an immutable font; the output is a local.
        (unsafe { hb_font_get_nominal_glyph(self.font, c as u32, &mut g) } != 0).then_some(g)
    }
}

struct FontList {
    /// The matched face of each family that exists, in the list's order.
    faces: Vec<FontId>,
    primary: FontId,
}

/// A [`FontQuery`] as a font list's key: family names compare ASCII case-insensitively.
#[derive(PartialEq, Eq, Hash)]
struct ListKey {
    families: Box<[Box<str>]>,
    weight: u16,
    style: FontStyle,
    stretch: u8,
}

impl ListKey {
    fn of(query: &FontQuery) -> ListKey {
        ListKey {
            families: query
                .families
                .iter()
                .map(|f| f.to_ascii_lowercase().into())
                .collect(),
            weight: query.weight,
            style: query.style,
            stretch: query.stretch.0,
        }
    }
}

/// Where a face was loaded from: what tells two faces apart.
#[derive(PartialEq, Eq, Hash, Clone)]
enum FaceKey {
    File(CString, c_int, bool, bool),
    Memory(usize),
}

#[derive(Default)]
struct State {
    faces: Vec<Arc<Face>>,
    by_key: HashMap<FaceKey, FontId>,
    lists: Vec<FontList>,
    list_keys: HashMap<ListKey, FontListId>,
    /// Web fonts, by the number in their internal name.
    web: Vec<FontId>,
    /// Fonts registered as if installed ([`TextEngine::add_local_font`]): family and face.
    local: Vec<(String, FontId)>,
    memory_fonts: usize,
}

/// One font of a language's fallback list.
struct FallbackFont {
    file: CString,
    index: c_int,
    /// Owned by the list's font set.
    charset: *const FcCharSet,
}

/// The fonts fontconfig sorts for a language, for per-character fallback.
struct FallbackSet {
    set: *mut FcFontSet,
    fonts: Vec<FallbackFont>,
    by_char: HashMap<u32, Option<FontId>>,
}

impl Drop for FallbackSet {
    fn drop(&mut self) {
        if !self.set.is_null() {
            // SAFETY: the set this owns; the charsets in `fonts` point into it and go with it.
            unsafe { FcFontSetDestroy(self.set) }
        }
    }
}

/// fontconfig's state: every call is made under the lock that holds this.
struct Fc {
    config: *mut FcConfig,
    fallback: HashMap<String, FallbackSet>,
    /// Families by (lowercase name, weight, style, stretch).
    families: HashMap<(String, u16, FontStyle, u8), Option<FontId>>,
}

impl Drop for Fc {
    fn drop(&mut self) {
        self.fallback.clear();
        // SAFETY: the configuration this owns.
        unsafe { FcConfigDestroy(self.config) }
    }
}

/// Text through fontconfig and HarfBuzz: see the module documentation.
pub struct TextEngine {
    fc: Mutex<Fc>,
    state: RwLock<State>,
    pub(crate) shared_text: SharedTextCaches,
    font_db: OnceLock<Arc<fontdb::Database>>,
    /// Glyph outlines as line segments in design units (y up), for `glyph_intercept`.
    outlines: Mutex<HashMap<(u32, u16), Outline>>,
}

/// A glyph's outline as line segments (x0, y0, x1, y1).
type Outline = Arc<Vec<[f32; 4]>>;

// SAFETY: the HarfBuzz objects the engine holds are immutable once shared (faces and fonts are made
// immutable before they are stored), and HarfBuzz documents immutable objects as safe to use from
// several threads; every fontconfig object is reached only under the `fc` lock.
unsafe impl Send for TextEngine {}
// SAFETY: as above.
unsafe impl Sync for TextEngine {}

/// Families pages name most that Linux systems rarely have, but whose metric-compatible stand-ins
/// fontconfig binds to them: the font database ([`TextEngine::font_database`]) knows them by these
/// names too.
const ALIASED: [&str; 8] = [
    "Arial",
    "Helvetica",
    "Times New Roman",
    "Times",
    "Courier New",
    "Courier",
    "Comic Sans MS",
    "Impact",
];

/// A CSS generic family: the family a browser's default settings name for it, if any, and the
/// fontconfig generic that stands in when no font is that family or bound to it. The defaults are
/// the usual ones of Linux browsers (Times New Roman, Arial, Comic Sans MS, Impact, which
/// fontconfig's rules bind to the metric-compatible Liberation faces and their like where those
/// fonts are not installed), so a page lays out with the metrics it gets on other systems.
fn generic_family(name: &str) -> Option<(Option<&'static str>, &'static str)> {
    Some(match name.to_ascii_lowercase().as_str() {
        "serif" | "ui-serif" => (Some("Times New Roman"), "serif"),
        "sans-serif" => (Some("Arial"), "sans-serif"),
        "system-ui" | "ui-sans-serif" | "-apple-system" | "ui-rounded" => (None, "sans-serif"),
        "monospace" | "ui-monospace" => (None, "monospace"),
        "cursive" => (Some("Comic Sans MS"), "cursive"),
        "fantasy" => (Some("Impact"), "fantasy"),
        "emoji" => (None, "emoji"),
        "math" => (None, "math"),
        _ => return None,
    })
}

/// fontconfig's weight for a CSS weight.
fn fc_weight(weight: u16) -> f64 {
    // SAFETY: a pure function.
    unsafe { FcWeightFromOpenTypeDouble(weight.clamp(1, 1000) as f64) }
}

fn fc_slant(style: FontStyle) -> c_int {
    match style {
        FontStyle::Normal => FC_SLANT_ROMAN,
        FontStyle::Italic => FC_SLANT_ITALIC,
        FontStyle::Oblique => FC_SLANT_OBLIQUE,
    }
}

/// fontconfig's width for OpenType's `usWidthClass`.
fn fc_width(stretch: FontStretch) -> c_int {
    [50, 63, 75, 87, 100, 113, 125, 150, 200][stretch.0.clamp(1, 9) as usize - 1]
}

fn obj(name: &[u8]) -> *const c_char {
    name.as_ptr().cast()
}

/// The strings of a pattern's object, in order.
///
/// # Safety
/// `p` must be a valid pattern.
unsafe fn pattern_strings(p: *const FcPattern, object: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0.. {
        let mut s = std::ptr::null_mut();
        // SAFETY: per the caller; the string is owned by the pattern and copied at once.
        if unsafe { FcPatternGetString(p, obj(object), i, &mut s) } != FC_RESULT_MATCH {
            break;
        }
        out.push(
            unsafe { CStr::from_ptr(s.cast()) }
                .to_string_lossy()
                .into_owned(),
        );
    }
    out
}

/// The strongly bound strings of a pattern's object (what was asked for, and what fontconfig's
/// rules made equivalent to it).
///
/// # Safety
/// `p` must be a valid pattern.
unsafe fn strong_strings(p: *const FcPattern, object: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0.. {
        let mut v = FcValue {
            kind: 0,
            u: FcValueUnion { i: 0 },
        };
        let mut binding = 0;
        // SAFETY: per the caller; the string is owned by the pattern and copied at once.
        if unsafe { FcPatternGetWithBinding(p, obj(object), i, &mut v, &mut binding) }
            != FC_RESULT_MATCH
        {
            break;
        }
        if binding != FC_VALUE_BINDING_WEAK && v.kind == FC_TYPE_STRING {
            // SAFETY: a string value (checked by its type tag).
            let s = unsafe { CStr::from_ptr(v.u.s.cast()) };
            out.push(s.to_string_lossy().into_owned());
        }
    }
    out
}

/// A face's file, index, and whether fontconfig asks to embolden or slant it.
///
/// # Safety
/// `p` must be a valid pattern.
unsafe fn pattern_face(p: *const FcPattern) -> Option<FaceKey> {
    let mut file = std::ptr::null_mut();
    // SAFETY: per the caller; outputs are locals and the string is copied.
    unsafe {
        if FcPatternGetString(p, obj(FC_FILE), 0, &mut file) != FC_RESULT_MATCH {
            return None;
        }
        let file = CStr::from_ptr(file.cast()).to_owned();
        let mut index = 0;
        FcPatternGetInteger(p, obj(FC_INDEX), 0, &mut index);
        let mut embolden = 0;
        FcPatternGetBool(p, obj(FC_EMBOLDEN), 0, &mut embolden);
        let mut matrix = std::ptr::null_mut();
        let oblique = FcPatternGetMatrix(p, obj(FC_MATRIX), 0, &mut matrix) == FC_RESULT_MATCH
            && !matrix.is_null()
            && (*matrix).xy != 0.0;
        Some(FaceKey::File(file, index, embolden != 0, oblique))
    }
}

/// A NUL-terminated copy (names with a NUL in them are cut there).
fn c_string(s: &str) -> CString {
    CString::new(s.split('\0').next().unwrap_or("")).unwrap_or_default()
}

/// Draws a glyph's outline into line segments.
struct OutlineSink {
    segments: Vec<[f32; 4]>,
}

unsafe extern "C" fn draw_move_to(
    _: *mut hb_draw_funcs_t,
    _: *mut c_void,
    _: *mut hb_draw_state_t,
    _: f32,
    _: f32,
    _: *mut c_void,
) {
}

unsafe extern "C" fn draw_line_to(
    _: *mut hb_draw_funcs_t,
    data: *mut c_void,
    st: *mut hb_draw_state_t,
    x: f32,
    y: f32,
    _: *mut c_void,
) {
    // SAFETY: HarfBuzz passes the sink given to `hb_font_draw_glyph` and its own draw state.
    let (sink, st) = unsafe { (&mut *data.cast::<OutlineSink>(), &*st) };
    sink.segments.push([st.current_x, st.current_y, x, y]);
}

unsafe extern "C" fn draw_quadratic_to(
    _: *mut hb_draw_funcs_t,
    data: *mut c_void,
    st: *mut hb_draw_state_t,
    cx: f32,
    cy: f32,
    x: f32,
    y: f32,
    _: *mut c_void,
) {
    // SAFETY: as in `draw_line_to`.
    let (sink, st) = unsafe { (&mut *data.cast::<OutlineSink>(), &*st) };
    let (x0, y0) = (st.current_x, st.current_y);
    let mut prev = (x0, y0);
    for i in 1..=8 {
        let t = i as f32 / 8.0;
        let u = 1.0 - t;
        let p = (
            u * u * x0 + 2.0 * u * t * cx + t * t * x,
            u * u * y0 + 2.0 * u * t * cy + t * t * y,
        );
        sink.segments.push([prev.0, prev.1, p.0, p.1]);
        prev = p;
    }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn draw_cubic_to(
    _: *mut hb_draw_funcs_t,
    data: *mut c_void,
    st: *mut hb_draw_state_t,
    c1x: f32,
    c1y: f32,
    c2x: f32,
    c2y: f32,
    x: f32,
    y: f32,
    _: *mut c_void,
) {
    // SAFETY: as in `draw_line_to`.
    let (sink, st) = unsafe { (&mut *data.cast::<OutlineSink>(), &*st) };
    let (x0, y0) = (st.current_x, st.current_y);
    let mut prev = (x0, y0);
    for i in 1..=8 {
        let t = i as f32 / 8.0;
        let u = 1.0 - t;
        let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
        let p = (
            a * x0 + b * c1x + c * c2x + d * x,
            a * y0 + b * c1y + c * c2y + d * y,
        );
        sink.segments.push([prev.0, prev.1, p.0, p.1]);
        prev = p;
    }
}

unsafe extern "C" fn draw_close_path(
    _: *mut hb_draw_funcs_t,
    data: *mut c_void,
    st: *mut hb_draw_state_t,
    _: *mut c_void,
) {
    // SAFETY: as in `draw_line_to`.
    let (sink, st) = unsafe { (&mut *data.cast::<OutlineSink>(), &*st) };
    sink.segments
        .push([st.current_x, st.current_y, st.path_start_x, st.path_start_y]);
}

/// The draw callbacks, made once.
struct DrawFuncs(*mut hb_draw_funcs_t);
// SAFETY: immutable once made (`hb_draw_funcs_make_immutable`).
unsafe impl Send for DrawFuncs {}
// SAFETY: as above.
unsafe impl Sync for DrawFuncs {}

fn draw_funcs() -> *mut hb_draw_funcs_t {
    static FUNCS: OnceLock<DrawFuncs> = OnceLock::new();
    FUNCS
        .get_or_init(|| {
            // SAFETY: a new object, set up and made immutable before it is shared.
            unsafe {
                let f = hb_draw_funcs_create();
                let none = std::ptr::null_mut();
                hb_draw_funcs_set_move_to_func(f, draw_move_to, none, None);
                hb_draw_funcs_set_line_to_func(f, draw_line_to, none, None);
                hb_draw_funcs_set_quadratic_to_func(f, draw_quadratic_to, none, None);
                hb_draw_funcs_set_cubic_to_func(f, draw_cubic_to, none, None);
                hb_draw_funcs_set_close_path_func(f, draw_close_path, none, None);
                hb_draw_funcs_make_immutable(f);
                DrawFuncs(f)
            }
        })
        .0
}

/// Characters that break a line wherever they are (UAX #14 classes BK, CR, LF, NL).
fn is_hard_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r' | '\u{0B}' | '\u{0C}' | '\u{85}' | '\u{2028}' | '\u{2029}'
    )
}

/// White space for layout (spaces, tabs, line breaks), not the no-break spaces.
fn is_space(c: char) -> bool {
    c.is_whitespace() && !matches!(c, '\u{A0}' | '\u{2007}' | '\u{202F}')
}

/// Characters that stay in the font of the character before them: marks, joiners, selectors,
/// format and control characters.
fn follows_previous(c: char) -> bool {
    let u = c as u32;
    is_mark(c)
        || (u < 0x10000 && crate::text::is_default_ignorable_bmp(u as u16))
        || (0xE0000..=0xE0FFF).contains(&u)
        || c.is_control()
}

/// Whether a code point of the BMP is Default_Ignorable_Code_Point (Unicode DerivedCoreProperties).
pub(crate) fn is_default_ignorable_bmp(u: u16) -> bool {
    matches!(
        u,
        0x00AD
            | 0x034F
            | 0x061C
            | 0x115F..=0x1160
            | 0x17B4..=0x17B5
            | 0x180B..=0x180F
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x206F
            | 0x3164
            | 0xFE00..=0xFE0F
            | 0xFEFF
            | 0xFFA0
            | 0xFFF0..=0xFFF8
    )
}

/// Characters shown as emoji by default (Unicode's Emoji_Presentation, by block: the pictograph,
/// emoticon, transport and supplemental symbol blocks and the regional indicators), which take a
/// color font before a text font that has them.
fn is_emoji_presentation(c: char) -> bool {
    matches!(c as u32,
        0x1F004 | 0x1F0CF | 0x1F18E | 0x1F191..=0x1F19A | 0x1F1E6..=0x1F1FF | 0x1F201
        | 0x1F21A | 0x1F22F | 0x1F232..=0x1F236 | 0x1F238..=0x1F23A | 0x1F250..=0x1F251
        | 0x1F300..=0x1F320 | 0x1F32D..=0x1F335 | 0x1F337..=0x1F37C | 0x1F37E..=0x1F393
        | 0x1F3A0..=0x1F3CA | 0x1F3CF..=0x1F3D3 | 0x1F3E0..=0x1F3F0 | 0x1F3F4 | 0x1F3F8..=0x1F43E
        | 0x1F440 | 0x1F442..=0x1F4FC | 0x1F4FF..=0x1F53D | 0x1F54B..=0x1F54E | 0x1F550..=0x1F567
        | 0x1F57A | 0x1F595..=0x1F596 | 0x1F5A4 | 0x1F5FB..=0x1F64F | 0x1F680..=0x1F6C5
        | 0x1F6CC | 0x1F6D0..=0x1F6D2 | 0x1F6D5..=0x1F6D7 | 0x1F6DC..=0x1F6DF | 0x1F6EB..=0x1F6EC
        | 0x1F6F4..=0x1F6FC | 0x1F7E0..=0x1F7EB | 0x1F7F0 | 0x1F90C..=0x1F93A | 0x1F93C..=0x1F945
        | 0x1F947..=0x1F9FF | 0x1FA70..=0x1FAFF
        | 0x231A..=0x231B | 0x23E9..=0x23EC | 0x23F0 | 0x23F3 | 0x25FD..=0x25FE | 0x2614..=0x2615
        | 0x2648..=0x2653 | 0x267F | 0x2693 | 0x26A1 | 0x26AA..=0x26AB | 0x26BD..=0x26BE
        | 0x26C4..=0x26C5 | 0x26CE | 0x26D4 | 0x26EA | 0x26F2..=0x26F3 | 0x26F5 | 0x26FA | 0x26FD
        | 0x2705 | 0x270A..=0x270B | 0x2728 | 0x274C | 0x274E | 0x2753..=0x2755 | 0x2757
        | 0x2795..=0x2797 | 0x27B0 | 0x27BF | 0x2B1B..=0x2B1C | 0x2B50 | 0x2B55)
}

/// A run to shape: bytes `start..end` of the text in one font, script, level and orientation.
struct RunSpec {
    start: usize,
    end: usize,
    font: FontId,
    level: u8,
    script: unicode_script::Script,
    upright: bool,
}

impl TextEngine {
    pub fn new() -> Result<TextEngine> {
        // SAFETY: loads the user's configuration; the result is owned (null on failure).
        let config = unsafe { FcInitLoadConfigAndFonts() };
        if config.is_null() {
            return Err(Error::Platform("fontconfig has no configuration".into()));
        }
        Ok(TextEngine {
            fc: Mutex::new(Fc {
                config,
                fallback: HashMap::new(),
                families: HashMap::new(),
            }),
            state: RwLock::new(State::default()),
            shared_text: Default::default(),
            font_db: OnceLock::new(),
            outlines: Mutex::new(HashMap::new()),
        })
    }

    /// The system font database (fontdb's, with every file mapped once: see
    /// [`crate::font_files`]), loaded the first time it is asked for. fontdb knows each face only
    /// by its own family names, so the families fontconfig resolves are given to it: its generic
    /// families are the faces this engine picks for them, and the families of [`ALIASED`] that are
    /// not installed name the faces fontconfig binds them to (Liberation Sans for Arial).
    pub fn font_database(&self) -> Arc<fontdb::Database> {
        self.font_db
            .get_or_init(|| {
                let mut db = fontdb::Database::new();
                db.load_system_fonts();
                crate::font_files::map_all(&mut db);
                self.name_resolved_families(&mut db);
                Arc::new(db)
            })
            .clone()
    }

    fn name_resolved_families(&self, db: &mut fontdb::Database) {
        let resolved = |name: &str| {
            self.find_family(name, 400, FontStyle::Normal, FontStretch::NORMAL)
                .and_then(|f| self.family_name(f))
        };
        if let Some(f) = resolved("serif") {
            db.set_serif_family(f);
        }
        if let Some(f) = resolved("sans-serif") {
            db.set_sans_serif_family(f);
        }
        if let Some(f) = resolved("monospace") {
            db.set_monospace_family(f);
        }
        if let Some(f) = resolved("cursive") {
            db.set_cursive_family(f);
        }
        if let Some(f) = resolved("fantasy") {
            db.set_fantasy_family(f);
        }
        let named = |db: &fontdb::Database, name: &str| {
            db.faces()
                .any(|f| f.families.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)))
        };
        for alias in ALIASED {
            if named(db, alias) {
                continue;
            }
            let Some(target) = resolved(alias) else {
                continue;
            };
            let faces: Vec<fontdb::FaceInfo> = db
                .faces()
                .filter(|f| f.families.iter().any(|(n, _)| *n == target))
                .cloned()
                .collect();
            for mut face in faces {
                face.families
                    .push((alias.to_string(), fontdb::Language::English_UnitedStates));
                db.push_face_info(face);
            }
        }
    }

    fn face(&self, font: FontId) -> Option<Arc<Face>> {
        self.state
            .read()
            .unwrap()
            .faces
            .get(font.0 as usize)
            .cloned()
    }

    /// What the raster side needs to draw `font`'s glyphs.
    pub fn font_data(&self, font: FontId) -> Option<FontData> {
        self.face(font).map(|f| f.data.clone())
    }

    /// Whether the face has color glyphs.
    pub fn is_color(&self, font: FontId) -> bool {
        self.face(font).is_some_and(|f| f.color)
    }

    /// Changes when `font` comes to stand for another face; an id stands for one face for the
    /// engine's life, so this is 0 for every id handed out.
    pub fn font_revision(&self, font: FontId) -> Option<u32> {
        self.face(font).map(|_| 0)
    }

    /// CSS Text Decoration 4 §2.6 `text-decoration-skip-ink`: the leftmost and rightmost x, from the
    /// glyph's origin, of `glyph`'s outline at `size` between `top` and `bottom` (CSS px below the
    /// baseline), or `None` when the outline does not reach into that band.
    pub fn glyph_intercept(
        &self,
        font: FontId,
        size: f32,
        glyph: u16,
        top: f32,
        bottom: f32,
    ) -> Option<(f32, f32)> {
        let face = self.face(font)?;
        let segments = {
            let mut cache = self.outlines.lock().unwrap();
            cache
                .entry((font.0, glyph))
                .or_insert_with(|| {
                    let mut sink = OutlineSink {
                        segments: Vec::new(),
                    };
                    // SAFETY: an immutable font, the immutable callbacks, and a sink that outlives
                    // the call.
                    unsafe {
                        hb_font_draw_glyph(
                            face.font,
                            glyph as u32,
                            draw_funcs(),
                            (&mut sink as *mut OutlineSink).cast(),
                        )
                    };
                    Arc::new(sink.segments)
                })
                .clone()
        };
        let s = size / face.upem;
        let mut range: Option<(f32, f32)> = None;
        for &[x0, y0, x1, y1] in segments.iter() {
            // In CSS px, y down.
            let (x0, y0, x1, y1) = (x0 * s, -y0 * s, x1 * s, -y1 * s);
            let (lo, hi) = (y0.min(y1), y0.max(y1));
            if hi < top || lo > bottom {
                continue;
            }
            let at = |y: f32| {
                if (y1 - y0).abs() < f32::EPSILON {
                    x0
                } else {
                    x0 + (x1 - x0) * (y - y0) / (y1 - y0)
                }
            };
            let (a, b) = (at(lo.max(top)), at(hi.min(bottom)));
            let (a, b) = if (y1 - y0).abs() < f32::EPSILON {
                (x0.min(x1), x0.max(x1))
            } else {
                (a.min(b), a.max(b))
            };
            range = Some(match range {
                Some((l, r)) => (l.min(a), r.max(b)),
                None => (a, b),
            });
        }
        range
    }

    /// The family name of a face (for diagnostics and tests).
    pub fn family_name(&self, font: FontId) -> Option<String> {
        self.face(font).map(|f| f.family.clone())
    }

    /// The face's weight (its `wght` value), for tests and diagnostics.
    pub fn weight(&self, font: FontId) -> Option<u16> {
        self.face(font).map(|f| f.weight)
    }

    /// Registers a web font file (TrueType, OpenType, WOFF or WOFF 2.0) and returns the unique
    /// internal family name to use in [`FontQuery::families`]. The file is checked and written out
    /// again by OTS first ([`crate::web_font`]): HarfBuzz and the rasterizer only ever read what OTS
    /// made of it.
    pub fn add_font_data(&self, data: &[u8]) -> std::result::Result<String, FontLoadError> {
        let sfnt = crate::web_font::sanitize(data).map_err(FontLoadError::Rejected)?;
        let id = self.add_memory_font(sfnt).ok_or(Error::Invalid)?;
        let mut state = self.state.write().unwrap();
        let name = format!("{WEB_PREFIX}{}", state.web.len());
        state.web.push(id);
        Ok(name)
    }

    /// Registers a font (TrueType/OpenType bytes) as if it were installed on the system: its family
    /// is then found by its own name, after the system's families. Returns the family name.
    pub fn add_local_font(&self, data: &[u8]) -> Result<String> {
        let id = self.add_memory_font(data.to_vec()).ok_or(Error::Invalid)?;
        let mut state = self.state.write().unwrap();
        let family = state.faces[id.0 as usize].family.clone();
        state.local.push((family.clone(), id));
        Ok(family)
    }

    fn add_memory_font(&self, bytes: Vec<u8>) -> Option<FontId> {
        let bytes = FontBytes::from_vec(bytes)?;
        let n = {
            let mut state = self.state.write().unwrap();
            state.memory_fonts += 1;
            state.memory_fonts
        };
        self.intern(FaceKey::Memory(n), Arc::new(bytes), 0, false, false)
    }

    /// Whether a font family of this name is installed (or loaded with `add_local_font`).
    pub fn has_family(&self, name: &str) -> bool {
        self.find_family(name, 400, FontStyle::Normal, FontStretch::NORMAL)
            .is_some()
    }

    /// Resolves a font query to a font list: the families that exist, in order, each with its
    /// matching face, and the first as the primary font. When no family exists, the default
    /// sans-serif family stands in.
    pub fn font_list(&self, query: &FontQuery) -> Result<FontListId> {
        let key = ListKey::of(query);
        if let Some(id) = self.state.read().unwrap().list_keys.get(&key) {
            return Ok(*id);
        }
        let mut faces = Vec::new();
        for name in query.families {
            if let Some(f) = self.find_family(name, query.weight, query.style, query.stretch) {
                if !faces.contains(&f) {
                    faces.push(f);
                }
            }
        }
        if faces.is_empty() {
            if let Some(f) =
                self.find_family("sans-serif", query.weight, query.style, query.stretch)
            {
                faces.push(f);
            }
        }
        let primary = *faces
            .first()
            .ok_or_else(|| Error::Platform("no fonts are installed".into()))?;
        let mut state = self.state.write().unwrap();
        if let Some(id) = state.list_keys.get(&key) {
            return Ok(*id);
        }
        let id = FontListId(state.lists.len() as u32);
        state.lists.push(FontList { faces, primary });
        state.list_keys.insert(key, id);
        Ok(id)
    }

    /// The list's first available font (the one CSS takes line metrics from).
    pub fn primary_font(&self, list: FontListId) -> FontId {
        self.state.read().unwrap().lists[list.0 as usize].primary
    }

    /// A face's metrics at `size` CSS px.
    pub fn metrics(&self, font: FontId, size: f32) -> FontMetrics {
        let Some(face) = self.face(font) else {
            return FontMetrics::default();
        };
        let m = &face.metrics;
        let s = size / face.upem;
        FontMetrics {
            ascent: m.ascent * s,
            descent: m.descent * s,
            line_gap: m.line_gap * s,
            x_height: m.x_height * s,
            cap_height: m.cap_height * s,
            underline_offset: m.underline_offset * s,
            underline_thickness: m.underline_thickness * s,
            strikeout_offset: m.strikeout_offset * s,
            strikeout_thickness: m.strikeout_thickness * s,
            units_per_em: m.units_per_em,
        }
    }

    /// The advance of a face's space glyph at `size` CSS px (0 when it has none).
    pub fn space_advance(&self, font: FontId, size: f32) -> f32 {
        let Some(face) = self.face(font) else {
            return 0.0;
        };
        let Some(g) = face.glyph(' ') else {
            return 0.0;
        };
        // SAFETY: an immutable font.
        let advance = unsafe { hb_font_get_glyph_h_advance(face.font, g) };
        advance as f32 * size / face.upem
    }

    /// The width of `text` in one line (shapes it; for a quick measurement).
    pub fn measure(&self, text: &str, list: FontListId, size: f32) -> Result<f32> {
        let mut out = ShapedText::default();
        self.shape(
            text,
            list,
            &ShapeOptions {
                size,
                ..Default::default()
            },
            &mut out,
        )?;
        Ok(out.width())
    }

    /// The advance of `text` set upright in a vertical line.
    pub fn measure_upright(&self, text: &str, list: FontListId, size: f32) -> Result<f32> {
        let mut out = ShapedText::default();
        self.shape(
            text,
            list,
            &ShapeOptions {
                size,
                vertical: VerticalText::Upright,
                ..Default::default()
            },
            &mut out,
        )?;
        Ok(out.width())
    }

    /// A family's face for the descriptors: a web font, a system family (through fontconfig) or a
    /// local font.
    fn find_family(
        &self,
        name: &str,
        weight: u16,
        style: FontStyle,
        stretch: FontStretch,
    ) -> Option<FontId> {
        if let Some(n) = name.strip_prefix(WEB_PREFIX) {
            let state = self.state.read().unwrap();
            return n
                .parse::<usize>()
                .ok()
                .and_then(|n| state.web.get(n).copied());
        }
        let key = (name.to_ascii_lowercase(), weight, style, stretch.0);
        let found = {
            let mut fc = self.fc.lock().unwrap();
            match fc.families.get(&key) {
                Some(found) => *found,
                None => {
                    let found = self.match_system_family(&mut fc, name, weight, style, stretch);
                    fc.families.insert(key, found);
                    found
                }
            }
        };
        found.or_else(|| {
            let state = self.state.read().unwrap();
            state
                .local
                .iter()
                .find(|(f, _)| f.eq_ignore_ascii_case(name))
                .map(|(_, id)| *id)
        })
    }

    /// fontconfig's match for a family, accepted only when it is that family or one bound strongly
    /// to it (generic families always match).
    fn match_system_family(
        &self,
        fc: &mut Fc,
        name: &str,
        weight: u16,
        style: FontStyle,
        stretch: FontStretch,
    ) -> Option<FontId> {
        let generic = generic_family(name);
        // (a generic family asks for its default family first, then fontconfig's generic)
        let families: Vec<_> = match generic {
            Some((default, fc_generic)) => default.into_iter().chain([fc_generic]).collect(),
            None => vec![name],
        };
        let families: Vec<_> = families.into_iter().map(c_string).collect();
        // SAFETY: fontconfig calls on a pattern this owns, under the `fc` lock; every pattern made
        // is destroyed before returning.
        unsafe {
            let p = FcPatternCreate();
            for family in &families {
                FcPatternAddString(p, obj(FC_FAMILY), family.as_ptr().cast());
            }
            FcPatternAddDouble(p, obj(FC_WEIGHT), fc_weight(weight));
            FcPatternAddInteger(p, obj(FC_SLANT), fc_slant(style));
            FcPatternAddInteger(p, obj(FC_WIDTH), fc_width(stretch));
            FcPatternAddBool(p, obj(FC_SCALABLE), 1);
            FcConfigSubstitute(fc.config, p, FC_MATCH_PATTERN);
            let strong = if generic.is_none() {
                strong_strings(p, FC_FAMILY)
            } else {
                Vec::new()
            };
            FcDefaultSubstitute(p);
            let mut result = 0;
            let m = FcFontMatch(fc.config, p, &mut result);
            FcPatternDestroy(p);
            if m.is_null() {
                return None;
            }
            let accepted = generic.is_some()
                || pattern_strings(m, FC_FAMILY)
                    .iter()
                    .any(|f| strong.iter().any(|s| s.eq_ignore_ascii_case(f)));
            let key = if accepted { pattern_face(m) } else { None };
            FcPatternDestroy(m);
            let key = key?;
            self.intern_file(key)
        }
    }

    fn intern_file(&self, key: FaceKey) -> Option<FontId> {
        if let Some(id) = self.state.read().unwrap().by_key.get(&key) {
            return Some(*id);
        }
        let FaceKey::File(ref file, index, embolden, oblique) = key else {
            return None;
        };
        let bytes = FontBytes::from_file(file)?;
        self.intern(key.clone(), Arc::new(bytes), index, embolden, oblique)
    }

    /// The id for a face, registering it the first time. `index` is fontconfig's: the face index
    /// in its low 16 bits, a variable font's named instance (plus one) above.
    fn intern(
        &self,
        key: FaceKey,
        bytes: Arc<FontBytes>,
        index: c_int,
        embolden: bool,
        oblique: bool,
    ) -> Option<FontId> {
        if let Some(id) = self.state.read().unwrap().by_key.get(&key) {
            return Some(*id);
        }
        let face_index = (index & 0xFFFF) as u32;
        let instance = (index >> 16) as u32;
        // SAFETY: HarfBuzz objects made here and owned by the `Face` (which destroys them); the
        // blob is referenced by the face for its life; the font is made immutable before it is
        // shared. Name and axis buffers have the lengths passed.
        let face = unsafe {
            let hb_face = hb_face_create(bytes.blob, face_index);
            if hb_face_get_glyph_count(hb_face) == 0 {
                hb_face_destroy(hb_face);
                return None;
            }
            let font = hb_font_create(hb_face);
            if instance > 0 {
                hb_font_set_var_named_instance(font, instance - 1);
            }
            hb_font_make_immutable(font);
            let upem = hb_face_get_upem(hb_face).max(1) as f32;
            let metric = |tag| {
                let mut v = 0;
                hb_ot_metrics_get_position(font, tag, &mut v);
                v as f32
            };
            let glyph_top = |c: char| {
                let mut g = 0;
                let mut e = hb_glyph_extents_t::default();
                if hb_font_get_nominal_glyph(font, c as u32, &mut g) != 0
                    && hb_font_get_glyph_extents(font, g, &mut e) != 0
                {
                    e.y_bearing as f32
                } else {
                    0.0
                }
            };
            let mut x_height = metric(HB_OT_METRICS_TAG_X_HEIGHT);
            if x_height <= 0.0 {
                x_height = glyph_top('x');
            }
            let mut cap_height = metric(HB_OT_METRICS_TAG_CAP_HEIGHT);
            if cap_height <= 0.0 {
                cap_height = glyph_top('H');
            }
            let metrics = FontMetrics {
                ascent: metric(HB_OT_METRICS_TAG_HORIZONTAL_ASCENDER),
                descent: -metric(HB_OT_METRICS_TAG_HORIZONTAL_DESCENDER),
                line_gap: metric(HB_OT_METRICS_TAG_HORIZONTAL_LINE_GAP),
                x_height,
                cap_height,
                underline_offset: -metric(HB_OT_METRICS_TAG_UNDERLINE_OFFSET),
                underline_thickness: metric(HB_OT_METRICS_TAG_UNDERLINE_SIZE),
                strikeout_offset: -metric(HB_OT_METRICS_TAG_STRIKEOUT_OFFSET),
                strikeout_thickness: metric(HB_OT_METRICS_TAG_STRIKEOUT_SIZE),
                units_per_em: upem as u16,
            };
            let color = hb_ot_color_has_layers(hb_face) != 0
                || hb_ot_color_has_paint(hb_face) != 0
                || hb_ot_color_has_png(hb_face) != 0
                || hb_ot_color_has_svg(hb_face) != 0;
            let name = |id: c_uint| {
                let mut buf = [0 as c_char; 256];
                let mut len = buf.len() as c_uint;
                let en = hb_language_from_string(c"en".as_ptr(), -1);
                hb_ot_name_get_utf8(hb_face, id, en, &mut len, buf.as_mut_ptr());
                CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
            };
            // The typographic family (name 16), else the family (name 1).
            let mut family = name(16);
            if family.is_empty() {
                family = name(1);
            }
            let weight = hb_style_get_value(font, HB_STYLE_TAG_WEIGHT).round() as u16;
            let vertical_variants = {
                let mut tags = [0; 256];
                let mut count = tags.len() as c_uint;
                hb_ot_layout_table_get_feature_tags(
                    hb_face,
                    tag(b"GSUB"),
                    0,
                    &mut count,
                    tags.as_mut_ptr(),
                );
                tags[..count as usize]
                    .iter()
                    .any(|&t| t == tag(b"vert") || t == tag(b"vrt2"))
            };
            let mut variations = Vec::new();
            let mut axis_count = 0;
            let mut coords_len = 0;
            let coords = hb_font_get_var_coords_design(font, &mut coords_len);
            if coords_len > 0 && !coords.is_null() {
                let total =
                    hb_ot_var_get_axis_infos(hb_face, 0, &mut axis_count, std::ptr::null_mut());
                let mut axes = vec![
                    hb_ot_var_axis_info_t {
                        axis_index: 0,
                        tag: 0,
                        name_id: 0,
                        flags: 0,
                        min_value: 0.0,
                        default_value: 0.0,
                        max_value: 0.0,
                        reserved: 0,
                    };
                    total as usize
                ];
                let mut n = total;
                hb_ot_var_get_axis_infos(hb_face, 0, &mut n, axes.as_mut_ptr());
                let coords = std::slice::from_raw_parts(coords, coords_len as usize);
                for (axis, &value) in axes.iter().take(n as usize).zip(coords) {
                    variations.push((axis.tag.to_be_bytes(), value));
                }
            }
            Face {
                data: FontData {
                    bytes,
                    index: face_index,
                    variations,
                    embolden,
                    oblique,
                },
                hb_face,
                font,
                upem,
                metrics,
                color,
                family,
                weight,
                vertical_variants,
            }
        };
        let mut state = self.state.write().unwrap();
        if let Some(id) = state.by_key.get(&key) {
            return Some(*id);
        }
        let id = FontId(state.faces.len() as u32);
        state.faces.push(Arc::new(face));
        state.by_key.insert(key, id);
        Some(id)
    }

    /// The first font of `locale`'s fallback list that has `c`.
    fn fallback_font(&self, c: char, locale: &str) -> Option<FontId> {
        let file = {
            let mut fc = self.fc.lock().unwrap();
            let config = fc.config;
            let set = fc
                .fallback
                .entry(locale.to_string())
                .or_insert_with(|| fallback_set(config, locale));
            if let Some(found) = set.by_char.get(&(c as u32)) {
                return *found;
            }
            // SAFETY: the charsets belong to the set, alive under the lock.
            let font = set
                .fonts
                .iter()
                .find(|f| unsafe { FcCharSetHasChar(f.charset, c as u32) } != 0);
            match font {
                Some(f) => FaceKey::File(f.file.clone(), f.index, false, false),
                None => {
                    set.by_char.insert(c as u32, None);
                    return None;
                }
            }
        };
        let id = self.intern_file(file);
        let mut fc = self.fc.lock().unwrap();
        if let Some(set) = fc.fallback.get_mut(locale) {
            set.by_char.insert(c as u32, id);
        }
        id
    }

    /// Shapes `text` with the list's fonts (falling back per character to other families in the
    /// list, then to the system fallback for characters none of them has) into `out`, which is
    /// cleared first.
    pub fn shape(
        &self,
        text: &str,
        list: FontListId,
        options: &ShapeOptions,
        out: &mut ShapedText,
    ) -> Result<()> {
        self.shape_cached(text, list, options, None, out)
    }

    /// [`TextEngine::shape`]; the per-word cache is not used on this platform yet.
    pub fn shape_cached(
        &self,
        text: &str,
        list: FontListId,
        options: &ShapeOptions,
        _cache: Option<&mut WordCache>,
        out: &mut ShapedText,
    ) -> Result<()> {
        out.clear();
        if text.is_empty() {
            return Ok(());
        }
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        let n = chars.len();

        // Per character: the bidi level.
        let base: u8 = if options.direction == Direction::Rtl {
            1
        } else {
            0
        };
        let utf16_len: usize = chars.iter().map(|(_, c)| c.len_utf16()).sum();
        let mut level_of = vec![base; n];
        if let Some(levels) = options.levels.filter(|l| l.len() == utf16_len) {
            let mut u = 0;
            for (i, (_, c)) in chars.iter().enumerate() {
                level_of[i] = levels[u];
                u += c.len_utf16();
            }
        } else if options.vertical != VerticalText::Upright
            && (base == 1 || text.chars().any(is_rtl_char))
        {
            let info = unicode_bidi::BidiInfo::new(
                text,
                Some(if base == 1 {
                    unicode_bidi::Level::rtl()
                } else {
                    unicode_bidi::Level::ltr()
                }),
            );
            for (i, (b, _)) in chars.iter().enumerate() {
                level_of[i] = info.levels[*b].number();
            }
        }

        // Per character: the font.
        let (faces, primary) = {
            let state = self.state.read().unwrap();
            let l = state.lists.get(list.0 as usize).ok_or(Error::Invalid)?;
            let faces: Vec<(FontId, Arc<Face>)> = l
                .faces
                .iter()
                .map(|&id| (id, state.faces[id.0 as usize].clone()))
                .collect();
            (faces, l.primary)
        };
        let mut font_of = vec![primary; n];
        let mut fallback_cache: Vec<(char, FontId)> = Vec::new();
        for i in 0..n {
            let c = chars[i].1;
            if i > 0 && follows_previous(c) {
                font_of[i] = font_of[i - 1];
                continue;
            }
            // An emoji-presentation character (or one a VS16 asks to show as emoji) takes the
            // list's color fonts first, then the emoji family, before a text font.
            let emoji = is_emoji_presentation(c)
                || chars
                    .get(i + 1)
                    .is_some_and(|(_, next)| *next == '\u{FE0F}');
            if emoji {
                if let Some((id, _)) = faces.iter().find(|(_, f)| f.color && f.glyph(c).is_some()) {
                    font_of[i] = *id;
                    continue;
                }
                let family = self.find_family("emoji", 400, FontStyle::Normal, FontStretch::NORMAL);
                if let Some(id) =
                    family.filter(|id| self.face(*id).is_some_and(|f| f.glyph(c).is_some()))
                {
                    font_of[i] = id;
                    continue;
                }
            }
            if let Some((id, _)) = faces.iter().find(|(_, f)| f.glyph(c).is_some()) {
                font_of[i] = *id;
                continue;
            }
            if c.is_control() {
                continue;
            }
            if let Some(&(_, id)) = fallback_cache.iter().find(|(k, _)| *k == c) {
                font_of[i] = id;
                continue;
            }
            let id = self.fallback_font(c, options.locale).unwrap_or(primary);
            fallback_cache.push((c, id));
            font_of[i] = id;
        }

        // Per character: set upright in a vertical line.
        let mut upright_of = vec![false; n];
        if let Some(orient) = orientations(text, options.vertical) {
            let mut u = 0;
            for (i, (_, c)) in chars.iter().enumerate() {
                upright_of[i] = match orient[u] {
                    CharOrientation::Rotated => false,
                    CharOrientation::Upright => true,
                    CharOrientation::UprightIfVariant => {
                        self.face(font_of[i]).is_some_and(|f| f.vertical_variants)
                    }
                };
                u += c.len_utf16();
            }
        }

        // Per character: the script (UAX #24), Common and Inherited taking their neighbour's.
        use unicode_script::{Script, UnicodeScript};
        let mut script_of: Vec<Script> = chars.iter().map(|(_, c)| c.script()).collect();
        let weak = |s: Script| matches!(s, Script::Common | Script::Inherited | Script::Unknown);
        let mut last = None;
        for s in script_of.iter_mut() {
            if weak(*s) {
                if let Some(l) = last {
                    *s = l;
                }
            } else {
                last = Some(*s);
            }
        }
        if let Some(first) = script_of.iter().copied().find(|s| !weak(*s)) {
            for s in script_of.iter_mut().take_while(|s| weak(**s)) {
                *s = first;
            }
        }

        // Runs.
        let mut runs = Vec::new();
        let mut i = 0;
        while i < n {
            let mut j = i + 1;
            while j < n
                && font_of[j] == font_of[i]
                && level_of[j] == level_of[i]
                && script_of[j] == script_of[i]
                && upright_of[j] == upright_of[i]
            {
                j += 1;
            }
            runs.push(RunSpec {
                start: chars[i].0,
                end: if j < n { chars[j].0 } else { text.len() },
                font: font_of[i],
                level: level_of[i],
                script: script_of[i],
                upright: upright_of[i],
            });
            i = j;
        }

        // Line-break opportunities (UAX #14), by byte offset.
        let mut breaks = vec![false; text.len() + 1];
        let segmenter = icu_segmenter::LineSegmenter::new_dictionary(Default::default());
        for b in segmenter.segment_str(text) {
            if b > 0 && b <= text.len() {
                breaks[b] = true;
            }
        }

        let features: Vec<hb_feature_t> = options
            .features
            .iter()
            .map(|f| hb_feature_t {
                tag: tag(&f.tag),
                value: f.value,
                start: 0,
                end: c_uint::MAX,
            })
            .collect();
        let locale = c_string(options.locale);
        // SAFETY: a NUL-terminated string; HarfBuzz interns languages for the process's life.
        let language = unsafe { hb_language_from_string(locale.as_ptr(), -1) };
        let buffer = Buffer::new();

        for run in runs {
            let face = self.face(run.font).ok_or(Error::Invalid)?;
            let rtl = run.level % 2 == 1;
            let direction = if run.upright {
                HB_DIRECTION_TTB
            } else if rtl {
                HB_DIRECTION_RTL
            } else {
                HB_DIRECTION_LTR
            };
            let script = run.script.short_name().as_bytes();
            let script = match script {
                [a, b, c, d] => tag(&[*a, *b, *c, *d]),
                _ => tag(b"Zyyy"),
            };
            let mut flags = 0;
            if run.start == 0 {
                flags |= HB_BUFFER_FLAG_BOT;
            }
            if run.end == text.len() {
                flags |= HB_BUFFER_FLAG_EOT;
            }
            // SAFETY: the buffer is this call's; the text pointer and length cover `text`, and the
            // item lies inside it; the feature array outlives `hb_shape`; the glyph arrays
            // HarfBuzz returns hold `count` entries and are read before the buffer changes.
            let (infos, positions) = unsafe {
                hb_buffer_reset(buffer.0);
                hb_buffer_add_utf8(
                    buffer.0,
                    text.as_ptr().cast(),
                    text.len() as c_int,
                    run.start as c_uint,
                    (run.end - run.start) as c_int,
                );
                hb_buffer_set_direction(buffer.0, direction);
                hb_buffer_set_script(buffer.0, hb_script_from_iso15924_tag(script));
                hb_buffer_set_language(buffer.0, language);
                hb_buffer_set_flags(buffer.0, flags);
                hb_shape(
                    face.font,
                    buffer.0,
                    features.as_ptr(),
                    features.len() as c_uint,
                );
                let mut count = 0;
                let infos = hb_buffer_get_glyph_infos(buffer.0, &mut count);
                let infos = std::slice::from_raw_parts(infos, count as usize);
                let mut count2 = 0;
                let positions = hb_buffer_get_glyph_positions(buffer.0, &mut count2);
                let positions = std::slice::from_raw_parts(positions, count2 as usize);
                (infos, positions)
            };
            let s = options.size / face.upem;
            let g0 = out.glyphs.len();
            let c0 = out.clusters.len();
            // Logical order: right-to-left runs come out of HarfBuzz in visual order.
            let order: Box<dyn Iterator<Item = usize>> = if rtl && !run.upright {
                Box::new((0..infos.len()).rev())
            } else {
                Box::new(0..infos.len())
            };
            let mut cluster_of: Vec<u32> = Vec::with_capacity(infos.len());
            for k in order {
                let (info, pos) = (&infos[k], &positions[k]);
                out.glyphs.push(info.codepoint as u16);
                cluster_of.push(info.cluster);
                if run.upright {
                    out.advances.push(-pos.y_advance as f32 * s);
                    out.offsets.push(GlyphOffset {
                        advance_offset: -pos.y_offset as f32 * s,
                        ascender_offset: pos.x_offset as f32 * s,
                    });
                } else {
                    out.advances.push(pos.x_advance as f32 * s);
                    out.offsets.push(GlyphOffset {
                        advance_offset: if rtl { -pos.x_offset } else { pos.x_offset } as f32 * s,
                        ascender_offset: pos.y_offset as f32 * s,
                    });
                }
            }
            // Clusters: glyphs of one HarfBuzz cluster value, in logical order.
            let count = cluster_of.len();
            let mut k = 0;
            while k < count {
                let mut m = k + 1;
                while m < count && cluster_of[m] == cluster_of[k] {
                    m += 1;
                }
                let a = cluster_of[k] as usize;
                let b = if m < count {
                    cluster_of[m] as usize
                } else {
                    run.end
                };
                let (a, b) = (a.min(b), a.max(b));
                let last = text[a..b].chars().next_back().unwrap_or(' ');
                let first = text[a..b].chars().next().unwrap_or(' ');
                let break_after = if is_hard_break(last) && (breaks[b] || b == text.len()) {
                    BreakAfter::Mandatory
                } else if breaks[b] || b == text.len() {
                    BreakAfter::Allowed
                } else {
                    BreakAfter::Never
                };
                out.clusters.push(Cluster {
                    text: (a as u32, b as u32),
                    glyphs: ((g0 + k) as u32, (g0 + m) as u32),
                    advance: out.advances[g0 + k..g0 + m].iter().sum(),
                    break_after,
                    whitespace: is_space(first),
                    soft_hyphen: first == '\u{AD}',
                });
                k = m;
            }
            let advance = out.advances[g0..].iter().sum();
            out.runs.push(ShapedRun {
                font: run.font,
                size: options.size,
                text: (run.start as u32, run.end as u32),
                glyphs: (g0 as u32, out.glyphs.len() as u32),
                clusters: (c0 as u32, out.clusters.len() as u32),
                bidi_level: run.level,
                script: run.script as u16,
                advance,
                color: face.color,
                upright: run.upright,
            });
        }
        Ok(())
    }
}

/// Strong right-to-left characters (bidi classes R and AL) and the explicit RTL controls: text
/// without them in a left-to-right paragraph is all at level 0.
fn is_rtl_char(c: char) -> bool {
    matches!(c as u32,
        0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF | 0x10800..=0x10FFF
        | 0x1E800..=0x1EFFF | 0x200F | 0x202B | 0x202E | 0x2067)
}

/// A HarfBuzz buffer for one shaping call.
struct Buffer(*mut hb_buffer_t);

impl Buffer {
    fn new() -> Buffer {
        // SAFETY: an owned new buffer, destroyed on drop.
        Buffer(unsafe { hb_buffer_create() })
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: the buffer made in `new`.
        unsafe { hb_buffer_destroy(self.0) }
    }
}

#[cfg(test)]
#[path = "text_tests.rs"]
mod tests;

/// fontconfig's sorted fonts for a language (scalable only), with their character sets.
fn fallback_set(config: *mut FcConfig, locale: &str) -> FallbackSet {
    let lang = c_string(locale);
    // SAFETY: fontconfig calls under the caller's `fc` lock on a pattern made and destroyed here;
    // the returned set is owned by the `FallbackSet`, whose charsets point into it.
    unsafe {
        let p = FcPatternCreate();
        if !locale.is_empty() {
            FcPatternAddString(p, obj(FC_LANG), lang.as_ptr().cast());
        }
        FcPatternAddBool(p, obj(FC_SCALABLE), 1);
        FcConfigSubstitute(config, p, FC_MATCH_PATTERN);
        FcDefaultSubstitute(p);
        if locale.is_empty() {
            FcPatternDel(p, obj(FC_LANG));
        }
        let mut result = 0;
        let set = FcFontSort(config, p, 0, std::ptr::null_mut(), &mut result);
        FcPatternDestroy(p);
        let mut fonts = Vec::new();
        if !set.is_null() {
            for i in 0..(*set).nfont as usize {
                let font = *(*set).fonts.add(i);
                let mut charset = std::ptr::null_mut();
                if FcPatternGetCharSet(font, obj(FC_CHARSET), 0, &mut charset) != FC_RESULT_MATCH {
                    continue;
                }
                if let Some(FaceKey::File(file, index, _, _)) = pattern_face(font) {
                    fonts.push(FallbackFont {
                        file,
                        index,
                        charset,
                    });
                }
            }
        }
        FallbackSet {
            set,
            fonts,
            by_char: HashMap::new(),
        }
    }
}
