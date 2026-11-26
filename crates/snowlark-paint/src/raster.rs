// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rasterizing a display list with Direct2D. The painter draws a list into whatever Direct2D device
//! context it is given; [`HeadlessRenderer`] gives it a WIC bitmap render target (software, no GPU
//! needed) and returns the pixels; [`crate::gpu::TileRasterizer`] gives it GPU tiles.
//!
//! Text is ClearType on opaque targets and grayscale on transparent ones (ClearType needs to know the
//! color under each glyph). Inside an opacity group Direct2D itself falls back to grayscale.

use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::sync::Arc;

use windows::core::{Interface, Result};
use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows_numerics::{Matrix3x2, Vector2};

use crate::com;
use crate::display_list::*;
use crate::filter::FilterRange;
use crate::geom::{Color, Matrix, Point, Radii, Rect, Sides, Size, Snap};
use crate::image::{DecodedImage, Pixmap, ShownImage};

/// Where a list's [`FontId`]s resolve: the [`crate::TextEngine`] that shaped its glyphs, or, on the
/// far side of the raster seam, a [`crate::raster_tables::FontTable`].
pub trait FontResolver {
    fn factory(&self) -> &IDWriteFactory5;
    /// The face behind an id, and whether it has color glyphs.
    fn face(&self, font: FontId) -> Option<(IDWriteFontFace, bool)>;
}

impl<T: FontResolver + ?Sized> FontResolver for Arc<T> {
    fn factory(&self) -> &IDWriteFactory5 {
        (**self).factory()
    }

    fn face(&self, font: FontId) -> Option<(IDWriteFontFace, bool)> {
        (**self).face(font)
    }
}

pub use snowlark_graphics::raster::{ImageResolver, Viewport};

/// What a display list refers to: the fonts that shaped its glyphs and the images it shows.
#[derive(Clone, Copy)]
pub struct Sources<'a> {
    pub text: &'a dyn FontResolver,
    pub images: &'a dyn ImageResolver,
}

pub(crate) fn rect_f(r: &Rect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.x,
        top: r.y,
        right: r.right(),
        bottom: r.bottom(),
    }
}

pub(crate) fn color_f(c: Color) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: c.r as f32 / 255.0,
        g: c.g as f32 / 255.0,
        b: c.b as f32 / 255.0,
        a: c.a as f32 / 255.0,
    }
}

pub(crate) fn matrix_3x2(m: &Matrix) -> Matrix3x2 {
    Matrix3x2 {
        M11: m.m11,
        M12: m.m12,
        M21: m.m21,
        M22: m.m22,
        M31: m.dx,
        M32: m.dy,
    }
}

fn v2(p: Point) -> Vector2 {
    Vector2 { X: p.x, Y: p.y }
}

/// Device-dependent objects a painter keeps between frames: they belong to one device context's
/// device (a brush, uploaded images) and are reused by every list drawn with that context.
pub(crate) struct Resources {
    brush: ID2D1SolidColorBrush,
    dashed: ID2D1StrokeStyle,
    dotted: ID2D1StrokeStyle,
    bitmaps: HashMap<(ImageId, usize), Upload>,
    /// The latest conic gradient bitmaps, by a hash of what they show (a tile raster draws each
    /// gradient again; the newest are last).
    conics: Vec<(u64, ID2D1Bitmap1)>,
    /// The latest gradient stop collections (what a gradient brush is made from), the newest last:
    /// making one builds and uploads the gradient's texture, so a page with many boxes of the same
    /// gradient (a repeated background) would make it once per box and tile, every raster.
    gradients: Vec<StopsEntry>,
    /// How many stop collections were made (for tests).
    pub(crate) stops_made: u64,
    dwrite: IDWriteFactory4,
    text_params: IDWriteRenderingParams,
    factory: ID2D1Factory,
    bounds: Vec<Rect>,
    /// (scratch for `DisplayList::group_bounds`)
    open_groups: Vec<(usize, Rect)>,
    /// Device contexts of the same device that paint masks and filter groups into command lists
    /// (each taken while in use: a group inside a group takes another).
    record_ctxs: Vec<ID2D1DeviceContext>,
    /// How this context blurs (see [`BlurPath`]).
    blur: BlurPath,
    /// The backdrop filter results of the frame being painted, each rendered once and drawn again
    /// wherever a later backdrop paints its backdrop over again ([`Painter::backdrop_filter`]).
    backdrops: Vec<BackdropResult>,
    /// Whether `backdrops` lasts over several paints (the tiles of one frame) rather than one.
    frame_open: bool,
    /// How many backdrop filter results were rendered (for tests).
    pub(crate) backdrops_made: u64,
}

/// A backdrop filter's result rendered into a bitmap: the item (`at` of the list at address
/// `list`), the transform and crop it was rendered with, and the part of the crop the bitmap
/// holds, all in the frame's device pixels (a tile's painter is offset from them).
struct BackdropResult {
    list: usize,
    at: usize,
    transform: Matrix,
    crop: Rect,
    region: Rect,
    bitmap: ID2D1Bitmap1,
}

/// The most pixels the backdrop results of one frame hold together (beyond it a backdrop filter is
/// drawn straight from its effect graph, and a later backdrop that reads it paints it again).
const BACKDROP_PIXELS: u64 = 32 << 20;

/// `r` grown to whole device pixels.
fn round_out(r: &Rect) -> Rect {
    Rect::from_ltrb(
        r.x.floor(),
        r.y.floor(),
        r.right().ceil(),
        r.bottom().ceil(),
    )
}

/// Whether `outer` holds all of `inner`.
fn covers(outer: &Rect, inner: &Rect) -> bool {
    outer.x <= inner.x
        && outer.y <= inner.y
        && outer.right() >= inner.right()
        && outer.bottom() >= inner.bottom()
}

/// Whether two transforms are the same but for rounding (a tile's offset added and taken away).
fn same_matrix(a: &Matrix, b: &Matrix) -> bool {
    let close = |x: f32, y: f32| (x - y).abs() <= 1e-4 * (1.0 + x.abs().max(y.abs()));
    close(a.m11, b.m11)
        && close(a.m12, b.m12)
        && close(a.m21, b.m21)
        && close(a.m22, b.m22)
        && close(a.dx, b.dx)
        && close(a.dy, b.dy)
}

/// A gradient stop collection and what it was made from.
struct StopsEntry {
    hash: u64,
    stops: Vec<D2D1_GRADIENT_STOP>,
    extend: Extend,
    collection: ID2D1GradientStopCollection1,
}

/// How many gradient stop collections [`Resources`] keeps.
const GRADIENTS_KEPT: usize = 64;

/// An image on the device, and which pixels it is.
struct Upload {
    version: u64,
    /// The image it came from, when it came from a store: stores number their versions apart, so
    /// another store's image can have the same id and version.
    image: Option<Arc<DecodedImage>>,
    bitmap: ID2D1Bitmap1,
}

impl Upload {
    fn shows(&self, now: &ShownImage) -> bool {
        self.version == now.version
            && match (&self.image, &now.image) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}

/// The bitmap of image `id` as `images` shows it now, uploaded to `ctx`'s device when it is not
/// there yet or its pixels changed.
// SAFETY contract: `ctx` is a live device context of the device `res` belongs to.
unsafe fn uploaded(
    ctx: &ID2D1DeviceContext,
    res: &mut Resources,
    images: &dyn ImageResolver,
    id: ImageId,
) -> Result<(ID2D1Bitmap1, ShownImage)> {
    let shown = images
        .shown(id)
        .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
    let key = (id, shown.frame);
    if !res.bitmaps.get(&key).is_some_and(|u| u.shows(&shown)) {
        // (a table's images are uploaded when they come: one that isn't there couldn't be)
        let image = shown.image.as_ref().ok_or(E_INVALIDARG)?;
        let f = &image.frames[shown.frame];
        let bitmap = make_bitmap(ctx, f.width, f.height, &f.pixels)?;
        let upload = Upload {
            version: shown.version,
            image: Some(image.clone()),
            bitmap,
        };
        res.bitmaps.insert(key, upload);
    }
    Ok((res.bitmaps[&key].bitmap.clone(), shown))
}

/// Makes a bitmap of premultiplied BGRA pixels, rows `width * 4` bytes apart.
// SAFETY contract: `pixels` holds at least `width * height * 4` bytes.
unsafe fn make_bitmap(
    ctx: &ID2D1DeviceContext,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<ID2D1Bitmap1> {
    let max = ctx.GetMaximumBitmapSize();
    if width > max || height > max {
        // (bigger than a texture can be: needs a CPU downscale first, not done yet)
        return Err(E_INVALIDARG.into());
    }
    ctx.CreateBitmap(
        D2D_SIZE_U { width, height },
        Some(pixels.as_ptr().cast()),
        width * 4,
        &D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
            colorContext: ManuallyDrop::new(None),
        },
    )
}

impl Resources {
    pub(crate) fn new(ctx: &ID2D1DeviceContext, text: &dyn FontResolver) -> Result<Resources> {
        // SAFETY: plain object creation on a live device context; all results are owned.
        unsafe {
            let factory = ctx.GetFactory()?;
            let brush = ctx.CreateSolidColorBrush(&color_f(Color::BLACK), None)?;
            let stroke = |cap: D2D1_CAP_STYLE, dashes: &[f32]| {
                factory.CreateStrokeStyle(
                    &D2D1_STROKE_STYLE_PROPERTIES {
                        startCap: cap,
                        endCap: cap,
                        dashCap: cap,
                        lineJoin: D2D1_LINE_JOIN_MITER,
                        miterLimit: 10.0,
                        dashStyle: D2D1_DASH_STYLE_CUSTOM,
                        dashOffset: 0.0,
                    },
                    Some(dashes),
                )
            };
            // (dash lengths are in stroke widths; a zero-length round-capped dash is a dot)
            let dashed = stroke(D2D1_CAP_STYLE_FLAT, &[3.0, 2.0])?;
            let dotted = stroke(D2D1_CAP_STYLE_ROUND, &[0.0, 2.0])?;
            let dwrite: IDWriteFactory4 = text.factory().cast()?;
            let text_params = dwrite.CreateRenderingParams()?;
            Ok(Resources {
                brush,
                dashed,
                dotted,
                bitmaps: HashMap::new(),
                conics: Vec::new(),
                gradients: Vec::new(),
                stops_made: 0,
                dwrite,
                text_params,
                factory,
                bounds: Vec::new(),
                open_groups: Vec::new(),
                record_ctxs: Vec::new(),
                blur: BlurPath::from_env(),
                backdrops: Vec::new(),
                frame_open: false,
                backdrops_made: 0,
            })
        }
    }

    /// Starts a frame painted in several pieces (tiles): backdrop results are kept until
    /// [`Resources::end_frame`], so each is rendered once for all of them.
    pub(crate) fn begin_frame(&mut self) {
        self.backdrops.clear();
        self.frame_open = true;
    }

    /// Ends what [`Resources::begin_frame`] started, freeing its backdrop results.
    pub(crate) fn end_frame(&mut self) {
        self.backdrops.clear();
        self.frame_open = false;
    }

    /// Drops uploads of images that are gone (or showing another frame or other pixels).
    pub(crate) fn trim(&mut self, images: &dyn ImageResolver) {
        self.bitmaps.retain(|(id, frame), upload| {
            images
                .shown(*id)
                .is_some_and(|now| upload.shows(&now) && now.frame == *frame)
        });
    }

    /// Uploads pixels that came ahead of the lists that show them, for a table to resolve to:
    /// `width * height * 4` bytes of `pixels`, checked by the table.
    pub(crate) fn upload(
        &mut self,
        ctx: &ID2D1DeviceContext,
        id: ImageId,
        shown: &ShownImage,
        pixels: &[u8],
    ) -> Result<()> {
        debug_assert_eq!(
            pixels.len() as u64,
            shown.width as u64 * shown.height as u64 * 4
        );
        // SAFETY: as the contract says; the table checked the length.
        let bitmap = unsafe { make_bitmap(ctx, shown.width, shown.height, pixels)? };
        let upload = Upload {
            version: shown.version,
            image: None,
            bitmap,
        };
        self.bitmaps.insert((id, shown.frame), upload);
        Ok(())
    }
}

enum Open {
    AxisClip,
    Layer,
    /// A group that changes nothing as it is drawn (an opacity group at 1: a backdrop root only).
    Group,
    /// Drawings add to what is below ([`Item::PushPlus`]); the blend to restore.
    Plus(D2D1_PRIMITIVE_BLEND),
    /// The transform and snapping to restore.
    Transform(Matrix, Option<Snap>),
}

/// Draws display lists into a device context that is between `BeginDraw` and `EndDraw`.
pub(crate) struct Painter<'a> {
    ctx: &'a ID2D1DeviceContext,
    res: &'a mut Resources,
    sources: Sources<'a>,
    transform: Matrix,
    /// How box geometry is snapped to device pixels under the current transform (see [`Snap`]):
    /// `None` inside a transform other than a translation by whole device pixels.
    snap: Option<Snap>,
    stack: Vec<Open>,
    /// Whether the target's text is ClearType outside layers, and how many layers are open: text
    /// drawn into a layer is grayscale (ClearType into a layer that starts transparent draws
    /// nothing).
    cleartype: bool,
    layers: usize,
    /// The context's color glyph interface (Windows 10 1607 and later).
    ctx4: Option<ID2D1DeviceContext4>,
    /// Where the painter's root content starts (the list's start, or a recorded group's first
    /// item) and the transform and snapping it starts with: the backdrop root of a backdrop filter
    /// with no other root open ([`Item::BackdropFilter`]).
    root: (usize, Matrix, Option<Snap>),
    /// The open groups that are backdrop roots (Filter Effects 2 §3.1: opacity and mask groups;
    /// filter groups are recorded by a painter of their own, whose root they are), innermost last.
    marks: Vec<Mark>,
    /// How many backdrops this painter paints again inside (see [`MAX_BACKDROP_DEPTH`]).
    depth: u32,
    /// The canvas color under the list, when the painter paints a page's whole list: part of a
    /// backdrop read from its root.
    canvas: Option<Color>,
    /// How many items it has drawn ([`Painter::drawn`]).
    drawn: u32,
    /// The device pixels of what it paints that are shown or read: the frame, or for a backdrop
    /// painted again, what its filters read. A backdrop filter renders its result there only.
    need: Rect,
    /// Where the painter's device pixel (0, 0) is in the frame's (a tile's top-left corner).
    origin: Point,
}

/// An open backdrop root: where its content starts, the transform and snapping there, and the
/// painter's stack depth with it open.
#[derive(Clone, Copy)]
struct Mark {
    start: usize,
    transform: Matrix,
    snap: Option<Snap>,
    depth: usize,
}

/// How deep backdrops are read within backdrops (each paints again what is under it).
const MAX_BACKDROP_DEPTH: u32 = 3;

/// The most device pixels a side of a pattern's tile is recorded at (a larger tile is recorded
/// smaller and drawn scaled up).
const MAX_PATTERN_TILE: f32 = 4096.0;

impl<'a> Painter<'a> {
    /// `base` maps CSS pixels to the target's device pixels; `opaque` says whether the target's
    /// pixels are opaque (ClearType) or not (grayscale text).
    pub(crate) fn new(
        ctx: &'a ID2D1DeviceContext,
        res: &'a mut Resources,
        sources: Sources<'a>,
        base: Matrix,
        opaque: bool,
    ) -> Painter<'a> {
        // SAFETY: state setters on a live context.
        unsafe {
            ctx.SetUnitMode(D2D1_UNIT_MODE_PIXELS);
            ctx.SetTextRenderingParams(&res.text_params);
            ctx.SetTextAntialiasMode(if opaque {
                D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE
            } else {
                D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE
            });
            ctx.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            ctx.SetTransform(&matrix_3x2(&base));
        }
        Painter {
            ctx,
            res,
            sources,
            transform: base,
            snap: Snap::of(&base),
            stack: Vec::new(),
            cleartype: opaque,
            layers: 0,
            ctx4: ctx.cast().ok(),
            root: (0, base, Snap::of(&base)),
            marks: Vec::new(),
            depth: 0,
            canvas: None,
            drawn: 0,
            need: Rect::infinite(),
            origin: Point::new(0.0, 0.0),
        }
    }

    /// Says the list is painted over the canvas color `c` (see [`Painter::canvas`]).
    pub(crate) fn over_canvas(mut self, c: Color) -> Self {
        self.canvas = Some(c);
        self
    }

    /// Says which device pixels are shown (`need`, the frame's in this painter's pixels) and where
    /// its pixel (0, 0) is in the frame (`origin`).
    pub(crate) fn within(mut self, need: Rect, origin: Point) -> Self {
        self.need = need;
        self.origin = origin;
        self
    }

    /// How many items it has drawn so far (a group's push counts; a group culled whole does not).
    pub(crate) fn drawn(&self) -> u32 {
        self.drawn
    }

    /// Marks the group just pushed, whose content starts at `start`, as a backdrop root.
    fn mark(&mut self, start: usize) {
        self.marks.push(Mark {
            start,
            transform: self.transform,
            snap: self.snap,
            depth: self.stack.len(),
        });
    }

    /// Draws `list`. With `cull`, drawing items whose device bounds (from
    /// [`DisplayList::item_bounds`] under the same base) miss the rectangle are skipped.
    pub(crate) fn paint(&mut self, list: &DisplayList, cull: Option<(&[Rect], Rect)>) {
        let own_frame = !self.res.frame_open;
        if own_frame {
            self.res.backdrops.clear();
        }
        self.paint_items(list, 0..list.items.len(), cull);
        // (a list is validated before painting, so nothing is left open; this is belt and braces)
        while let Some(open) = self.stack.pop() {
            self.close(open);
        }
        if own_frame {
            self.res.backdrops.clear();
        }
    }

    fn paint_items(
        &mut self,
        list: &DisplayList,
        range: std::ops::Range<usize>,
        cull: Option<(&[Rect], Rect)>,
    ) {
        let mut i = range.start;
        while i < range.end {
            let item = &list.items[i];
            if let Item::PushMask { bounds, luminance } = *item {
                // The mask's items, up to its MaskContent, paint the mask.
                let mut depth = 0;
                let mut end = i + 1;
                while end < range.end {
                    match list.items[end] {
                        Item::PushMask { .. } => depth += 1,
                        Item::MaskContent if depth == 0 => break,
                        Item::MaskContent => depth -= 1,
                        _ => {}
                    }
                    end += 1;
                }
                // SAFETY: drawing on this painter's context between BeginDraw and EndDraw.
                let pushed =
                    unsafe { self.push_mask_image(list, i + 1..end, bounds, luminance, cull) };
                if pushed.is_err() {
                    // (a mask that can't be made shows nothing of what it masks)
                    // SAFETY: as above.
                    unsafe { self.push_hidden() };
                }
                self.mark(end + 1);
                i = end + 1;
                continue;
            }
            if let Item::PushFilter { filters, .. } = *item {
                // The group's items, up to its PopFilter, are its content.
                let mut depth = 0;
                let mut end = i + 1;
                while end < range.end {
                    match list.items[end] {
                        Item::PushFilter { .. } => depth += 1,
                        Item::PopFilter if depth == 0 => break,
                        Item::PopFilter => depth -= 1,
                        _ => {}
                    }
                    end += 1;
                }
                // (the cull bounds and area are frame device pixels, where `bounds[i]` is the
                // group's region; the painter's transform holds the tile's offset too)
                let shows = cull.is_none_or(|(b, area)| b[i].intersects(&area));
                if shows {
                    // SAFETY: drawing on this painter's context between BeginDraw and EndDraw.
                    let _ = unsafe { self.filter_group(list, i + 1..end, filters, cull) };
                }
                i = end + 1;
                continue;
            }
            if let Some((bounds, area)) = cull {
                if item.is_drawing() && !bounds[i].intersects(&area) {
                    i += 1;
                    continue;
                }
                // A group whose content paints nothing in the area is skipped whole, its clip or
                // layer too (the cull bounds give a group's push its content's union,
                // `DisplayList::group_bounds`).
                if matches!(
                    item,
                    Item::PushClipRect { .. }
                        | Item::PushClipRoundedRect { .. }
                        | Item::PushClipPath { .. }
                        | Item::PushTransform { .. }
                        | Item::PushOpacity { .. }
                        | Item::PushLayer { .. }
                        | Item::PushPlus
                ) && !bounds[i].intersects(&area)
                {
                    i = group_end(list, i, range.end) + 1;
                    continue;
                }
            }
            // A group of opacity 0 is composited fully transparent (CSS Color 4 §3.2): nothing of
            // it shows, so nothing of it is drawn.
            if let Item::PushOpacity { opacity, .. } = *item {
                if opacity <= 0.0 {
                    i = group_end(list, i, range.end) + 1;
                    continue;
                }
            }
            if let Item::BackdropFilter {
                filters,
                rect,
                radii,
                own,
            } = *item
            {
                // SAFETY: drawing on this painter's context between BeginDraw and EndDraw.
                let _ = unsafe { self.backdrop_filter(list, i, filters, rect, radii, own, cull) };
                i += 1;
                continue;
            }
            // A failed item (an unusable font or image, a resource Direct2D refused) is skipped:
            // one bad item mustn't lose the frame.
            let _ = self.item(list, item);
            self.drawn += 1;

            if let Item::PushOpacity { .. } = item {
                self.mark(i + 1);
            }
            i += 1;
        }
    }

    /// Paints a mask's items (`range` of `list`) into a command list, with a device context of
    /// their own, and pushes a layer whose opacity brush is that image (its alpha, or with
    /// `luminance` its luminance as alpha), bounded by `bounds`.
    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread.
    unsafe fn push_mask_image(
        &mut self,
        list: &DisplayList,
        range: std::ops::Range<usize>,
        bounds: Rect,
        luminance: bool,
        cull: Option<(&[Rect], Rect)>,
    ) -> Result<()> {
        // The mask's layers ([`Item::MaskLayer`] between them, outside their own groups), each
        // recorded on its own and composited onto those below it.
        let mut layers = Vec::new();
        let mut start = range.start;
        let mut layer = (MaskComposite::SourceOver, luminance);
        let mut depth = 0usize;
        for i in range.clone() {
            match list.items[i] {
                Item::MaskLayer {
                    composite,
                    luminance,
                } if depth == 0 => {
                    layers.push((start..i, layer));
                    layer = (composite, luminance);
                    start = i + 1;
                }
                Item::PushMask { .. } => depth += 1,
                Item::PopMask => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        layers.push((start..range.end, layer));
        let mut image: Option<ID2D1Image> = None;
        for (items, (composite, luminance)) in layers {
            let empty = items.start..items.start;
            let source = self.mask_layer_image(list, items, luminance, cull)?;
            image = Some(match (image, composite) {
                (None, _) | (Some(_), MaskComposite::Copy) => source,
                (Some(_), MaskComposite::Clear) => self.record(list, empty, cull)?.cast()?,
                (Some(below), op) => {
                    let effect = self.ctx.CreateEffect(&CLSID_D2D1Composite)?;
                    effect.SetInput(0, &below, true);
                    effect.SetInput(1, &source, true);
                    let mode = match op {
                        MaskComposite::SourceIn => D2D1_COMPOSITE_MODE_SOURCE_IN,
                        MaskComposite::SourceOut => D2D1_COMPOSITE_MODE_SOURCE_OUT,
                        MaskComposite::SourceAtop => D2D1_COMPOSITE_MODE_SOURCE_ATOP,
                        MaskComposite::DestinationOver => D2D1_COMPOSITE_MODE_DESTINATION_OVER,
                        MaskComposite::DestinationIn => D2D1_COMPOSITE_MODE_DESTINATION_IN,
                        MaskComposite::DestinationOut => D2D1_COMPOSITE_MODE_DESTINATION_OUT,
                        MaskComposite::DestinationAtop => D2D1_COMPOSITE_MODE_DESTINATION_ATOP,
                        MaskComposite::Xor => D2D1_COMPOSITE_MODE_XOR,
                        MaskComposite::Plus => D2D1_COMPOSITE_MODE_PLUS,
                        _ => D2D1_COMPOSITE_MODE_SOURCE_OVER,
                    };
                    effect.SetValue(
                        D2D1_COMPOSITE_PROP_MODE.0 as u32,
                        D2D1_PROPERTY_TYPE_ENUM,
                        &(mode.0 as u32).to_le_bytes(),
                    )?;
                    effect.GetOutput()?
                }
            });
        }
        // (there is always one layer)
        let Some(image) = image else {
            return Err(E_INVALIDARG.into());
        };
        // (the mask was recorded in device pixels: the layer is pushed there too, and the brush,
        // which starts its source rectangle at its origin, is moved back to where it was drawn)
        let device = self.transform.apply_rect(&bounds);
        let brush = self.ctx.CreateImageBrush(
            &image,
            &D2D1_IMAGE_BRUSH_PROPERTIES {
                sourceRectangle: rect_f(&device),
                extendModeX: D2D1_EXTEND_MODE_CLAMP,
                extendModeY: D2D1_EXTEND_MODE_CLAMP,
                interpolationMode: D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            },
            Some(&D2D1_BRUSH_PROPERTIES {
                opacity: 1.0,
                transform: matrix_3x2(&Matrix::translation(device.x, device.y)),
            }),
        )?;
        let params = D2D1_LAYER_PARAMETERS1 {
            contentBounds: rect_f(&device),
            geometricMask: ManuallyDrop::new(None),
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: Matrix3x2::identity(),
            opacity: 1.0,
            opacityBrush: ManuallyDrop::new(Some(brush.cast()?)),
            layerOptions: D2D1_LAYER_OPTIONS1_NONE,
        };
        self.ctx.SetTransform(&Matrix3x2::identity());
        self.ctx.PushLayer(&params, None);
        self.ctx.SetTransform(&matrix_3x2(&self.transform));
        // (Direct2D holds its own reference to the brush)
        drop(ManuallyDrop::into_inner(params.opacityBrush));
        self.layer_opened();
        Ok(())
    }

    /// One mask layer's items (`range` of `list`) recorded as an image of its coverage: its alpha,
    /// or with `luminance` its luminance as alpha.
    // SAFETY contract: as `record`.
    unsafe fn mask_layer_image(
        &mut self,
        list: &DisplayList,
        range: std::ops::Range<usize>,
        luminance: bool,
        cull: Option<(&[Rect], Rect)>,
    ) -> Result<ID2D1Image> {
        let cmd = self.record(list, range, cull)?;
        if luminance {
            let effect = self.ctx.CreateEffect(&CLSID_D2D1LuminanceToAlpha)?;
            effect.SetInput(0, &cmd, true);
            effect.GetOutput()
        } else {
            cmd.cast()
        }
    }

    /// Paints `range` of `list` into a command list (device pixels, under the painter's transform
    /// and snapping) with a painter of its own on a recording context of the same device.
    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread.
    unsafe fn record(
        &mut self,
        list: &DisplayList,
        range: std::ops::Range<usize>,
        cull: Option<(&[Rect], Rect)>,
    ) -> Result<ID2D1CommandList> {
        let space = (self.need, self.origin);
        self.record_at(
            list,
            range,
            self.transform,
            self.snap,
            cull,
            self.depth,
            space,
        )
    }

    /// [`Painter::record`] starting with `transform` and `snap`, `depth` backdrops deep, the
    /// recording's needed pixels and frame origin `space` (see [`Painter::within`]).
    // SAFETY contract: as `record`.
    #[allow(clippy::too_many_arguments)]
    unsafe fn record_at(
        &mut self,
        list: &DisplayList,
        range: std::ops::Range<usize>,
        transform: Matrix,
        snap: Option<Snap>,
        cull: Option<(&[Rect], Rect)>,
        depth: u32,
        space: (Rect, Point),
    ) -> Result<ID2D1CommandList> {
        let rctx = match self.res.record_ctxs.pop() {
            Some(c) => c,
            None => self
                .ctx
                .GetDevice()?
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?,
        };
        let drawn = rctx.CreateCommandList().and_then(|cmd| {
            rctx.SetTarget(&cmd);
            rctx.BeginDraw();
            {
                let mut p = Painter::new(&rctx, self.res, self.sources, transform, false);
                p.snap = snap;
                p.root = (range.start, transform, snap);
                p.depth = depth;
                (p.need, p.origin) = space;
                p.paint_items(list, range, cull);
                while let Some(open) = p.stack.pop() {
                    p.close(open);
                }
            }
            let r = rctx.EndDraw(None, None);
            rctx.SetTarget(None);
            r?;
            cmd.Close()?;
            Ok(cmd)
        });
        self.res.record_ctxs.push(rctx);
        drawn
    }

    /// A pattern fill ([`Item::FillPattern`]): the tile's list recorded once at about the device
    /// pixels it covers (its size rounded to whole pixels, at most [`MAX_PATTERN_TILE`] a side),
    /// then `rect` filled with an image brush that repeats it both ways through `transform`.
    // SAFETY contract: as `record`.
    unsafe fn fill_pattern(&mut self, p: &Pattern, rect: Rect, transform: Matrix) -> Result<()> {
        let to_device = transform.then(&self.transform);
        if to_device.determinant() == 0.0 || !to_device.is_finite() {
            return Ok(());
        }
        // (the device pixels a tile unit covers along each of the tile's axes)
        let sx = to_device.m11.hypot(to_device.m12);
        let sy = to_device.m21.hypot(to_device.m22);
        let pixels = |units: f32, scale: f32| (units * scale).round().clamp(1.0, MAX_PATTERN_TILE);
        let (pw, ph) = (pixels(p.tile.width, sx), pixels(p.tile.height, sy));
        let (kx, ky) = (pw / p.tile.width, ph / p.tile.height);
        let cmd = self.record_at(
            &p.list,
            0..p.list.items.len(),
            Matrix::scale(kx, ky),
            None,
            None,
            self.depth,
            // (a tile's own pixels: the same in every tile of the frame)
            (Rect::infinite(), Point::new(0.0, 0.0)),
        )?;
        let image: ID2D1Image = cmd.cast()?;
        let brush = self.ctx.CreateImageBrush(
            &image,
            &D2D1_IMAGE_BRUSH_PROPERTIES {
                sourceRectangle: rect_f(&Rect::new(0.0, 0.0, pw, ph)),
                extendModeX: D2D1_EXTEND_MODE_WRAP,
                extendModeY: D2D1_EXTEND_MODE_WRAP,
                interpolationMode: D2D1_INTERPOLATION_MODE_LINEAR,
            },
            Some(&D2D1_BRUSH_PROPERTIES {
                opacity: 1.0,
                transform: matrix_3x2(&Matrix::scale(1.0 / kx, 1.0 / ky).then(&transform)),
            }),
        )?;
        self.ctx.FillRectangle(&rect_f(&rect), &brush);
        Ok(())
    }

    /// A filter group ([`Item::PushFilter`]): its content (`range` of `list`) recorded in device
    /// pixels, put through the functions, drawn where it was recorded (what the painter has open
    /// clips it). With `cull`, the content is culled to the part that can reach the area through
    /// the functions.
    // SAFETY contract: as `record`.
    unsafe fn filter_group(
        &mut self,
        list: &DisplayList,
        range: std::ops::Range<usize>,
        filters: FilterRange,
        cull: Option<(&[Rect], Rect)>,
    ) -> Result<()> {
        let ops = list.filter(filters);
        let cull = match (cull, self.transform.inverse()) {
            (Some((bounds, area)), Some(inverse)) => {
                let local = crate::filter::source_rect(ops, inverse.apply_rect(&area));
                Some((bounds, self.transform.apply_rect(&local)))
            }
            (Some((bounds, area)), None) => Some((bounds, area)),
            (None, _) => None,
        };
        // (what the functions read of the content to show the needed pixels)
        let need = match self.transform.inverse() {
            Some(inverse) => self.transform.apply_rect(&crate::filter::source_rect(
                ops,
                inverse.apply_rect(&self.need),
            )),
            None => Rect::infinite(),
        };
        let content: ID2D1Image = self
            .record_at(
                list,
                range,
                self.transform,
                self.snap,
                cull,
                self.depth,
                (need, self.origin),
            )?
            .cast()?;
        let (ctx, res, images) = (self.ctx, &mut *self.res, self.sources.images);
        let mut bitmaps = |id| uploaded(ctx, res, images, id).ok().map(|(b, _)| b);
        let out = crate::effects::apply(self.ctx, ops, &content, &self.transform, &mut bitmaps)?;
        self.ctx.SetTransform(&Matrix3x2::identity());
        self.ctx.DrawImage(
            &out,
            None,
            None,
            D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            D2D1_COMPOSITE_MODE_SOURCE_OVER,
        );
        self.ctx.SetTransform(&matrix_3x2(&self.transform));
        Ok(())
    }

    /// A backdrop filter ([`Item::BackdropFilter`], item `at` of `list`): what was painted since
    /// the backdrop root is painted again into a command list (culled to the item's area), cut to
    /// the border box's device rectangle with mirrored edges, filtered, and drawn clipped to the
    /// rounded border box. (Painting it again, rather than reading the target, works inside
    /// recorded groups and layers, whose pixels can't be read back.)
    ///
    /// The result is rendered once per frame, for the pixels that are shown or that a later
    /// backdrop reads ([`Painter::need`]), into a bitmap ([`Resources::backdrops`]): a later
    /// backdrop filter that paints this one again as part of its own backdrop draws that bitmap,
    /// as reading the target's pixels would give it. Without it, each backdrop painted every
    /// earlier one again, and stacked backdrops grew as the cube of their number.
    // SAFETY contract: as `record`.
    #[allow(clippy::too_many_arguments)]
    unsafe fn backdrop_filter(
        &mut self,
        list: &DisplayList,
        at: usize,
        filters: FilterRange,
        rect: Rect,
        radii: Radii,
        own: u8,
        cull: Option<(&[Rect], Rect)>,
    ) -> Result<()> {
        // (the border box, snapped as the box was, in whole device pixels)
        let device = self.transform.apply_rect(&self.snapped(&rect));
        let crop = Rect::from_ltrb(
            device.x.round(),
            device.y.round(),
            (device.x + device.width).round(),
            (device.y + device.height).round(),
        );
        if crop.is_empty() {
            return Ok(());
        }
        // (of the result, what is shown or read later, in whole device pixels)
        let region = crop.intersect(&round_out(&crop.intersect(&self.need)));
        if region.is_empty() {
            return Ok(());
        }
        let (ox, oy) = (self.origin.x, self.origin.y);
        let list_key = list as *const DisplayList as usize;
        let frame_transform = self.transform.then(&Matrix::translation(ox, oy));
        let frame_crop = crop.translate(ox, oy);
        let found = self.res.backdrops.iter().position(|b| {
            b.list == list_key
                && b.at == at
                && b.crop == frame_crop
                && same_matrix(&b.transform, &frame_transform)
        });
        let wanted = region.translate(ox, oy);
        let (image, offset): (ID2D1Image, Option<Vector2>) = match found
            .map(|i| &self.res.backdrops[i])
            .filter(|b| covers(&b.region, &wanted))
        {
            Some(b) => (
                b.bitmap.cast()?,
                Some(Vector2 {
                    X: b.region.x - ox,
                    Y: b.region.y - oy,
                }),
            ),
            None => {
                if self.depth >= MAX_BACKDROP_DEPTH {
                    return Ok(());
                }
                // (a part rendered before is rendered again with this one)
                let region = match found {
                    Some(i) => self.res.backdrops[i]
                        .region
                        .translate(-ox, -oy)
                        .union(&region),
                    None => region,
                };
                let ops = list.filter(filters);
                let keep = self.marks.len().saturating_sub(own as usize);
                let (from, transform, snap) = match keep.checked_sub(1) {
                    Some(k) => {
                        let m = self.marks[k];
                        (m.start, m.transform, m.snap)
                    }
                    None => self.root,
                };
                // (what the filters read of the backdrop for the region: with mirrored edges,
                // nothing beyond the border box)
                let reads = match self.transform.inverse() {
                    Some(inverse) => self
                        .transform
                        .apply_rect(&crate::filter::source_rect(
                            ops,
                            inverse.apply_rect(&region),
                        ))
                        .intersect(&crop),
                    None => crop,
                };
                // (cull bounds are the frame's device pixels)
                let cull = cull
                    .map(|(bounds, _)| (bounds, bounds[at].intersect(&reads.translate(ox, oy))));
                let under: ID2D1Image = self
                    .record_at(
                        list,
                        from..at.max(from),
                        transform,
                        snap,
                        cull,
                        self.depth + 1,
                        (reads, self.origin),
                    )?
                    .cast()?;
                let base = match (keep, self.canvas) {
                    (0, Some(c)) => c,
                    _ => Color::TRANSPARENT,
                };
                self.res.backdrops_made += 1;
                let held: f64 = self
                    .res
                    .backdrops
                    .iter()
                    .map(|b| b.region.width as f64 * b.region.height as f64)
                    .sum();
                let max = self.ctx.GetMaximumBitmapSize() as f32;
                if region.width <= max
                    && region.height <= max
                    && held + region.width as f64 * region.height as f64 <= BACKDROP_PIXELS as f64
                {
                    let bitmap = self.render_backdrop(ops, &under, base, &crop, &region)?;
                    let result = BackdropResult {
                        list: list_key,
                        at,
                        transform: frame_transform,
                        crop: frame_crop,
                        region: region.translate(ox, oy),
                        bitmap: bitmap.clone(),
                    };
                    match found {
                        Some(i) => self.res.backdrops[i] = result,
                        None => self.res.backdrops.push(result),
                    }
                    (
                        bitmap.cast()?,
                        Some(Vector2 {
                            X: region.x,
                            Y: region.y,
                        }),
                    )
                } else {
                    let (ctx, res, images) = (self.ctx, &mut *self.res, self.sources.images);
                    let mut bitmaps = |id| uploaded(ctx, res, images, id).ok().map(|(b, _)| b);
                    let out = crate::effects::backdrop(
                        self.ctx,
                        ops,
                        &under,
                        base,
                        &crop,
                        &self.transform,
                        &mut bitmaps,
                    )?;
                    (out, None)
                }
            }
        };
        self.push_shape_clip(&rect, &radii)?;
        self.ctx.SetTransform(&Matrix3x2::identity());
        self.ctx.DrawImage(
            &image,
            offset.as_ref().map(|p| p as *const Vector2),
            None,
            D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            D2D1_COMPOSITE_MODE_SOURCE_OVER,
        );
        self.ctx.SetTransform(&matrix_3x2(&self.transform));
        if let Some(open) = self.stack.pop() {
            self.close(open);
        }
        Ok(())
    }

    /// A backdrop filter's result ([`crate::effects::backdrop`] of `under` over `base`, cut to
    /// `crop`) rendered for the device pixels `region` (whole, inside `crop`) into a bitmap of its
    /// own, with a context of the same device.
    // SAFETY contract: as `record`.
    unsafe fn render_backdrop(
        &mut self,
        ops: &[crate::filter::FilterOp],
        under: &ID2D1Image,
        base: Color,
        crop: &Rect,
        region: &Rect,
    ) -> Result<ID2D1Bitmap1> {
        let bctx = match self.res.record_ctxs.pop() {
            Some(c) => c,
            None => self
                .ctx
                .GetDevice()?
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?,
        };
        let made = (|| -> Result<ID2D1Bitmap1> {
            let bitmap = bctx.CreateBitmap(
                D2D_SIZE_U {
                    width: region.width as u32,
                    height: region.height as u32,
                },
                None,
                0,
                &D2D1_BITMAP_PROPERTIES1 {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
                    colorContext: ManuallyDrop::new(None),
                },
            )?;
            let (res, images) = (&mut *self.res, self.sources.images);
            let mut bitmaps = |id| uploaded(&bctx, res, images, id).ok().map(|(b, _)| b);
            let out = crate::effects::backdrop(
                &bctx,
                ops,
                under,
                base,
                crop,
                &self.transform,
                &mut bitmaps,
            )?;
            bctx.SetTarget(&bitmap);
            bctx.BeginDraw();
            bctx.SetUnitMode(D2D1_UNIT_MODE_PIXELS);
            bctx.SetTransform(&Matrix3x2::identity());
            bctx.Clear(Some(&color_f(Color::TRANSPARENT)));
            bctx.DrawImage(
                &out,
                Some(&Vector2 {
                    X: -region.x,
                    Y: -region.y,
                }),
                None,
                D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                D2D1_COMPOSITE_MODE_SOURCE_OVER,
            );
            let r = bctx.EndDraw(None, None);
            bctx.SetTarget(None);
            r?;
            Ok(bitmap)
        })();
        self.res.record_ctxs.push(bctx);
        made
    }

    /// Pushes a layer that shows nothing.
    // SAFETY contract: as `push_mask_image`.
    unsafe fn push_hidden(&mut self) {
        let params = D2D1_LAYER_PARAMETERS1 {
            contentBounds: rect_f(&Rect::infinite()),
            geometricMask: ManuallyDrop::new(None),
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: Matrix3x2::identity(),
            opacity: 0.0,
            opacityBrush: ManuallyDrop::new(None),
            layerOptions: D2D1_LAYER_OPTIONS1_NONE,
        };
        self.ctx.PushLayer(&params, None);
        self.layer_opened();
    }

    /// `r` with its edges on device pixels, when snapping applies.
    fn snapped(&self, r: &Rect) -> Rect {
        match self.snap {
            Some(s) => s.rect(r),
            None => *r,
        }
    }

    /// An image tile's snapped rectangle and how far its origin moved (what is drawn inside it
    /// moves with it).
    fn snapped_tile(&self, r: &Rect) -> (Rect, Point) {
        let s = self.snapped(r);
        (s, Point::new(s.x - r.x, s.y - r.y))
    }

    fn brush(&self, c: Color) -> &ID2D1SolidColorBrush {
        // SAFETY: setting a brush color has no preconditions.
        unsafe { self.res.brush.SetColor(&color_f(c)) };
        &self.res.brush
    }

    /// Records a layer that was just pushed: text inside it is grayscale.
    fn layer_opened(&mut self) {
        self.stack.push(Open::Layer);
        self.layers += 1;
        if self.cleartype && self.layers == 1 {
            // SAFETY: a state setter on a live context.
            unsafe {
                self.ctx
                    .SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE)
            };
        }
    }

    fn close(&mut self, open: Open) {
        // SAFETY: pops match the pushes recorded in `stack`, in order.
        unsafe {
            match open {
                Open::AxisClip => self.ctx.PopAxisAlignedClip(),
                Open::Layer => {
                    self.ctx.PopLayer();
                    self.layers -= 1;
                    if self.cleartype && self.layers == 0 {
                        self.ctx
                            .SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE);
                    }
                }
                Open::Group => {}
                Open::Plus(previous) => self.ctx.SetPrimitiveBlend(previous),
                Open::Transform(previous, snap) => {
                    self.transform = previous;
                    self.snap = snap;
                    self.ctx.SetTransform(&matrix_3x2(&previous));
                }
            }
        }
        let depth = self.stack.len();
        self.marks.retain(|m| m.depth <= depth);
    }

    fn item(&mut self, list: &DisplayList, item: &Item) -> Result<()> {
        // SAFETY (for the block): Direct2D calls on a context between BeginDraw and EndDraw, with
        // pointers to locals that outlive each call.
        unsafe {
            match *item {
                Item::FillRect { rect, color } => {
                    let rect = self.snapped(&rect);
                    self.ctx.FillRectangle(&rect_f(&rect), self.brush(color));
                }
                Item::FillRoundedRect { rect, radii, color } => {
                    self.fill_rounded(&self.snapped(&rect), &radii, color)?;
                }
                Item::StrokeRect { rect, width, color } => {
                    self.ctx
                        .DrawRectangle(&rect_f(&rect), self.brush(color), width, None);
                }
                Item::StrokeRoundedRect {
                    rect,
                    radii,
                    width,
                    color,
                } => {
                    let radii = radii.fitted(&rect);
                    if radii.is_uniform() {
                        let rr = rounded(&rect, radii.top_left);
                        self.ctx
                            .DrawRoundedRectangle(&rr, self.brush(color), width, None);
                    } else {
                        let path = self.rounded_path(&rect, &radii)?;
                        self.ctx.DrawGeometry(&path, self.brush(color), width, None);
                    }
                }
                Item::Border(ref b) => {
                    let b = match self.snap {
                        Some(snap) => Border {
                            rect: snap.rect(&b.rect),
                            widths: snap.border_widths(&b.widths),
                            ..*b
                        },
                        None => *b,
                    };
                    self.border(&b)?
                }
                Item::Text(ref run) => self.text(list, run)?,
                Item::Image {
                    image,
                    dest,
                    src,
                    opacity,
                    rendering,
                } => self.image(image, &self.snapped(&dest), &src, opacity, rendering)?,
                Item::Video {
                    image, dest, clip, ..
                } => {
                    let (dest, clip) = (self.snapped(&dest), self.snapped(&clip));
                    // The frame copied to memory, or black until there is one (a compositor draws
                    // the GPU frame over it).
                    let shown = dest.intersect(&clip);
                    match image {
                        Some(image) => {
                            let geometry: ID2D1Geometry = self
                                .res
                                .factory
                                .CreateRectangleGeometry(&rect_f(&clip))?
                                .cast()?;
                            self.push_mask(&geometry, None);
                            self.image(
                                image,
                                &dest,
                                &Rect::ZERO,
                                1.0,
                                crate::ImageRendering::Smooth,
                            )?;
                            if let Some(open) = self.stack.pop() {
                                self.close(open);
                            }
                        }
                        None => {
                            self.ctx
                                .FillRectangle(&rect_f(&shown), self.brush(Color::BLACK));
                        }
                    }
                }
                Item::ProtectedVideo { dest, clip, .. } => {
                    // (black: its frames are the compositor's to show, never drawn here)
                    let shown = self.snapped(&dest).intersect(&self.snapped(&clip));
                    self.ctx
                        .FillRectangle(&rect_f(&shown), self.brush(Color::BLACK));
                }
                Item::LinearGradient {
                    rect,
                    radii,
                    start,
                    end,
                    stops,
                    extend,
                } => {
                    // (the gradient is an image drawn at the snapped box's origin)
                    let (rect, d) = self.snapped_tile(&rect);
                    let (start, end) = (
                        Point::new(start.x + d.x, start.y + d.y),
                        Point::new(end.x + d.x, end.y + d.y),
                    );
                    let range = linear_range(&rect, start, end);
                    if let Some(c) = solid_color(list.stops(stops), extend, range) {
                        self.fill_rounded(&rect, &radii, c)?;
                        return Ok(());
                    }
                    if self.hard_stops(&rect, &radii, start, end, list.stops(stops), extend)? {
                        return Ok(());
                    }
                    let collection = self.stops(list.stops(stops), extend)?;
                    let brush = self.ctx.CreateLinearGradientBrush(
                        &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                            startPoint: v2(start),
                            endPoint: v2(end),
                        },
                        None,
                        &collection,
                    )?;
                    self.fill_with(&rect, &radii, &brush.cast()?)?;
                }
                Item::RadialGradient {
                    rect,
                    radii,
                    center,
                    radius,
                    stops,
                    extend,
                } => {
                    let (rect, d) = self.snapped_tile(&rect);
                    let center = Point::new(center.x + d.x, center.y + d.y);
                    let range = radial_range(&rect, center, radius);
                    if let Some(c) = solid_color(list.stops(stops), extend, range) {
                        self.fill_rounded(&rect, &radii, c)?;
                        return Ok(());
                    }
                    if self.hard_radial_stops(
                        &rect,
                        &radii,
                        center,
                        radius,
                        list.stops(stops),
                        extend,
                    )? {
                        return Ok(());
                    }
                    let collection = self.stops(list.stops(stops), extend)?;
                    let brush = self.ctx.CreateRadialGradientBrush(
                        &D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES {
                            center: v2(center),
                            gradientOriginOffset: Vector2 { X: 0.0, Y: 0.0 },
                            radiusX: radius.width,
                            radiusY: radius.height,
                        },
                        None,
                        &collection,
                    )?;
                    self.fill_with(&rect, &radii, &brush.cast()?)?;
                }
                Item::ConicGradient {
                    rect,
                    radii,
                    center,
                    angle,
                    stops,
                    extend,
                } => {
                    let (rect, d) = self.snapped_tile(&rect);
                    let center = Point::new(center.x + d.x, center.y + d.y);
                    let stops = list.stops(stops);
                    if let Some(c) = solid_color(stops, extend, None) {
                        self.fill_rounded(&rect, &radii, c)?;
                        return Ok(());
                    }
                    self.conic_gradient(&rect, &radii, center, angle, stops, extend)?;
                }
                Item::BoxShadow(ref s) => {
                    let s = BoxShadow {
                        rect: self.snapped(&s.rect),
                        ..*s
                    };
                    self.box_shadow(&s)?
                }
                Item::FillPattern {
                    rect,
                    pattern,
                    transform,
                } => {
                    if let Some(p) = list.patterns.get(pattern as usize) {
                        self.fill_pattern(p, rect, transform)?;
                    }
                }
                Item::FillPath {
                    path,
                    color,
                    evenodd,
                    antialias,
                    ..
                } => {
                    let geometry = self.path_geometry(list, path, evenodd)?;
                    if !antialias {
                        self.ctx.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED);
                    }
                    self.ctx.FillGeometry(&geometry, self.brush(color), None);
                    if !antialias {
                        self.ctx.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
                    }
                }
                Item::StrokePath {
                    path,
                    color,
                    ref stroke,
                    antialias,
                    ..
                } => {
                    let geometry = self.path_geometry(list, path, false)?;
                    let style = self.stroke_style(stroke, list.dashes(stroke.dashes))?;
                    if !antialias {
                        self.ctx.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED);
                    }
                    self.ctx.DrawGeometry(
                        &geometry,
                        self.brush(color),
                        stroke.width,
                        style.as_ref(),
                    );
                    if !antialias {
                        self.ctx.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
                    }
                }
                Item::PushClipRect { rect } => {
                    let rect = self.snapped(&rect);
                    if self.transform.is_axis_aligned() {
                        self.ctx
                            .PushAxisAlignedClip(&rect_f(&rect), D2D1_ANTIALIAS_MODE_ALIASED);
                        self.stack.push(Open::AxisClip);
                    } else {
                        // (an axis-aligned clip under a rotation would clip to its bounding box)
                        let geometry: ID2D1Geometry = self
                            .res
                            .factory
                            .CreateRectangleGeometry(&rect_f(&rect))?
                            .cast()?;
                        self.push_mask(&geometry, None);
                    }
                }
                Item::PushClipRoundedRect { rect, radii } => {
                    let geometry = self.rounded_geometry(&self.snapped(&rect), &radii)?;
                    self.push_mask(&geometry, None);
                }
                Item::PushClipPath { path, evenodd, .. } => {
                    // (a path that is a rectangle along the device's axes clips as a rectangle,
                    // antialiased where its edges fall inside pixels, without a layer: an SVG
                    // `clipPath` of one `rect` under a scaling `viewBox`, CSS Masking 1 §6)
                    if let Some(rect) = list
                        .path_rect(path)
                        .filter(|_| self.transform.is_axis_aligned())
                    {
                        self.ctx
                            .PushAxisAlignedClip(&rect_f(&rect), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
                        self.stack.push(Open::AxisClip);
                        return Ok(());
                    }
                    // (a path Direct2D refuses clips everything: its pop must close a group)
                    match self
                        .path_geometry(list, path, evenodd)
                        .and_then(|g| g.cast::<ID2D1Geometry>())
                    {
                        Ok(geometry) => self.push_mask(&geometry, None),
                        Err(_) => self.push_hidden(),
                    }
                }
                Item::PushTransform { matrix } => {
                    let previous = self.transform;
                    let snap = self.snap;
                    self.transform = matrix.then(&previous);
                    self.snap = snap
                        .filter(|s| s.keeps(&matrix))
                        .and_then(|_| Snap::of(&self.transform));
                    self.ctx.SetTransform(&matrix_3x2(&self.transform));
                    self.stack.push(Open::Transform(previous, snap));
                }
                // (a group at full opacity draws its content as it is: it is a backdrop root, which
                // `paint_items` marks, and nothing else)
                Item::PushOpacity { opacity, .. } if opacity >= 1.0 => {
                    self.stack.push(Open::Group);
                }
                Item::PushOpacity { opacity, bounds } => {
                    let params = D2D1_LAYER_PARAMETERS1 {
                        contentBounds: rect_f(&bounds),
                        geometricMask: ManuallyDrop::new(None),
                        maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                        maskTransform: Matrix3x2::identity(),
                        opacity,
                        opacityBrush: ManuallyDrop::new(None),
                        layerOptions: D2D1_LAYER_OPTIONS1_NONE,
                    };
                    self.ctx.PushLayer(&params, None);
                    self.layer_opened();
                }
                // (a layer is for a compositor; painted whole, the list ignores it)
                Item::PushLayer { .. } | Item::PopLayer => {}
                // (a mask's items are painted when its push is met, as a filter group's: see
                // `paint_items`)
                Item::PushMask { .. }
                | Item::MaskLayer { .. }
                | Item::MaskContent
                | Item::PushFilter { .. }
                | Item::PopFilter
                | Item::BackdropFilter { .. } => {}
                // (the drawings are added in a layer of their own, whose sum is composited over what
                // is below once)
                Item::PushPlus => {
                    let params = D2D1_LAYER_PARAMETERS1 {
                        contentBounds: rect_f(&Rect::infinite()),
                        geometricMask: ManuallyDrop::new(None),
                        maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                        maskTransform: Matrix3x2::identity(),
                        opacity: 1.0,
                        opacityBrush: ManuallyDrop::new(None),
                        layerOptions: D2D1_LAYER_OPTIONS1_NONE,
                    };
                    self.ctx.PushLayer(&params, None);
                    self.layer_opened();
                    let previous = self.ctx.GetPrimitiveBlend();
                    self.ctx.SetPrimitiveBlend(D2D1_PRIMITIVE_BLEND_ADD);
                    self.stack.push(Open::Plus(previous));
                }
                Item::PopPlus => {
                    for _ in 0..2 {
                        if let Some(open) = self.stack.pop() {
                            self.close(open);
                        }
                    }
                }
                Item::PopClip | Item::PopTransform | Item::PopOpacity | Item::PopMask => {
                    if let Some(open) = self.stack.pop() {
                        self.close(open);
                    }
                }
            }
        }
        Ok(())
    }

    /// Pushes a layer clipped to `mask` (local coordinates), optionally with an opacity.
    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread; Direct2D gets pointers to locals that outlive each call.
    unsafe fn push_mask(&mut self, mask: &ID2D1Geometry, opacity: Option<f32>) {
        self.push_mask_with(mask, opacity, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
    }

    /// [`Painter::push_mask`] with the mask's antialiasing mode (aliased: pixels whose centers are
    /// inside).
    // SAFETY contract: as `push_mask`.
    unsafe fn push_mask_with(
        &mut self,
        mask: &ID2D1Geometry,
        opacity: Option<f32>,
        antialias: D2D1_ANTIALIAS_MODE,
    ) {
        let params = D2D1_LAYER_PARAMETERS1 {
            contentBounds: rect_f(&Rect::infinite()),
            // SAFETY: a borrowed copy of the pointer without AddRef; ManuallyDrop never releases it,
            // and `mask` outlives the PushLayer call (Direct2D takes its own reference).
            geometricMask: std::mem::transmute_copy(mask),
            maskAntialiasMode: antialias,
            maskTransform: Matrix3x2::identity(),
            opacity: opacity.unwrap_or(1.0),
            opacityBrush: ManuallyDrop::new(None),
            layerOptions: D2D1_LAYER_OPTIONS1_NONE,
        };
        self.ctx.PushLayer(&params, None);
        self.layer_opened();
    }

    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread; Direct2D gets pointers to locals that outlive each call.
    unsafe fn rounded_path(&self, rect: &Rect, radii: &Radii) -> Result<ID2D1PathGeometry> {
        let path = self.res.factory.CreatePathGeometry()?;
        let sink = path.Open()?;
        add_rounded(&sink, rect, &radii.fitted(rect));
        sink.Close()?;
        Ok(path)
    }

    /// A display list path as a Direct2D geometry (the list is validated: the range's verbs take
    /// exactly its points). A segment with no open figure starts one at the current point; after a
    /// close, the current point is the closed figure's start (SVG 2 9.3.4).
    // SAFETY contract: as `rounded_path`.
    unsafe fn path_geometry(
        &self,
        list: &DisplayList,
        path: PathRange,
        evenodd: bool,
    ) -> Result<ID2D1PathGeometry> {
        let geometry = self.res.factory.CreatePathGeometry()?;
        let sink = geometry.Open()?;
        sink.SetFillMode(if evenodd {
            D2D1_FILL_MODE_ALTERNATE
        } else {
            D2D1_FILL_MODE_WINDING
        });
        let (verbs, points) = list.path(path);
        let mut points = points.iter().map(|p| v2(*p));
        let mut next = || points.next().unwrap_or(Vector2 { X: 0.0, Y: 0.0 });
        let origin = Vector2 { X: 0.0, Y: 0.0 };
        let (mut open, mut start, mut current) = (false, origin, origin);
        for &verb in verbs {
            let verb = PathVerb::from_u8(verb).unwrap_or(PathVerb::Close);
            if !open && !matches!(verb, PathVerb::Move | PathVerb::Close) {
                sink.BeginFigure(current, D2D1_FIGURE_BEGIN_FILLED);
                open = true;
                start = current;
            }
            match verb {
                PathVerb::Move => {
                    if open {
                        sink.EndFigure(D2D1_FIGURE_END_OPEN);
                    }
                    current = next();
                    start = current;
                    sink.BeginFigure(current, D2D1_FIGURE_BEGIN_FILLED);
                    open = true;
                }
                PathVerb::Line => {
                    current = next();
                    sink.AddLine(current);
                }
                PathVerb::Quad => {
                    let (a, b) = (next(), next());
                    sink.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT {
                        point1: a,
                        point2: b,
                    });
                    current = b;
                }
                PathVerb::Cubic => {
                    let (a, b, c) = (next(), next(), next());
                    sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                        point1: a,
                        point2: b,
                        point3: c,
                    });
                    current = c;
                }
                PathVerb::Close => {
                    if open {
                        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                        open = false;
                    }
                    current = start;
                }
            }
        }
        if open {
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
        }
        sink.Close()?;
        Ok(geometry)
    }

    /// The Direct2D stroke style of a display list stroke. Direct2D measures dashes and their
    /// offset in stroke widths.
    // SAFETY contract: as `rounded_path`.
    unsafe fn stroke_style(
        &self,
        stroke: &Stroke,
        dashes: &[f32],
    ) -> Result<Option<ID2D1StrokeStyle>> {
        let cap = match stroke.cap {
            LineCap::Butt => D2D1_CAP_STYLE_FLAT,
            LineCap::Round => D2D1_CAP_STYLE_ROUND,
            LineCap::Square => D2D1_CAP_STYLE_SQUARE,
        };
        let join = match stroke.join {
            LineJoin::Miter => D2D1_LINE_JOIN_MITER_OR_BEVEL,
            LineJoin::Round => D2D1_LINE_JOIN_ROUND,
            LineJoin::Bevel => D2D1_LINE_JOIN_BEVEL,
        };
        let w = stroke.width.max(f32::MIN_POSITIVE);
        let scaled: smallvec::SmallVec<[f32; 8]> = dashes.iter().map(|d| d / w).collect();
        let props = D2D1_STROKE_STYLE_PROPERTIES {
            startCap: cap,
            endCap: cap,
            dashCap: cap,
            lineJoin: join,
            miterLimit: stroke.miter_limit.max(1.0),
            dashStyle: if scaled.is_empty() {
                D2D1_DASH_STYLE_SOLID
            } else {
                D2D1_DASH_STYLE_CUSTOM
            },
            dashOffset: stroke.dash_offset / w,
        };
        let dashes = (!scaled.is_empty()).then_some(&scaled[..]);
        Ok(Some(self.res.factory.CreateStrokeStyle(&props, dashes)?))
    }

    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread; Direct2D gets pointers to locals that outlive each call.
    unsafe fn rounded_geometry(&self, rect: &Rect, radii: &Radii) -> Result<ID2D1Geometry> {
        let radii = radii.fitted(rect);
        if radii.is_zero() {
            self.res
                .factory
                .CreateRectangleGeometry(&rect_f(rect))?
                .cast()
        } else if radii.is_uniform() {
            self.res
                .factory
                .CreateRoundedRectangleGeometry(&rounded(rect, radii.top_left))?
                .cast()
        } else {
            self.rounded_path(rect, &radii)?.cast()
        }
    }

    /// A ring between two rounded shapes (outer minus inner).
    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread; Direct2D gets pointers to locals that outlive each call.
    unsafe fn ring(
        &self,
        outer: &Rect,
        outer_r: &Radii,
        inner: &Rect,
        inner_r: &Radii,
    ) -> Result<ID2D1PathGeometry> {
        let path = self.res.factory.CreatePathGeometry()?;
        let sink = path.Open()?;
        sink.SetFillMode(D2D1_FILL_MODE_ALTERNATE);
        add_rounded(&sink, outer, &outer_r.fitted(outer));
        if !inner.is_empty() {
            add_rounded(&sink, inner, &inner_r.fitted(inner));
        }
        sink.Close()?;
        Ok(path)
    }

    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread; Direct2D gets pointers to locals that outlive each call.
    unsafe fn fill_rounded(&self, rect: &Rect, radii: &Radii, color: Color) -> Result<()> {
        let radii = radii.fitted(rect);
        if radii.is_zero() {
            self.ctx.FillRectangle(&rect_f(rect), self.brush(color));
        } else if radii.is_uniform() {
            self.ctx
                .FillRoundedRectangle(&rounded(rect, radii.top_left), self.brush(color));
        } else {
            let path = self.rounded_path(rect, &radii)?;
            self.ctx.FillGeometry(&path, self.brush(color), None);
        }
        Ok(())
    }

    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread; Direct2D gets pointers to locals that outlive each call.
    unsafe fn fill_with(&self, rect: &Rect, radii: &Radii, brush: &ID2D1Brush) -> Result<()> {
        let radii = radii.fitted(rect);
        if radii.is_zero() {
            self.ctx.FillRectangle(&rect_f(rect), brush);
        } else if radii.is_uniform() {
            self.ctx
                .FillRoundedRectangle(&rounded(rect, radii.top_left), brush);
        } else {
            let path = self.rounded_path(rect, &radii)?;
            self.ctx.FillGeometry(&path, brush, None);
        }
        Ok(())
    }

    /// A linear gradient with hard color stops (two stops at one position: CSS Images 3 §3.5.1),
    /// painted as Chrome evaluates it per pixel center: each run of stops between hard ones is its
    /// own gradient, in the band of the gradient line it covers, with aliased band edges, so no
    /// row or column blends the colors on either side. (Direct2D samples a gradient through a
    /// texture, which would blend them.) False, painting nothing, when there are no hard stops
    /// or too many bands.
    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread.
    unsafe fn hard_stops(
        &mut self,
        rect: &Rect,
        radii: &Radii,
        start: Point,
        end: Point,
        stops: &[GradientStop],
        extend: Extend,
    ) -> Result<bool> {
        const MAX_BANDS: usize = 512;
        let hard = hard_stop_indices(stops);
        let (dx, dy) = (end.x - start.x, end.y - start.y);
        let len2 = dx * dx + dy * dy;
        if hard.is_empty() || len2.is_nan() || len2 <= 1e-6 || rect.is_empty() {
            return Ok(false);
        }
        // the gradient line's parameter over the rectangle
        let t = |x: f32, y: f32| ((x - start.x) * dx + (y - start.y) * dy) / len2;
        let corners = [
            t(rect.x, rect.y),
            t(rect.right(), rect.y),
            t(rect.x, rect.bottom()),
            t(rect.right(), rect.bottom()),
        ];
        let tmin = corners.iter().copied().fold(f32::INFINITY, f32::min);
        let tmax = corners.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let segments = hard_segments(stops, &hard, extend);
        let repeat = extend == Extend::Repeat;
        let periods = if repeat {
            let (k0, k1) = (tmin.floor(), tmax.floor());
            if (k1 - k0).is_nan() || k1 - k0 >= MAX_BANDS as f32 {
                return Ok(false);
            }
            (k0 as i64, k1 as i64)
        } else {
            (0, 0)
        };
        if ((periods.1 - periods.0 + 1) as usize) * segments.len() > MAX_BANDS {
            return Ok(false);
        }
        let axis = (dx == 0.0 || dy == 0.0) && self.transform.is_axis_aligned();
        let depth = self.stack.len();
        self.push_shape_clip(rect, radii)?;
        // One brush per run of stops, moved to each period; each band is filled with aliased
        // edges (no layer per band).
        let mut brushes = Vec::with_capacity(segments.len());
        for &(a, b, _, _) in &segments {
            let collection = self.stops(&stops[a..=b], Extend::Clamp)?;
            brushes.push(self.ctx.CreateLinearGradientBrush(
                &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                    startPoint: v2(start),
                    endPoint: v2(end),
                },
                None,
                &collection,
            )?);
        }
        self.ctx.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED);
        let painted = (|| -> Result<()> {
            for k in periods.0..=periods.1 {
                let shift = k as f32;
                for (j, &(_, _, from, to)) in segments.iter().enumerate() {
                    let (lo, hi) = ((from + shift).max(tmin), (to + shift).min(tmax));
                    if hi.is_nan() || lo.is_nan() || hi <= lo {
                        continue;
                    }
                    let brush = &brushes[j];
                    brush.SetTransform(&matrix_3x2(&Matrix::translation(dx * shift, dy * shift)));
                    // the band between the gradient line's points at lo and hi, across the rectangle
                    let at = |v: f32| Point::new(start.x + dx * v, start.y + dy * v);
                    let (p, q) = (at(lo), at(hi));
                    if axis {
                        let band = if dy == 0.0 {
                            Rect::from_ltrb(p.x.min(q.x), rect.y, p.x.max(q.x), rect.bottom())
                        } else {
                            Rect::from_ltrb(rect.x, p.y.min(q.y), rect.right(), p.y.max(q.y))
                        };
                        let band = band.intersect(rect);
                        if !band.is_empty() {
                            self.ctx.FillRectangle(&rect_f(&band), brush);
                        }
                    } else {
                        let len = len2.sqrt();
                        let reach = (rect.width + rect.height) * 2.0 / len;
                        let (nx, ny) = (-dy * reach, dx * reach);
                        let band = self.polygon(&[
                            (p.x + nx, p.y + ny),
                            (q.x + nx, q.y + ny),
                            (q.x - nx, q.y - ny),
                            (p.x - nx, p.y - ny),
                        ])?;
                        self.ctx.FillGeometry(&band, brush, None);
                    }
                }
            }
            Ok(())
        })();
        self.ctx.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        while self.stack.len() > depth {
            if let Some(open) = self.stack.pop() {
                self.close(open);
            }
        }
        painted.map(|_| true)
    }

    /// Clips what follows to a rounded rectangle: an aliased axis-aligned clip when it has no
    /// rounding (cheap), else a layer.
    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread.
    unsafe fn push_shape_clip(&mut self, rect: &Rect, radii: &Radii) -> Result<()> {
        if radii.fitted(rect).is_zero() && self.transform.is_axis_aligned() {
            self.ctx
                .PushAxisAlignedClip(&rect_f(rect), D2D1_ANTIALIAS_MODE_ALIASED);
            self.stack.push(Open::AxisClip);
        } else {
            let shape = self.rounded_geometry(rect, radii)?;
            self.push_mask(&shape, None);
        }
        Ok(())
    }

    /// [`Painter::hard_stops`] for a radial gradient: each run of stops between hard ones fills
    /// the ring of the ending shape it covers, with aliased edges.
    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread.
    #[allow(clippy::too_many_arguments)]
    unsafe fn hard_radial_stops(
        &mut self,
        rect: &Rect,
        radii: &Radii,
        center: Point,
        radius: Size,
        stops: &[GradientStop],
        extend: Extend,
    ) -> Result<bool> {
        const MAX_BANDS: usize = 256;
        let hard = hard_stop_indices(stops);
        if hard.is_empty() || rect.is_empty() {
            return Ok(false);
        }
        let Some((tmin, tmax)) = radial_range(rect, center, radius) else {
            return Ok(false);
        };
        let (tmin, tmax) = (tmin as f32, tmax as f32);
        let segments = hard_segments(stops, &hard, extend);
        let repeat = extend == Extend::Repeat;
        let periods = if repeat {
            let (k0, k1) = (tmin.floor(), tmax.floor());
            if (k1 - k0).is_nan() || k1 - k0 >= MAX_BANDS as f32 {
                return Ok(false);
            }
            (k0 as i64, k1 as i64)
        } else {
            (0, 0)
        };
        if ((periods.1 - periods.0 + 1) as usize) * segments.len() > MAX_BANDS {
            return Ok(false);
        }
        let depth = self.stack.len();
        self.push_shape_clip(rect, radii)?;
        self.ctx.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED);
        let outside = rect.inflate(1.0, 1.0);
        let painted = (|| -> Result<()> {
            for k in periods.0..=periods.1 {
                let shift = k as f32;
                for &(a, b, from, to) in &segments {
                    let (lo, hi) = ((from + shift).max(tmin).max(0.0), (to + shift).min(tmax));
                    if hi.is_nan() || lo.is_nan() || hi <= lo {
                        continue;
                    }
                    // the ring between the ending shape scaled by lo and by hi (or the rectangle's
                    // edge when the band reaches past it)
                    let path = self.res.factory.CreatePathGeometry()?;
                    let sink = path.Open()?;
                    sink.SetFillMode(D2D1_FILL_MODE_ALTERNATE);
                    let ellipse = |f: f32| {
                        Rect::new(
                            center.x - radius.width * f,
                            center.y - radius.height * f,
                            2.0 * radius.width * f,
                            2.0 * radius.height * f,
                        )
                    };
                    let ellipse_radii = |f: f32| {
                        let c = Size::new(radius.width * f, radius.height * f);
                        Radii {
                            top_left: c,
                            top_right: c,
                            bottom_right: c,
                            bottom_left: c,
                        }
                    };
                    if hi >= tmax {
                        add_rounded(&sink, &outside, &Radii::ZERO);
                    } else {
                        add_rounded(&sink, &ellipse(hi), &ellipse_radii(hi));
                    }
                    if lo > 0.0 {
                        add_rounded(&sink, &ellipse(lo), &ellipse_radii(lo));
                    }
                    sink.Close()?;
                    // (period k: the brush covers 0..k + 1 of the ray, its stops moved to k)
                    let span = shift + 1.0;
                    let moved: Vec<GradientStop> = stops[a..=b]
                        .iter()
                        .map(|s| GradientStop {
                            offset: (s.offset + shift) / span,
                            color: s.color,
                        })
                        .collect();
                    let collection = self.stops(&moved, Extend::Clamp)?;
                    let brush = self.ctx.CreateRadialGradientBrush(
                        &D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES {
                            center: v2(center),
                            gradientOriginOffset: Vector2 { X: 0.0, Y: 0.0 },
                            radiusX: radius.width * span,
                            radiusY: radius.height * span,
                        },
                        None,
                        &collection,
                    )?;
                    self.ctx.FillGeometry(&path, &brush, None);
                }
            }
            Ok(())
        })();
        self.ctx.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        while self.stack.len() > depth {
            if let Some(open) = self.stack.pop() {
                self.close(open);
            }
        }
        painted.map(|_| true)
    }

    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread; Direct2D gets pointers to locals that outlive each call.
    unsafe fn stops(
        &mut self,
        stops: &[GradientStop],
        extend: Extend,
    ) -> Result<ID2D1GradientStopCollection1> {
        // (a small fixed buffer: gradients rarely have many stops)
        let mut converted: smallvec_stops::Stops = Default::default();
        let mut hash = matches!(extend, Extend::Repeat) as u64;
        for s in stops {
            let stop = D2D1_GRADIENT_STOP {
                position: s.offset,
                color: color_f(s.color),
            };
            for v in [
                stop.position,
                stop.color.r,
                stop.color.g,
                stop.color.b,
                stop.color.a,
            ] {
                hash = crate::display_list::hash_combine(hash, u64::from(v.to_bits()));
            }
            converted.push(stop);
        }
        let same = |e: &StopsEntry| {
            e.hash == hash
                && e.extend == extend
                && e.stops.len() == converted.as_slice().len()
                && e.stops.iter().zip(converted.as_slice()).all(|(a, b)| {
                    a.position.to_bits() == b.position.to_bits()
                        && [a.color.r, a.color.g, a.color.b, a.color.a]
                            .iter()
                            .zip([b.color.r, b.color.g, b.color.b, b.color.a])
                            .all(|(x, y)| x.to_bits() == y.to_bits())
                })
        };
        if let Some(e) = self.res.gradients.iter().find(|e| same(e)) {
            return Ok(e.collection.clone());
        }
        let collection = self.make_stops(converted.as_slice(), extend)?;
        self.res.stops_made += 1;
        if self.res.gradients.len() >= GRADIENTS_KEPT {
            self.res.gradients.remove(0);
        }
        self.res.gradients.push(StopsEntry {
            hash,
            stops: converted.as_slice().to_vec(),
            extend,
            collection: collection.clone(),
        });
        Ok(collection)
    }

    // SAFETY contract: as `stops`.
    unsafe fn make_stops(
        &self,
        converted: &[D2D1_GRADIENT_STOP],
        extend: Extend,
    ) -> Result<ID2D1GradientStopCollection1> {
        self.ctx.CreateGradientStopCollection(
            converted,
            D2D1_COLOR_SPACE_SRGB,
            D2D1_COLOR_SPACE_SRGB,
            D2D1_BUFFER_PRECISION_8BPC_UNORM,
            match extend {
                Extend::Clamp => D2D1_EXTEND_MODE_CLAMP,
                Extend::Repeat => D2D1_EXTEND_MODE_WRAP,
            },
            // CSS interpolates gradients in premultiplied space
            D2D1_COLOR_INTERPOLATION_MODE_PREMULTIPLIED,
        )
    }

    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread; Direct2D gets pointers to locals that outlive each call.
    unsafe fn border(&mut self, b: &Border) -> Result<()> {
        let outer = b.rect;
        let outer_r = b.radii.fitted(&outer);
        let inner = outer.deflate(&b.widths);
        let inner_r = outer_r.inner(&b.widths);
        let widths = b.widths.to_array();
        let visible = |i: usize| {
            !b.styles[i].is_invisible() && !b.colors[i].is_transparent() && widths[i] > 0.0
        };
        let uniform = (1..4).all(|i| b.colors[i] == b.colors[0] && b.styles[i] == b.styles[0])
            && (0..4).all(visible);
        if uniform && b.styles[0] == BorderStyle::Solid {
            if outer_r.is_zero() {
                // four rectangles: no geometry to build
                let brush = self.brush(b.colors[0]);
                let w = &b.widths;
                for r in [
                    Rect::new(outer.x, outer.y, outer.width, w.top),
                    Rect::new(outer.x, outer.bottom() - w.bottom, outer.width, w.bottom),
                    Rect::new(
                        outer.x,
                        outer.y + w.top,
                        w.left,
                        outer.height - w.top - w.bottom,
                    ),
                    Rect::new(
                        outer.right() - w.right,
                        outer.y + w.top,
                        w.right,
                        outer.height - w.top - w.bottom,
                    ),
                ] {
                    if !r.is_empty() {
                        self.ctx.FillRectangle(&rect_f(&r), brush);
                    }
                }
            } else {
                let ring = self.ring(&outer, &outer_r, &inner, &inner_r)?;
                self.ctx.FillGeometry(&ring, self.brush(b.colors[0]), None);
            }
            return Ok(());
        }
        // Each side inside its region: from the outer corners through the inner corners to the middle
        // of the box. (Stopping at the inner corners would cut off rounded corners, which reach into
        // the padding box when radii exceed the border widths.)
        let (ol, ot, or, ob) = (outer.x, outer.y, outer.right(), outer.bottom());
        let (il, it, ir, ib) = (inner.x, inner.y, inner.right(), inner.bottom());
        // (with square corners the regions end at the inner corners: through the middle, a side
        // next to a zero-width one would lose the part of its band beyond the middle)
        let mid = (outer.x + outer.width / 2.0, outer.y + outer.height / 2.0);
        let square = outer_r.is_zero();
        let traps = [
            [(ol, ot), (or, ot), (ir, it), mid, (il, it)],
            [(or, ot), (or, ob), (ir, ib), mid, (ir, it)],
            [(or, ob), (ol, ob), (il, ib), mid, (ir, ib)],
            [(ol, ob), (ol, ot), (il, it), mid, (il, ib)],
        ];
        for (side, region) in traps.iter().enumerate() {
            if !visible(side) {
                continue;
            }
            let quad = [region[0], region[1], region[2], region[4]];
            let trap = self.polygon(if square { &quad } else { region })?;
            self.push_mask(&trap.cast()?, None);
            let r = self.border_side(b, side, &outer, &outer_r, &inner, &inner_r);
            if let Some(open) = self.stack.pop() {
                self.close(open);
            }
            r?;
        }
        Ok(())
    }

    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread; Direct2D gets pointers to locals that outlive each call.
    unsafe fn polygon(&self, points: &[(f32, f32)]) -> Result<ID2D1PathGeometry> {
        let path = self.res.factory.CreatePathGeometry()?;
        let sink = path.Open()?;
        sink.BeginFigure(
            Vector2 {
                X: points[0].0,
                Y: points[0].1,
            },
            D2D1_FIGURE_BEGIN_FILLED,
        );
        for p in &points[1..] {
            sink.AddLine(Vector2 { X: p.0, Y: p.1 });
        }
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        sink.Close()?;
        Ok(path)
    }

    /// One side of a border, drawn over the whole ring; the caller clips it to the side.
    #[allow(clippy::too_many_arguments)]
    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread; Direct2D gets pointers to locals that outlive each call.
    unsafe fn border_side(
        &mut self,
        b: &Border,
        side: usize,
        outer: &Rect,
        outer_r: &Radii,
        inner: &Rect,
        inner_r: &Radii,
    ) -> Result<()> {
        let color = b.colors[side];
        let w = &b.widths;
        let scaled = |f: f32| Sides::new(w.top * f, w.right * f, w.bottom * f, w.left * f);
        // the ring between fractions a and b of the border width (0 = outer edge, 1 = inner edge)
        let band = |p: &Self, a: f32, bb: f32, c: Color| -> Result<()> {
            let o = outer.deflate(&scaled(a));
            let i = outer.deflate(&scaled(bb));
            let ring = p.ring(
                &o,
                &outer_r.inner(&scaled(a)),
                &i,
                &outer_r.inner(&scaled(bb)),
            )?;
            p.ctx.FillGeometry(&ring, p.brush(c), None);
            Ok(())
        };
        // top and left are the "light source" sides for inset/outset/groove/ridge
        let top_left = side == 0 || side == 3;
        let (dark, light) = (color.shade(0.6), color);
        match b.styles[side] {
            BorderStyle::None | BorderStyle::Hidden => {}
            BorderStyle::Solid => {
                let ring = self.ring(outer, outer_r, inner, inner_r)?;
                self.ctx.FillGeometry(&ring, self.brush(color), None);
            }
            BorderStyle::Double => {
                band(self, 0.0, 1.0 / 3.0, color)?;
                band(self, 2.0 / 3.0, 1.0, color)?;
            }
            BorderStyle::Inset | BorderStyle::Outset => {
                let inset = b.styles[side] == BorderStyle::Inset;
                band(self, 0.0, 1.0, if inset == top_left { dark } else { light })?;
            }
            BorderStyle::Groove | BorderStyle::Ridge => {
                let groove = b.styles[side] == BorderStyle::Groove;
                let (first, second) = if groove == top_left {
                    (dark, light)
                } else {
                    (light, dark)
                };
                band(self, 0.0, 0.5, first)?;
                band(self, 0.5, 1.0, second)?;
            }
            BorderStyle::Dashed | BorderStyle::Dotted if outer_r.is_zero() => {
                self.dashes(b.styles[side] == BorderStyle::Dotted, side, outer, w, color);
            }
            BorderStyle::Dashed | BorderStyle::Dotted => {
                // stroked along the middle of the border, as wide as this side
                let mid = outer.deflate(&scaled(0.5));
                let mid_r = outer_r.inner(&scaled(0.5));
                let width = w.to_array()[side];
                let style = if b.styles[side] == BorderStyle::Dashed {
                    &self.res.dashed
                } else {
                    &self.res.dotted
                };
                let path = self.rounded_path(&mid, &mid_r)?;
                self.ctx
                    .DrawGeometry(&path, self.brush(color), width, style);
            }
        }
        Ok(())
    }

    /// A straight dashed or dotted side (`side` of `outer`, widths `w`) as Chrome draws it: dashes
    /// along the side's middle line from one outer corner to the other, starting and ending with a
    /// dash, the gaps stretched or shrunk to fit ([`dash_gap`]). Dashes are twice the width long
    /// with gaps of one width (three and two below 3 px); dots are one width long with gaps of
    /// one width, drawn round from 3 px up and square below (CSS Backgrounds 3 §4.2 leaves the
    /// exact pattern to the UA).
    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread.
    unsafe fn dashes(&self, dotted: bool, side: usize, outer: &Rect, w: &Sides, color: Color) {
        let width = w.to_array()[side];
        if width <= 0.0 {
            return;
        }
        let horizontal = side == 0 || side == 2;
        let length = if horizontal {
            outer.width
        } else {
            outer.height
        };
        let (dash, gap) = match (dotted, width >= 3.0) {
            (true, _) => (width, width),
            (false, true) => (2.0 * width, width),
            (false, false) => (3.0 * width, 2.0 * width),
        };
        let Some((count, gap)) = dash_gap(length, dash, gap) else {
            return;
        };
        // where the line runs: the side's middle, across the box
        let (x0, y0) = match side {
            0 => (outer.x, outer.y),
            1 => (outer.right() - width, outer.y),
            2 => (outer.x, outer.bottom() - width),
            _ => (outer.x, outer.y),
        };
        let brush = self.brush(color);
        let round = dotted && width >= 3.0;
        for i in 0..count {
            let along = i as f32 * (dash + gap);
            let r = if horizontal {
                Rect::new(x0 + along, y0, dash.min(length - along), width)
            } else {
                Rect::new(x0, y0 + along, width, dash.min(length - along))
            };
            if round {
                self.ctx.FillEllipse(
                    &D2D1_ELLIPSE {
                        point: Vector2 {
                            X: r.x + r.width / 2.0,
                            Y: r.y + r.height / 2.0,
                        },
                        radiusX: width / 2.0,
                        radiusY: width / 2.0,
                    },
                    brush,
                );
            } else {
                self.ctx.FillRectangle(&rect_f(&r), brush);
            }
        }
    }

    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread; Direct2D gets pointers to locals that outlive each call.
    unsafe fn text(&mut self, list: &DisplayList, run: &TextRun) -> Result<()> {
        let (face, color_font) = self
            .sources
            .text
            .face(run.font)
            .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
        let (ids, advances, offsets) = list.glyphs(run.glyphs);
        let width: f32 = advances.iter().sum();
        // Right-to-left runs are drawn leftwards from their right end.
        let x = if run.bidi_level % 2 == 1 {
            run.origin.x + width
        } else {
            run.origin.x
        };
        // Glyphs keep their subpixel position along the line; the baseline is on a device pixel
        // row, as Chrome's text is (Skia positions horizontal text subpixel in x only).
        let origin = Vector2 {
            X: x,
            Y: self.snap.map_or(run.origin.y, |s| s.y(run.origin.y)),
        };
        let glyph_run = DWRITE_GLYPH_RUN {
            // SAFETY: a borrowed copy of the face pointer, never released (ManuallyDrop); `face`
            // outlives every use of `glyph_run`.
            fontFace: std::mem::transmute_copy(&face),
            fontEmSize: run.size,
            glyphCount: ids.len() as u32,
            glyphIndices: ids.as_ptr(),
            glyphAdvances: advances.as_ptr(),
            // (GlyphOffset has DWRITE_GLYPH_OFFSET's layout)
            glyphOffsets: offsets.as_ptr().cast(),
            isSideways: run.sideways.into(),
            bidiLevel: run.bidi_level as u32,
        };
        if run.stroke > 0.0 {
            // (the outline of the glyphs as DirectWrite places them, from the run's origin: a
            // right-to-left run's from its right end)
            let path = self.res.factory.CreatePathGeometry()?;
            let sink = path.Open()?;
            face.GetGlyphRunOutline(
                run.size,
                ids.as_ptr(),
                Some(advances.as_ptr()),
                Some(offsets.as_ptr().cast()),
                ids.len() as u32,
                run.sideways,
                run.bidi_level % 2 == 1,
                &sink,
            )?;
            sink.Close()?;
            let outer = self.transform;
            self.ctx.SetTransform(&matrix_3x2(
                &Matrix::translation(origin.X, origin.Y).then(&outer),
            ));
            let brush = self.brush(run.color).clone();
            self.ctx.DrawGeometry(&path, &brush, run.stroke, None);
            self.ctx.SetTransform(&matrix_3x2(&outer));
            return Ok(());
        }
        if color_font && self.color_glyphs(&glyph_run, origin, run.color)? {
            return Ok(());
        }
        self.ctx.DrawGlyphRun(
            origin,
            &glyph_run,
            None,
            self.brush(run.color),
            DWRITE_MEASURING_MODE_NATURAL,
        );
        Ok(())
    }

    /// Draws a run of a color font layer by layer. False when the glyphs have no color layers.
    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread; Direct2D gets pointers to locals that outlive each call.
    unsafe fn color_glyphs(
        &mut self,
        run: &DWRITE_GLYPH_RUN,
        origin: Vector2,
        text: Color,
    ) -> Result<bool> {
        let mut formats = DWRITE_GLYPH_IMAGE_FORMATS_TRUETYPE.0
            | DWRITE_GLYPH_IMAGE_FORMATS_CFF.0
            | DWRITE_GLYPH_IMAGE_FORMATS_COLR.0;
        if self.ctx4.is_some() {
            formats |= DWRITE_GLYPH_IMAGE_FORMATS_SVG.0
                | DWRITE_GLYPH_IMAGE_FORMATS_PNG.0
                | DWRITE_GLYPH_IMAGE_FORMATS_JPEG.0
                | DWRITE_GLYPH_IMAGE_FORMATS_TIFF.0
                | DWRITE_GLYPH_IMAGE_FORMATS_PREMULTIPLIED_B8G8R8A8.0;
        }
        let transform = self.transform;
        let world = DWRITE_MATRIX {
            m11: transform.m11,
            m12: transform.m12,
            m21: transform.m21,
            m22: transform.m22,
            dx: transform.dx,
            dy: transform.dy,
        };
        let layers = match self.res.dwrite.TranslateColorGlyphRun(
            origin,
            run,
            None,
            DWRITE_GLYPH_IMAGE_FORMATS(formats),
            DWRITE_MEASURING_MODE_NATURAL,
            Some(&world),
            0,
        ) {
            Ok(layers) => layers,
            // DWRITE_E_NOCOLOR: these glyphs have no color version
            Err(_) => return Ok(false),
        };
        let alpha = text.a as f32 / 255.0;
        while layers.MoveNext()?.as_bool() {
            let layer = &*layers.GetCurrentRun()?;
            let base = &layer.Base;
            let at = Vector2 {
                X: base.baselineOriginX,
                Y: base.baselineOriginY,
            };
            // (a layer's format can carry the outline flag too: COLR layers come as TRUETYPE | COLR)
            let format = layer.glyphImageFormat;
            let bitmap_formats = DWRITE_GLYPH_IMAGE_FORMATS_PNG.0
                | DWRITE_GLYPH_IMAGE_FORMATS_JPEG.0
                | DWRITE_GLYPH_IMAGE_FORMATS_TIFF.0
                | DWRITE_GLYPH_IMAGE_FORMATS_PREMULTIPLIED_B8G8R8A8.0;
            let ctx4 = self.ctx4.as_ref();
            match ctx4 {
                Some(ctx4) if format.0 & bitmap_formats != 0 => {
                    ctx4.DrawColorBitmapGlyphRun(
                        DWRITE_GLYPH_IMAGE_FORMATS(format.0 & bitmap_formats),
                        at,
                        &base.glyphRun,
                        layer.measuringMode,
                        D2D1_COLOR_BITMAP_GLYPH_SNAP_OPTION_DEFAULT,
                    );
                }
                Some(ctx4) if format.0 & DWRITE_GLYPH_IMAGE_FORMATS_SVG.0 != 0 => {
                    self.res.brush.SetColor(&color_f(text));
                    ctx4.DrawSvgGlyphRun(
                        at,
                        &base.glyphRun,
                        &self.res.brush,
                        None,
                        0,
                        layer.measuringMode,
                    );
                }
                _ => {
                    // An outline layer: in its palette color, or the text color for palette index
                    // 0xFFFF and for the plain (non-color) glyphs of the run.
                    let colr = format.0 & DWRITE_GLYPH_IMAGE_FORMATS_COLR.0 != 0;
                    let color = if !colr || base.paletteIndex == 0xFFFF {
                        color_f(text)
                    } else {
                        let c = base.runColor;
                        D2D1_COLOR_F {
                            r: c.r,
                            g: c.g,
                            b: c.b,
                            a: c.a * alpha,
                        }
                    };
                    self.res.brush.SetColor(&color);
                    self.ctx.DrawGlyphRun(
                        at,
                        &base.glyphRun,
                        None,
                        &self.res.brush,
                        layer.measuringMode,
                    );
                }
            }
        }
        Ok(true)
    }

    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread; Direct2D gets pointers to locals that outlive each call.
    unsafe fn image(
        &mut self,
        id: ImageId,
        dest: &Rect,
        src: &Rect,
        opacity: f32,
        rendering: ImageRendering,
    ) -> Result<()> {
        let (bitmap, shown) = uploaded(self.ctx, self.res, self.sources.images, id)?;
        let bitmap = &bitmap;
        let source = if src.is_empty() {
            Rect::new(0.0, 0.0, shown.width as f32, shown.height as f32)
        } else {
            *src
        };
        // Pixelated when asked; plain bilinear when drawn at its own size (stays crisp); otherwise the
        // high-quality cubic filter (which prefilters when shrinking).
        let device = self.transform.apply_rect(dest);
        let exact = self.transform.is_axis_aligned()
            && (device.width - source.width).abs() < 0.5
            && (device.height - source.height).abs() < 0.5;
        let mode = match rendering {
            ImageRendering::Pixelated => D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            _ if exact => D2D1_INTERPOLATION_MODE_LINEAR,
            _ => D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
        };
        // (shrinking, the cubic filter's prefilter matters more than the edge; enlarging, the
        // edge's last half pixel would show)
        let enlarged = device.width > source.width && device.height > source.height;
        if exact || !enlarged {
            self.ctx.DrawBitmap(
                bitmap,
                Some(&rect_f(dest)),
                opacity,
                mode,
                Some(&rect_f(&source)),
                None,
            );
            return Ok(());
        }
        // Enlarged: through a brush whose edges clamp, so the image's edge pixels stay solid to
        // the destination's edges (as Skia samples an image clamped to its bounds); an enlarged
        // bitmap drawn directly fades out over its last half source pixel.
        if !(source.width > 0.0 && source.height > 0.0) {
            return Ok(());
        }
        let (sx, sy) = (dest.width / source.width, dest.height / source.height);
        let brush = self.ctx.CreateBitmapBrush(
            bitmap,
            Some(&D2D1_BITMAP_BRUSH_PROPERTIES1 {
                extendModeX: D2D1_EXTEND_MODE_CLAMP,
                extendModeY: D2D1_EXTEND_MODE_CLAMP,
                interpolationMode: mode,
            }),
            Some(&D2D1_BRUSH_PROPERTIES {
                opacity,
                transform: matrix_3x2(&Matrix::new(
                    sx,
                    0.0,
                    0.0,
                    sy,
                    dest.x - source.x * sx,
                    dest.y - source.y * sy,
                )),
            }),
        )?;
        self.ctx.FillRectangle(&rect_f(dest), &brush);
        Ok(())
    }

    /// A conic gradient, which Direct2D has no brush for: its colors are computed per device pixel
    /// center over the part of the rectangle in the target (as Skia's sweep gradient samples it),
    /// into a bitmap drawn through a brush clipped to the rounded rectangle.
    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread; Direct2D gets pointers to locals that outlive each call.
    unsafe fn conic_gradient(
        &mut self,
        rect: &Rect,
        radii: &Radii,
        center: Point,
        angle: f32,
        stops: &[GradientStop],
        extend: Extend,
    ) -> Result<()> {
        const MAX_PIXELS: u64 = 1 << 24;
        let Some(inverse) = self.transform.inverse() else {
            return Ok(());
        };
        let size = self.ctx.GetPixelSize();
        let target = Rect::new(0.0, 0.0, size.width as f32, size.height as f32);
        let device = self.transform.apply_rect(rect).intersect(&target);
        if device.is_empty() {
            return Ok(());
        }
        let (x0, y0) = (device.x.floor(), device.y.floor());
        let w = (device.right().ceil() - x0).max(1.0) as u32;
        let h = (device.bottom().ceil() - y0).max(1.0) as u32;
        if w as u64 * h as u64 > MAX_PIXELS {
            return Ok(());
        }
        // (a hash of everything that decides the pixels)
        let key = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            for v in [
                x0,
                y0,
                center.x,
                center.y,
                angle,
                inverse.m11,
                inverse.m12,
                inverse.m21,
                inverse.m22,
                inverse.dx,
                inverse.dy,
            ] {
                v.to_bits().hash(&mut hasher);
            }
            (w, h, extend as u8).hash(&mut hasher);
            for s in stops {
                s.offset.to_bits().hash(&mut hasher);
                (s.color.r, s.color.g, s.color.b, s.color.a).hash(&mut hasher);
            }
            hasher.finish()
        };
        const KEPT: usize = 32;
        let cached = self.res.conics.iter().position(|(k, _)| *k == key);
        let bitmap = match cached {
            Some(i) => {
                let entry = self.res.conics.remove(i);
                let bitmap = entry.1.clone();
                self.res.conics.push(entry);
                bitmap
            }
            None => {
                let pixels = conic_pixels((x0, y0, w, h), &inverse, center, angle, stops, extend);
                let bitmap = self.ctx.CreateBitmap(
                    D2D_SIZE_U {
                        width: w,
                        height: h,
                    },
                    Some(pixels.as_ptr().cast()),
                    w * 4,
                    &D2D1_BITMAP_PROPERTIES1 {
                        pixelFormat: D2D1_PIXEL_FORMAT {
                            format: DXGI_FORMAT_B8G8R8A8_UNORM,
                            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                        },
                        dpiX: 96.0,
                        dpiY: 96.0,
                        bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
                        colorContext: ManuallyDrop::new(None),
                    },
                )?;
                if self.res.conics.len() >= KEPT {
                    self.res.conics.remove(0);
                }
                self.res.conics.push((key, bitmap.clone()));
                bitmap
            }
        };
        // (bitmap pixels are device pixels from (x0, y0); the brush maps them back through the
        // inverse of the current transform)
        let brush = self.ctx.CreateBitmapBrush(
            &bitmap,
            Some(&D2D1_BITMAP_BRUSH_PROPERTIES1 {
                extendModeX: D2D1_EXTEND_MODE_CLAMP,
                extendModeY: D2D1_EXTEND_MODE_CLAMP,
                interpolationMode: D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            }),
            Some(&D2D1_BRUSH_PROPERTIES {
                opacity: 1.0,
                transform: matrix_3x2(&Matrix::translation(x0, y0).then(&inverse)),
            }),
        )?;
        self.fill_with(rect, radii, &brush.cast()?)
    }

    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread; Direct2D gets pointers to locals that outlive each call.
    unsafe fn box_shadow(&mut self, s: &BoxShadow) -> Result<()> {
        let sigma = s.blur / 2.0;
        let margin = (sigma * 3.0).ceil() + 1.0;
        if !s.inset {
            let shape = s
                .rect
                .translate(s.offset.x, s.offset.y)
                .inflate(s.spread, s.spread);
            if shape.is_empty() {
                return Ok(());
            }
            let radii = s
                .radii
                .outset(s.spread, Size::new(s.rect.width, s.rect.height));
            // Outer shadows show only outside the border box.
            let area = shape
                .inflate(margin, margin)
                .union(&s.rect)
                .inflate(1.0, 1.0);
            let outside = self.ring(&area, &Radii::ZERO, &s.rect, &s.radii)?;
            self.push_mask(&outside.cast()?, None);
            let geometry = self.rounded_geometry(&shape, &radii)?;
            let r = self.blurred(&area, sigma, &geometry, s.color);
            if let Some(open) = self.stack.pop() {
                self.close(open);
            }
            r
        } else {
            // Inset shadows show inside the padding box, cast by the edges of a hole in a big shape.
            let hole = s
                .rect
                .translate(s.offset.x, s.offset.y)
                .inflate(-s.spread, -s.spread);
            let hole_radii = s
                .radii
                .outset(-s.spread, Size::new(s.rect.width, s.rect.height));
            let clip = self.rounded_geometry(&s.rect, &s.radii)?;
            self.push_mask(&clip, None);
            let grow = margin + s.spread.abs() + s.offset.x.abs().max(s.offset.y.abs());
            let frame = s.rect.inflate(grow, grow);
            // The blur reads the shape up to three deviations beyond the box: the ring outside it
            // is what darkens the inside of its edges.
            let area = s.rect.inflate(margin, margin);
            let ring: ID2D1Geometry = self
                .ring(&frame, &Radii::ZERO, &hole, &hole_radii)?
                .cast()?;
            let r = self.blurred(&area, sigma, &ring, s.color);
            if let Some(open) = self.stack.pop() {
                self.close(open);
            }
            r
        }
    }

    /// Draws `geometry` (local coordinates) filled with `color` and blurred with a Gaussian of
    /// standard deviation `sigma` (CSS px) within `area`; without blur, straight into the target.
    /// The GPU blurs when this context's device was found to draw a blur as the CPU does
    /// ([`BlurPath`]); otherwise, and once a GPU blur has failed, the CPU does.
    // SAFETY contract: the painter's context is between BeginDraw and EndDraw on this thread; Direct2D gets pointers to locals that outlive each call.
    unsafe fn blurred(
        &mut self,
        area: &Rect,
        sigma: f32,
        geometry: &ID2D1Geometry,
        color: Color,
    ) -> Result<()> {
        if sigma < 0.25 {
            self.ctx.FillGeometry(geometry, self.brush(color), None);
            return Ok(());
        }
        // Only what is shown or read is blurred: the area cut to the needed pixels and as far beyond
        // them as the blur reaches, so its intermediate is bounded by them and not by the box (a
        // shadow wholly outside them draws nothing).
        let reach = (sigma * 3.0 * self.transform.approximate_scale()).ceil() + 2.0;
        let need = self.need.inflate(reach, reach);
        let device = self.transform.apply_rect(area);
        if !device.intersects(&need) {
            return Ok(());
        }
        let area = match self.transform.inverse() {
            Some(inverse) if !covers(&need, &device) => {
                inverse.apply_rect(&device.intersect(&need)).intersect(area)
            }
            _ => *area,
        };
        if self.res.blur == BlurPath::Unchecked {
            self.res.blur = check_blur(self.ctx);
        }
        let blur = Blur::new(&area, sigma, &self.transform);
        if self.res.blur == BlurPath::Gpu {
            match blur.gpu(self.ctx, geometry, color) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    eprintln!("[raster] a GPU blur failed ({e}): blurring on the CPU");
                    self.res.blur = BlurPath::Cpu;
                }
            }
        }
        blur.cpu(self.ctx, &self.res.factory, geometry, color)
    }
}

/// Which way a device context blurs ([`Painter::blurred`]): the GPU's Gaussian blur effect when it
/// draws as the CPU does, the CPU otherwise. Some adapters' Direct2D blur fails or draws something
/// else (a hard block where a soft shadow should be), so it is checked, not assumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlurPath {
    /// Not known yet: checked at the context's first blur ([`check_blur`]).
    Unchecked,
    Gpu,
    Cpu,
}

impl BlurPath {
    /// `SNOWLARK_BLUR=gpu` or `cpu` forces one, for before and after comparisons; otherwise each
    /// context checks.
    fn from_env() -> BlurPath {
        match std::env::var("SNOWLARK_BLUR").as_deref() {
            Ok("gpu") => BlurPath::Gpu,
            Ok("cpu") => BlurPath::Cpu,
            _ => BlurPath::Unchecked,
        }
    }
}

/// A blur's area (local CSS px) and the intermediate it is drawn in: device resolution, at most
/// 4096 px a side.
struct Blur {
    area: Rect,
    sigma: f32,
    /// Intermediate pixels per local unit.
    scale: f32,
    pw: u32,
    ph: u32,
}

impl Blur {
    fn new(area: &Rect, sigma: f32, transform: &Matrix) -> Blur {
        let mut scale = transform.approximate_scale().clamp(0.05, 8.0);
        let longest = area.width.max(area.height) * scale;
        if longest > 4096.0 {
            scale *= 4096.0 / longest;
        }
        Blur {
            area: *area,
            sigma,
            scale,
            pw: (area.width * scale).ceil().max(1.0) as u32,
            ph: (area.height * scale).ceil().max(1.0) as u32,
        }
    }

    /// On the GPU: `geometry` drawn into an intermediate of `ctx`'s device, then through its Gaussian
    /// blur effect into `ctx`.
    // SAFETY contract: `ctx` is between BeginDraw and EndDraw on this thread, and `geometry` is of its factory.
    unsafe fn gpu(
        &self,
        ctx: &ID2D1DeviceContext,
        geometry: &ID2D1Geometry,
        color: Color,
    ) -> Result<()> {
        // Everything in device pixels at 96 DPI: the intermediate, the blur's deviation and the
        // image drawn back (scaled to local units by the transform). An intermediate of another
        // DPI drawn into a context in pixel units came back unblurred and filled to its edges at
        // any scale but 1. Premultiplied whatever the target's alpha mode: an opaque target's
        // alpha is undefined, and the blur reads it.
        let size = D2D_SIZE_U {
            width: self.pw,
            height: self.ph,
        };
        let layer = ctx.CreateCompatibleRenderTarget(
            Some(&D2D_SIZE_F {
                width: self.pw as f32,
                height: self.ph as f32,
            }),
            Some(&size),
            Some(&D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            }),
            D2D1_COMPATIBLE_RENDER_TARGET_OPTIONS_NONE,
        )?;
        let brush = layer.CreateSolidColorBrush(&color_f(color), None)?;
        layer.BeginDraw();
        layer.Clear(Some(&color_f(Color::TRANSPARENT)));
        layer.SetTransform(&matrix_3x2(
            &Matrix::translation(-self.area.x, -self.area.y)
                .then(&Matrix::scale(self.scale, self.scale)),
        ));
        layer.FillGeometry(geometry, &brush, None);
        layer.EndDraw(None, None)?;
        let bitmap = layer.GetBitmap()?;
        let effect = ctx.CreateEffect(&CLSID_D2D1GaussianBlur)?;
        effect.SetInput(0, &bitmap, true);
        effect.SetValue(
            D2D1_GAUSSIANBLUR_PROP_STANDARD_DEVIATION.0 as u32,
            D2D1_PROPERTY_TYPE_FLOAT,
            &(self.sigma * self.scale).to_le_bytes(),
        )?;
        let output = effect.GetOutput()?;
        let mut outer = Matrix3x2::default();
        ctx.GetTransform(&mut outer);
        let world = Matrix::new(
            outer.M11, outer.M12, outer.M21, outer.M22, outer.M31, outer.M32,
        );
        ctx.SetTransform(&matrix_3x2(
            &Matrix::scale(1.0 / self.scale, 1.0 / self.scale)
                .then(&Matrix::translation(self.area.x, self.area.y))
                .then(&world),
        ));
        ctx.DrawImage(
            &output,
            None,
            None,
            D2D1_INTERPOLATION_MODE_LINEAR,
            D2D1_COMPOSITE_MODE_SOURCE_OVER,
        );
        ctx.SetTransform(&outer);
        Ok(())
    }

    /// On the CPU: `geometry`'s coverage from Direct2D's software rasterizer, blurred in memory
    /// ([`crate::soft_blur`]), colored and drawn into `ctx` as a bitmap.
    // SAFETY contract: `ctx` is between BeginDraw and EndDraw on this thread, and `geometry` is of `factory`.
    unsafe fn cpu(
        &self,
        ctx: &ID2D1DeviceContext,
        factory: &ID2D1Factory,
        geometry: &ID2D1Geometry,
        color: Color,
    ) -> Result<()> {
        let mut mask = self.coverage(factory, geometry)?;
        crate::soft_blur::blur_mask(
            &mut mask,
            self.pw as usize,
            self.ph as usize,
            self.sigma * self.scale,
        );
        let a = color.a as u32;
        let pm = |c: u8, alpha: u32| ((c as u32 * alpha + 127) / 255) as u8;
        let mut pixels = Vec::with_capacity(mask.len() * 4);
        for m in mask {
            let alpha = (m as u32 * a + 127) / 255;
            pixels.extend_from_slice(&[
                pm(color.b, alpha),
                pm(color.g, alpha),
                pm(color.r, alpha),
                alpha as u8,
            ]);
        }
        let bitmap = make_bitmap(ctx, self.pw, self.ph, &pixels)?;
        let dest = Rect::new(
            self.area.x,
            self.area.y,
            self.pw as f32 / self.scale,
            self.ph as f32 / self.scale,
        );
        ctx.DrawBitmap(
            &bitmap,
            Some(&rect_f(&dest)),
            1.0,
            D2D1_INTERPOLATION_MODE_LINEAR,
            None,
            None,
        );
        Ok(())
    }

    /// The intermediate's coverage of `geometry`, one byte a pixel.
    // SAFETY contract: `geometry` is of `factory`.
    unsafe fn coverage(&self, factory: &ID2D1Factory, geometry: &ID2D1Geometry) -> Result<Vec<u8>> {
        com::ensure();
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory2, None, CLSCTX_INPROC_SERVER)?;
        let bitmap = wic.CreateBitmap(
            self.pw,
            self.ph,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapCacheOnLoad,
        )?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt = factory.CreateWicBitmapRenderTarget(&bitmap, &props)?;
        let brush = rt.CreateSolidColorBrush(&color_f(Color::BLACK), None)?;
        rt.BeginDraw();
        rt.Clear(Some(&color_f(Color::TRANSPARENT)));
        rt.SetTransform(&matrix_3x2(
            &Matrix::translation(-self.area.x, -self.area.y)
                .then(&Matrix::scale(self.scale, self.scale)),
        ));
        rt.FillGeometry(geometry, &brush, None);
        rt.EndDraw(None, None)?;
        let mut data = vec![0u8; self.pw as usize * self.ph as usize * 4];
        bitmap.CopyPixels(std::ptr::null(), self.pw * 4, &mut data)?;
        Ok(data.as_chunks::<4>().0.iter().map(|p| p[3]).collect())
    }
}

/// Which way `ctx` should blur: a test shadow drawn the GPU's way and the CPU's into small targets
/// of `ctx`'s device and pixel format, at a display scale of 1 and at one of 1.5, and compared. A
/// GPU blur that fails or draws something else at either is not used.
// SAFETY contract: `ctx` is a live device context used on this thread.
unsafe fn check_blur(ctx: &ID2D1DeviceContext) -> BlurPath {
    let differs = blur_differs(ctx, 1.0).and_then(|d| match d {
        None => blur_differs(ctx, 1.5),
        d => Ok(d),
    });
    match differs {
        Ok(None) => BlurPath::Gpu,
        Ok(Some(what)) => {
            eprintln!("[raster] the GPU's blur draws {what}: blurring on the CPU");
            BlurPath::Cpu
        }
        Err(e) => {
            eprintln!("[raster] the GPU's blur could not be checked ({e}): blurring on the CPU");
            BlurPath::Cpu
        }
    }
}

/// Where the GPU's blur of a test shadow differs from the CPU's, if it does (see [`check_blur`]).
// SAFETY contract: as `check_blur`'s.
unsafe fn blur_differs(ctx: &ID2D1DeviceContext, scale: f32) -> Result<Option<String>> {
    let device = ctx.GetDevice()?;
    let probe = device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
    let factory = ctx.GetFactory()?;
    let mut format = ctx.GetPixelFormat();
    if format.format == DXGI_FORMAT_UNKNOWN {
        format.format = DXGI_FORMAT_B8G8R8A8_UNORM;
    }
    if format.alphaMode == D2D1_ALPHA_MODE_UNKNOWN {
        format.alphaMode = D2D1_ALPHA_MODE_PREMULTIPLIED;
    }
    // A 16 px square blurred by 4 px, in a 48 px area on gray (CSS px).
    let geometry: ID2D1Geometry = factory
        .CreateRectangleGeometry(&rect_f(&Rect::new(16.0, 16.0, 16.0, 16.0)))?
        .cast()?;
    let blur = Blur::new(
        &Rect::new(0.0, 0.0, 48.0, 48.0),
        4.0,
        &Matrix::scale(scale, scale),
    );
    let color = Color::rgba(0, 0, 160, 200);
    let size = D2D_SIZE_U {
        width: blur.pw,
        height: blur.ph,
    };
    let props = |options| D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: format,
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: options,
        colorContext: ManuallyDrop::new(None),
    };
    let mut shots = Vec::new();
    for gpu in [true, false] {
        let target = probe.CreateBitmap(size, None, 0, &props(D2D1_BITMAP_OPTIONS_TARGET))?;
        probe.SetTarget(&target);
        probe.BeginDraw();
        probe.SetUnitMode(D2D1_UNIT_MODE_PIXELS);
        probe.SetTransform(&matrix_3x2(&Matrix::scale(scale, scale)));
        probe.Clear(Some(&color_f(Color::rgb(200, 200, 200))));
        let drawn = if gpu {
            blur.gpu(&probe, &geometry, color)
        } else {
            blur.cpu(&probe, &factory, &geometry, color)
        };
        let ended = probe.EndDraw(None, None);
        probe.SetTarget(None);
        drawn?;
        ended?;
        let staging = probe.CreateBitmap(
            size,
            None,
            0,
            &props(D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW),
        )?;
        staging.CopyFromBitmap(None, &target, None)?;
        let mapped = staging.Map(D2D1_MAP_OPTIONS_READ)?;
        let row = blur.pw as usize * 4;
        let mut data = vec![0u8; row * blur.ph as usize];
        for y in 0..blur.ph as usize {
            let src = std::slice::from_raw_parts(mapped.bits.add(y * mapped.pitch as usize), row);
            data[y * row..(y + 1) * row].copy_from_slice(src);
        }
        staging.Unmap()?;
        shots.push(data);
    }
    // (the colors only: an opaque target's fourth byte is undefined)
    let at = shots[0]
        .as_chunks::<4>()
        .0
        .iter()
        .zip(shots[1].as_chunks::<4>().0)
        .position(|(g, c)| (0..3).any(|i| (g[i] as i32 - c[i] as i32).abs() > 24));
    Ok(at.map(|i| {
        let (x, y) = (i % blur.pw as usize, i / blur.pw as usize);
        let px = |s: &[u8]| {
            let p = &s[i * 4..i * 4 + 3];
            format!("rgb({}, {}, {})", p[2], p[1], p[0])
        };
        format!(
            "{} at ({x}, {y}) of a test shadow at {scale}x where the CPU draws {}",
            px(&shots[0]),
            px(&shots[1])
        )
    }))
}

/// The premultiplied color (0..=1 channels) of `stops` at `t`, interpolated premultiplied and
/// clamped to the end stops.
fn premultiplied_at(stops: &[GradientStop], t: f32) -> [f32; 4] {
    let pm = |c: Color| {
        let a = c.a as f32 / 255.0;
        [
            c.r as f32 / 255.0 * a,
            c.g as f32 / 255.0 * a,
            c.b as f32 / 255.0 * a,
            a,
        ]
    };
    let (Some(first), Some(last)) = (stops.first(), stops.last()) else {
        return [0.0; 4];
    };
    if t.is_nan() || t <= first.offset {
        return pm(first.color);
    }
    for w in stops.windows(2) {
        if t < w[1].offset {
            let span = w[1].offset - w[0].offset;
            let f = if span > 0.0 {
                (t - w[0].offset) / span
            } else {
                1.0
            };
            let (a, b) = (pm(w[0].color), pm(w[1].color));
            return std::array::from_fn(|i| a[i] + (b[i] - a[i]) * f);
        }
    }
    pm(last.color)
}

/// A conic gradient's premultiplied BGRA pixels over the device rectangle `(x0, y0, w, h)`:
/// each pixel center taken back to local coordinates by `inverse`, its angle around `center`
/// clockwise from up past `angle` (radians) as a fraction of a turn, looked up in `stops` (CSS
/// Images 4 §3.3; `Repeat` repeats their first-to-last span).
fn conic_pixels(
    (x0, y0, w, h): (f32, f32, u32, u32),
    inverse: &Matrix,
    center: Point,
    angle: f32,
    stops: &[GradientStop],
    extend: Extend,
) -> Vec<u8> {
    use std::f32::consts::TAU;
    let (first, period) = match (stops.first(), stops.last()) {
        (Some(f), Some(l)) => (f.offset, l.offset - f.offset),
        _ => (0.0, 0.0),
    };
    let mut out = vec![0u8; (w * h * 4) as usize];
    for j in 0..h {
        for i in 0..w {
            let p = inverse.apply(Point::new(x0 + i as f32 + 0.5, y0 + j as f32 + 0.5));
            let (dx, dy) = (p.x - center.x, p.y - center.y);
            let mut t = ((dx.atan2(-dy) - angle) / TAU).rem_euclid(1.0);
            if extend == Extend::Repeat && period > 1e-6 {
                t = first + (t - first).rem_euclid(period);
            }
            let c = premultiplied_at(stops, t);
            let k = ((j * w + i) * 4) as usize;
            let byte = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
            out[k] = byte(c[2]);
            out[k + 1] = byte(c[1]);
            out[k + 2] = byte(c[0]);
            out[k + 3] = byte(c[3]);
        }
    }
    out
}

/// The stops followed by a hard stop: the next one at the same position, in another color.
fn hard_stop_indices(stops: &[GradientStop]) -> Vec<usize> {
    (0..stops.len().saturating_sub(1))
        .filter(|&i| {
            stops[i + 1].offset - stops[i].offset <= 1e-6 && stops[i + 1].color != stops[i].color
        })
        .collect()
}

/// The runs of stops between hard stops (`hard`, from [`hard_stop_indices`]): (first stop, last
/// stop, from, to) on the gradient's parameter. Clamped, the first and last runs reach out
/// forever; repeating, they cover one period, 0 to 1.
fn hard_segments(
    stops: &[GradientStop],
    hard: &[usize],
    extend: Extend,
) -> Vec<(usize, usize, f32, f32)> {
    let mut segments = Vec::with_capacity(hard.len() + 1);
    let mut first = 0;
    for &h in hard {
        segments.push((first, h, stops[first].offset, stops[h].offset));
        first = h + 1;
    }
    segments.push((first, stops.len() - 1, stops[first].offset, 1.0));
    if extend == Extend::Repeat {
        segments[0].2 = 0.0;
    } else {
        segments[0].2 = f32::NEG_INFINITY;
        segments.last_mut().unwrap().3 = f32::INFINITY;
    }
    segments
}

/// The range of a linear gradient's parameter (0 at `start`, 1 at `end`) over a rectangle.
fn linear_range(rect: &Rect, start: Point, end: Point) -> Option<(f64, f64)> {
    let (dx, dy) = ((end.x - start.x) as f64, (end.y - start.y) as f64);
    let len2 = dx * dx + dy * dy;
    if len2.is_nan() || len2 <= 0.0 {
        return None;
    }
    let t = |x: f32, y: f32| ((x - start.x) as f64 * dx + (y - start.y) as f64 * dy) / len2;
    let c = [
        t(rect.x, rect.y),
        t(rect.right(), rect.y),
        t(rect.x, rect.bottom()),
        t(rect.right(), rect.bottom()),
    ];
    Some((
        c.iter().copied().fold(f64::INFINITY, f64::min),
        c.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    ))
}

/// The range of a radial gradient's parameter (0 at the center, 1 on the ending shape) over a
/// rectangle: from its nearest point to its farthest corner.
fn radial_range(rect: &Rect, center: Point, radius: Size) -> Option<(f64, f64)> {
    let (rx, ry) = (radius.width as f64, radius.height as f64);
    if !(rx > 0.0 && ry > 0.0) {
        return None;
    }
    let (cx, cy) = (center.x as f64, center.y as f64);
    let r = |x: f64, y: f64| (((x - cx) / rx).powi(2) + ((y - cy) / ry).powi(2)).sqrt();
    let (l, t, rr, b) = (
        rect.x as f64,
        rect.y as f64,
        rect.right() as f64,
        rect.bottom() as f64,
    );
    let near = r(cx.clamp(l, rr), cy.clamp(t, b));
    let far = [r(l, t), r(rr, t), r(l, b), r(rr, b)]
        .into_iter()
        .fold(0.0, f64::max);
    Some((near, far))
}

/// The one color a gradient shows over a parameter `range`, when it is one: all its stops have
/// it, or (not repeating) the range lies before the first stop, after the last, or between two
/// stops of the same color. Painting it as a fill is exact where the gradient's own geometry is
/// huge (a center or radius clamped from an infinite length: CSS Values 4 §10.9) and cheaper.
fn solid_color(stops: &[GradientStop], extend: Extend, range: Option<(f64, f64)>) -> Option<Color> {
    let first = stops.first()?;
    if stops.iter().all(|s| s.color == first.color) {
        return Some(first.color);
    }
    let (lo, hi) = range?;
    if extend != Extend::Clamp {
        return None;
    }
    let last = stops.last()?;
    // (within single precision of a stop counts as on it: geometry clamped from infinity puts a
    // box's far corner exactly on the ending shape only up to rounding)
    const EPS: f64 = 1e-6;
    if hi <= first.offset as f64 + EPS {
        return Some(first.color);
    }
    if lo >= last.offset as f64 - EPS {
        return Some(last.color);
    }
    stops.windows(2).find_map(|w| {
        (w[0].offset as f64 - EPS <= lo
            && hi <= w[1].offset as f64 + EPS
            && w[0].color == w[1].color)
            .then_some(w[0].color)
    })
}

/// How many dashes of `dash` fit along an open line of `length` starting and ending with a dash,
/// and the gap between them: the number whose gap is nearest to `gap` (Blink's
/// `SelectBestDashGap`). `None` when not even one dash fits.
pub(crate) fn dash_gap(length: f32, dash: f32, gap: f32) -> Option<(usize, f32)> {
    if !(length > 0.0 && dash > 0.0) {
        return None;
    }
    if length <= dash {
        return Some((1, 0.0));
    }
    let min = ((length + gap) / (dash + gap)).floor().max(1.0);
    let max = min + 1.0;
    let gap_for = |n: f32| {
        if n <= 1.0 {
            f32::INFINITY
        } else {
            (length - n * dash) / (n - 1.0)
        }
    };
    let (g_min, g_max) = (gap_for(min), gap_for(max));
    let n = if g_max < 0.0 || (g_min - gap).abs() < (g_max - gap).abs() {
        min
    } else {
        max
    };
    let g = gap_for(n);
    Some((n as usize, if g.is_finite() { g } else { 0.0 }))
}

fn rounded(rect: &Rect, r: Size) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT {
        rect: rect_f(rect),
        radiusX: r.width,
        radiusY: r.height,
    }
}

/// A clip's geometry: `rect` with `radii` fitted to it (a rectangle when they are zero).
// SAFETY contract: Direct2D gets pointers to locals that outlive each call.
pub(crate) unsafe fn clip_geometry(
    res: &Resources,
    rect: &Rect,
    radii: &Radii,
) -> Result<ID2D1Geometry> {
    let radii = radii.fitted(rect);
    if radii.is_zero() {
        return res.factory.CreateRectangleGeometry(&rect_f(rect))?.cast();
    }
    let path = res.factory.CreatePathGeometry()?;
    let sink = path.Open()?;
    add_rounded(&sink, rect, &radii);
    sink.Close()?;
    path.cast()
}

/// Adds a closed rounded-rectangle figure (radii already fitted).
// SAFETY contract: `sink` is an open geometry sink (between Open and Close).
unsafe fn add_rounded(sink: &ID2D1GeometrySink, r: &Rect, radii: &Radii) {
    let (l, t, rr, b) = (r.x, r.y, r.right(), r.bottom());
    let arc = |to: (f32, f32), size: Size| {
        if size.width > 0.0 && size.height > 0.0 {
            sink.AddArc(&D2D1_ARC_SEGMENT {
                point: Vector2 { X: to.0, Y: to.1 },
                size: D2D_SIZE_F {
                    width: size.width,
                    height: size.height,
                },
                rotationAngle: 0.0,
                sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                arcSize: D2D1_ARC_SIZE_SMALL,
            });
        } else {
            sink.AddLine(Vector2 { X: to.0, Y: to.1 });
        }
    };
    let tl = radii.top_left;
    let tr = radii.top_right;
    let br = radii.bottom_right;
    let bl = radii.bottom_left;
    sink.BeginFigure(
        Vector2 {
            X: l + tl.width,
            Y: t,
        },
        D2D1_FIGURE_BEGIN_FILLED,
    );
    sink.AddLine(Vector2 {
        X: rr - tr.width,
        Y: t,
    });
    arc((rr, t + tr.height), tr);
    sink.AddLine(Vector2 {
        X: rr,
        Y: b - br.height,
    });
    arc((rr - br.width, b), br);
    sink.AddLine(Vector2 {
        X: l + bl.width,
        Y: b,
    });
    arc((l, b - bl.height), bl);
    sink.AddLine(Vector2 {
        X: l,
        Y: t + tl.height,
    });
    arc((l + tl.width, t), tl);
    sink.EndFigure(D2D1_FIGURE_END_CLOSED);
}

/// A tiny inline vector for gradient stops (no allocation for up to 16 stops).
mod smallvec_stops {
    use windows::Win32::Graphics::Direct2D::Common::D2D1_GRADIENT_STOP;

    #[derive(Default)]
    pub struct Stops {
        inline: [D2D1_GRADIENT_STOP; 16],
        len: usize,
        spill: Vec<D2D1_GRADIENT_STOP>,
    }

    impl Stops {
        pub fn push(&mut self, s: D2D1_GRADIENT_STOP) {
            if self.spill.is_empty() && self.len < 16 {
                self.inline[self.len] = s;
                self.len += 1;
            } else {
                if self.spill.is_empty() {
                    self.spill.extend_from_slice(&self.inline[..self.len]);
                }
                self.spill.push(s);
            }
        }

        pub fn as_slice(&self) -> &[D2D1_GRADIENT_STOP] {
            if self.spill.is_empty() {
                &self.inline[..self.len]
            } else {
                &self.spill
            }
        }
    }
}

/// Software rendering into memory: Direct2D onto a WIC bitmap. Needs no GPU, so it works in services,
/// on CI and over remote desktop; it is what `--render` and the tests use. Thread-affine (it holds a
/// single-threaded Direct2D factory and its render target); make one per thread.
pub struct HeadlessRenderer {
    factory: ID2D1Factory1,
    wic: IWICImagingFactory,
    target: Option<Target>,
    /// Grayscale text antialiasing on opaque targets too ([`HeadlessRenderer::set_grayscale_text`]).
    grayscale_text: bool,
}

struct Target {
    width: u32,
    height: u32,
    opaque: bool,
    bitmap: IWICBitmap,
    ctx: ID2D1DeviceContext,
    res: Resources,
}

impl HeadlessRenderer {
    pub fn new() -> Result<HeadlessRenderer> {
        com::ensure();
        // SAFETY: plain factory creation on a COM-initialized thread.
        unsafe {
            Ok(HeadlessRenderer {
                factory: D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?,
                wic: CoCreateInstance(&CLSID_WICImagingFactory2, None, CLSCTX_INPROC_SERVER)?,
                target: None,
                grayscale_text: false,
            })
        }
    }

    /// Antialiases text in grayscale even on opaque targets, instead of ClearType: no color
    /// fringes, so a glyph whose edges fall on pixel boundaries (Ahem's squares) draws exactly
    /// like a box (reftests compare screenshots this way, as browsers' test modes do).
    pub fn set_grayscale_text(&mut self, on: bool) {
        self.grayscale_text = on;
    }

    /// Paints `list` and returns the pixels. The render target is kept for the next call of the same
    /// size and opacity.
    pub fn render(
        &mut self,
        list: &DisplayList,
        sources: Sources,
        view: &Viewport,
    ) -> Result<Pixmap> {
        list.validate()
            .map_err(|e| windows::core::Error::new(E_INVALIDARG, e.to_string()))?;
        let opaque = view.background.is_opaque();
        let (w, h) = (view.width.max(1), view.height.max(1));
        let reuse =
            matches!(&self.target, Some(t) if t.width == w && t.height == h && t.opaque == opaque);
        if !reuse {
            self.target = None;
            self.target = Some(self.make_target(w, h, opaque, sources.text)?);
        }
        let target = self.target.as_mut().unwrap();
        target.res.trim(sources.images);
        // SAFETY: the context draws into the WIC bitmap it was made for; pixels are copied out after
        // EndDraw into a buffer of exactly height rows of width * 4 bytes.
        unsafe {
            target.ctx.BeginDraw();
            target.ctx.SetTransform(&Matrix3x2::identity());
            target.ctx.Clear(Some(&color_f(view.background)));
            {
                let mut painter = Painter::new(
                    &target.ctx,
                    &mut target.res,
                    sources,
                    Matrix::scale(view.scale, view.scale),
                    opaque && !self.grayscale_text,
                )
                .over_canvas(view.background)
                .within(
                    Rect::new(0.0, 0.0, w as f32, h as f32),
                    Point::new(0.0, 0.0),
                );
                painter.paint(list, None);
            }
            target.ctx.EndDraw(None, None)?;
            let mut data = vec![0u8; w as usize * h as usize * 4];
            target
                .bitmap
                .CopyPixels(std::ptr::null(), w * 4, &mut data)?;
            if opaque {
                // (32bppBGR leaves the fourth byte undefined)
                for px in data.as_chunks_mut::<4>().0 {
                    px[3] = 255;
                }
            }
            Ok(Pixmap {
                width: w,
                height: h,
                data,
            })
        }
    }

    fn make_target(&self, w: u32, h: u32, opaque: bool, text: &dyn FontResolver) -> Result<Target> {
        // SAFETY: WIC and Direct2D object creation; the render target keeps the bitmap alive.
        unsafe {
            let format = if opaque {
                GUID_WICPixelFormat32bppBGR
            } else {
                GUID_WICPixelFormat32bppPBGRA
            };
            let bitmap = self.wic.CreateBitmap(w, h, &format, WICBitmapCacheOnLoad)?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: if opaque {
                        D2D1_ALPHA_MODE_IGNORE
                    } else {
                        D2D1_ALPHA_MODE_PREMULTIPLIED
                    },
                },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let rt = self.factory.CreateWicBitmapRenderTarget(&bitmap, &props)?;
            let ctx: ID2D1DeviceContext = rt.cast()?;
            let res = Resources::new(&ctx, text)?;
            Ok(Target {
                width: w,
                height: h,
                opaque,
                bitmap,
                ctx,
                res,
            })
        }
    }
}

/// The index of the pop that closes the group pushed at `push`, or the last index before `end`
/// when the list leaves it open.
fn group_end(list: &DisplayList, push: usize, end: usize) -> usize {
    let mut depth = 0usize;
    for (i, item) in list.items[push..end].iter().enumerate() {
        match item {
            Item::PushClipRect { .. }
            | Item::PushClipRoundedRect { .. }
            | Item::PushClipPath { .. }
            | Item::PushTransform { .. }
            | Item::PushOpacity { .. }
            | Item::PushLayer { .. }
            | Item::PushPlus
            | Item::PushFilter { .. }
            | Item::PushMask { .. } => depth += 1,
            Item::PopClip
            | Item::PopTransform
            | Item::PopOpacity
            | Item::PopLayer
            | Item::PopPlus
            | Item::PopFilter
            | Item::PopMask => {
                depth -= 1;
                if depth == 0 {
                    return push + i;
                }
            }
            _ => {}
        }
    }
    end.saturating_sub(1)
}

/// Bounds used by tiled rasterization (kept in the resources to reuse the vector), with each
/// group's push given its content's union ([`DisplayList::group_bounds`]) for culling.
pub(crate) fn bounds_into<'r>(
    res: &'r mut Resources,
    list: &DisplayList,
    base: &Matrix,
) -> &'r [Rect] {
    list.item_bounds(base, &mut res.bounds);
    list.group_bounds(&mut res.bounds, &mut res.open_groups);
    &res.bounds
}

pub(crate) fn take_bounds(res: &mut Resources) -> Vec<Rect> {
    std::mem::take(&mut res.bounds)
}

pub(crate) fn put_bounds(res: &mut Resources, bounds: Vec<Rect>) {
    res.bounds = bounds;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The blur check finds the GPU's blur and the CPU's alike on WARP and on Direct2D's software
    /// target (an opaque one, as a page's), at display scales other than 1 too: neither falls back.
    #[test]
    fn the_gpu_blur_draws_as_the_cpu_blur() {
        let gpu = crate::gpu::Gpu::with_driver(crate::gpu::Driver::Warp).unwrap();
        com::ensure();
        // SAFETY: plain object creation; the contexts are used on this thread only, outside any
        // other draw.
        unsafe {
            let warp = gpu
                .d2d_device()
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
                .unwrap();
            let factory: ID2D1Factory =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).unwrap();
            let wic: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory2, None, CLSCTX_INPROC_SERVER).unwrap();
            let bitmap = wic
                .CreateBitmap(8, 8, &GUID_WICPixelFormat32bppBGR, WICBitmapCacheOnLoad)
                .unwrap();
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let soft: ID2D1DeviceContext = factory
                .CreateWicBitmapRenderTarget(&bitmap, &props)
                .unwrap()
                .cast()
                .unwrap();
            let mut contexts = vec![("WARP", warp), ("software", soft)];
            // (and the graphics card, when there is one)
            if let Ok(card) = crate::gpu::Gpu::with_driver(crate::gpu::Driver::Hardware) {
                let ctx = card
                    .d2d_device()
                    .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
                    .unwrap();
                contexts.push(("the graphics card", ctx));
            }
            for scale in [1.0f32, 1.25, 1.5, 2.0] {
                for (what, ctx) in &contexts {
                    assert_eq!(
                        blur_differs(ctx, scale).unwrap(),
                        None,
                        "{what} at {scale}x"
                    );
                }
            }
        }
    }

    #[test]
    fn conic_pixels_sweep_clockwise_from_the_start_angle() {
        let red = Color::rgb(255, 0, 0);
        let blue = Color::rgb(0, 0, 255);
        // quarters: red until a quarter turn, then blue (a hard stop at 90 degrees)
        let stops = [
            GradientStop {
                offset: 0.25,
                color: red,
            },
            GradientStop {
                offset: 0.25,
                color: blue,
            },
        ];
        let px = conic_pixels(
            (0.0, 0.0, 4, 4),
            &Matrix::IDENTITY,
            Point::new(2.0, 2.0),
            0.0,
            &stops,
            Extend::Clamp,
        );
        let at = |x: usize, y: usize| {
            let k = (y * 4 + x) * 4;
            (px[k + 2], px[k + 1], px[k])
        };
        // top right quadrant (0..90 degrees from up): red; the others blue
        assert_eq!(at(3, 0), (255, 0, 0));
        assert_eq!(at(3, 3), (0, 0, 255));
        assert_eq!(at(0, 0), (0, 0, 255));
        // from 90 degrees: the top right quadrant is the last quarter of the turn
        let px = conic_pixels(
            (0.0, 0.0, 4, 4),
            &Matrix::IDENTITY,
            Point::new(2.0, 2.0),
            std::f32::consts::FRAC_PI_2,
            &stops,
            Extend::Clamp,
        );
        assert_eq!((px[(3 * 4 + 3) * 4 + 2], px[(3 * 4 + 3) * 4]), (255, 0));
    }

    #[test]
    fn dashes_start_and_end_with_a_dash() {
        // 100 px with 10 px dots and 10 px gaps: 5 dots with gaps of 12.5 or 6 with gaps of 8;
        // 8 is nearer 10
        assert_eq!(dash_gap(100.0, 10.0, 10.0), Some((6, 8.0)));
        // 90 px: 5 with 10
        assert_eq!(dash_gap(90.0, 10.0, 10.0), Some((5, 10.0)));
        // 110 px: exactly 6 dots with 10 px gaps
        assert_eq!(dash_gap(110.0, 10.0, 10.0), Some((6, 10.0)));
        // shorter than a dash: one dash
        assert_eq!(dash_gap(5.0, 10.0, 10.0), Some((1, 0.0)));
        assert_eq!(dash_gap(0.0, 10.0, 10.0), None);
    }

    #[test]
    fn gradients_over_one_color_are_fills() {
        let stop = |offset: f32, color: Color| GradientStop { offset, color };
        let (red, lime) = (Color::rgb(255, 0, 0), Color::rgb(0, 255, 0));
        // a hard stop at the very end, all of the box before it: lime
        let stops = [stop(1.0, lime), stop(1.0, red)];
        assert_eq!(
            solid_color(&stops, Extend::Clamp, Some((0.99, 0.9999))),
            Some(lime)
        );
        // past the last stop: its color; across a change: none
        assert_eq!(
            solid_color(&stops, Extend::Clamp, Some((1.5, 2.0))),
            Some(red)
        );
        let ramp = [stop(0.0, lime), stop(1.0, red)];
        assert_eq!(solid_color(&ramp, Extend::Clamp, Some((0.2, 0.3))), None);
        // one color everywhere, repeating or not
        let flat = [stop(0.0, lime), stop(0.5, lime)];
        assert_eq!(solid_color(&flat, Extend::Repeat, None), Some(lime));
        // the parameter ranges: a radial gradient's nearest point and farthest corner
        let r = Rect::new(0.0, 0.0, 100.0, 100.0);
        let (near, far) =
            radial_range(&r, Point::new(50.0, 300.0), Size::new(100.0, 100.0)).unwrap();
        assert!(
            (near - 2.0).abs() < 1e-9
                && (far - (50f64.powi(2) + 300f64.powi(2)).sqrt() / 100.0).abs() < 1e-9
        );
        let (a, b) = linear_range(&r, Point::new(0.0, 0.0), Point::new(200.0, 0.0)).unwrap();
        assert_eq!((a, b), (0.0, 0.5));
    }
}
