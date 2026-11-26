// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Text through DirectWrite: font matching, web fonts from memory, shaping with `IDWriteTextAnalyzer`
//! (script itemization, bidi levels, line-break opportunities, glyphs and placements with OpenType
//! features) and font fallback across scripts. Layout measures with what [`TextEngine::shape`]
//! returns and paints the same glyphs ([`ShapedText::paint`]), so measuring and drawing always agree.
//!
//! Thread safety: a `TextEngine` is `Send + Sync`. DirectWrite's shared factory, font collections,
//! font faces, the system fallback and the text analyzer are free-threaded (they hold no per-call
//! state); the engine's own registries (faces, font lists, web fonts) sit behind a lock. The COM
//! analysis source/sink is made per [`TextEngine::shape`] call and never leaves that call.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use windows::core::{implement, Interface, Result, PCWSTR};
use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::Graphics::DirectWrite::*;
use windows_core::{ComObject, OutRef, Ref, BOOL};

pub use snowlark_graphics::text::*;

use crate::display_list::{FontId, GlyphOffset};
use crate::raster::FontResolver;
use crate::raster_tables::{FontFile, FontSource};
use crate::text_cache::{SharedTextCaches, WordCache};
use crate::web_font::FontLoadError;

/// A face the engine handed out an id for.
struct Face {
    face: IDWriteFontFace,
    /// Other objects DirectWrite made for the same face: held, so that no other face's object can
    /// come to sit at an address `by_pointer` maps to this id.
    aliases: Vec<IDWriteFontFace>,
    font: Option<IDWriteFont>,
    metrics: DWRITE_FONT_METRICS,
    color: bool,
    family: String,
}

/// One family of a resolved list.
struct Family {
    collection: IDWriteFontCollection,
    /// NUL-terminated, as the collection names it.
    name: Vec<u16>,
}

struct FontList {
    families: Vec<Family>,
    weight: DWRITE_FONT_WEIGHT,
    style: DWRITE_FONT_STYLE,
    stretch: DWRITE_FONT_STRETCH,
    primary: FontId,
}

struct WebFont {
    collection: IDWriteFontCollection,
    family: Vec<u16>,
}

type FaceIdentity = (usize, Vec<u8>, u32, i32, Vec<(u32, u32)>);

/// A [`FontQuery`] as a font list's key: family names compare ASCII case-insensitively.
struct ListKey {
    families: Box<[Box<str>]>,
    weight: u16,
    style: FontStyle,
    stretch: u8,
}

impl ListKey {
    fn of(query: &FontQuery) -> ListKey {
        ListKey {
            families: query.families.iter().map(|f| (*f).into()).collect(),
            weight: query.weight,
            style: query.style,
            stretch: query.stretch.0,
        }
    }

    fn hash_of(query: &FontQuery) -> u64 {
        use std::hash::{BuildHasher, Hasher};
        let mut h = rustc_hash::FxBuildHasher.build_hasher();
        for f in query.families {
            for b in f.bytes() {
                h.write_u8(b.to_ascii_lowercase());
            }
            h.write_u8(1);
        }
        h.write_u16(query.weight);
        h.write_u8(query.style as u8);
        h.write_u8(query.stretch.0);
        h.finish()
    }

    fn matches(&self, query: &FontQuery) -> bool {
        self.weight == query.weight
            && self.style == query.style
            && self.stretch == query.stretch.0
            && self.families.len() == query.families.len()
            && self
                .families
                .iter()
                .zip(query.families)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
    }
}

#[derive(Default)]
struct State {
    faces: Vec<Face>,
    by_pointer: HashMap<usize, FontId>,
    /// A face's file (loader and key), index in it, simulations, and axis values (as bits).
    by_identity: HashMap<FaceIdentity, FontId>,
    lists: Vec<FontList>,
    /// The lists by [`ListKey::hash_of`] their query: layout asks for one per text, so a lookup
    /// allocates nothing.
    list_keys: rustc_hash::FxHashMap<u64, smallvec::SmallVec<[(ListKey, FontListId); 1]>>,
    web: Vec<WebFont>,
    /// Fonts registered as if installed on the system ([`TextEngine::add_local_font`]).
    local: Vec<IDWriteFontCollection>,
    memory_loader: Option<IDWriteInMemoryFontFileLoader>,
}

/// The prefix of the internal family names given to web fonts.
const WEB_PREFIX: &str = "__snowlark_web_";

/// All the bytes of a font file.
fn font_file_bytes(stream: &IDWriteFontFileStream) -> Result<Vec<u8>> {
    // SAFETY: the fragment is the whole file, DirectWrite keeps it valid until it is released, and
    // it is copied before that.
    unsafe {
        let size = stream.GetFileSize()?;
        let mut start = std::ptr::null_mut();
        let mut context = std::ptr::null_mut();
        stream.ReadFileFragment(&mut start, 0, size, &mut context)?;
        let bytes = std::slice::from_raw_parts(start as *const u8, size as usize).to_vec();
        stream.ReleaseFileFragment(context);
        Ok(bytes)
    }
}

/// The error of the text engine's calls (an HRESULT with its message).
pub type TextEngineError = windows::core::Error;

/// DirectWrite text: see the module documentation.
pub struct TextEngine {
    factory: IDWriteFactory5,
    system: IDWriteFontCollection,
    fallback: IDWriteFontFallback,
    analyzer: IDWriteTextAnalyzer,
    state: RwLock<State>,
    /// DirectWrite's script analysis of Latin, Greek and Cyrillic (see [`crate::text_cache`]).
    pub(crate) simple_scripts: OnceLock<Option<[DWRITE_SCRIPT_ANALYSIS; 4]>>,
    pub(crate) shared_text: SharedTextCaches,
    /// The system fonts as `fontdb` lists them (for usvg's SVG and canvas text), loaded once.
    font_db: OnceLock<std::sync::Arc<fontdb::Database>>,
    /// Glyph outlines' extents in decoration bands (`text-decoration-skip-ink`).
    ink: crate::ink::InkCache,
}

// SAFETY: every COM object the engine holds comes from DirectWrite's *shared* factory, whose objects are
// documented free-threaded (factory, collections, font faces, IDWriteFont, the system font fallback and
// the stateless text analyzer). Mutable engine state is behind the RwLock.
unsafe impl Send for TextEngine {}
// SAFETY: as above; `&self` methods only call free-threaded DirectWrite methods or lock `state`.
unsafe impl Sync for TextEngine {}

impl FontResolver for TextEngine {
    fn factory(&self) -> &IDWriteFactory5 {
        self.factory()
    }

    fn face(&self, font: FontId) -> Option<(IDWriteFontFace, bool)> {
        self.face(font)
    }
}

impl TextEngine {
    pub fn new() -> Result<TextEngine> {
        // SAFETY: plain factory creation; the returned interfaces are owned and released on drop.
        unsafe {
            let factory: IDWriteFactory5 = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let mut system = None;
            factory
                .cast::<IDWriteFactory>()?
                .GetSystemFontCollection(&mut system, false)?;
            let system = system.ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
            let fallback = factory.GetSystemFontFallback()?;
            let analyzer = factory.CreateTextAnalyzer()?;
            Ok(TextEngine {
                factory,
                system,
                fallback,
                analyzer,
                state: RwLock::new(State::default()),
                simple_scripts: OnceLock::new(),
                shared_text: Default::default(),
                font_db: OnceLock::new(),
                ink: Default::default(),
            })
        }
    }

    pub(crate) fn factory(&self) -> &IDWriteFactory5 {
        &self.factory
    }

    /// The system font database (fontdb's, with every file mapped once: see
    /// [`crate::font_files`]), loaded the first time it is asked for and shared by every user of this
    /// engine (it lists the system's fonts, which every page sees anyway).
    pub fn font_database(&self) -> std::sync::Arc<fontdb::Database> {
        self.font_db
            .get_or_init(|| {
                let mut db = fontdb::Database::new();
                db.load_system_fonts();
                crate::font_files::map_all(&mut db);
                std::sync::Arc::new(db)
            })
            .clone()
    }

    pub(crate) fn analyzer(&self) -> &IDWriteTextAnalyzer {
        &self.analyzer
    }

    /// Whether a face has a glyph for `ch` (a Unicode scalar value).
    pub(crate) fn has_character(&self, font: FontId, ch: u32) -> bool {
        let state = self.state.read().unwrap();
        let Some(f) = state
            .faces
            .get(font.0 as usize)
            .and_then(|f| f.font.as_ref())
        else {
            return false;
        };
        // SAFETY: IDWriteFont::HasCharacter has no preconditions.
        unsafe { f.HasCharacter(ch) }.is_ok_and(|b| b.as_bool())
    }

    /// Changes when `font` comes to stand for another face: a raster side that holds its
    /// [`FontSource`] needs it again. An id stands for one face for the engine's life, so this is
    /// 0 for every id handed out.
    pub fn font_revision(&self, font: FontId) -> Option<u32> {
        let state = self.state.read().unwrap();
        state.faces.get(font.0 as usize).map(|_| 0)
    }

    /// What the raster side needs to find the face behind `font` by itself.
    pub fn font_source(&self, font: FontId) -> Result<FontSource> {
        let (face, _) = self.face(font).ok_or(E_INVALIDARG)?;
        // SAFETY: owned interfaces; GetFiles is called first for the count, then with a buffer of
        // exactly that many slots; the reference key pointer is valid while `file` lives.
        unsafe {
            let mut count = 0;
            face.GetFiles(&mut count, None)?;
            let mut files: Vec<Option<IDWriteFontFile>> = vec![None; count as usize];
            face.GetFiles(&mut count, Some(files.as_mut_ptr()))?;
            let [Some(file)] = files.as_slice() else {
                return Err(E_INVALIDARG.into());
            };
            let file = match crate::raster_tables::local_file(file)? {
                Some((path, time)) => FontFile::System {
                    path,
                    last_write: (time.dwHighDateTime as u64) << 32 | time.dwLowDateTime as u64,
                },
                None => {
                    let mut key = std::ptr::null_mut();
                    let mut size = 0;
                    file.GetReferenceKey(&mut key, &mut size)?;
                    let stream = file.GetLoader()?.CreateStreamFromKey(key, size)?;
                    FontFile::Data(font_file_bytes(&stream)?)
                }
            };
            let mut axes = Vec::new();
            if let Ok(face) = face.cast::<IDWriteFontFace5>() {
                if face.HasVariations().as_bool() {
                    let mut values = vec![
                        DWRITE_FONT_AXIS_VALUE::default();
                        face.GetFontAxisValueCount() as usize
                    ];
                    face.GetFontAxisValues(&mut values)?;
                    axes.extend(values.iter().map(|v| (v.axisTag.0, v.value)));
                }
            }
            Ok(FontSource {
                file,
                index: face.GetIndex(),
                simulations: face.GetSimulations().0 as u32,
                axes,
            })
        }
    }

    /// The face behind an id, and whether it has color glyphs.
    pub(crate) fn face(&self, font: FontId) -> Option<(IDWriteFontFace, bool)> {
        let state = self.state.read().unwrap();
        state
            .faces
            .get(font.0 as usize)
            .map(|f| (f.face.clone(), f.color))
    }

    /// CSS Text Decoration 4 §2.6 `text-decoration-skip-ink`: the leftmost and rightmost x, from the
    /// glyph's origin, of `glyph`'s outline at `size` between `top` and `bottom` (CSS px below the
    /// baseline), or `None` when the outline does not reach into that band. Cached.
    pub fn glyph_intercept(
        &self,
        font: FontId,
        size: f32,
        glyph: u16,
        top: f32,
        bottom: f32,
    ) -> Option<(f32, f32)> {
        self.ink.intercept(
            || self.face(font).map(|f| f.0),
            font,
            size,
            glyph,
            top,
            bottom,
        )
    }

    /// The family name of a face (for diagnostics and tests).
    pub fn family_name(&self, font: FontId) -> Option<String> {
        let state = self.state.read().unwrap();
        state.faces.get(font.0 as usize).map(|f| f.family.clone())
    }

    /// The face's weight as DirectWrite reports it (after matching), for tests and diagnostics.
    pub fn weight(&self, font: FontId) -> Option<u16> {
        let state = self.state.read().unwrap();
        let face = state.faces.get(font.0 as usize)?;
        // SAFETY: IDWriteFont::GetWeight has no preconditions.
        face.font
            .as_ref()
            .map(|f| unsafe { f.GetWeight().0 as u16 })
    }

    /// Registers a web font file (TrueType, OpenType, WOFF or WOFF 2.0) and returns the unique
    /// internal family name to use in [`FontQuery::families`]. The file is checked and written out
    /// again by OTS first ([`crate::web_font`]): DirectWrite only ever reads what OTS made of it. Names
    /// never collide with system families or with other web fonts, so pages can't shadow each other's
    /// fonts or system ones.
    pub fn add_font_data(&self, data: &[u8]) -> std::result::Result<String, FontLoadError> {
        let sfnt = crate::web_font::sanitize(data).map_err(FontLoadError::Rejected)?;
        let mut state = self.state.write().unwrap();
        let collection = self.collection_from_data(&mut state, &sfnt)?;
        // SAFETY: an owned collection with at least one family.
        let family = unsafe { family_name_w(&collection.GetFontFamily(0)?)? };
        let name = format!("{WEB_PREFIX}{}", state.web.len());
        state.web.push(WebFont { collection, family });
        Ok(name)
    }

    /// Registers a font (TrueType/OpenType bytes) as if it were installed on the system: its family
    /// is then found by its own name, after the system's families (test harnesses install fonts
    /// this way, such as web-platform-tests' Ahem). Returns the family name. The bytes are copied.
    /// Font lists resolved before the call keep their families: register local fonts first.
    pub fn add_local_font(&self, data: &[u8]) -> Result<String> {
        let mut state = self.state.write().unwrap();
        let collection = self.collection_from_data(&mut state, data)?;
        // SAFETY: an owned collection with at least one family.
        let family = unsafe { family_name_w(&collection.GetFontFamily(0)?)? };
        state.local.push(collection);
        Ok(String::from_utf16_lossy(
            &family[..family.len().saturating_sub(1)],
        ))
    }

    /// A font collection of one font file's bytes (in memory, copied).
    fn collection_from_data(
        &self,
        state: &mut State,
        data: &[u8],
    ) -> Result<IDWriteFontCollection> {
        // SAFETY: DirectWrite calls with owned interfaces; `data` is copied by
        // CreateInMemoryFontFileReference (no owner object is passed), so it need not outlive the call.
        unsafe {
            if state.memory_loader.is_none() {
                let loader = self.factory.CreateInMemoryFontFileLoader()?;
                self.factory.RegisterFontFileLoader(&loader)?;
                state.memory_loader = Some(loader);
            }
            let loader = state.memory_loader.as_ref().unwrap();
            let file = loader.CreateInMemoryFontFileReference(
                &self.factory,
                data.as_ptr().cast(),
                data.len() as u32,
                None,
            )?;
            let builder: IDWriteFontSetBuilder1 = self.factory.CreateFontSetBuilder()?.cast()?;
            builder.AddFontFile(&file)?;
            let set = builder.CreateFontSet()?;
            let collection: IDWriteFontCollection =
                self.factory.CreateFontCollectionFromFontSet(&set)?.cast()?;
            if collection.GetFontFamilyCount() == 0 {
                return Err(E_INVALIDARG.into());
            }
            Ok(collection)
        }
    }

    /// Whether a font family of this name is installed (or loaded from a file with `--font`).
    pub fn has_family(&self, name: &str) -> bool {
        matches!(self.find_family(name), Ok(Some(_)))
    }

    /// Resolves a font query to a font list: the families that exist, in order, and the first one's
    /// matching face as the primary font. When no family exists, Segoe UI stands in.
    pub fn font_list(&self, query: &FontQuery) -> Result<FontListId> {
        let hash = ListKey::hash_of(query);
        let find = |state: &State| {
            state
                .list_keys
                .get(&hash)?
                .iter()
                .find(|(k, _)| k.matches(query))
                .map(|(_, id)| *id)
        };
        if let Some(id) = find(&self.state.read().unwrap()) {
            return Ok(id);
        }
        let weight = DWRITE_FONT_WEIGHT(query.weight.clamp(1, 999) as i32);
        let style = match query.style {
            FontStyle::Normal => DWRITE_FONT_STYLE_NORMAL,
            FontStyle::Italic => DWRITE_FONT_STYLE_ITALIC,
            FontStyle::Oblique => DWRITE_FONT_STYLE_OBLIQUE,
        };
        let stretch = DWRITE_FONT_STRETCH(query.stretch.0.clamp(1, 9) as i32);
        let mut families = Vec::new();
        for name in query.families {
            if let Some(f) = self.find_family(name)? {
                families.push(f);
            }
        }
        if families.is_empty() {
            if let Some(f) = self.find_family("Segoe UI")? {
                families.push(f);
            }
        }
        let first = families
            .first()
            .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
        // SAFETY: owned interfaces; the name is NUL-terminated.
        let font = unsafe {
            let mut index = 0;
            let mut exists = BOOL(0);
            first.collection.FindFamilyName(
                PCWSTR(first.name.as_ptr()),
                &mut index,
                &mut exists,
            )?;
            first
                .collection
                .GetFontFamily(index)?
                .GetFirstMatchingFont(weight, stretch, style)?
        };
        let primary = self.intern(&font)?;
        let mut state = self.state.write().unwrap();
        if let Some(id) = find(&state) {
            return Ok(id);
        }
        let id = FontListId(state.lists.len() as u32);
        state.lists.push(FontList {
            families,
            weight,
            style,
            stretch,
            primary,
        });
        state
            .list_keys
            .entry(hash)
            .or_default()
            .push((ListKey::of(query), id));
        Ok(id)
    }

    /// The list's first available font (the one CSS takes line metrics from).
    pub fn primary_font(&self, list: FontListId) -> FontId {
        self.state.read().unwrap().lists[list.0 as usize].primary
    }

    /// A face's metrics at `size` CSS px.
    pub fn metrics(&self, font: FontId, size: f32) -> FontMetrics {
        let state = self.state.read().unwrap();
        let Some(face) = state.faces.get(font.0 as usize) else {
            return FontMetrics::default();
        };
        let m = &face.metrics;
        let s = size / m.designUnitsPerEm.max(1) as f32;
        FontMetrics {
            ascent: m.ascent as f32 * s,
            descent: m.descent as f32 * s,
            line_gap: m.lineGap as f32 * s,
            x_height: m.xHeight as f32 * s,
            cap_height: m.capHeight as f32 * s,
            underline_offset: -(m.underlinePosition as f32) * s,
            underline_thickness: m.underlineThickness as f32 * s,
            strikeout_offset: -(m.strikethroughPosition as f32) * s,
            strikeout_thickness: m.strikethroughThickness as f32 * s,
            units_per_em: m.designUnitsPerEm,
        }
    }

    /// The advance of a face's space glyph at `size` CSS px (0 when it has none).
    pub fn space_advance(&self, font: FontId, size: f32) -> f32 {
        let Some((face, _)) = self.face(font) else {
            return 0.0;
        };
        let mut glyph = [0u16; 1];
        // SAFETY: one code point in, one glyph index out, both arrays of length one.
        if unsafe { face.GetGlyphIndices([32u32].as_ptr(), 1, glyph.as_mut_ptr()) }.is_err()
            || glyph[0] == 0
        {
            return 0.0;
        }
        let mut m = [DWRITE_GLYPH_METRICS::default()];
        // SAFETY: one glyph index in, one metrics record out, both arrays of length one.
        if unsafe { face.GetDesignGlyphMetrics(glyph.as_ptr(), 1, m.as_mut_ptr(), false) }.is_err()
        {
            return 0.0;
        }
        let units = self.metrics(font, size).units_per_em.max(1);
        m[0].advanceWidth as f32 * size / units as f32
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

    fn find_family(&self, name: &str) -> Result<Option<Family>> {
        if let Some(n) = name.strip_prefix(WEB_PREFIX) {
            let state = self.state.read().unwrap();
            return Ok(n
                .parse::<usize>()
                .ok()
                .and_then(|n| state.web.get(n))
                .map(|w| Family {
                    collection: w.collection.clone(),
                    name: w.family.clone(),
                }));
        }
        let name = generic_family(name).unwrap_or(name);
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        unsafe {
            let mut index = 0;
            let mut exists = BOOL(0);
            self.system
                .FindFamilyName(PCWSTR(wide.as_ptr()), &mut index, &mut exists)?;
            if !exists.as_bool() {
                let state = self.state.read().unwrap();
                for collection in &state.local {
                    collection.FindFamilyName(PCWSTR(wide.as_ptr()), &mut index, &mut exists)?;
                    if exists.as_bool() {
                        return Ok(Some(Family {
                            collection: collection.clone(),
                            name: family_name_w(&collection.GetFontFamily(index)?)?,
                        }));
                    }
                }
                return Ok(None);
            }
            let canonical = family_name_w(&self.system.GetFontFamily(index)?)?;
            Ok(Some(Family {
                collection: self.system.clone(),
                name: canonical,
            }))
        }
    }

    /// The id for a font, registering its face the first time.
    fn intern(&self, font: &IDWriteFont) -> Result<FontId> {
        // SAFETY: owned interfaces; GetFiles is called first for the count, then with a buffer of
        // exactly that many slots; the reference key pointer is valid while `file` lives, and is copied.
        unsafe {
            let face = font.CreateFontFace()?;
            let pointer = face.as_raw() as usize;
            if let Some(id) = self.state.read().unwrap().by_pointer.get(&pointer) {
                return Ok(*id);
            }
            let mut count = 0;
            face.GetFiles(&mut count, None)?;
            let mut files: Vec<Option<IDWriteFontFile>> = vec![None; count as usize];
            face.GetFiles(&mut count, Some(files.as_mut_ptr()))?;
            let mut key_bytes = Vec::new();
            let mut loader = 0usize;
            if let Some(Some(file)) = files.first() {
                let mut key = std::ptr::null_mut();
                let mut size = 0;
                file.GetReferenceKey(&mut key, &mut size)?;
                key_bytes = std::slice::from_raw_parts(key as *const u8, size as usize).to_vec();
                loader = file.GetLoader()?.as_raw() as usize;
            }
            // Instances of one variable font file are different faces: their axis values tell
            // them apart.
            let mut axes = Vec::new();
            if let Ok(face) = face.cast::<IDWriteFontFace5>() {
                if face.HasVariations().as_bool() {
                    let mut values = vec![
                        DWRITE_FONT_AXIS_VALUE::default();
                        face.GetFontAxisValueCount() as usize
                    ];
                    face.GetFontAxisValues(&mut values)?;
                    axes.extend(values.iter().map(|v| (v.axisTag.0, v.value.to_bits())));
                }
            }
            let identity = (
                loader,
                key_bytes,
                face.GetIndex(),
                face.GetSimulations().0,
                axes,
            );
            let mut state = self.state.write().unwrap();
            if let Some(id) = state.by_pointer.get(&pointer).copied() {
                return Ok(id);
            }
            if let Some(id) = state.by_identity.get(&identity).copied() {
                // Another object for a face the id already stands for. It is held as long as the
                // id is: the address is a key of `by_pointer`, and an object let go of could be
                // replaced at that address by another font's face.
                state.faces[id.0 as usize].aliases.push(face);
                state.by_pointer.insert(pointer, id);
                return Ok(id);
            }
            let mut metrics = DWRITE_FONT_METRICS::default();
            font.GetMetrics(&mut metrics);
            let color = font
                .cast::<IDWriteFont2>()
                .map(|f| f.IsColorFont().as_bool())
                .unwrap_or(false);
            let family = font
                .GetFontFamily()
                .and_then(|f| family_name_w(&f))
                .map(|w| String::from_utf16_lossy(&w[..w.len() - 1]))
                .unwrap_or_default();
            let id = FontId(state.faces.len() as u32);
            state.faces.push(Face {
                face,
                aliases: Vec::new(),
                font: Some(font.clone()),
                metrics,
                color,
                family,
            });
            state.by_pointer.insert(pointer, id);
            state.by_identity.insert(identity, id);
            Ok(id)
        }
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

    /// Shapes `text` with the list's fonts (falling back per character to other families in the list,
    /// then to the system fallback for scripts none of them cover) into `out`, which is cleared first.
    pub fn shape(
        &self,
        text: &str,
        list: FontListId,
        options: &ShapeOptions,
        out: &mut ShapedText,
    ) -> Result<()> {
        self.shape_cached(text, list, options, None, out)
    }

    /// [`TextEngine::shape`] through a [`WordCache`]: runs of simple scripts are shaped per word
    /// (see [`crate::text_cache`]), so words shaped before with the same font, size, script, locale and
    /// features are copied from the cache instead of shaped again. The result is the same as without
    /// the cache.
    pub fn shape_cached(
        &self,
        text: &str,
        list: FontListId,
        options: &ShapeOptions,
        mut cache: Option<&mut WordCache>,
        out: &mut ShapedText,
    ) -> Result<()> {
        out.clear();
        if text.is_empty() {
            return Ok(());
        }
        // UTF-16 for DirectWrite, with each code unit's byte offset in `text` (plus the end).
        let mut wide: Vec<u16> = Vec::with_capacity(text.len());
        let mut byte_of: Vec<u32> = Vec::with_capacity(text.len() + 1);
        if text.is_ascii() {
            wide.extend(text.bytes().map(u16::from));
            byte_of.extend(0..text.len() as u32);
        } else {
            for (i, ch) in text.char_indices() {
                let mut buf = [0u16; 2];
                for unit in ch.encode_utf16(&mut buf) {
                    wide.push(*unit);
                    byte_of.push(i as u32);
                }
            }
        }
        byte_of.push(text.len() as u32);
        let len = wide.len() as u32;
        let n = len as usize;
        out.glyphs.reserve(n);
        out.advances.reserve(n);
        out.offsets.reserve(n);
        out.clusters.reserve(n);
        let locale: Vec<u16> = options.locale.encode_utf16().chain(Some(0)).collect();

        let orient = orientations(text, options.vertical);
        // Text of one simple script in a left-to-right paragraph, all of it in the list's first font:
        // one run whose script, level (0) and font are known without DirectWrite's analyses.
        let simple = match cache.as_deref_mut() {
            Some(c)
                if options.direction == Direction::Ltr
                    && orient.is_none()
                    && options.levels.is_none() =>
            {
                self.simple_run(c, &wide, list)
            }
            _ => None,
        };

        let analysis = ComObject::new(Analysis {
            text: wide,
            locale,
            rtl: options.direction == Direction::Rtl,
            scripts: RefCell::new(Vec::new()),
            levels: RefCell::new(Vec::new()),
            breaks: RefCell::new(vec![DWRITE_LINE_BREAKPOINT::default(); n]),
        });
        let source: IDWriteTextAnalysisSource = analysis.to_interface();
        let sink: IDWriteTextAnalysisSink = analysis.to_interface();
        // SAFETY: the source and sink are our COM object, alive for the whole call; DirectWrite calls
        // back into it synchronously on this thread.
        unsafe {
            if simple.is_none() {
                self.analyzer.AnalyzeScript(&source, 0, len, &sink)?;
                if options.levels.is_none() {
                    self.analyzer.AnalyzeBidi(&source, 0, len, &sink)?;
                }
            }
            self.analyzer
                .AnalyzeLineBreakpoints(&source, 0, len, &sink)?;
        }
        let runs = match simple {
            Some(run) => vec![run],
            None => self.itemize(&analysis, &source, list, options, orient.as_deref())?,
        };

        // The features, applied to each run whole.
        let feature_list: Vec<DWRITE_FONT_FEATURE> = options
            .features
            .iter()
            .map(|f| DWRITE_FONT_FEATURE {
                nameTag: DWRITE_FONT_FEATURE_TAG(u32::from_le_bytes(f.tag)),
                parameter: f.value,
            })
            .collect();

        let breaks = analysis.breaks.borrow();
        let wide = &analysis.text;
        let mut scratch = GlyphScratch::default();
        // Per code unit of the current run: the index of its cluster's first glyph in the run.
        let mut run_map: Vec<u32> = Vec::with_capacity(n);
        for run in runs {
            let RunSpec {
                start,
                end,
                font,
                level,
                script,
                upright,
            } = run;
            let (face, color) = self
                .face(font)
                .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
            let rtl = level % 2 == 1;
            let g0 = out.glyphs.len();
            run_map.clear();
            let shape = RunShape {
                units: &wide[start..end],
                font,
                face: &face,
                script,
                rtl,
                locale: &analysis.locale,
                features: &feature_list,
                size: options.size,
                upright,
            };
            let by_words = match cache.as_deref_mut() {
                Some(c) if !rtl && !upright => {
                    c.shape_run(self, &shape, &mut scratch, out, &mut run_map)?
                }
                _ => false,
            };
            if !by_words {
                self.shape_units(&shape, &mut scratch, out, &mut run_map)?;
            }
            let count = out.glyphs.len() - g0;
            hide_default_ignorables(&wide[start..end], &face, &run_map, g0, count, out);

            // Clusters: a new one starts wherever the cluster map moves on.
            let run_len = end - start;
            let c0 = out.clusters.len();
            let mut i = 0usize;
            while i < run_len {
                let mut j = i + 1;
                while j < run_len && run_map[j] == run_map[i] {
                    j += 1;
                }
                let first_glyph = run_map[i] as usize;
                let last_glyph = if j < run_len {
                    run_map[j] as usize
                } else {
                    count
                };
                let advance: f32 = out.advances[g0 + first_glyph..g0 + last_glyph].iter().sum();
                let (unit_a, unit_b) = (start + i, start + j);
                let after = (breaks[unit_b - 1]._bitfield >> 2) & 3;
                let before_next = if unit_b < n {
                    breaks[unit_b]._bitfield & 3
                } else {
                    0
                };
                let first = breaks[unit_a]._bitfield;
                out.clusters.push(Cluster {
                    text: (byte_of[unit_a], byte_of[unit_b]),
                    glyphs: ((g0 + first_glyph) as u32, (g0 + last_glyph) as u32),
                    advance,
                    break_after: combine_breaks(after, before_next, unit_b == n),
                    whitespace: first & 0x10 != 0,
                    soft_hyphen: first & 0x20 != 0,
                });
                i = j;
            }
            let advance = out.advances[g0..].iter().sum();
            out.runs.push(ShapedRun {
                font,
                size: options.size,
                text: (byte_of[start], byte_of[end]),
                glyphs: (g0 as u32, out.glyphs.len() as u32),
                clusters: (c0 as u32, out.clusters.len() as u32),
                bidi_level: level,
                script: script.script,
                advance,
                color,
                upright,
            });
        }
        Ok(())
    }

    /// Splits analyzed text into runs of one font, script and bidi level (DirectWrite's script and
    /// bidi analyses must have run).
    fn itemize(
        &self,
        analysis: &Analysis,
        source: &IDWriteTextAnalysisSource,
        list: FontListId,
        options: &ShapeOptions,
        orient: Option<&[CharOrientation]>,
    ) -> Result<Vec<RunSpec>> {
        // Per code unit: the font, the script run, the bidi level.
        let n = analysis.text.len();
        let mut font_of = vec![FontId(0); n];
        self.map_fonts(source, list, &mut font_of)?;
        let base_level = if options.direction == Direction::Rtl {
            1
        } else {
            0
        };
        let mut level_of = vec![base_level; n];
        // (`text-orientation: upright` treats every character as strong left-to-right, §5.1)
        if let Some(levels) = options.levels.filter(|l| l.len() == n) {
            level_of.copy_from_slice(levels);
        } else if options.vertical != VerticalText::Upright {
            for &(pos, l, level) in analysis.levels.borrow().iter() {
                for v in &mut level_of[pos as usize..(pos + l) as usize] {
                    *v = level;
                }
            }
        }
        // Whether each unit is set upright in its font.
        let mut upright_of = vec![false; n];
        if let Some(orient) = orient {
            let mut variants: HashMap<u32, bool> = HashMap::new();
            for (i, o) in orient.iter().enumerate() {
                upright_of[i] = match o {
                    CharOrientation::Rotated => false,
                    CharOrientation::Upright => true,
                    CharOrientation::UprightIfVariant => {
                        *variants.entry(font_of[i].0).or_insert_with(|| {
                            self.face(font_of[i]).is_some_and(|(f, _)| {
                                f.cast::<IDWriteFontFace1>()
                                    // SAFETY: a plain query on an owned interface.
                                    .is_ok_and(|f| {
                                        unsafe { f.HasVerticalGlyphVariants() }.as_bool()
                                    })
                            })
                        })
                    }
                };
            }
        }
        let scripts = analysis.scripts.borrow();
        let mut script_of = vec![usize::MAX; n];
        for (k, &(pos, l, _)) in scripts.iter().enumerate() {
            for v in &mut script_of[pos as usize..(pos + l) as usize] {
                *v = k;
            }
        }
        let mut runs = Vec::new();
        let mut start = 0usize;
        while start < n {
            let mut end = start + 1;
            while end < n
                && font_of[end] == font_of[start]
                && level_of[end] == level_of[start]
                && script_of[end] == script_of[start]
                && upright_of[end] == upright_of[start]
            {
                end += 1;
            }
            runs.push(RunSpec {
                start,
                end,
                font: font_of[start],
                level: level_of[start],
                script: scripts
                    .get(script_of[start])
                    .map(|s| s.2)
                    .unwrap_or_default(),
                upright: upright_of[start],
            });
            start = end;
        }
        Ok(runs)
    }

    /// Shapes one run with DirectWrite: appends its glyphs, advances and offsets to `out`'s glyph
    /// arrays and, per code unit, the index of its cluster's first glyph (relative to the run) to
    /// `map`.
    pub(crate) fn shape_units(
        &self,
        run: &RunShape,
        scratch: &mut GlyphScratch,
        out: &mut ShapedText,
        map: &mut Vec<u32>,
    ) -> Result<()> {
        let run_len = run.units.len() as u32;
        if run_len == 0 {
            return Ok(());
        }
        let typographic = DWRITE_TYPOGRAPHIC_FEATURES {
            // DirectWrite only reads the features (the field is not const in the header).
            features: run.features.as_ptr() as *mut DWRITE_FONT_FEATURE,
            featureCount: run.features.len() as u32,
        };
        let typographic_ptr: *const DWRITE_TYPOGRAPHIC_FEATURES = &typographic;
        let range_lengths = [run_len];
        let (features, lengths, ranges) = if run.features.is_empty() {
            (None, None, 0)
        } else {
            (
                Some(&typographic_ptr as *const *const DWRITE_TYPOGRAPHIC_FEATURES),
                Some(range_lengths.as_ptr()),
                1,
            )
        };
        let GlyphScratch {
            cluster_map,
            text_props,
            glyph_ids,
            glyph_props,
        } = scratch;
        let mut max_glyphs = run_len * 3 / 2 + 16;
        let mut count = 0u32;
        cluster_map.resize(run_len as usize, 0);
        text_props.resize(run_len as usize, Default::default());
        loop {
            glyph_ids.resize(max_glyphs as usize, 0);
            glyph_props.resize(max_glyphs as usize, Default::default());
            // SAFETY: every output buffer has the length passed for it (run_len for the per-character
            // arrays, max_glyphs for the per-glyph ones); the text pointer covers run_len units; the
            // locale is NUL-terminated; `features` points at one DWRITE_TYPOGRAPHIC_FEATURES covering
            // the run, whose feature array outlives the call and is only read.
            let r = unsafe {
                self.analyzer.GetGlyphs(
                    PCWSTR(run.units.as_ptr()),
                    run_len,
                    run.face,
                    run.upright,
                    run.rtl,
                    &run.script,
                    PCWSTR(run.locale.as_ptr()),
                    None,
                    features,
                    lengths,
                    ranges,
                    max_glyphs,
                    cluster_map.as_mut_ptr(),
                    text_props.as_mut_ptr(),
                    glyph_ids.as_mut_ptr(),
                    glyph_props.as_mut_ptr(),
                    &mut count,
                )
            };
            match r {
                Ok(()) => break,
                // E_NOT_SUFFICIENT_BUFFER
                Err(e) if e.code().0 as u32 == 0x8007_007A && max_glyphs < 1 << 20 => {
                    max_glyphs *= 2
                }
                Err(e) => return Err(e),
            }
        }
        let g0 = out.glyphs.len();
        out.glyphs.extend_from_slice(&glyph_ids[..count as usize]);
        out.advances.resize(g0 + count as usize, 0.0);
        out.offsets
            .resize(g0 + count as usize, GlyphOffset::default());
        // SAFETY: as for GetGlyphs; the advance and offset outputs have `count` slots each, and
        // GlyphOffset has DWRITE_GLYPH_OFFSET's layout (two f32, repr(C); checked below).
        unsafe {
            const _: () = assert!(
                std::mem::size_of::<GlyphOffset>() == std::mem::size_of::<DWRITE_GLYPH_OFFSET>()
            );
            self.analyzer.GetGlyphPlacements(
                PCWSTR(run.units.as_ptr()),
                cluster_map.as_ptr(),
                text_props.as_mut_ptr(),
                run_len,
                glyph_ids.as_ptr(),
                glyph_props.as_ptr(),
                count,
                run.face,
                run.size,
                run.upright,
                run.rtl,
                &run.script,
                PCWSTR(run.locale.as_ptr()),
                features,
                lengths,
                ranges,
                out.advances.as_mut_ptr().add(g0),
                out.offsets
                    .as_mut_ptr()
                    .add(g0)
                    .cast::<DWRITE_GLYPH_OFFSET>(),
            )?;
        }
        map.extend(cluster_map.iter().map(|&g| u32::from(g)));
        Ok(())
    }

    /// Fills `font_of` (one entry per UTF-16 unit) with the font for each character.
    fn map_fonts(
        &self,
        source: &IDWriteTextAnalysisSource,
        list: FontListId,
        font_of: &mut [FontId],
    ) -> Result<()> {
        let (families, weight, style, stretch, primary) = {
            let state = self.state.read().unwrap();
            let l = &state.lists[list.0 as usize];
            let families: Vec<(IDWriteFontCollection, Vec<u16>)> = l
                .families
                .iter()
                .map(|f| (f.collection.clone(), f.name.clone()))
                .collect();
            (families, l.weight, l.style, l.stretch, l.primary)
        };
        let ctx = MapContext {
            source,
            families: &families,
            weight,
            style,
            stretch,
            primary,
        };
        self.map_range(&ctx, 0, font_of.len() as u32, 0, font_of)
    }

    fn map_range(
        &self,
        ctx: &MapContext,
        start: u32,
        end: u32,
        level: usize,
        font_of: &mut [FontId],
    ) -> Result<()> {
        let mut pos = start;
        while pos < end {
            let mut mapped_len = 0u32;
            let mut mapped: Option<IDWriteFont> = None;
            let mut scale = 1.0f32;
            let base = ctx.families.get(level);
            // SAFETY: the source covers [0, len); the family name is NUL-terminated; outputs are
            // locals. A null base family asks for pure system fallback.
            unsafe {
                self.fallback.MapCharacters(
                    ctx.source,
                    pos,
                    end - pos,
                    base.map(|b| &b.0),
                    match base {
                        Some(b) => PCWSTR(b.1.as_ptr()),
                        None => PCWSTR::null(),
                    },
                    ctx.weight,
                    ctx.style,
                    ctx.stretch,
                    &mut mapped_len,
                    &mut mapped,
                    &mut scale,
                )?;
            }
            let mapped_len = mapped_len.clamp(1, end - pos);
            let piece = pos..pos + mapped_len;
            match (&mapped, base) {
                // The list's family itself covers these characters.
                (Some(font), Some(b)) if is_family(font, &b.1) => {
                    let id = self.intern(font)?;
                    font_of[piece.start as usize..piece.end as usize].fill(id);
                }
                // Another family of the list may: CSS tries them in order before system fallback.
                (_, Some(_)) => self.map_range(ctx, piece.start, piece.end, level + 1, font_of)?,
                (Some(font), None) => {
                    let id = self.intern(font)?;
                    font_of[piece.start as usize..piece.end as usize].fill(id);
                }
                // No font has them: the primary font's .notdef boxes.
                (None, None) => font_of[piece.start as usize..piece.end as usize].fill(ctx.primary),
            }
            pos = piece.end;
        }
        Ok(())
    }
}

/// One run to shape: text of one font, script and direction.
pub(crate) struct RunShape<'a> {
    pub units: &'a [u16],
    pub font: FontId,
    pub face: &'a IDWriteFontFace,
    pub script: DWRITE_SCRIPT_ANALYSIS,
    pub rtl: bool,
    /// NUL-terminated.
    pub locale: &'a [u16],
    pub features: &'a [DWRITE_FONT_FEATURE],
    pub size: f32,
    /// Shaped for vertical layout (DirectWrite's `isSideways`).
    pub upright: bool,
}

/// A run found by itemization: code units `start..end`, their font, bidi level and script.
pub(crate) struct RunSpec {
    pub start: usize,
    pub end: usize,
    pub font: FontId,
    pub level: u8,
    pub script: DWRITE_SCRIPT_ANALYSIS,
    pub upright: bool,
}

/// Whether a UTF-16 code unit is a Default_Ignorable_Code_Point of the BMP (Unicode
/// DerivedCoreProperties): the soft hyphen, format and bidi controls, joiners, variation selectors...
/// (Layout adds the hyphen where a line breaks at a soft hyphen, CSS Text 3 §6.1.)
pub(crate) fn is_default_ignorable(u: u16) -> bool {
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

/// Unicode §5.21: default-ignorable code points are rendered invisibly. A cluster made only of them
/// (a bidi isolate's FSI/PDI, a zero-width space) gets the font's space glyph and no advance, rather
/// than the notdef box or a fallback font's visible control picture. (Clusters where a font uses
/// them, a base character with a variation selector, keep their glyphs.)
fn hide_default_ignorables(
    units: &[u16],
    face: &IDWriteFontFace,
    map: &[u32],
    g0: usize,
    count: usize,
    out: &mut ShapedText,
) {
    if !units.iter().any(|&u| is_default_ignorable(u)) {
        return;
    }
    let mut space = [0u16; 1];
    // SAFETY: one code point in, one glyph index out, both arrays of length one.
    let _ = unsafe { face.GetGlyphIndices([32u32].as_ptr(), 1, space.as_mut_ptr()) };
    let n = units.len();
    let mut i = 0;
    while i < n {
        let mut j = i + 1;
        while j < n && map[j] == map[i] {
            j += 1;
        }
        if units[i..j].iter().all(|&u| is_default_ignorable(u)) {
            let first = map[i] as usize;
            let last = if j < n { map[j] as usize } else { count };
            for g in g0 + first..g0 + last.max(first) {
                out.glyphs[g] = space[0];
                out.advances[g] = 0.0;
            }
        }
        i = j;
    }
}

/// Buffers `GetGlyphs` writes into, kept across runs.
#[derive(Default)]
pub(crate) struct GlyphScratch {
    cluster_map: Vec<u16>,
    text_props: Vec<DWRITE_SHAPING_TEXT_PROPERTIES>,
    glyph_ids: Vec<u16>,
    glyph_props: Vec<DWRITE_SHAPING_GLYPH_PROPERTIES>,
}

struct MapContext<'a> {
    source: &'a IDWriteTextAnalysisSource,
    families: &'a [(IDWriteFontCollection, Vec<u16>)],
    weight: DWRITE_FONT_WEIGHT,
    style: DWRITE_FONT_STYLE,
    stretch: DWRITE_FONT_STRETCH,
    primary: FontId,
}

/// The CSS generic families, as Windows browsers map them.
fn generic_family(name: &str) -> Option<&'static str> {
    Some(match name.to_ascii_lowercase().as_str() {
        "serif" | "ui-serif" => "Times New Roman",
        "sans-serif" => "Arial",
        "monospace" | "ui-monospace" => "Consolas",
        "cursive" => "Comic Sans MS",
        "fantasy" => "Impact",
        "system-ui" | "ui-sans-serif" | "-apple-system" => "Segoe UI",
        "ui-rounded" => "Segoe UI",
        "emoji" => "Segoe UI Emoji",
        "math" => "Cambria Math",
        "fangsong" => "FangSong",
        _ => return None,
    })
}

/// Combines DirectWrite's break conditions after one unit and before the next (`last`: end of text).
fn combine_breaks(after: u8, before_next: u8, last: bool) -> BreakAfter {
    const CAN: u8 = 1;
    const MAY_NOT: u8 = 2;
    const MUST: u8 = 3;
    if after == MUST || before_next == MUST {
        BreakAfter::Mandatory
    } else if last {
        BreakAfter::Allowed
    } else if after == MAY_NOT || before_next == MAY_NOT {
        BreakAfter::Never
    } else if after == CAN || before_next == CAN {
        BreakAfter::Allowed
    } else {
        BreakAfter::Never
    }
}

/// A family's name (en-us, else the first), NUL-terminated.
fn family_name_w(family: &IDWriteFontFamily) -> Result<Vec<u16>> {
    // SAFETY: the buffer has room for the reported length plus the NUL.
    unsafe {
        let names = family.GetFamilyNames()?;
        let mut index = 0;
        let mut exists = BOOL(0);
        let en: Vec<u16> = "en-us".encode_utf16().chain(Some(0)).collect();
        names.FindLocaleName(PCWSTR(en.as_ptr()), &mut index, &mut exists)?;
        if !exists.as_bool() {
            index = 0;
        }
        let len = names.GetStringLength(index)?;
        let mut buf = vec![0u16; len as usize + 1];
        names.GetString(index, &mut buf)?;
        Ok(buf)
    }
}

fn is_family(font: &IDWriteFont, name: &[u16]) -> bool {
    // SAFETY: owned interfaces.
    let mapped = unsafe { font.GetFontFamily() }.and_then(|f| family_name_w(&f));
    match mapped {
        Ok(m) => {
            let a = String::from_utf16_lossy(&m);
            let b = String::from_utf16_lossy(name);
            a.eq_ignore_ascii_case(&b)
        }
        Err(_) => false,
    }
}

/// The text DirectWrite analyzes, and what its analyses report back.
#[implement(IDWriteTextAnalysisSource, IDWriteTextAnalysisSink)]
pub(crate) struct Analysis {
    text: Vec<u16>,
    /// NUL-terminated.
    locale: Vec<u16>,
    rtl: bool,
    pub(crate) scripts: RefCell<Vec<(u32, u32, DWRITE_SCRIPT_ANALYSIS)>>,
    levels: RefCell<Vec<(u32, u32, u8)>>,
    breaks: RefCell<Vec<DWRITE_LINE_BREAKPOINT>>,
}

impl Analysis {
    pub(crate) fn new(text: Vec<u16>, locale: &str, rtl: bool) -> Analysis {
        let n = text.len();
        Analysis {
            text,
            locale: locale.encode_utf16().chain(Some(0)).collect(),
            rtl,
            scripts: RefCell::new(Vec::new()),
            levels: RefCell::new(Vec::new()),
            breaks: RefCell::new(vec![DWRITE_LINE_BREAKPOINT::default(); n]),
        }
    }
}

impl IDWriteTextAnalysisSource_Impl for Analysis_Impl {
    fn GetTextAtPosition(
        &self,
        position: u32,
        text: *mut *mut u16,
        length: *mut u32,
    ) -> Result<()> {
        // SAFETY: DirectWrite passes valid out pointers; the returned pointer stays valid while this
        // object lives (the text is never modified).
        unsafe {
            if position as usize >= self.text.len() {
                *text = std::ptr::null_mut();
                *length = 0;
            } else {
                *text = self.text.as_ptr().add(position as usize) as *mut u16;
                *length = self.text.len() as u32 - position;
            }
        }
        Ok(())
    }

    fn GetTextBeforePosition(
        &self,
        position: u32,
        text: *mut *mut u16,
        length: *mut u32,
    ) -> Result<()> {
        // SAFETY: as above.
        unsafe {
            if position == 0 || position as usize > self.text.len() {
                *text = std::ptr::null_mut();
                *length = 0;
            } else {
                *text = self.text.as_ptr() as *mut u16;
                *length = position;
            }
        }
        Ok(())
    }

    fn GetParagraphReadingDirection(&self) -> DWRITE_READING_DIRECTION {
        if self.rtl {
            DWRITE_READING_DIRECTION_RIGHT_TO_LEFT
        } else {
            DWRITE_READING_DIRECTION_LEFT_TO_RIGHT
        }
    }

    fn GetLocaleName(&self, position: u32, length: *mut u32, locale: *mut *mut u16) -> Result<()> {
        // SAFETY: valid out pointers; the locale buffer lives as long as this object.
        unsafe {
            *length = (self.text.len() as u32).saturating_sub(position);
            *locale = self.locale.as_ptr() as *mut u16;
        }
        Ok(())
    }

    fn GetNumberSubstitution(
        &self,
        position: u32,
        length: *mut u32,
        substitution: OutRef<IDWriteNumberSubstitution>,
    ) -> Result<()> {
        // SAFETY: valid out pointer.
        unsafe {
            *length = (self.text.len() as u32).saturating_sub(position);
        }
        substitution.write(None)
    }
}

impl IDWriteTextAnalysisSink_Impl for Analysis_Impl {
    fn SetScriptAnalysis(
        &self,
        position: u32,
        length: u32,
        analysis: *const DWRITE_SCRIPT_ANALYSIS,
    ) -> Result<()> {
        // SAFETY: DirectWrite passes a valid pointer for the duration of the call.
        let a = unsafe { *analysis };
        self.scripts.borrow_mut().push((position, length, a));
        Ok(())
    }

    fn SetLineBreakpoints(
        &self,
        position: u32,
        length: u32,
        breakpoints: *const DWRITE_LINE_BREAKPOINT,
    ) -> Result<()> {
        let mut breaks = self.breaks.borrow_mut();
        let end = (position + length) as usize;
        if end > breaks.len() {
            return Err(E_INVALIDARG.into());
        }
        // SAFETY: DirectWrite passes `length` breakpoints.
        let src = unsafe { std::slice::from_raw_parts(breakpoints, length as usize) };
        breaks[position as usize..end].copy_from_slice(src);
        Ok(())
    }

    fn SetBidiLevel(&self, position: u32, length: u32, _explicit: u8, resolved: u8) -> Result<()> {
        if (position + length) as usize > self.text.len() {
            return Err(E_INVALIDARG.into());
        }
        self.levels.borrow_mut().push((position, length, resolved));
        Ok(())
    }

    fn SetNumberSubstitution(
        &self,
        _position: u32,
        _length: u32,
        _substitution: Ref<IDWriteNumberSubstitution>,
    ) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display_list::DisplayListBuilder;
    use crate::geom::{Color, Point};

    fn engine() -> TextEngine {
        TextEngine::new().expect("DirectWrite")
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

    /// Splitting the shaping of "AVA" after "A" keeps the "AV" kerning in the first part (narrower
    /// than "A" shaped alone), gives each part clusters, runs and glyphs of its own with text offsets
    /// from its start and a wrap opportunity at its end, and loses no advance; a cluster across a
    /// boundary (a ligature) cannot be split.
    #[test]
    fn a_shaping_splits_into_the_parts_of_its_text() {
        let e = engine();
        let arial = list(&e, &["Arial"], 400);
        let options = ShapeOptions {
            size: 100.0,
            ..Default::default()
        };
        let mut whole = ShapedText::default();
        e.shape("AVA", arial, &options, &mut whole).unwrap();
        let mut alone = ShapedText::default();
        e.shape("A", arial, &options, &mut alone).unwrap();
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
        assert_eq!(parts[1].clusters[0].glyphs.0, 0);
        assert_eq!(parts[1].runs[0].text, (0, 2));
        assert_eq!(parts[1].glyphs.len(), 2);
        assert_eq!(parts[0].clusters[0].break_after, BreakAfter::Allowed);

        let mut ligature = whole.clone();
        ligature.clusters[0].text.1 = 2;
        ligature.clusters[1].text.0 = 2;
        assert!(ligature.split(&[1, 3]).is_none());
    }

    /// A query resolves to the same list whatever the case of its family names (CSS Fonts 4 §5.1:
    /// family names match ASCII case-insensitively), and to another list when a family, its order,
    /// or a descriptor differs.
    #[test]
    fn font_lists_are_found_by_their_query() {
        let e = engine();
        let a = list(&e, &["Nonexistent Family", "Arial", "Georgia"], 400);
        assert_eq!(
            list(&e, &["NONEXISTENT family", "arial", "GEORGIA"], 400),
            a
        );
        assert_ne!(
            list(&e, &["Nonexistent Family", "Georgia", "Arial"], 400),
            a
        );
        assert_ne!(list(&e, &["Nonexistent Family", "Arial"], 400), a);
        assert_ne!(
            list(&e, &["Nonexistent Family", "Arial", "Georgia"], 700),
            a
        );
        // (the separator between names is part of the key: "a b" + "c" is not "a" + "b c")
        assert_ne!(
            list(&e, &["Arial Black", "Georgia"], 400),
            list(&e, &["Arial", "Black Georgia"], 400)
        );
        let italic = e
            .font_list(&FontQuery {
                families: &["arial", "Georgia", "nonexistent family"],
                weight: 400,
                style: FontStyle::Italic,
                stretch: FontStretch::NORMAL,
            })
            .unwrap();
        assert_ne!(
            italic,
            list(&e, &["Arial", "Georgia", "Nonexistent Family"], 400)
        );
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

    /// Instances of one variable font file are different faces, each with an id of its own, and
    /// an id is only ever found again through a face object the engine still holds: a face object
    /// let go of may have its address reused by another font's face, whose glyphs would then be
    /// drawn with the wrong face (empty boxes but for the glyphs both happen to share).
    #[test]
    fn variable_instances_have_ids_of_their_own() {
        let e = engine();
        if e.find_family("Bahnschrift").unwrap().is_none() {
            return;
        }
        let ids: Vec<FontId> = [300, 400, 700, 400, 300]
            .iter()
            .map(|&w| e.primary_font(list(&e, &["Bahnschrift"], w)))
            .collect();
        assert_eq!(ids[1], ids[3]);
        assert_eq!(ids[0], ids[4]);
        assert!(
            ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2],
            "{ids:?}"
        );
        let weight = |id| {
            e.font_source(id)
                .unwrap()
                .axes
                .iter()
                .find(|a| a.0 == u32::from_le_bytes(*b"wght"))
                .map(|a| a.1)
        };
        assert!(weight(ids[0]) < weight(ids[2]));
        let state = e.state.read().unwrap();
        let held: std::collections::HashSet<usize> = state
            .faces
            .iter()
            .flat_map(|f| std::iter::once(&f.face).chain(&f.aliases))
            .map(|f| f.as_raw() as usize)
            .collect();
        for pointer in state.by_pointer.keys() {
            assert!(held.contains(pointer), "{pointer:#x} is not held");
        }
    }

    /// A local font is found by its family name, as an installed one is (and a web font is not).
    #[test]
    fn local_fonts_are_found_by_family_name() {
        let e = engine();
        let ahem = include_bytes!("../../snowlark-layout/tests/fixtures/Ahem.ttf");
        let web = e.add_font_data(ahem).unwrap();
        assert_ne!(web, "Ahem");
        if e.find_family("Ahem").unwrap().is_none() {
            assert_eq!(e.add_local_font(ahem).unwrap(), "Ahem");
        }
        let l = list(&e, &["ahem", "Arial"], 400);
        let primary = e.state.read().unwrap().lists[l.0 as usize].primary;
        assert_eq!(e.family_name(primary).as_deref(), Some("Ahem"));
        // Ahem's glyphs are 1em squares.
        let shaped = shape(&e, "XX", l, 20.0);
        assert!((shaped.width() - 40.0).abs() < 0.01, "{}", shaped.width());
    }

    /// Bidi isolates (Reddit's search placeholder wraps the community name in FSI...PDI) and other
    /// default-ignorable characters draw nothing and take no room.
    #[test]
    fn default_ignorables_are_invisible() {
        let e = engine();
        let l = list(&e, &["Segoe UI"], 400);
        let plain = shape(&e, "in r/videos", l, 16.0);
        let isolated = shape(&e, "in \u{2068}r/videos\u{2069}\u{200B}", l, 16.0);
        assert!(
            (plain.width() - isolated.width()).abs() < 0.01,
            "{} {}",
            plain.width(),
            isolated.width()
        );
        let space = shape(&e, " ", l, 16.0).glyphs[0];
        let hidden: Vec<_> = isolated
            .clusters
            .iter()
            .filter(|c| c.advance == 0.0)
            .collect();
        assert_eq!(hidden.len(), 3);
        for c in hidden {
            for g in c.glyphs.0..c.glyphs.1 {
                assert_eq!(isolated.glyphs[g as usize], space);
            }
        }
        assert!(is_default_ignorable(0x2068));
        assert!(is_default_ignorable(0x00AD) && !is_default_ignorable(0x41));
    }

    #[test]
    fn visual_order_reverses_rtl_runs() {
        assert_eq!(visual_order(&[0, 1, 1, 0]), vec![0, 2, 1, 3]);
        assert_eq!(visual_order(&[1, 1, 2, 2, 1]), vec![4, 2, 3, 1, 0]);
        assert_eq!(visual_order(&[0, 0]), vec![0, 1]);
    }

    #[test]
    fn family_list_skips_missing_families_and_matches_weight() {
        let e = engine();
        let l = list(&e, &["No Such Font 123", "Georgia", "serif"], 700);
        let font = e.primary_font(l);
        assert_eq!(e.family_name(font).as_deref(), Some("Georgia"));
        assert_eq!(e.weight(font), Some(700));
        // the same query is the same list
        assert_eq!(list(&e, &["No Such Font 123", "Georgia", "serif"], 700), l);
        let mono = e.primary_font(list(&e, &["monospace"], 400));
        assert_eq!(e.family_name(mono).as_deref(), Some("Consolas"));
    }

    #[test]
    fn metrics_scale_with_size() {
        let e = engine();
        let font = e.primary_font(list(&e, &["Segoe UI"], 400));
        let m = e.metrics(font, 20.0);
        let m2 = e.metrics(font, 40.0);
        assert!(m.ascent > 15.0 && m.ascent < 25.0, "{m:?}");
        assert!((m2.ascent - 2.0 * m.ascent).abs() < 1e-3);
        assert!(m.x_height > 0.0 && m.x_height < m.cap_height && m.cap_height < m.ascent);
    }

    #[test]
    fn shaping_gives_clusters_breaks_and_widths() {
        let e = engine();
        let l = list(&e, &["Segoe UI"], 400);
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
        let newline = shape(&e, "a\nb", l, 16.0);
        assert_eq!(newline.clusters[1].break_after, BreakAfter::Mandatory);
    }

    #[test]
    fn ligatures_and_kerning_follow_features() {
        let e = engine();
        let l = list(&e, &["Calibri"], 400);
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
        // Calibri ligates "ffi" into one glyph: one cluster for three characters
        assert!(with.glyphs.len() < without.glyphs.len());
        assert_eq!(with.clusters[0].text, (0, 3));
        // kerning pulls "AV" together
        assert!(with.width() < without.width());
    }

    #[test]
    fn fallback_covers_cjk_arabic_and_emoji() {
        let e = engine();
        let l = list(&e, &["Segoe UI"], 400);
        let s = shape(&e, "Hi 你好 مرحبا 😀", l, 16.0);
        let families: Vec<String> = s
            .runs
            .iter()
            .map(|r| e.family_name(r.font).unwrap())
            .collect();
        assert!(families.iter().any(|f| f != "Segoe UI"), "{families:?}");
        assert!(s.runs.iter().any(|r| r.bidi_level == 1), "Arabic is RTL");
        let emoji = s.runs.last().unwrap();
        assert!(emoji.color, "{:?}", families);
        // no .notdef glyphs
        assert!(s.glyphs.iter().all(|g| *g != 0));
        // the clusters cover the text in order
        assert_eq!(
            s.clusters.last().unwrap().text.1 as usize,
            "Hi 你好 مرحبا 😀".len()
        );
    }

    #[test]
    fn family_list_fallback_prefers_later_families() {
        let e = engine();
        // Consolas has no CJK; the list's next family (Yu Gothic) must supply it before system fallback
        let l = list(&e, &["Consolas", "Yu Gothic"], 400);
        let s = shape(&e, "ab日本", l, 16.0);
        let names: Vec<String> = s
            .runs
            .iter()
            .map(|r| e.family_name(r.font).unwrap())
            .collect();
        assert_eq!(names, vec!["Consolas", "Yu Gothic"]);
    }

    #[test]
    fn paint_emits_runs_in_visual_order() {
        let e = engine();
        let l = list(&e, &["Segoe UI"], 400);
        let s = shape(&e, "abc مرحبا def", l, 16.0);
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
        let path = std::path::Path::new(r"C:\Windows\Fonts\georgia.ttf");
        let data = std::fs::read(path).expect("Georgia is installed with Windows");
        let a = e.add_font_data(&data).unwrap();
        let b = e.add_font_data(&data).unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with(WEB_PREFIX));
        let font = e.primary_font(list(&e, &[a.as_str()], 400));
        assert_eq!(e.family_name(font).as_deref(), Some("Georgia"));
        let s = shape(&e, "Web font", list(&e, &[a.as_str()], 400), 20.0);
        assert!(s.width() > 50.0);
        assert!(e.add_font_data(b"not a font").is_err());
    }
}
