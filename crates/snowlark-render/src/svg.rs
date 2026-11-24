// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SVG images: `<img src=x.svg>`, CSS `url()`s (backgrounds, `list-style-image`, `content`) and
//! `<picture>` sources that are SVG documents. Not inline `<svg>` in an HTML document, which is an
//! element tree with its own layout and painting (layout keeps treating an outermost `svg` element as
//! a replaced box that paints nothing).
//!
//! An SVG image is a vector: it has no pixel size, so it is rasterized for each size it is drawn at.
//! - **Natural dimensions** ([`SvgImage::natural`]) come from the root element's `width`, `height` and
//!   `viewBox` (SVG 2 §8.2 and CSS Images 3 §4.1): an absolute `width`/`height` is a natural size (a
//!   percentage, `auto` or a missing one is not); the natural ratio is `width / height` when both are
//!   absolute, else the `viewBox` ratio. Layout sizes the box with them through CSS Images 3's default
//!   sizing algorithm, so a `viewBox`-only image (ratio only) fills its containing block's width.
//! - **Rasterization** ([`SvgImage::rasterize`]): the image is drawn at its concrete object size, which
//!   becomes the SVG viewport: the root's `width`/`height` are replaced by that size before parsing (so
//!   `viewBox` with `preserveAspectRatio`, and percentages, resolve against it as the SVG
//!   integration rules say), and an image with absolute sizes but no `viewBox` gets the synthesized
//!   `viewBox="0 0 width height"` browsers use, with `preserveAspectRatio="none"`, so its content
//!   scales with the drawn size as a bitmap's would. The tree is rendered at the
//!   device size (CSS size x device pixel ratio x the current transform's scale), so a 20 px icon on a
//!   2x display is a 40 px bitmap.
//! - **Caching** ([`SvgImages`]): each SVG URL gets one placeholder [`ImageId`] that layout and the
//!   display list use; after painting, [`SvgImages::resolve`] swaps every placeholder in the display
//!   list for a bitmap of the exact device size, rasterized once per (image, size) and kept while it
//!   is used (at most [`MAX_SIZES`] sizes per image).
//!
//! - **The image's document** ([`SvgDocument`]): an SVG image is a document of its own (HTML §4.8.4.1,
//!   SVG Integration §3.4 secure animated mode), parsed as XML into a Snowlark DOM and styled by stylo:
//!   its `<style>` sheets, presentation attributes and CSS animations, media queries against the
//!   viewport it is drawn at and the embedding document's color scheme (`prefers-color-scheme`, CSS
//!   Color Adjust 1 §2.3), its timeline the embedding document's. Each rasterization serializes the
//!   elements with the styles they compute at that time, as an inline `svg` element is serialized
//!   ([`crate::inline_svg`]), and resvg draws that; while its animations run and it shows, the page's
//!   frames are due ([`SvgImages::frame_due`]) and the image is drawn again whenever its serialization
//!   changes. A document that does not parse is drawn by resvg from its source, statically.
//!
//! **Secure animated mode** (SVG Integration §3.3/§3.4: SVG documents loaded as images run no script,
//! load no external resource, and have no interactivity; declarative animations run): no script runs
//! (the document is only parsed and styled) and resvg has no `foreignObject` HTML; its `<image>` href
//! resolver here only accepts `data:` URLs (the default one would read local files), nothing resolves
//! relative paths (`resources_dir` is unset), `@import`s and web fonts are not fetched, and the fonts
//! are the system's installed fonts (loaded once, only for images that have text).

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, OnceLock};

use resvg::tiny_skia;
use resvg::usvg;
use snowlark_layout::style::NaturalSize;
use snowlark_paint::svg_filter::Kind;
use snowlark_paint::{
    DecodedImage, DisplayList, FilterOp, Frame, ImageId, ImageRendering, ImageStore, Item,
};
use usvg::{fontdb, roxmltree};

/// The largest SVG source accepted (after gzip decompression of `.svgz`).
pub const MAX_SOURCE_BYTES: usize = 16 << 20;
/// The most device pixels one rasterization may have; larger draws are rasterized at this budget and
/// scaled up by the painter.
pub const MAX_RASTER_PIXELS: u64 = 16 << 20;
/// How many sizes of one image are kept.
pub const MAX_SIZES: usize = 8;

const SVG_NS: &str = "http://www.w3.org/2000/svg";

/// Whether a response is an SVG image: its MIME type is `image/svg+xml` (MIME Sniffing's image
/// sniffing never yields SVG, so the type decides).
pub fn is_svg_mime(essence: &str) -> bool {
    essence.eq_ignore_ascii_case("image/svg+xml")
}

/// A parsed SVG image: its source and what the root element says about its size.
#[derive(Clone, Debug)]
pub struct SvgImage {
    source: String,
    natural: NaturalSize,
    /// The root's absolute `width` and `height` with no `viewBox`: rasterizing synthesizes one.
    synthesize_view_box: Option<(f32, f32)>,
    /// Where the root start tag's name ends (attributes are inserted there).
    insert_at: usize,
    /// The root's `width` and `height` attributes, which rasterizing replaces.
    remove: Vec<Range<usize>>,
    has_text: bool,
    /// The fetched bytes of the images an inline `svg` element's `<image>` elements show, by
    /// absolute URL (see [`crate::inline_svg`]); empty for an SVG image, which loads nothing.
    images: HashMap<String, Arc<Vec<u8>>>,
    /// The root's `viewBox` has a zero width or height, which disables its rendering (SVG 2
    /// §8.6): the image draws nothing.
    disabled: bool,
    /// The canvas color: the root element's background (CSS Backgrounds 3 §2.11.2: the root
    /// background paints the whole canvas), for an image styled as a document.
    background: Option<snowlark_paint::Color>,
}

/// A length attribute of the root in CSS px, or `None` for a percentage, `auto`, a missing value or
/// an unparsable one (SVG 2 §8.2: those give no natural size). Font-relative units use the initial
/// `font-size` (16 px).
fn absolute_length(value: Option<&str>) -> Option<f32> {
    let v = value?.trim();
    const UNITS: [(&str, f32); 10] = [
        ("px", 1.0),
        ("in", 96.0),
        ("cm", 96.0 / 2.54),
        ("mm", 96.0 / 25.4),
        ("q", 96.0 / 101.6),
        ("pt", 96.0 / 72.0),
        ("pc", 16.0),
        ("em", 16.0),
        ("ex", 8.0),
        ("", 1.0),
    ];
    let (number, factor) = UNITS.iter().find_map(|&(unit, factor)| {
        let n = v.len().checked_sub(unit.len())?;
        (v.is_char_boundary(n) && v[n..].eq_ignore_ascii_case(unit)).then(|| (&v[..n], factor))
    })?;
    let px = number.trim_end().parse::<f32>().ok()? * factor;
    (px.is_finite() && px >= 0.0).then_some(px)
}

/// A `viewBox` value's width and height, when it is valid (four numbers, positive size).
fn view_box_size(value: Option<&str>) -> Option<(f32, f32)> {
    let n: Vec<f32> = value?
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|s| !s.is_empty())
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    (n.len() == 4 && n[2] > 0.0 && n[3] > 0.0 && n.iter().all(|v| v.is_finite()))
        .then(|| (n[2], n[3]))
}

/// An SVG image's source text (gunzipped for `.svgz`), within [`MAX_SOURCE_BYTES`].
fn source_text(bytes: &[u8]) -> Result<String, String> {
    let source = if bytes.starts_with(&[0x1f, 0x8b]) {
        use std::io::Read;
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes)
            .take(MAX_SOURCE_BYTES as u64 + 1)
            .read_to_end(&mut out)
            .map_err(|e| e.to_string())?;
        out
    } else {
        bytes.to_vec()
    };
    if source.len() > MAX_SOURCE_BYTES {
        return Err("SVG image too large".into());
    }
    String::from_utf8(source).map_err(|_| "SVG image is not UTF-8".to_string())
}

/// An SVG image's document: parsed, styled and serialized as the module documentation says.
pub struct SvgDocument {
    dom: snowlark_dom::Dom,
    root: snowlark_dom::NodeId,
    engine: snowlark_style::StyleEngine,
    data: snowlark_style::StyleData,
    base: snowlark_net::Url,
    /// The viewport and color scheme (dark) the engine's device has.
    device: Option<((f32, f32), bool)>,
}

impl SvgDocument {
    /// Parses an SVG image's bytes as an XML document at `url` and styles it at the embedding
    /// document's timeline time `now`; `None` when it is not well-formed or its root is not an SVG
    /// `svg` element.
    pub fn parse(bytes: &[u8], url: &snowlark_net::Url, now: f64) -> Option<SvgDocument> {
        let text = source_text(bytes).ok()?;
        let mut dom = snowlark_dom::Dom::new();
        let (document, parsed) = dom.parse_xml_str(&text);
        parsed.ok()?;
        dom.set_document_url(document, url.as_str());
        let root = dom.document_element(document)?;
        let is_svg = dom.element(root).is_some_and(|e| {
            e.name().ns == snowlark_dom::ns!(svg)
                && e.name().local == snowlark_dom::local_name!("svg")
        });
        if !is_svg {
            return None;
        }
        let config = snowlark_style::DeviceConfig {
            viewport: (300.0, 150.0),
            // (no script runs in an image)
            scripting: false,
            ..snowlark_style::DeviceConfig::default()
        };
        let mut engine = snowlark_style::StyleEngine::new(&dom, document, config);
        engine.set_parallelism(snowlark_style::Parallelism::Sequential);
        let mut data = snowlark_style::StyleData::new();
        engine.update_all_style_blocks(&dom, &data);
        // (its timeline is the embedding document's: its animations start when it arrives)
        engine.restyle_at(&dom, &mut data, now);
        Some(SvgDocument {
            dom,
            root,
            engine,
            data,
            base: url.clone(),
            device: None,
        })
    }

    /// The document as a standalone SVG source with its computed styles written in, styled for a
    /// `viewport` (CSS px) and color scheme at time `now` (seconds on the embedding document's
    /// timeline).
    pub fn markup(&mut self, viewport: (f32, f32), dark: bool, now: f64) -> String {
        if self.device != Some((viewport, dark)) {
            let config = snowlark_style::DeviceConfig {
                viewport,
                color_scheme: if dark {
                    snowlark_style::ColorScheme::Dark
                } else {
                    snowlark_style::ColorScheme::Light
                },
                scripting: false,
                ..snowlark_style::DeviceConfig::default()
            };
            self.engine.set_device(&self.data, config);
            self.device = Some((viewport, dark));
        }
        self.engine.restyle_at(&self.dom, &mut self.data, now);
        // (no external resources in an image: SVG Integration §3.4)
        let resources = HashMap::new();
        let external = crate::inline_svg::External::new(&resources, &self.base);
        crate::inline_svg::serialize(&self.dom, &self.data, self.root, &self.base, &external).markup
    }

    /// Whether its CSS animations or transitions are running: it looks different as time moves on.
    pub fn animating(&self) -> bool {
        self.engine.animations_active()
    }

    /// The root element's background color, which paints the canvas (as last styled).
    fn background(&self) -> Option<snowlark_paint::Color> {
        let values = self.data.styles(self.root)?;
        let c = values.resolve_color(&values.get_background().background_color);
        let c = crate::adapter::to_color(&c);
        (c.a > 0).then_some(c)
    }
}

/// What an SVG root's `width`, `height` and `viewBox` attributes give its image: the natural
/// dimensions, the `viewBox` rasterizing synthesizes (absolute sizes and no `viewBox`), and whether
/// a zero-sized `viewBox` disables rendering (SVG 2 §8.6).
fn root_size(
    width: Option<&str>,
    height: Option<&str>,
    view_box_attr: Option<&str>,
) -> (NaturalSize, Option<(f32, f32)>, bool) {
    let width = absolute_length(width);
    let height = absolute_length(height);
    let view_box = view_box_size(view_box_attr);
    let disabled = view_box_attr.is_some_and(|v| {
        let n: Vec<f32> = v
            .split(|c: char| c == ',' || c.is_ascii_whitespace())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();
        n.len() == 4 && (n[2] == 0.0 || n[3] == 0.0)
    });
    let ratio = match (width, height) {
        (Some(w), Some(h)) if w > 0.0 && h > 0.0 => Some(w / h),
        _ => view_box.map(|(w, h)| w / h),
    };
    let synthesize = match (width, height, view_box) {
        (Some(w), Some(h), None) if w > 0.0 && h > 0.0 => Some((w, h)),
        _ => None,
    };
    (
        NaturalSize {
            width,
            height,
            ratio,
        },
        synthesize,
        disabled,
    )
}

impl SvgImage {
    /// Parses an SVG image (gzip-compressed `.svgz` too). Fails for data that is not well-formed XML
    /// with an SVG root element: the image is then broken.
    pub fn parse(bytes: &[u8]) -> Result<SvgImage, String> {
        let source = source_text(bytes)?;
        let options = roxmltree::ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        };
        let doc =
            roxmltree::Document::parse_with_options(&source, options).map_err(|e| e.to_string())?;
        let root = doc.root_element();
        if root.tag_name().name() != "svg" || root.tag_name().namespace() != Some(SVG_NS) {
            return Err("not an SVG document".into());
        }
        let (natural, synthesize_view_box, disabled) = root_size(
            root.attribute("width"),
            root.attribute("height"),
            root.attribute("viewBox"),
        );
        let start = root.range().start;
        let name_len = source[start + 1..]
            .find(|c: char| c.is_ascii_whitespace() || c == '/' || c == '>')
            .unwrap_or(0);
        let mut remove: Vec<Range<usize>> = root
            .attributes()
            .filter(|a| a.namespace().is_none() && matches!(a.name(), "width" | "height"))
            .map(|a| a.range())
            .collect();
        remove.sort_by_key(|r| r.start);
        let has_text = doc
            .descendants()
            .any(|n| n.tag_name().name() == "text" && n.tag_name().namespace() == Some(SVG_NS));
        Ok(SvgImage {
            natural,
            synthesize_view_box,
            insert_at: start + 1 + name_len,
            remove,
            has_text,
            source,
            images: HashMap::new(),
            disabled,
            background: None,
        })
    }

    /// The image of an inline `svg` element drawn as paint operations ([`crate::svg_paint`]),
    /// without its markup: the natural dimensions from the root's `width`, `height` and `viewBox`,
    /// as [`SvgImage::parse`] reads them from the markup the element is written as. The markup comes
    /// with [`SvgImages::set_markup`] when it is drawn as an image after all; until then it draws
    /// nothing.
    pub(crate) fn without_markup(
        width: Option<&str>,
        height: Option<&str>,
        view_box: Option<&str>,
    ) -> SvgImage {
        let (natural, synthesize_view_box, disabled) = root_size(width, height, view_box);
        SvgImage {
            source: String::new(),
            natural,
            synthesize_view_box,
            insert_at: 0,
            remove: Vec::new(),
            has_text: false,
            images: HashMap::new(),
            disabled,
            background: None,
        }
    }

    /// Whether it has its markup (see [`SvgImage::without_markup`]).
    pub(crate) fn has_markup(&self) -> bool {
        !self.source.is_empty()
    }

    /// Gives an inline `svg` element's image the fetched images its `<image>` elements show (by the
    /// absolute URL its serialization wrote): they are drawn as `data:` images are.
    pub(crate) fn with_images(mut self, images: HashMap<String, Arc<Vec<u8>>>) -> SvgImage {
        self.images = images;
        self
    }

    /// The SVG documents among its images whose animations run from timeline time `now`, each with
    /// the size it is styled at.
    fn animated_images(&self, now: f64) -> Vec<(String, SvgDocument, (f32, f32))> {
        let base = snowlark_net::Url::parse("about:blank");
        let mut out = Vec::new();
        for (url, bytes) in &self.images {
            let (Ok(base), Ok(image)) = (&base, SvgImage::parse(bytes)) else {
                continue;
            };
            let Some(mut doc) = SvgDocument::parse(bytes, base, now) else {
                continue;
            };
            let natural = image.natural();
            let size = (
                natural.width.unwrap_or(300.0),
                natural.height.unwrap_or(150.0),
            );
            doc.markup(size, false, now);
            if doc.animating() {
                out.push((url.clone(), doc, size));
            }
        }
        out
    }

    /// The natural dimensions, in CSS px.
    pub fn natural(&self) -> NaturalSize {
        self.natural
    }

    /// Whether the image has text (rasterizing it needs the system fonts).
    pub fn has_text(&self) -> bool {
        self.has_text
    }

    /// The source with the root's viewport set to `width` x `height` CSS px.
    fn sized_source(&self, width: f32, height: f32) -> String {
        let mut out = String::with_capacity(self.source.len() + 64);
        out.push_str(&self.source[..self.insert_at]);
        out.push_str(&format!(" width=\"{width}\" height=\"{height}\""));
        if let Some((w, h)) = self.synthesize_view_box {
            // (scaled as a whole to the size it is drawn at, as a bitmap would be: the synthesized
            // viewBox does not keep the aspect ratio)
            out.push_str(&format!(
                " viewBox=\"0 0 {w} {h}\" preserveAspectRatio=\"none\""
            ));
        }
        let mut at = self.insert_at;
        for r in &self.remove {
            out.push_str(&self.source[at..r.start]);
            at = r.end;
        }
        out.push_str(&self.source[at..]);
        out
    }

    /// Renders the image with a viewport of `css` CSS px into a `device`-pixel premultiplied BGRA
    /// bitmap, in secure static mode. `fonts` is given for images with text.
    pub fn rasterize(
        &self,
        css: (f32, f32),
        device: (u32, u32),
        rendering: ImageRendering,
        fonts: Option<Arc<fontdb::Database>>,
    ) -> Option<DecodedImage> {
        let (cw, ch) = css;
        let (dw, dh) = device;
        self.rasterize_part(
            css,
            (dw as f32 / cw, dh as f32 / ch),
            (0.0, 0.0, cw, ch),
            device,
            rendering,
            fonts,
        )
    }

    /// [`SvgImage::rasterize`] of the part `part` (CSS px from the image's origin) of the image
    /// drawn at `css` px, `scale` device pixels per CSS px, into a `device`-pixel bitmap: what shows
    /// of an image much larger than the area it is clipped to.
    pub fn rasterize_part(
        &self,
        css: (f32, f32),
        scale: (f32, f32),
        part: (f32, f32, f32, f32),
        device: (u32, u32),
        rendering: ImageRendering,
        fonts: Option<Arc<fontdb::Database>>,
    ) -> Option<DecodedImage> {
        let (cw, ch) = css;
        let (dw, dh) = device;
        if !(cw > 0.0 && ch > 0.0 && cw.is_finite() && ch.is_finite()) || dw == 0 || dh == 0 {
            return None;
        }
        if dw as u64 * dh as u64 > MAX_RASTER_PIXELS {
            return None;
        }
        if self.disabled {
            return Some(DecodedImage::still(Frame {
                width: 1,
                height: 1,
                pixels: vec![0; 4],
                delay_ms: 0,
            }));
        }
        let mut options = usvg::Options {
            // CSS's initial font-size, for em units and text without one.
            font_size: 16.0,
            image_rendering: match rendering {
                ImageRendering::Pixelated => usvg::ImageRendering::OptimizeSpeed,
                ImageRendering::Smooth => usvg::ImageRendering::OptimizeQuality,
            },
            image_href_resolver: usvg::ImageHrefResolver {
                // `data:` images are part of the document; nothing else is fetched or read here
                // (an inline `svg`'s images were fetched by its document).
                resolve_data: usvg::ImageHrefResolver::default_data_resolver(),
                resolve_string: {
                    let images = self.images.clone();
                    Box::new(move |href, _| {
                        let (url, oriented) = match href.strip_suffix(UNORIENTED) {
                            Some(url) => (url, false),
                            None => (href, true),
                        };
                        images.get(url).and_then(|b| image_kind(b, oriented))
                    })
                },
            },
            ..usvg::Options::default()
        };
        if let Some(fonts) = fonts {
            options.fontdb = fonts;
        }
        let tree = usvg::Tree::from_str(&self.sized_source(cw, ch), &options).ok()?;
        let mut pixmap = tiny_skia::Pixmap::new(dw, dh)?;
        if let Some(c) = self.background.filter(|c| c.a > 0) {
            pixmap.fill(tiny_skia::Color::from_rgba8(c.r, c.g, c.b, c.a));
        }
        // (the tree's size is the CSS size; the part's origin goes to the bitmap's)
        let size = tree.size();
        let (sx, sy) = (
            scale.0 * cw / size.width().max(f32::EPSILON),
            scale.1 * ch / size.height().max(f32::EPSILON),
        );
        let transform =
            tiny_skia::Transform::from_row(sx, 0.0, 0.0, sy, -part.0 * scale.0, -part.1 * scale.1);
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        // tiny-skia's premultiplied RGBA to the paint crate's premultiplied BGRA.
        let mut pixels = pixmap.take();
        for px in pixels.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        Some(DecodedImage {
            frames: vec![Frame {
                width: dw,
                height: dh,
                pixels,
                delay_ms: 0,
            }],
            orientation: 1,
            animation: None,
        })
    }
}

/// The suffix an inline `svg`'s serialization adds to an `<image>` URL whose `image-orientation` is
/// `none` (a fragment no fetched URL has: the resolver strips it).
pub(crate) const UNORIENTED: &str = "#snowlark-image-orientation-none";

/// What a fetched image's bytes are, for resvg (sniffed as MIME Sniffing §7.1 does for images): PNG,
/// JPEG, GIF or WebP, turned as their EXIF orientation says when `oriented` (CSS Images 3 §5.3), or
/// an SVG document (styled as an SVG image is, see [`SvgDocument`]); anything else is not drawn.
fn image_kind(bytes: &Arc<Vec<u8>>, oriented: bool) -> Option<usvg::ImageKind> {
    let b = bytes.as_slice();
    if oriented && snowlark_paint::image::exif_orientation(b).is_some_and(|o| o > 1) {
        return oriented_png(b).map(|png| usvg::ImageKind::PNG(Arc::new(png)));
    }
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(usvg::ImageKind::PNG(bytes.clone()))
    } else if b.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(usvg::ImageKind::JPEG(bytes.clone()))
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some(usvg::ImageKind::GIF(bytes.clone()))
    } else if b.len() >= 14 && &b[..4] == b"RIFF" && &b[8..14] == b"WEBPVP" {
        Some(usvg::ImageKind::WEBP(bytes.clone()))
    } else {
        // An SVG document: styled (its `<style>` sheets, a light color scheme, time 0) and
        // serialized, then parsed by usvg (secure static mode: it fetches nothing).
        let url = snowlark_net::Url::parse("about:blank").ok()?;
        let mut doc = SvgDocument::parse(b, &url, 0.0)?;
        let natural = SvgImage::parse(b).ok()?.natural();
        let viewport = (
            natural.width.unwrap_or(300.0),
            natural.height.unwrap_or(150.0),
        );
        let markup = doc.markup(viewport, false, 0.0);
        let tree = usvg::Tree::from_str(&markup, &usvg::Options::default()).ok()?;
        Some(usvg::ImageKind::SVG(tree))
    }
}

/// A JPEG or PNG decoded with its EXIF orientation applied, encoded as a PNG for resvg.
fn oriented_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let image = snowlark_paint::image::decode(bytes).ok()?;
    let f = &image.frames[0];
    let mut rgba = f.pixels.clone();
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    let size = tiny_skia::IntSize::from_wh(f.width, f.height)?;
    tiny_skia::Pixmap::from_vec(rgba, size)?.encode_png().ok()
}

/// One SVG URL's image and its rasters.
struct Entry {
    image: Arc<SvgImage>,
    /// The image's document, when it is styled as one (an SVG image; an inline `svg` element's
    /// image was styled by its own document).
    doc: Option<SvgDocument>,
    /// Its document is styled for a dark embedding element.
    dark: bool,
    /// The SVG images its `<image>` elements show that have running animations: their URL, document
    /// and the size they are styled at (an inline `svg` element's images).
    nested: Vec<(String, SvgDocument, (f32, f32))>,
    /// Rasters by (device width, device height, pixelated, dark), with the paint pass that last used
    /// them and the hash of the serialization they show (0 for a static image).
    rasters: HashMap<RasterKey, (ImageId, u64, u64)>,
}

/// A raster's device width and height, whether it is pixelated, whether it was styled dark, and
/// for a part of a larger image, where the part starts and the whole image's size (device pixels).
type RasterKey = (u32, u32, bool, bool, i64, i64, i64, i64);

/// Images of more device pixels than this are rasterized only where they show.
const PART_THRESHOLD_PIXELS: u64 = 4 << 20;

/// The part of an image drawn into `dest` (transformed by `m`, an axis-aligned scale and
/// translation) that the device-space `clip` lets show, in CSS px from `dest`'s origin, when the
/// image is large ([`PART_THRESHOLD_PIXELS`]) and the part at most half of it; `None` to
/// rasterize all of it.
fn visible_part(
    dest: &snowlark_paint::Rect,
    m: &snowlark_paint::Matrix,
    clip: &snowlark_paint::Rect,
    k: (f32, f32),
) -> Option<(f32, f32, f32, f32)> {
    if !m.is_axis_aligned() || clip.width.is_infinite() || clip.height.is_infinite() {
        return None;
    }
    let inverse = m.inverse()?;
    let local = inverse.apply_rect(clip).intersect(dest);
    if local.is_empty() {
        return None;
    }
    let whole = (dest.width * k.0) as f64 * (dest.height * k.1) as f64;
    let shown = (local.width * k.0) as f64 * (local.height * k.1) as f64;
    // (images of ordinary sizes are rasterized whole, once for all their tiles and parts)
    if whole <= PART_THRESHOLD_PIXELS as f64 || shown * 2.0 > whole {
        return None;
    }
    // (whole device pixels from the image's origin, a pixel of margin)
    let x0 = (((local.x - dest.x) * k.0).floor() - 1.0).max(0.0) / k.0;
    let y0 = (((local.y - dest.y) * k.1).floor() - 1.0).max(0.0) / k.1;
    let x1 = ((((local.right() - dest.x) * k.0).ceil() + 1.0) / k.0).min(dest.width);
    let y1 = ((((local.bottom() - dest.y) * k.1).ceil() + 1.0) / k.1).min(dest.height);
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
}

/// What [`SvgImages::resolve`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SvgStats {
    /// Rasterizations so far (cache misses).
    pub rasterizations: u64,
    /// Image items drawn from a cached raster so far.
    pub hits: u64,
}

/// What [`SvgImages::resolve`] draws for: the display's scale, the embedding document's timeline time,
/// the viewport in the list's coordinates, and the scroll position that takes them to the document's.
#[derive(Clone, Copy, Debug)]
pub struct SvgFrame {
    pub scale: f32,
    pub now: f64,
    pub viewport: snowlark_paint::Rect,
    pub scroll: snowlark_paint::Point,
}

/// The SVG images of a renderer: placeholders and the rasters drawn for them.
#[derive(Default)]
pub struct SvgImages {
    entries: HashMap<ImageId, Entry>,
    fonts: OnceLock<Arc<fontdb::Database>>,
    /// The text engine whose system font database to use (else one of its own).
    text: Option<Arc<snowlark_paint::TextEngine>>,
    pass: u64,
    stats: SvgStats,
    /// Where images with running animations were drawn in the last [`SvgImages::resolve`] (document
    /// coordinates), whether one of them is in a layer, and the time it drew them at.
    animated: Vec<snowlark_paint::Rect>,
    animated_layered: bool,
    drawn_at: f64,
}

/// What stands for an SVG image in the display list until a pass resolves it to a raster.
fn placeholder() -> DecodedImage {
    DecodedImage {
        frames: vec![Frame {
            width: 1,
            height: 1,
            pixels: vec![0; 4],
            delay_ms: 0,
        }],
        orientation: 1,
        animation: None,
    }
}

/// The device pixels for `css` px at `scale`, within the raster budget.
fn device_size(css: (f32, f32), scale: (f32, f32)) -> (u32, u32) {
    let mut w = (css.0 * scale.0).round().max(1.0);
    let mut h = (css.1 * scale.1).round().max(1.0);
    let budget = MAX_RASTER_PIXELS as f32;
    if w * h > budget {
        let k = (budget / (w * h)).sqrt();
        w = (w * k).floor().max(1.0);
        h = (h * k).floor().max(1.0);
    }
    (w as u32, h as u32)
}

impl SvgImages {
    pub fn new() -> SvgImages {
        SvgImages::default()
    }

    /// SVG images whose text uses `text`'s system font database (loaded once for every renderer
    /// sharing the engine).
    pub fn with_text(text: Arc<snowlark_paint::TextEngine>) -> SvgImages {
        SvgImages {
            text: Some(text),
            ..SvgImages::default()
        }
    }

    /// Registers an SVG image: a transparent 1x1 placeholder in `store` stands for it until a paint
    /// pass resolves it to a raster.
    pub fn add(&mut self, store: &ImageStore, image: SvgImage) -> ImageId {
        self.add_with_document(store, image, None)
    }

    /// [`SvgImages::add`] for an SVG image with its document (see [`SvgDocument`]).
    pub fn add_with_document(
        &mut self,
        store: &ImageStore,
        image: SvgImage,
        doc: Option<SvgDocument>,
    ) -> ImageId {
        let nested = image.animated_images(self.drawn_at);
        let id = store.add(placeholder());
        self.entries.insert(
            id,
            Entry {
                image: Arc::new(image),
                doc,
                dark: false,
                nested,
                rasters: HashMap::new(),
            },
        );
        id
    }

    /// A variant of image `of` for embedding elements of a dark used color scheme, with a document
    /// of its own.
    pub fn add_dark_variant(
        &mut self,
        store: &ImageStore,
        of: ImageId,
        doc: SvgDocument,
    ) -> Option<ImageId> {
        let image = self.entries.get(&of)?.image.clone();
        let id = store.add(placeholder());
        self.entries.insert(
            id,
            Entry {
                image,
                doc: Some(doc),
                dark: true,
                nested: Vec::new(),
                rasters: HashMap::new(),
            },
        );
        Some(id)
    }

    /// Whether image `id` has its markup ([`SvgImage::without_markup`]); `false` for no image.
    pub(crate) fn has_markup(&self, id: ImageId) -> bool {
        self.entries.get(&id).is_some_and(|e| e.image.has_markup())
    }

    /// Gives image `id`, made [`SvgImage::without_markup`], the image of its markup.
    pub(crate) fn set_markup(&mut self, id: ImageId, image: SvgImage) {
        let nested = image.animated_images(self.drawn_at);
        if let Some(e) = self.entries.get_mut(&id) {
            e.image = Arc::new(image);
            e.nested = nested;
        }
    }

    /// Forgets an image, freeing its placeholder and rasters.
    pub fn remove(&mut self, store: &ImageStore, id: ImageId) {
        if let Some(e) = self.entries.remove(&id) {
            store.remove(id);
            for (raster, _, _) in e.rasters.values() {
                store.remove(*raster);
            }
        }
    }

    /// Forgets every image (a new document), freeing its bitmaps; the system font database stays.
    pub fn clear(&mut self, store: &ImageStore) {
        for (id, e) in self.entries.drain() {
            store.remove(id);
            for (raster, _, _) in e.rasters.values() {
                store.remove(*raster);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn stats(&self) -> SvgStats {
        self.stats
    }

    /// Whether the clock at `time` shows an image with running animations differently from the last
    /// [`SvgImages::resolve`]: one drawn in `view` (document coordinates; `None`: anywhere, an empty
    /// view nowhere) at an earlier time. Images out of it wait, as animated bitmaps do.
    pub fn frame_due(&self, view: Option<snowlark_paint::Rect>, time: f64) -> bool {
        let shown = match view {
            None => self.animated_layered || !self.animated.is_empty(),
            Some(v) if v.is_empty() => false,
            Some(v) => self.animated_layered || self.animated.iter().any(|r| r.intersects(&v)),
        };
        shown && time > self.drawn_at
    }

    /// The system fonts, loaded on first use.
    pub(crate) fn fonts(&self) -> Arc<fontdb::Database> {
        if let Some(text) = &self.text {
            return text.font_database();
        }
        self.fonts
            .get_or_init(|| {
                let mut db = fontdb::Database::new();
                db.load_system_fonts();
                snowlark_paint::font_files::map_all(&mut db);
                Arc::new(db)
            })
            .clone()
    }

    /// Replaces every SVG placeholder in `list` (its image items' and its SVG filters' `feImage`s')
    /// with a raster of the size it is drawn at: its
    /// destination rectangle in CSS px times `scale` (device pixels per CSS px) times the scale of
    /// the transforms around it. `image-rendering: pixelated` rasterizes at the CSS size and lets the
    /// painter scale it with nearest-neighbour sampling. An image with a document is styled for that
    /// size, the color scheme `dark` and the time `now` (seconds on the embedding document's
    /// timeline), and drawn again when its serialization changed.
    pub fn resolve(&mut self, list: &mut DisplayList, store: &ImageStore, at: &SvgFrame) {
        let now = at.now;
        self.animated.clear();
        self.animated_layered = false;
        self.drawn_at = now;
        if self.entries.is_empty() {
            return;
        }
        self.pass += 1;
        let pass = self.pass;
        let mut stack = vec![snowlark_paint::Matrix::IDENTITY];
        // (the clips around each item, in the list's coordinates)
        let mut clips = vec![snowlark_paint::Rect::infinite()];
        // (how many layers a compositor moves are open: where their images show is not known here)
        let mut layers = 0usize;
        for item in list.items.iter_mut() {
            match item {
                Item::PushLayer { .. } => layers += 1,
                Item::PopLayer => layers = layers.saturating_sub(1),
                Item::PushTransform { matrix } => {
                    let top = *stack.last().unwrap();
                    stack.push(matrix.then(&top));
                }
                Item::PopTransform => {
                    if stack.len() > 1 {
                        stack.pop();
                    }
                }
                Item::PushClipRect { rect }
                | Item::PushClipRoundedRect { rect, .. }
                | Item::PushClipPath { bounds: rect, .. }
                | Item::PushMask { bounds: rect, .. } => {
                    let m = stack.last().unwrap();
                    let c = m.apply_rect(rect).intersect(clips.last().unwrap());
                    clips.push(c);
                }
                Item::PopClip | Item::PopMask => {
                    if clips.len() > 1 {
                        clips.pop();
                    }
                }
                Item::Image {
                    image,
                    dest,
                    src,
                    rendering,
                    ..
                } => {
                    let m = *stack.last().unwrap();
                    let clip = *clips.last().unwrap();
                    self.resolve_one(
                        store, at, pass, image, dest, src, rendering, &m, &clip, layers,
                    );
                }
                // (a pattern fill's images, a background repeated over many tiles: drawn in the
                // tile's space, through the fill's transform)
                Item::FillPattern {
                    pattern, transform, ..
                } => {
                    let m = transform.then(stack.last().unwrap());
                    let Some(p) = list.patterns.get_mut(*pattern as usize) else {
                        continue;
                    };
                    for inner in p.list.items.iter_mut() {
                        if let Item::Image {
                            image,
                            dest,
                            src,
                            rendering,
                            ..
                        } = inner
                        {
                            self.resolve_one(
                                store,
                                at,
                                pass,
                                image,
                                dest,
                                src,
                                rendering,
                                &m,
                                &snowlark_paint::Rect::infinite(),
                                layers,
                            );
                        }
                    }
                }
                // (an SVG filter's `feImage`s: drawn where the filter's content is)
                Item::PushFilter { filters: range, .. }
                | Item::BackdropFilter { filters: range, .. } => {
                    let m = *stack.last().unwrap();
                    let clip = *clips.last().unwrap();
                    let (start, len) = (range.start as usize, range.len as usize);
                    let Some(ops) = list.filters.get_mut(start..start.saturating_add(len)) else {
                        continue;
                    };
                    for op in ops {
                        let FilterOp::Svg(f) = op else { continue };
                        if !f.images().any(|i| self.entries.contains_key(&i)) {
                            continue;
                        }
                        for p in &mut std::sync::Arc::make_mut(f).primitives {
                            if let Kind::Image { image, dest, src } = &mut p.kind {
                                self.resolve_one(
                                    store,
                                    at,
                                    pass,
                                    image,
                                    dest,
                                    src,
                                    &ImageRendering::Smooth,
                                    &m,
                                    &clip,
                                    layers,
                                );
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// [`SvgImages::resolve`] for one drawing of an image: `image` (a placeholder, swapped for a
    /// raster) drawn into `dest` (`src`: the part of it, empty for all of it) under `m`, within
    /// `clip` (both in the list's coordinates), `layers` compositor layers deep.
    #[allow(clippy::too_many_arguments)]
    fn resolve_one(
        &mut self,
        store: &ImageStore,
        at: &SvgFrame,
        pass: u64,
        image: &mut ImageId,
        dest: &mut snowlark_paint::Rect,
        src: &snowlark_paint::Rect,
        rendering: &ImageRendering,
        m: &snowlark_paint::Matrix,
        clip: &snowlark_paint::Rect,
        layers: usize,
    ) {
        let SvgFrame {
            scale,
            now,
            viewport,
            scroll,
        } = *at;
        let Some(entry) = self.entries.get_mut(image) else {
            return;
        };
        let css = (dest.width, dest.height);
        // (an inline svg's image without markup is drawn as paint operations, or nothing)
        if !(css.0 > 0.0 && css.1 > 0.0) || !src.is_empty() || !entry.image.has_markup() {
            return;
        }
        let bounds = m.apply_rect(dest).intersect(clip);
        let shown = layers > 0 || bounds.intersects(&viewport);
        let pixelated = *rendering == ImageRendering::Pixelated;
        let k = if pixelated {
            (1.0, 1.0)
        } else {
            (
                scale * m.m11.hypot(m.m12).max(f32::EPSILON),
                scale * m.m21.hypot(m.m22).max(f32::EPSILON),
            )
        };
        // An image much larger than what its clips let show (a `cover` background of
        // an extreme ratio, a huge image in a small box) is rasterized only there:
        // the part, in CSS px from its origin, with a pixel of margin.
        let part = visible_part(dest, m, clip, k).filter(|_| !pixelated);
        let device = match part {
            Some(p) => device_size((p.2, p.3), k),
            None => device_size(css, k),
        };
        let note = |animated: &mut Vec<snowlark_paint::Rect>, layered: &mut bool| {
            *layered |= layers > 0;
            if !bounds.is_empty() {
                animated.push(bounds.translate(scroll.x, scroll.y));
            }
        };
        let dark = entry.dark;
        let key = match part {
            Some(p) => (
                device.0,
                device.1,
                pixelated,
                dark,
                (p.0 * k.0).round() as i64,
                (p.1 * k.1).round() as i64,
                (css.0 * k.0).round() as i64,
                (css.1 * k.1).round() as i64,
            ),
            None => (device.0, device.1, pixelated, dark, 0, 0, 0, 0),
        };
        if let Some(p) = part {
            *dest = snowlark_paint::Rect::new(dest.x + p.0, dest.y + p.1, p.2, p.3);
        }
        let animating = entry.doc.as_ref().is_some_and(|d| d.animating())
            || entry.nested.iter().any(|(_, d, _)| d.animating());
        // A static image drawn at a size it has a raster for keeps it, and so does an
        // animated one out of view.
        if !animating || !shown {
            if let Some((raster, used, _)) = entry.rasters.get_mut(&key) {
                if animating {
                    note(&mut self.animated, &mut self.animated_layered);
                }
                *used = pass;
                self.stats.hits += 1;
                *image = *raster;
                return;
            }
        }
        // An image with a document: its source as styled now; the raster it has for
        // the same source stays.
        let source = entry.doc.as_mut().map(|d| {
            let markup = d.markup(css, dark, now);
            let hash = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                markup.hash(&mut h);
                h.finish().max(1)
            };
            (markup, hash)
        });
        if entry.doc.as_ref().is_some_and(|d| d.animating()) {
            note(&mut self.animated, &mut self.animated_layered);
        }
        // Its `<image>`s that animate: as they are styled now.
        let mut nested_hash = 0;
        let mut current = entry.image.images.clone();
        if animating && !entry.nested.is_empty() {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            for (url, doc, size) in &mut entry.nested {
                let markup = doc.markup(*size, false, now);
                markup.hash(&mut h);
                current.insert(url.clone(), Arc::new(markup.into_bytes()));
            }
            nested_hash = h.finish().max(1);
            if entry.doc.is_none() {
                note(&mut self.animated, &mut self.animated_layered);
            }
        }
        let hash = source.as_ref().map_or(nested_hash, |s| s.1);
        if let Some((raster, used, h)) = entry.rasters.get_mut(&key) {
            if *h == hash {
                *used = pass;
                self.stats.hits += 1;
                *image = *raster;
                return;
            }
        }
        let svg = match &source {
            Some((markup, _)) => match SvgImage::parse(markup.as_bytes()) {
                Ok(mut styled) => {
                    styled.background = entry.doc.as_ref().and_then(|d| d.background());
                    Arc::new(styled)
                }
                Err(_) => entry.image.clone(),
            },
            None if nested_hash != 0 => Arc::new((*entry.image).clone().with_images(current)),
            None => entry.image.clone(),
        };
        let fonts = svg.has_text().then(|| self.fonts());
        self.stats.rasterizations += 1;
        let bitmap = match part {
            Some(p) => svg.rasterize_part(css, k, p, device, *rendering, fonts),
            None => svg.rasterize(css, device, *rendering, fonts),
        };
        let Some(bitmap) = bitmap else {
            return;
        };
        let raster = store.add(bitmap);
        let entry = self.entries.get_mut(image).unwrap();
        if let Some((old, _, _)) = entry.rasters.remove(&key) {
            store.remove(old);
        }
        if entry.rasters.len() >= MAX_SIZES {
            // Drop the least recently used size not drawn in this pass.
            if let Some((&old_key, &(old, _, _))) = entry
                .rasters
                .iter()
                .filter(|(_, (_, used, _))| *used != pass)
                .min_by_key(|(_, (_, used, _))| *used)
            {
                entry.rasters.remove(&old_key);
                store.remove(old);
            }
        }
        entry.rasters.insert(key, (raster, pass, hash));
        *image = raster;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_paint::{Color, DisplayListBuilder, Matrix, Rect};

    fn natural(svg: &str) -> NaturalSize {
        SvgImage::parse(svg.as_bytes()).unwrap().natural()
    }

    const NS: &str = "xmlns='http://www.w3.org/2000/svg'";

    fn frame(scale: f32) -> SvgFrame {
        SvgFrame {
            scale,
            now: 0.0,
            viewport: Rect::new(0.0, 0.0, 800.0, 600.0),
            scroll: snowlark_paint::Point::new(0.0, 0.0),
        }
    }

    /// `then_some((n[2], n[3]))` evaluated its indexing before the length check, so an
    /// empty, short or non-numeric `viewBox` panicked.
    #[test]
    fn invalid_view_boxes_are_ignored() {
        for vb in [
            "",
            "0 0",
            "0 0 5",
            "a b c d",
            "0 0 5 5 5",
            "0 0 0 5",
            "0,0,10,20",
        ] {
            let n = natural(&format!("<svg {NS} viewBox='{vb}'/>"));
            let _ = n.ratio();
        }
        assert_eq!(super::view_box_size(Some("0 0 10 20")), Some((10.0, 20.0)));
        assert_eq!(super::view_box_size(Some("")), None);
        assert_eq!(super::view_box_size(Some("1 2")), None);
    }

    #[test]
    fn natural_size_rules() {
        // Both sizes absolute: natural size and ratio from them (the viewBox doesn't matter).
        let n = natural(&format!(
            "<svg {NS} width='20' height='10' viewBox='0 0 1 1'/>"
        ));
        assert_eq!(
            (n.width, n.height, n.ratio()),
            (Some(20.0), Some(10.0), Some(2.0))
        );
        // Only a viewBox: a ratio, no size.
        let n = natural(&format!("<svg {NS} viewBox='0 0 300 100'/>"));
        assert_eq!((n.width, n.height, n.ratio()), (None, None, Some(3.0)));
        // A width with a viewBox: the width and the viewBox's ratio.
        let n = natural(&format!("<svg {NS} width='60' viewBox='0,0,30,10'/>"));
        assert_eq!(
            (n.width, n.height, n.ratio()),
            (Some(60.0), None, Some(3.0))
        );
        // Percentages are no natural size.
        let n = natural(&format!(
            "<svg {NS} width='100%' height='50%' viewBox='0 0 4 2'/>"
        ));
        assert_eq!((n.width, n.height, n.ratio()), (None, None, Some(2.0)));
        // Nothing: no natural dimensions at all (layout then uses 300x150).
        let n = natural(&format!("<svg {NS}/>"));
        assert_eq!(n, NaturalSize::default());
        // Units.
        let n = natural(&format!("<svg {NS} width='1in' height='12pt'/>"));
        assert_eq!((n.width, n.height), (Some(96.0), Some(16.0)));
        let n = natural(&format!("<svg {NS} width='2em' height='1.5e1px'/>"));
        assert_eq!((n.width, n.height), (Some(32.0), Some(15.0)));
        // An invalid viewBox gives no ratio; a zero size gives no ratio but is a size.
        let n = natural(&format!("<svg {NS} viewBox='0 0 -5 2'/>"));
        assert_eq!(n.ratio(), None);
        let n = natural(&format!(
            "<svg {NS} width='0' height='10' viewBox='0 0 2 1'/>"
        ));
        assert_eq!(
            (n.width, n.height, n.ratio()),
            (Some(0.0), Some(10.0), Some(2.0))
        );
    }

    #[test]
    fn rejects_non_svg() {
        assert!(SvgImage::parse(b"<html/>").is_err());
        assert!(SvgImage::parse(b"<svg>no namespace</svg>").is_err());
        assert!(
            SvgImage::parse(b"<svg xmlns='http://www.w3.org/2000/svg'><unclosed></svg>").is_err()
        );
        assert!(SvgImage::parse(&[0xff, 0xfe, 0x00]).is_err());
    }

    #[test]
    fn sized_source_replaces_the_viewport() {
        let svg = SvgImage::parse(
            format!("<svg {NS} width='10' id='r' height=\"5\"><rect/></svg>").as_bytes(),
        )
        .unwrap();
        let s = svg.sized_source(40.0, 20.0);
        assert_eq!(
            s,
            format!(
                "<svg width=\"40\" height=\"20\" viewBox=\"0 0 10 5\" preserveAspectRatio=\"none\" {NS}  id='r' ><rect/></svg>"
            )
        );
    }

    fn red_square(extra: &str) -> SvgImage {
        SvgImage::parse(
            format!(
                "<svg {NS} {extra}><rect x='0' y='0' width='10' height='10' fill='#f00'/></svg>"
            )
            .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn rasterizes_at_the_drawn_size() {
        // 10x10 with no viewBox: the synthesized viewBox scales the content to 40x40 pixels.
        let svg = red_square("width='10' height='10'");
        let img = svg
            .rasterize((20.0, 20.0), (40, 40), ImageRendering::Smooth, None)
            .unwrap();
        let f = &img.frames[0];
        assert_eq!((f.width, f.height), (40, 40));
        assert_eq!(f.pixel(1, 1), Color::rgb(255, 0, 0));
        assert_eq!(f.pixel(38, 38), Color::rgb(255, 0, 0));
        // A viewBox twice the rect's size: the rect fills the top-left quarter; preserveAspectRatio
        // centers the 20x20 viewBox in a 40x20 viewport.
        let svg = red_square("viewBox='0 0 20 20'");
        let img = svg
            .rasterize((40.0, 20.0), (40, 20), ImageRendering::Smooth, None)
            .unwrap();
        let f = &img.frames[0];
        assert_eq!(f.pixel(0, 0), Color::TRANSPARENT);
        assert_eq!(f.pixel(11, 1), Color::rgb(255, 0, 0));
        assert_eq!(f.pixel(19, 9), Color::rgb(255, 0, 0));
        assert_eq!(f.pixel(21, 11), Color::TRANSPARENT);
    }

    #[test]
    fn loads_nothing_external() {
        // A PNG on disk referenced by path and by file: URL is not read (the default usvg resolver
        // would); a data: URL image is part of the document and is drawn.
        let dir = std::env::temp_dir().join(format!("snowlark-svg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("green.png");
        let green: Vec<u8> = [0u8, 255, 0, 255].repeat(4);
        snowlark_paint::image::encode_png(2, 2, &green, &png).unwrap();
        let bytes = std::fs::read(&png).unwrap();
        let b64 = base64_encode(&bytes);
        for href in [
            png.display().to_string(),
            url::Url::from_file_path(&png).unwrap().to_string(),
        ] {
            let svg = SvgImage::parse(
                format!("<svg {NS} width='4' height='4'><image href='{href}' width='4' height='4'/></svg>")
                    .as_bytes(),
            )
            .unwrap();
            let img = svg
                .rasterize((4.0, 4.0), (4, 4), ImageRendering::Smooth, None)
                .unwrap();
            assert_eq!(img.frames[0].pixel(2, 2), Color::TRANSPARENT, "{href}");
        }
        let svg = SvgImage::parse(
            format!("<svg {NS} width='4' height='4'><image href='data:image/png;base64,{b64}' width='4' height='4'/></svg>")
                .as_bytes(),
        )
        .unwrap();
        let img = svg
            .rasterize((4.0, 4.0), (4, 4), ImageRendering::Smooth, None)
            .unwrap();
        assert_eq!(img.frames[0].pixel(2, 2).g, 255);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn base64_encode(data: &[u8]) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    #[test]
    fn resolve_caches_by_size() {
        let store = ImageStore::new();
        let mut svgs = SvgImages::new();
        let id = svgs.add(&store, red_square("width='10' height='10'"));
        let mut b = DisplayListBuilder::new();
        for i in 0..30 {
            b.image(
                id,
                Rect::new(0.0, i as f32 * 12.0, 10.0, 10.0),
                None,
                1.0,
                ImageRendering::Smooth,
            );
        }
        // A transformed one at twice the scale.
        b.push_transform(Matrix::scale(2.0, 2.0));
        b.image(
            id,
            Rect::new(0.0, 0.0, 10.0, 10.0),
            None,
            1.0,
            ImageRendering::Smooth,
        );
        b.pop_transform();
        let mut list = b.build();
        svgs.resolve(&mut list, &store, &frame(1.0));
        // One raster for the thirty 10x10 draws, one for the 20x20 one.
        assert_eq!(svgs.stats().rasterizations, 2);
        assert_eq!(svgs.stats().hits, 29);
        let ids: Vec<ImageId> = list
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Image { image, .. } => Some(*image),
                _ => None,
            })
            .collect();
        assert!(ids.iter().all(|i| *i != id));
        let small = store.get(ids[0]).unwrap();
        assert_eq!((small.width(), small.height()), (10, 10));
        let big = store.get(ids[30]).unwrap();
        assert_eq!((big.width(), big.height()), (20, 20));
        // A second pass at 2x device pixels: 20x20 is cached, 40x40 is new.
        let mut b = DisplayListBuilder::new();
        b.image(
            id,
            Rect::new(0.0, 0.0, 10.0, 10.0),
            None,
            1.0,
            ImageRendering::Smooth,
        );
        b.image(
            id,
            Rect::new(0.0, 0.0, 20.0, 20.0),
            None,
            1.0,
            ImageRendering::Smooth,
        );
        let mut list = b.build();
        svgs.resolve(&mut list, &store, &frame(2.0));
        assert_eq!(svgs.stats().rasterizations, 3);
    }

    #[test]
    fn svg_documents_are_styled_with_media_and_animations() {
        let svg = format!(
            "<svg {NS} viewBox='0 0 10 10'><style>
               rect {{ fill: red }}
               @media (prefers-color-scheme: dark) {{ rect {{ fill: blue }} }}
               @media (min-width: 50px) {{ circle {{ fill: green }} }}
               #spin {{ animation: turn 1s linear infinite }}
               @keyframes turn {{ from {{ transform: rotate(0deg) }} to {{ transform: rotate(360deg) }} }}
             </style><rect width='5' height='5'/><circle r='1'/><g id='spin'/></svg>"
        );
        let url = snowlark_net::Url::parse("https://example.test/a.svg").unwrap();
        let mut doc = SvgDocument::parse(svg.as_bytes(), &url, 0.0).expect("parses");
        let light = doc.markup((10.0, 10.0), false, 0.0);
        assert!(light.contains("fill:rgb(255, 0, 0)"), "{light}");
        assert!(!light.contains("<style"), "{light}");
        // the embedder's color scheme answers prefers-color-scheme
        let dark = doc.markup((10.0, 10.0), true, 0.0);
        assert!(dark.contains("fill:rgb(0, 0, 255)"), "{dark}");
        // the viewport it is drawn at answers width queries
        let wide = doc.markup((100.0, 100.0), false, 0.0);
        assert!(wide.contains("fill:rgb(0, 128, 0)"), "{wide}");
        // CSS animations run on the timeline: a quarter turn after a quarter second
        assert!(doc.animating());
        let turned = doc.markup((10.0, 10.0), false, 0.25);
        assert!(turned.contains("transform=\"matrix("), "{turned}");
        assert_ne!(turned, light);
    }

    #[test]
    fn animated_images_draw_new_frames_only_where_they_show() {
        let svg = format!(
            "<svg {NS} viewBox='0 0 10 10'><style>
               rect {{ animation: turn 1s linear infinite }}
               @keyframes turn {{ from {{ fill: red }} to {{ fill: blue }} }}
             </style><rect width='10' height='10'/></svg>"
        );
        let url = snowlark_net::Url::parse("https://example.test/a.svg").unwrap();
        let document = SvgDocument::parse(svg.as_bytes(), &url, 0.0).expect("parses");
        let image = SvgImage::parse(svg.as_bytes()).unwrap();
        let store = ImageStore::new();
        let mut svgs = SvgImages::new();
        let id = svgs.add_with_document(&store, image, Some(document));
        let draw = |svgs: &mut SvgImages, y: f32, now: f64| {
            let mut b = DisplayListBuilder::new();
            b.image(
                id,
                Rect::new(0.0, y, 50.0, 50.0),
                None,
                1.0,
                ImageRendering::Smooth,
            );
            let mut list = b.build();
            let at = SvgFrame { now, ..frame(1.0) };
            svgs.resolve(&mut list, &store, &at);
            svgs.stats().rasterizations
        };
        let view = Rect::new(0.0, 0.0, 800.0, 600.0);
        // in view: every moment of the clock is a frame, and a new raster
        let first = draw(&mut svgs, 0.0, 0.0);
        assert!(
            !svgs.frame_due(Some(view), 0.0),
            "not before the clock moves"
        );
        assert!(svgs.frame_due(Some(view), 0.1));
        assert!(svgs.frame_due(None, 0.1));
        assert!(!svgs.frame_due(Some(Rect::ZERO), 0.1), "a hidden page");
        assert_eq!(draw(&mut svgs, 0.0, 0.25), first + 1);
        // a list painted again with the image out of view keeps the raster it has for that size
        let drawn = draw(&mut svgs, 5000.0, 0.5);
        assert_eq!(draw(&mut svgs, 5000.0, 0.75), drawn);
        assert!(!svgs.frame_due(Some(view), 1.0));
        // and it runs on in view again (at a moment of the loop no raster shows)
        assert_eq!(draw(&mut svgs, 0.0, 1.4), drawn + 1);
        assert!(svgs.frame_due(Some(view), 1.5));
    }

    #[test]
    fn an_animated_svg_in_an_image_element_animates() {
        let inner = format!(
            "<svg {NS} width='10' height='10'><style>
               rect {{ animation: turn 1s linear infinite }}
               @keyframes turn {{ from {{ fill: red }} to {{ fill: blue }} }}
             </style><rect width='10' height='10'/></svg>"
        );
        let url = "https://example.test/a.svg".to_owned();
        let images = HashMap::from([(url.clone(), Arc::new(inner.into_bytes()))]);
        let outer = format!(
            "<svg {NS} viewBox='0 0 10 10'><image href='{url}' width='10' height='10'/></svg>"
        );
        let image = SvgImage::parse(outer.as_bytes())
            .unwrap()
            .with_images(images);
        let store = ImageStore::new();
        let mut svgs = SvgImages::new();
        let id = svgs.add(&store, image);
        let draw = |svgs: &mut SvgImages, now: f64| {
            let mut b = DisplayListBuilder::new();
            b.image(
                id,
                Rect::new(0.0, 0.0, 50.0, 50.0),
                None,
                1.0,
                ImageRendering::Smooth,
            );
            let mut list = b.build();
            svgs.resolve(&mut list, &store, &SvgFrame { now, ..frame(1.0) });
            svgs.stats().rasterizations
        };
        let first = draw(&mut svgs, 0.0);
        assert!(svgs.frame_due(None, 0.1));
        assert_eq!(draw(&mut svgs, 0.25), first + 1);
    }

    #[test]
    fn huge_images_are_rasterized_where_they_show() {
        let store = ImageStore::new();
        let mut svgs = SvgImages::new();
        let id = svgs.add(
            &store,
            red_square("viewBox='0 0 10 10' preserveAspectRatio='none'"),
        );
        let mut b = DisplayListBuilder::new();
        b.push_clip_rect(Rect::new(0.0, 0.0, 100.0, 50.0));
        b.image(
            id,
            Rect::new(-50_000.0, 0.0, 100_000.0, 100.0),
            None,
            1.0,
            ImageRendering::Smooth,
        );
        b.pop_clip();
        let mut list = b.build();
        svgs.resolve(&mut list, &store, &frame(1.0));
        let (image, dest) = list
            .items
            .iter()
            .find_map(|i| match i {
                Item::Image { image, dest, .. } => Some((*image, *dest)),
                _ => None,
            })
            .unwrap();
        // the clipped part with a pixel of margin, not 100000 x 100 pixels
        assert_eq!(dest, Rect::new(-1.0, 0.0, 102.0, 51.0));
        let bitmap = store.get(image).unwrap();
        assert_eq!((bitmap.width(), bitmap.height()), (102, 51));
        assert_eq!(bitmap.frames[0].pixel(50, 25), Color::rgb(255, 0, 0));
    }

    #[test]
    fn svg_documents_show_in_image_elements() {
        // an inline svg's `<image>` of an SVG document: styled by its own sheet
        let inner = format!(
            "<svg {NS} width='10' height='10'><style>rect {{ fill: #00f }}</style>             <rect width='10' height='10'/></svg>"
        );
        let url = "https://example.test/i.svg".to_string();
        let mut images = HashMap::new();
        images.insert(url.clone(), Arc::new(inner.into_bytes()));
        let outer = SvgImage::parse(
            format!("<svg {NS} width='10' height='10'><image href='{url}' width='10' height='10'/></svg>")
                .as_bytes(),
        )
        .unwrap()
        .with_images(images);
        let bitmap = outer
            .rasterize((10.0, 10.0), (10, 10), ImageRendering::Smooth, None)
            .unwrap();
        assert_eq!(bitmap.frames[0].pixel(5, 5), Color::rgb(0, 0, 255));
    }

    #[test]
    fn a_zero_view_box_draws_nothing() {
        let svg = SvgImage::parse(
            format!("<svg {NS} viewBox='0 0 8 0'><rect width='100%' height='100%'/></svg>")
                .as_bytes(),
        )
        .unwrap();
        let bitmap = svg
            .rasterize((10.0, 10.0), (10, 10), ImageRendering::Smooth, None)
            .unwrap();
        assert!(bitmap.frames[0].pixels.iter().all(|b| *b == 0));
    }

    #[test]
    fn device_size_stays_in_budget() {
        assert_eq!(device_size((10.0, 10.0), (2.0, 2.0)), (20, 20));
        assert_eq!(device_size((0.2, 0.2), (1.0, 1.0)), (1, 1));
        let (w, h) = device_size((100_000.0, 100_000.0), (1.0, 1.0));
        assert!(w as u64 * h as u64 <= MAX_RASTER_PIXELS && w == h);
    }
}
