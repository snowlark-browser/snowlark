// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The raster side's fonts on Linux: a face per [`FontId`], from the installed font files fontconfig
//! lists or from bytes that pass OTS, drawn through Skia from the same [`FontData`] the text engine
//! gives its own painter.

use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::sync::Arc;
use std::time::Instant;

use snowlark_graphics::{Error, Result};

use super::{FontFile, FontSource, MAX_FONTS, MAX_FONT_BYTES, REREAD_FONT_FILES_AFTER};
use crate::display_list::FontId;
use crate::raster::FontResolver;
use crate::text::ffi::*;
use crate::text::{FontBytes, FontData};

/// The files of this machine's installed fonts, as fontconfig lists them (the user's own fonts too):
/// the only files a renderer may have a raster side open. A path is taken only exactly as
/// fontconfig gives it, so no other spelling of a file (`..`, a link, a terminator inside) names one
/// through it. Paths are a [`FontFile::System`]'s: the path's bytes, one per unit.
pub struct SystemFontFiles {
    paths: HashSet<Vec<u16>>,
    read: Instant,
}

impl SystemFontFiles {
    /// Reads the list from a fontconfig configuration of its own.
    pub fn read() -> Result<SystemFontFiles> {
        let mut paths = HashSet::new();
        // SAFETY: a configuration owned here and destroyed at the end; the system set belongs to
        // it and is only read while it lives; each pattern's string is copied.
        unsafe {
            let config = FcInitLoadConfigAndFonts();
            if config.is_null() {
                return Err(Error::Platform("fontconfig has no configuration".into()));
            }
            let set = FcConfigGetFonts(config, FC_SET_SYSTEM);
            if !set.is_null() {
                let set = &*set;
                for i in 0..set.nfont.max(0) as usize {
                    let p = *set.fonts.add(i);
                    let mut file = std::ptr::null_mut();
                    if FcPatternGetString(p, FC_FILE.as_ptr().cast(), 0, &mut file)
                        == FC_RESULT_MATCH
                    {
                        let bytes = CStr::from_ptr(file.cast()).to_bytes();
                        paths.insert(bytes.iter().map(|&b| b as u16).collect());
                    }
                }
            }
            FcConfigDestroy(config);
        }
        Ok(SystemFontFiles {
            paths,
            read: Instant::now(),
        })
    }

    /// Whether `path` is one of the files.
    pub fn contains(&self, path: &[u16]) -> bool {
        self.paths.contains(path)
    }

    /// How many files there are.
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// A renderer's fonts on the raster side: a face's bytes and how it is drawn, per [`FontId`].
pub struct FontTable {
    /// The files a face may be in, read when the first one is named.
    installed: Option<SystemFontFiles>,
    faces: Vec<Option<FontData>>,
}

impl FontTable {
    pub fn new() -> Result<FontTable> {
        Ok(FontTable {
            installed: None,
            faces: Vec::new(),
        })
    }

    /// Refuses a file that is not one of the installed fonts. A file not in the list read at
    /// first is looked for again in a list read anew, at most every [`REREAD_FONT_FILES_AFTER`].
    fn check_installed(&mut self, path: &[u16]) -> Result<()> {
        let listed =
            |files: &Option<SystemFontFiles>| files.as_ref().is_some_and(|f| f.contains(path));
        if listed(&self.installed) {
            return Ok(());
        }
        let stale = self
            .installed
            .as_ref()
            .is_none_or(|f| f.read.elapsed() >= REREAD_FONT_FILES_AFTER);
        if stale {
            self.installed = Some(SystemFontFiles::read()?);
        }
        if listed(&self.installed) {
            Ok(())
        } else {
            Err(Error::Platform("not an installed font file".into()))
        }
    }

    /// Makes `id` stand for the face `source` says, in place of any face it stood for. A source
    /// that is refused leaves `id` standing for none: the renderer's glyph ids are for the font it
    /// named, never to be drawn with the face the id named before.
    pub fn add(&mut self, id: FontId, source: &FontSource) -> Result<()> {
        if id.0 >= MAX_FONTS {
            return Err(Error::Invalid);
        }
        let face = self.make_face(source);
        let index = id.0 as usize;
        if self.faces.len() <= index {
            if face.is_err() {
                return face.map(|_| ());
            }
            self.faces.resize(index + 1, None);
        }
        match face {
            Ok(face) => {
                self.faces[index] = Some(face);
                Ok(())
            }
            Err(e) => {
                self.faces[index] = None;
                Err(e)
            }
        }
    }

    fn make_face(&mut self, source: &FontSource) -> Result<FontData> {
        let bytes = match &source.file {
            FontFile::System { path, .. } => {
                self.check_installed(path)?;
                // (a listed path is one fontconfig gave: bytes, no NUL inside)
                let bytes: Vec<u8> = path.iter().map(|&u| u as u8).collect();
                let path = CString::new(bytes).map_err(|_| Error::Invalid)?;
                FontBytes::from_file(&path).ok_or(Error::Invalid)?
            }
            FontFile::Data(bytes) => {
                if bytes.len() > MAX_FONT_BYTES {
                    return Err(Error::TooLarge);
                }
                // The renderer sends what OTS made of a web font, but a renderer is not trusted:
                // the bytes go through OTS again here before the rasterizer reads them.
                let bytes = crate::web_font::sanitize(bytes).map_err(|_| Error::Invalid)?;
                FontBytes::from_vec(bytes).ok_or(Error::Invalid)?
            }
        };
        Ok(FontData {
            bytes: Arc::new(bytes),
            index: source.index,
            variations: source
                .axes
                .iter()
                .map(|&(tag, value)| (tag.to_le_bytes(), value))
                .collect(),
            embolden: source.simulations & 1 != 0,
            oblique: source.simulations & 2 != 0,
        })
    }
}

impl FontResolver for FontTable {
    fn font_data(&self, font: FontId) -> Option<FontData> {
        self.faces.get(font.0 as usize)?.clone()
    }
}
