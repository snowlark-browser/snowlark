// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The raster side's fonts on DirectWrite: a face per [`FontId`], from installed files only.

use std::collections::HashSet;
use std::time::Instant;

use windows::core::{Error, Interface, Result, PCWSTR};
use windows::Win32::Foundation::{E_ACCESSDENIED, E_INVALIDARG, FILETIME};
use windows::Win32::Graphics::DirectWrite::*;

use super::{FontFile, FontSource, MAX_FONTS, MAX_FONT_BYTES, REREAD_FONT_FILES_AFTER};
use crate::display_list::FontId;
use crate::raster::FontResolver;

/// The files of this machine's installed fonts, as DirectWrite names them in its system font set
/// (which has the user's own installed fonts too): the only files a renderer may have a raster
/// side open. A path is taken only exactly as DirectWrite gives it, so no other spelling of a file
/// (`..`, another case, a link, a terminator inside) names one through it.
pub struct SystemFontFiles {
    paths: HashSet<Vec<u16>>,
    read: Instant,
}

impl SystemFontFiles {
    /// Reads the list from `factory`'s system font set.
    pub fn read(factory: &IDWriteFactory5) -> Result<SystemFontFiles> {
        let mut paths = HashSet::new();
        // SAFETY: DirectWrite calls with owned interfaces and an index below the set's count.
        unsafe {
            let set = factory.GetSystemFontSet()?;
            for i in 0..set.GetFontCount() {
                // (a face with no file on this machine, such as a downloadable one, is left out)
                let Ok(file) = set.GetFontFaceReference(i).and_then(|r| r.GetFontFile()) else {
                    continue;
                };
                if let Ok(Some((path, _))) = local_file(&file) {
                    paths.insert(path);
                }
            }
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

/// The path (UTF-16, no terminator) and last write time of a font file on disk; `None` for one that
/// is not (a font in memory).
pub(crate) fn local_file(file: &IDWriteFontFile) -> Result<Option<(Vec<u16>, FILETIME)>> {
    // SAFETY: the reference key pointer is valid while `file` lives; the path buffer has room for
    // the length the loader gives and its terminator.
    unsafe {
        let mut key = std::ptr::null_mut();
        let mut size = 0;
        file.GetReferenceKey(&mut key, &mut size)?;
        let Ok(local) = file.GetLoader()?.cast::<IDWriteLocalFontFileLoader>() else {
            return Ok(None);
        };
        let length = local.GetFilePathLengthFromKey(key, size)? as usize;
        let mut path = vec![0u16; length + 1];
        local.GetFilePathFromKey(key, size, &mut path)?;
        path.truncate(length);
        let time = local.GetLastWriteTimeFromKey(key, size)?;
        Ok(Some((path, time)))
    }
}

/// A renderer's fonts on the raster side: a face per [`FontId`], resolved by this table's own
/// DirectWrite factory.
pub struct FontTable {
    factory: IDWriteFactory5,
    /// Holds the fonts that came as bytes.
    memory: Option<IDWriteInMemoryFontFileLoader>,
    /// The files a face may be in, read when the first one is named.
    installed: Option<SystemFontFiles>,
    faces: Vec<Option<(IDWriteFontFace, bool)>>,
}

impl FontTable {
    pub fn new() -> Result<FontTable> {
        // SAFETY: plain factory creation; the returned interface is owned and released on drop.
        let factory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        Ok(FontTable {
            factory,
            memory: None,
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
            self.installed = Some(SystemFontFiles::read(&self.factory)?);
        }
        if listed(&self.installed) {
            Ok(())
        } else {
            Err(E_ACCESSDENIED.into())
        }
    }

    /// Makes `id` stand for the face `source` says, in place of any face it stood for. A source
    /// that is refused leaves `id` standing for none: the renderer's glyph ids are for the font it
    /// named, never to be drawn with the face the id named before.
    pub fn add(&mut self, id: FontId, source: &FontSource) -> Result<()> {
        if id.0 >= MAX_FONTS {
            return Err(E_INVALIDARG.into());
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

    /// The face `source` says, and whether it is a color font.
    fn make_face(&mut self, source: &FontSource) -> Result<(IDWriteFontFace, bool)> {
        let file = self.file(&source.file)?;
        let simulations = DWRITE_FONT_SIMULATIONS(source.simulations as i32);
        // SAFETY: DirectWrite calls with owned interfaces and slices that outlive each call; the
        // out-parameters of Analyze are locals.
        let face: IDWriteFontFace = unsafe {
            if source.axes.is_empty() {
                let mut supported = Default::default();
                let mut kind = DWRITE_FONT_FILE_TYPE_UNKNOWN;
                let mut face_kind = DWRITE_FONT_FACE_TYPE_UNKNOWN;
                let mut faces = 0;
                file.Analyze(&mut supported, &mut kind, Some(&mut face_kind), &mut faces)?;
                if !supported.as_bool() {
                    return Err(E_INVALIDARG.into());
                }
                let files = [Some(file)];
                self.factory
                    .CreateFontFace(face_kind, &files, source.index, simulations)?
            } else {
                let axes: Vec<DWRITE_FONT_AXIS_VALUE> = source
                    .axes
                    .iter()
                    .map(|&(tag, value)| DWRITE_FONT_AXIS_VALUE {
                        axisTag: DWRITE_FONT_AXIS_TAG(tag),
                        value,
                    })
                    .collect();
                self.factory
                    .cast::<IDWriteFactory6>()?
                    .CreateFontResource(&file, source.index)?
                    .CreateFontFace(simulations, &axes)?
                    .cast()?
            }
        };
        // SAFETY: IDWriteFontFace2::IsColorFont has no preconditions.
        let color = face
            .cast::<IDWriteFontFace2>()
            .is_ok_and(|f| unsafe { f.IsColorFont() }.as_bool());
        Ok((face, color))
    }

    fn file(&mut self, file: &FontFile) -> Result<IDWriteFontFile> {
        match file {
            FontFile::System { path, last_write } => {
                self.check_installed(path)?;
                let mut wide = Vec::with_capacity(path.len() + 1);
                wide.extend_from_slice(path);
                wide.push(0);
                let time = FILETIME {
                    dwLowDateTime: *last_write as u32,
                    dwHighDateTime: (*last_write >> 32) as u32,
                };
                // SAFETY: the path is terminated and it and the time outlive the call.
                unsafe {
                    self.factory
                        .CreateFontFileReference(PCWSTR(wide.as_ptr()), Some(&time))
                }
            }
            FontFile::Data(bytes) => {
                if bytes.len() > MAX_FONT_BYTES {
                    return Err(E_INVALIDARG.into());
                }
                // The renderer sends what OTS made of a web font, but a renderer is not trusted:
                // the bytes go through OTS again here before DirectWrite reads them.
                let bytes =
                    crate::web_font::sanitize(bytes).map_err(|_| Error::from(E_INVALIDARG))?;
                // SAFETY: DirectWrite calls with owned interfaces; the bytes are copied by
                // CreateInMemoryFontFileReference (no owner object is passed), so they need not
                // outlive the call.
                unsafe {
                    if self.memory.is_none() {
                        let loader = self.factory.CreateInMemoryFontFileLoader()?;
                        self.factory.RegisterFontFileLoader(&loader)?;
                        self.memory = Some(loader);
                    }
                    let loader = self.memory.as_ref().unwrap();
                    loader.CreateInMemoryFontFileReference(
                        &self.factory,
                        bytes.as_ptr().cast(),
                        bytes.len() as u32,
                        None,
                    )
                }
            }
        }
    }
}

impl Drop for FontTable {
    fn drop(&mut self) {
        // The shared factory would otherwise keep the fonts' bytes for the life of the process.
        if let Some(loader) = &self.memory {
            // SAFETY: the loader was registered with this factory and is owned here.
            let _ = unsafe { self.factory.UnregisterFontFileLoader(loader) };
        }
    }
}

impl FontResolver for FontTable {
    fn factory(&self) -> &IDWriteFactory5 {
        &self.factory
    }

    fn face(&self, font: FontId) -> Option<(IDWriteFontFace, bool)> {
        self.faces.get(font.0 as usize)?.clone()
    }
}
