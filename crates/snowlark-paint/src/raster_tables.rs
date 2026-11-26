// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a raster thread keeps of a renderer's world, so that a display list resolves on its side
//! without reaching into the page (`docs/process-isolation.md`, "Fonts" and "Images"). The list only
//! names fonts and images by the renderer's ids; the renderer sends what stands behind an id before
//! the first list that uses it, as a [`Resource`], and these tables resolve the ids from then on.
//!
//! Everything in a [`Resource`] is plain data that will cross a process boundary, so a table trusts
//! none of it: sizes are checked, a font file must be one of the installed fonts
//! ([`SystemFontFiles`]), and anything refused leaves the id unresolved, which makes the items that
//! use it fail as any unusable font does.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::display_list::{FontId, ImageId};
use crate::image::{ShownImage, MAX_PIXELS};
use crate::raster::ImageResolver;

// The font table and the list of installed font files: each platform's own.
#[cfg_attr(windows, path = "font_table.rs")]
#[cfg_attr(not(windows), path = "linux/font_table.rs")]
mod font_table;
pub use font_table::*;

/// The most fonts a table holds: ids are small numbers, handed out from zero.
pub const MAX_FONTS: u32 = 1 << 16;
/// The largest font file a table takes (the largest web font too).
pub use crate::web_font::MAX_FONT_BYTES;
/// The most bytes of lists a frame can have: about a million items, where a long page's band has
/// tens of thousands.
pub const MAX_LIST_BYTES: usize = 256 << 20;
/// The most tiles a frame can take: three viewports of an 8K screen are 780.
pub const MAX_TILES: u64 = 4096;

/// How long a table goes on with the list of installed font files it read before it reads it again
/// for a file it does not find there (a font installed since).
pub const REREAD_FONT_FILES_AFTER: Duration = Duration::from_secs(10);

/// The file a face is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FontFile {
    /// A file of this machine's installed fonts: its path (UTF-16, no terminator) and last write
    /// time (a FILETIME).
    System { path: Vec<u16>, last_write: u64 },
    /// The bytes of a font no file holds: a web font, or one registered in memory.
    Data(Vec<u8>),
}

/// Which face a [`FontId`] stands for.
#[derive(Clone, Debug, PartialEq)]
pub struct FontSource {
    pub file: FontFile,
    /// The face's index in the file (a collection holds several).
    pub index: u32,
    /// The bold and oblique simulations (`DWRITE_FONT_SIMULATIONS`).
    pub simulations: u32,
    /// The axis values of a variable font's instance, as (tag, value); empty for other fonts.
    pub axes: Vec<(u32, f32)>,
}

/// What a renderer sends ahead of the lists that need it.
#[derive(Clone, Debug, PartialEq)]
pub enum Resource {
    FontAdd {
        id: FontId,
        source: FontSource,
    },
    /// `id` shows these pixels now (premultiplied BGRA, rows `width * 4` bytes apart), in place of
    /// any it showed: `frame` of the image, `version` as [`crate::ImageStore::version`] numbers them.
    ImageAdd {
        id: ImageId,
        frame: usize,
        version: u64,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
    /// `id` shows nothing any more, or other pixels that follow.
    ImageRemove(ImageId),
    /// Every id is free: the ids that follow are of another store.
    ImagesClear,
}

impl Resource {
    /// Whether this one, not yet applied, is of no use once `later` comes after it. It is what keeps
    /// a queue of these from growing while frames are made faster than they are drawn: only frames
    /// are dropped then, and every canvas frame would otherwise stay queued with its pixels.
    pub fn outdated_by(&self, later: &Resource) -> bool {
        match (self, later) {
            (Resource::ImageAdd { id, .. }, Resource::ImageRemove(gone))
            | (Resource::ImageRemove(id), Resource::ImageRemove(gone)) => id == gone,
            (
                Resource::ImageAdd { .. } | Resource::ImageRemove(_) | Resource::ImagesClear,
                Resource::ImagesClear,
            ) => true,
            _ => false,
        }
    }
}

/// The most axis values a variable font's instance can name (OpenType fonts have a handful).
pub const MAX_FONT_AXES: usize = 64;
/// The longest path a [`FontFile::System`] can have, in UTF-16 units (Windows' own limit).
pub const MAX_FONT_PATH: usize = 32_767;
/// `DWRITE_FONT_SIMULATIONS_BOLD | DWRITE_FONT_SIMULATIONS_OBLIQUE`: every simulation there is.
const ALL_SIMULATIONS: u32 = 3;

/// A [`Resource`] as a message carries it: all of it but its bytes (an image's pixels, the data of a
/// font no file holds), which travel beside the message, because a message is at most a pipe frame
/// and pixels are megabytes (across processes the bytes will be in a shared section; in one, the
/// vector itself moves). [`Resource::join`] is the only way back to a [`Resource`], and it checks
/// everything: a head and bytes from another process are trusted no more than a display list is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ResourceHead {
    FontAdd {
        id: FontId,
        file: FileHead,
        index: u32,
        simulations: u32,
        axes: Vec<(u32, f32)>,
    },
    /// The pixels, `width * height * 4` bytes, are beside it.
    ImageAdd {
        id: ImageId,
        frame: u32,
        version: u64,
        width: u32,
        height: u32,
    },
    ImageRemove(ImageId),
    ImagesClear,
}

/// [`FontFile`] in a [`ResourceHead`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FileHead {
    System {
        path: Vec<u16>,
        last_write: u64,
    },
    /// The font's bytes are beside the message.
    Data,
}

/// Why a message of the raster seam and its bytes were refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused(pub &'static str);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "refused: {}", self.0)
    }
}

impl std::error::Error for Refused {}

impl Resource {
    /// The message and the bytes that travel beside it (empty when it has none).
    pub fn split(self) -> (ResourceHead, Vec<u8>) {
        match self {
            Resource::FontAdd { id, source } => {
                let FontSource {
                    file,
                    index,
                    simulations,
                    axes,
                } = source;
                let (file, bytes) = match file {
                    FontFile::System { path, last_write } => {
                        (FileHead::System { path, last_write }, Vec::new())
                    }
                    FontFile::Data(bytes) => (FileHead::Data, bytes),
                };
                let head = ResourceHead::FontAdd {
                    id,
                    file,
                    index,
                    simulations,
                    axes,
                };
                (head, bytes)
            }
            Resource::ImageAdd {
                id,
                frame,
                version,
                width,
                height,
                pixels,
            } => {
                let head = ResourceHead::ImageAdd {
                    id,
                    // (an image has far fewer frames; the raster side only keeps the number)
                    frame: u32::try_from(frame).unwrap_or(u32::MAX),
                    version,
                    width,
                    height,
                };
                (head, pixels)
            }
            Resource::ImageRemove(id) => (ResourceHead::ImageRemove(id), Vec::new()),
            Resource::ImagesClear => (ResourceHead::ImagesClear, Vec::new()),
        }
    }

    /// The resource `head` and `bytes` make, if it is one a raster side takes: a font id in range,
    /// a path that can name a file, finite axis values, known simulations, and exactly the bytes
    /// the head says, within the tables' caps.
    pub fn join(head: ResourceHead, bytes: Vec<u8>) -> std::result::Result<Resource, Refused> {
        let no_bytes = |bytes: &[u8]| {
            if bytes.is_empty() {
                Ok(())
            } else {
                Err(Refused("bytes beside a message that has none"))
            }
        };
        match head {
            ResourceHead::FontAdd {
                id,
                file,
                index,
                simulations,
                axes,
            } => {
                if id.0 >= MAX_FONTS {
                    return Err(Refused("font id out of range"));
                }
                if simulations & !ALL_SIMULATIONS != 0 {
                    return Err(Refused("unknown font simulations"));
                }
                if axes.len() > MAX_FONT_AXES || axes.iter().any(|(_, v)| !v.is_finite()) {
                    return Err(Refused("font axis values"));
                }
                let file = match file {
                    FileHead::System { path, last_write } => {
                        no_bytes(&bytes)?;
                        if path.is_empty() || path.len() > MAX_FONT_PATH || path.contains(&0) {
                            return Err(Refused("font path"));
                        }
                        FontFile::System { path, last_write }
                    }
                    FileHead::Data => {
                        if bytes.is_empty() || bytes.len() > MAX_FONT_BYTES {
                            return Err(Refused("font data size"));
                        }
                        FontFile::Data(bytes)
                    }
                };
                Ok(Resource::FontAdd {
                    id,
                    source: FontSource {
                        file,
                        index,
                        simulations,
                        axes,
                    },
                })
            }
            ResourceHead::ImageAdd {
                id,
                frame,
                version,
                width,
                height,
            } => {
                let area = width as u64 * height as u64;
                if area > MAX_PIXELS {
                    return Err(Refused("image larger than an image can be"));
                }
                if bytes.len() as u64 != area * 4 {
                    return Err(Refused("pixels not what the image's size says"));
                }
                Ok(Resource::ImageAdd {
                    id,
                    frame: frame as usize,
                    version,
                    width,
                    height,
                    pixels: bytes,
                })
            }
            ResourceHead::ImageRemove(id) => {
                no_bytes(&bytes)?;
                Ok(Resource::ImageRemove(id))
            }
            ResourceHead::ImagesClear => {
                no_bytes(&bytes)?;
                Ok(Resource::ImagesClear)
            }
        }
    }
}

/// A renderer's images on the raster side: which pixels each [`ImageId`] shows. The pixels themselves
/// go to the device when they come ([`crate::TileRasterizer::upload`]) and are not kept.
#[derive(Default)]
pub struct ImageTable {
    shown: HashMap<ImageId, ShownImage>,
    /// Keeps the pixels instead ([`ImageTable::keeping`]).
    keep: bool,
}

impl ImageTable {
    pub fn new() -> ImageTable {
        ImageTable::default()
    }

    /// A table that keeps the pixels of what each id shows, for a side that draws the same images
    /// on more than one device context, made at any time (a compositor's windows): each uploads
    /// them when it first draws them, as from a store.
    pub fn keeping() -> ImageTable {
        ImageTable {
            keep: true,
            ..ImageTable::default()
        }
    }

    /// Makes `id` show what a [`Resource::ImageAdd`] says, and returns that, unless `pixels` are not
    /// what the size says or the size is more than an image can be: `id` then shows nothing.
    pub fn add(
        &mut self,
        id: ImageId,
        frame: usize,
        version: u64,
        (width, height): (u32, u32),
        pixels: &[u8],
    ) -> Option<&ShownImage> {
        self.shown.remove(&id);
        let area = width as u64 * height as u64;
        if area > MAX_PIXELS || pixels.len() as u64 != area * 4 {
            return None;
        }
        let shown = if self.keep {
            // (the one frame kept is the image's only one here)
            let still = crate::image::DecodedImage::still(crate::image::Frame {
                width,
                height,
                pixels: pixels.to_vec(),
                delay_ms: 0,
            });
            ShownImage {
                frame: 0,
                version,
                width,
                height,
                image: Some(Arc::new(still)),
            }
        } else {
            ShownImage {
                frame,
                version,
                width,
                height,
                image: None,
            }
        };
        Some(self.shown.entry(id).or_insert(shown))
    }

    pub fn remove(&mut self, id: ImageId) {
        self.shown.remove(&id);
    }

    pub fn clear(&mut self) {
        self.shown.clear();
    }
}

impl ImageResolver for ImageTable {
    fn shown(&self, id: ImageId) -> Option<ShownImage> {
        self.shown.get(&id).cloned()
    }

    fn version(&self, id: ImageId) -> u64 {
        self.shown.get(&id).map_or(0, |s| s.version)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::raster::FontResolver;
    use crate::text::{FontQuery, FontStretch, FontStyle, ShapeOptions, ShapedText, TextEngine};
    use std::time::Instant;
    use windows::Win32::Graphics::DirectWrite::*;

    fn font(text: &TextEngine, families: &[&str], weight: u16, style: FontStyle) -> FontId {
        let list = text
            .font_list(&FontQuery {
                families,
                weight,
                style,
                stretch: FontStretch::NORMAL,
            })
            .unwrap();
        text.primary_font(list)
    }

    fn windows_font(name: &str) -> Vec<u8> {
        std::fs::read(std::path::Path::new(r"C:\Windows\Fonts").join(name)).unwrap()
    }

    fn lower(path: &[u16]) -> String {
        String::from_utf16_lossy(path).to_lowercase()
    }

    #[test]
    fn the_image_table_checks_sizes() {
        let mut table = ImageTable::new();
        let id = ImageId(4);
        let pixels = [0u8; 2 * 3 * 4];
        assert!(table.add(id, 1, 9, (2, 3), &pixels).is_some());
        let shown = table.shown(id).unwrap();
        assert_eq!((shown.frame, shown.version), (1, 9));
        assert_eq!((shown.width, shown.height), (2, 3));
        assert_eq!(table.version(id), 9);
        // (the wrong size takes away what the id showed)
        assert!(table.add(id, 1, 10, (3, 3), &pixels).is_none());
        assert!(table.shown(id).is_none());
        assert_eq!(table.version(id), 0);
        let huge = u32::MAX;
        assert!(table.add(id, 0, 1, (huge, huge), &pixels).is_none());
        let edge = (MAX_PIXELS as u32 + 1, 1);
        assert!(table.add(id, 0, 1, edge, &pixels).is_none());
        assert!(table.add(id, 0, 1, (0, 0), &[]).is_some());
    }

    #[test]
    fn a_keeping_table_keeps_the_pixels_as_one_frame() {
        let mut table = ImageTable::keeping();
        let id = ImageId(4);
        let pixels: Vec<u8> = (0..2 * 3 * 4).collect();
        assert!(table.add(id, 5, 9, (2, 3), &pixels).is_some());
        let shown = table.shown(id).unwrap();
        // (frame 0 of the image kept: the only one it has)
        assert_eq!((shown.frame, shown.version), (0, 9));
        let image = shown.image.expect("the pixels are kept");
        assert_eq!(image.frames.len(), 1);
        assert_eq!(image.frames[0].pixels, pixels);
        assert_eq!((image.frames[0].width, image.frames[0].height), (2, 3));
        // (the same pixels each time it is asked, so a context's upload stays)
        let again = table.shown(id).unwrap().image.unwrap();
        assert!(Arc::ptr_eq(&image, &again));
        assert!(table.add(id, 0, 10, (3, 3), &pixels).is_none());
        assert!(table.shown(id).is_none());
    }

    #[test]
    fn a_later_removal_outdates_what_is_queued_for_the_image() {
        let add = |id| Resource::ImageAdd {
            id: ImageId(id),
            frame: 0,
            version: 1,
            width: 0,
            height: 0,
            pixels: Vec::new(),
        };
        let remove = |id| Resource::ImageRemove(ImageId(id));
        assert!(add(1).outdated_by(&remove(1)));
        assert!(remove(1).outdated_by(&remove(1)));
        assert!(!add(1).outdated_by(&remove(2)));
        assert!(!remove(1).outdated_by(&add(1)));
        assert!(add(1).outdated_by(&Resource::ImagesClear));
        assert!(remove(1).outdated_by(&Resource::ImagesClear));
        assert!(!Resource::ImagesClear.outdated_by(&add(1)));
        let font = Resource::FontAdd {
            id: FontId(0),
            source: FontSource {
                file: FontFile::Data(Vec::new()),
                index: 0,
                simulations: 0,
                axes: Vec::new(),
            },
        };
        assert!(!font.outdated_by(&Resource::ImagesClear));
    }

    #[test]
    fn a_system_face_is_named_by_its_file() {
        let text = TextEngine::new().unwrap();
        let id = font(&text, &["Arial"], 400, FontStyle::Normal);
        let source = text.font_source(id).unwrap();
        let FontFile::System { path, last_write } = &source.file else {
            panic!("{source:?}");
        };
        assert!(lower(path).ends_with("arial.ttf"), "{}", lower(path));
        assert_ne!(*last_write, 0);
        assert_eq!((source.index, source.simulations), (0, 0));
        assert!(source.axes.is_empty());
    }

    #[test]
    fn faces_keep_their_index_simulations_and_axes() {
        let text = TextEngine::new().unwrap();
        // (Impact has no bold of its own)
        let id = font(&text, &["Impact"], 700, FontStyle::Normal);
        let bold = DWRITE_FONT_SIMULATIONS_BOLD.0 as u32;
        assert_eq!(text.font_source(id).unwrap().simulations, bold);
        let id = font(&text, &["MS PGothic"], 400, FontStyle::Normal);
        let source = text.font_source(id).unwrap();
        assert!(matches!(&source.file, FontFile::System { path, .. }
            if lower(path).ends_with(".ttc")));
        assert_ne!(source.index, 0);
        let id = font(&text, &["Bahnschrift"], 700, FontStyle::Normal);
        let source = text.font_source(id).unwrap();
        let weight = u32::from_le_bytes(*b"wght");
        assert!(source.axes.contains(&(weight, 700.0)), "{:?}", source.axes);
    }

    #[test]
    fn a_web_font_goes_as_what_the_sanitizer_made_of_it() {
        let text = TextEngine::new().unwrap();
        let bytes = windows_font("georgia.ttf");
        let family = text.add_font_data(&bytes).unwrap();
        let id = font(&text, &[&family], 400, FontStyle::Normal);
        let source = text.font_source(id).unwrap();
        let sanitized = crate::web_font::sanitize(&bytes).unwrap();
        assert!(
            source.file == FontFile::Data(sanitized),
            "not the same bytes"
        );
    }

    #[test]
    fn the_table_makes_the_faces_it_is_told_of() {
        let text = TextEngine::new().unwrap();
        let bytes = windows_font("georgia.ttf");
        let family = text.add_font_data(&bytes).unwrap();
        let emoji = font(&text, &["Segoe UI Emoji"], 400, FontStyle::Normal);
        let web = font(&text, &[&family], 400, FontStyle::Normal);
        let bahn = font(&text, &["Bahnschrift"], 700, FontStyle::Normal);
        let mut table = FontTable::new().unwrap();
        assert!(table.face(emoji).is_none());
        for id in [emoji, web, bahn] {
            table.add(id, &text.font_source(id).unwrap()).unwrap();
        }
        let metrics = |face: &IDWriteFontFace| {
            let mut m = DWRITE_FONT_METRICS::default();
            // SAFETY: GetMetrics fills a local.
            unsafe { face.GetMetrics(&mut m) };
            (m.designUnitsPerEm, m.ascent, m.descent, m.capHeight)
        };
        for id in [emoji, web, bahn] {
            let (ours, color) = table.face(id).unwrap();
            let (theirs, their_color) = FontResolver::face(&text, id).unwrap();
            assert_eq!(metrics(&ours), metrics(&theirs));
            assert_eq!(color, their_color);
        }
        assert!(table.face(emoji).unwrap().1, "emoji have color");
    }

    #[test]
    fn the_installed_font_files_are_listed() {
        let text = TextEngine::new().unwrap();
        let started = Instant::now();
        let files = SystemFontFiles::read(text.factory()).unwrap();
        eprintln!(
            "{} installed font files, read in {:.1} ms",
            files.len(),
            started.elapsed().as_secs_f64() * 1000.0
        );
        assert!(files.len() > 50, "{} files", files.len());
        let id = font(&text, &["Arial"], 400, FontStyle::Normal);
        let FontFile::System { path, .. } = text.font_source(id).unwrap().file else {
            panic!("Arial is not a file");
        };
        assert!(files.contains(&path));
    }

    #[test]
    fn only_installed_font_files_are_opened() {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::fs::MetadataExt;

        let text = TextEngine::new().unwrap();
        let id = font(&text, &["Georgia"], 400, FontStyle::Normal);
        let installed = text.font_source(id).unwrap();
        let FontFile::System { path, last_write } = installed.file.clone() else {
            panic!("Georgia is not a file");
        };
        let named = |path: Vec<u16>, last_write| FontSource {
            file: FontFile::System { path, last_write },
            ..installed.clone()
        };
        let mut table = FontTable::new().unwrap();
        table.add(FontId(1), &installed).unwrap();

        // (a font file, but not an installed one)
        let dir = std::env::temp_dir().join(format!("snowlark-fonts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("georgia.ttf");
        std::fs::copy(String::from_utf16(&path).unwrap(), &copy).unwrap();
        let copied = std::fs::metadata(&copy).unwrap().last_write_time();
        let copy_path = copy.as_os_str().encode_wide().collect();
        let refused = table.add(FontId(2), &named(copy_path, copied));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(refused.is_err());
        // (the installed file, spelled another way)
        let spelled = String::from_utf16(&path).unwrap();
        let (folder, file) = spelled.rsplit_once('\\').unwrap();
        let (_, last) = folder.rsplit_once('\\').unwrap();
        let dotted = format!("{folder}\\..\\{last}\\{file}");
        assert!(table
            .add(
                FontId(3),
                &named(dotted.encode_utf16().collect(), last_write)
            )
            .is_err());
        let mut terminated = path.clone();
        terminated.extend("\0x".encode_utf16());
        assert!(table
            .add(FontId(4), &named(terminated, last_write))
            .is_err());

        assert!(table.face(FontId(1)).is_some());
        for id in 2..=4 {
            assert!(table.face(FontId(id)).is_none());
        }
    }

    #[test]
    fn fallback_faces_are_in_installed_files() {
        let text = TextEngine::new().unwrap();
        let families = ["Arial"];
        let list = text
            .font_list(&FontQuery {
                families: &families,
                weight: 400,
                style: FontStyle::Normal,
                stretch: FontStretch::NORMAL,
            })
            .unwrap();
        let mut shaped = ShapedText::default();
        let sample = "Latin \u{65E5}\u{672C}\u{8A9E} \u{645}\u{631}\u{62D}\u{628}\u{627} \u{939}\u{93F}\u{902}\u{926}\u{940} \u{1F600} \u{2603}";
        text.shape(sample, list, &ShapeOptions::default(), &mut shaped)
            .unwrap();
        let mut fonts: Vec<FontId> = shaped.runs.iter().map(|r| r.font).collect();
        fonts.sort();
        fonts.dedup();
        assert!(fonts.len() >= 4, "{} fonts", fonts.len());
        let mut table = FontTable::new().unwrap();
        for id in fonts {
            let source = text.font_source(id).unwrap();
            let name = text.family_name(id);
            table
                .add(id, &source)
                .unwrap_or_else(|e| panic!("{name:?} refused: {e}"));
        }
    }

    #[test]
    fn the_table_refuses_what_it_cannot_use() {
        let mut table = FontTable::new().unwrap();
        let data = |bytes: Vec<u8>| FontSource {
            file: FontFile::Data(bytes),
            index: 0,
            simulations: 0,
            axes: Vec::new(),
        };
        let junk = data(vec![7; 1000]);
        assert!(table.add(FontId(3), &junk).is_err());
        assert!(table.face(FontId(3)).is_none());
        assert!(table
            .add(FontId(1), &data(vec![0; MAX_FONT_BYTES + 1]))
            .is_err());
        let font = data(windows_font("georgia.ttf"));
        assert!(table.add(FontId(MAX_FONTS), &font).is_err());
        let missing = FontSource {
            file: FontFile::System {
                path: "C:\\no such file.ttf".encode_utf16().collect(),
                last_write: 0,
            },
            ..font.clone()
        };
        assert!(table.add(FontId(4), &missing).is_err());
        let past_the_end = FontSource {
            index: 40,
            ..font.clone()
        };
        assert!(table.add(FontId(5), &past_the_end).is_err());
        table.add(FontId(2), &font).unwrap();
        assert!(table.face(FontId(2)).is_some());
    }

    #[test]
    fn a_refused_font_unresolves_the_id_it_was_sent_for() {
        // The id stood for one face; what it stands for now is refused. Drawing the new font's
        // glyph ids with the old face would show its .notdef boxes, or other characters.
        let mut table = FontTable::new().unwrap();
        let font = FontSource {
            file: FontFile::Data(windows_font("georgia.ttf")),
            index: 0,
            simulations: 0,
            axes: Vec::new(),
        };
        table.add(FontId(6), &font).unwrap();
        assert!(table.face(FontId(6)).is_some());
        let missing = FontSource {
            file: FontFile::System {
                path: "C:\\no such file.ttf".encode_utf16().collect(),
                last_write: 0,
            },
            ..font.clone()
        };
        assert!(table.add(FontId(6), &missing).is_err());
        assert!(table.face(FontId(6)).is_none());
        let junk = FontSource {
            file: FontFile::Data(vec![7; 1000]),
            ..font.clone()
        };
        table.add(FontId(6), &font).unwrap();
        assert!(table.add(FontId(6), &junk).is_err());
        assert!(table.face(FontId(6)).is_none());
    }

    fn path(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn system_font() -> Resource {
        Resource::FontAdd {
            id: FontId(3),
            source: FontSource {
                file: FontFile::System {
                    path: path(r"C:\Windows\Fonts\arial.ttf"),
                    last_write: 0x01DA_0000_1234_5678,
                },
                index: 0,
                simulations: 1,
                axes: Vec::new(),
            },
        }
    }

    fn web_font() -> Resource {
        Resource::FontAdd {
            id: FontId(7),
            source: FontSource {
                file: FontFile::Data(vec![0, 1, 0, 0, 9, 9]),
                index: 1,
                simulations: 3,
                axes: vec![(u32::from_be_bytes(*b"wght"), 650.0)],
            },
        }
    }

    fn pixels(width: u32, height: u32) -> Resource {
        Resource::ImageAdd {
            id: ImageId(9),
            frame: 2,
            version: 41,
            width,
            height,
            pixels: (0..width * height * 4).map(|i| i as u8).collect(),
        }
    }

    #[test]
    fn every_resource_splits_and_joins_back() {
        for r in [
            system_font(),
            web_font(),
            pixels(3, 2),
            pixels(0, 0),
            Resource::ImageRemove(ImageId(1)),
            Resource::ImagesClear,
        ] {
            let (head, bytes) = r.clone().split();
            assert_eq!(Resource::join(head, bytes), Ok(r));
        }
    }

    /// `head` with `bytes`, or with the bytes it came with when `bytes` is `None`.
    fn joined(
        r: Resource,
        change: impl FnOnce(&mut ResourceHead, &mut Vec<u8>),
    ) -> std::result::Result<Resource, Refused> {
        let (mut head, mut bytes) = r.split();
        change(&mut head, &mut bytes);
        Resource::join(head, bytes)
    }

    fn font_head(
        head: &mut ResourceHead,
    ) -> (&mut FontId, &mut FileHead, &mut u32, &mut Vec<(u32, f32)>) {
        match head {
            ResourceHead::FontAdd {
                id,
                file,
                simulations,
                axes,
                ..
            } => (id, file, simulations, axes),
            _ => unreachable!(),
        }
    }

    #[test]
    fn corrupt_resources_are_refused() {
        let refused = |r: std::result::Result<Resource, Refused>, why: &str| {
            assert_eq!(r.map_err(|e| e.0), Err(why), "{why}");
        };
        // Pixels not what the size says, either way.
        refused(
            joined(pixels(3, 2), |_, b| {
                b.pop();
            }),
            "pixels not what the image's size says",
        );
        refused(
            joined(pixels(3, 2), |_, b| b.push(0)),
            "pixels not what the image's size says",
        );
        refused(
            joined(pixels(1, 1), |h, _| {
                if let ResourceHead::ImageAdd { width, height, .. } = h {
                    (*width, *height) = (u32::MAX, u32::MAX);
                }
            }),
            "image larger than an image can be",
        );
        // Fonts.
        refused(
            joined(system_font(), |h, _| *font_head(h).0 = FontId(MAX_FONTS)),
            "font id out of range",
        );
        refused(
            joined(system_font(), |h, _| *font_head(h).2 = 4),
            "unknown font simulations",
        );
        refused(
            joined(web_font(), |h, _| font_head(h).3[0].1 = f32::NAN),
            "font axis values",
        );
        refused(
            joined(web_font(), |h, _| {
                *font_head(h).3 = vec![(1, 1.0); MAX_FONT_AXES + 1]
            }),
            "font axis values",
        );
        let with_path = |p: Vec<u16>| {
            joined(system_font(), move |h, _| {
                if let FileHead::System { path, .. } = font_head(h).1 {
                    *path = p;
                }
            })
        };
        refused(with_path(Vec::new()), "font path");
        // (a terminator inside would name another file than the one the path spells)
        refused(
            with_path(path("C:\\Windows\\Fonts\\arial.ttf\0evil")),
            "font path",
        );
        refused(with_path(vec![b'a' as u16; MAX_FONT_PATH + 1]), "font path");
        assert!(with_path(vec![b'a' as u16; MAX_FONT_PATH]).is_ok());
        refused(
            joined(system_font(), |_, b| b.push(1)),
            "bytes beside a message that has none",
        );
        refused(joined(web_font(), |_, b| b.clear()), "font data size");
        refused(
            joined(web_font(), |_, b| b.resize(MAX_FONT_BYTES + 1, 0)),
            "font data size",
        );
        refused(
            joined(Resource::ImageRemove(ImageId(1)), |_, b| b.push(1)),
            "bytes beside a message that has none",
        );
        refused(
            joined(Resource::ImagesClear, |_, b| b.push(1)),
            "bytes beside a message that has none",
        );
    }
}
