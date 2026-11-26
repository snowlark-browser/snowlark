// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What tiled rasterization decides apart from the device that paints: the tile grid, which tiles
//! show the same as the last frame's (raster invalidation by content hashes), and what a raster
//! did. Both platforms' [`crate::gpu::TileRasterizer`]s use it.

use crate::display_list::{hash_combine, DisplayList, Item};
use crate::geom::{Color, Rect};
use crate::raster::Viewport;

/// The tile size in device pixels.
pub const TILE_SIZE: u32 = 512;

/// What a raster did: tiles painted and tiles copied unchanged from the previous frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RasterStats {
    pub painted: u32,
    pub reused: u32,
}

/// The work of the last full raster and the pieces after it: where its time went, in
/// milliseconds (working out what each tile shows, its bounds and hashes; painting tiles; copying
/// unchanged ones), and how many items the painted tiles drew (an item drawn into two tiles counts
/// twice; groups culled whole are not drawn).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RasterWork {
    pub prepare: f64,
    pub paint: f64,
    pub copy: f64,
    pub items: u32,
}

/// The geometry a frame was rasterized for: tiles are reusable between frames that share it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FrameKey {
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub background: Color,
    pub origin: (i32, i32),
    pub opaque: bool,
}

impl FrameKey {
    pub(crate) fn of(view: &Viewport, origin: (i32, i32)) -> FrameKey {
        FrameKey {
            width: view.width,
            height: view.height,
            scale: view.scale,
            background: view.background,
            origin,
            opaque: view.background.is_opaque(),
        }
    }
}

/// The tile grid of a frame: columns, rows, and the side of each tile's texture (512, or for a
/// frame that fits in one tile, a smaller square that holds it: a power of two from 64, a small
/// layer's tile, as Chromium sizes a small layer's tiles to it).
pub(crate) fn grid(view: &Viewport) -> (u32, u32, u32) {
    let (columns, rows) = (
        view.width.div_ceil(TILE_SIZE),
        view.height.div_ceil(TILE_SIZE),
    );
    let size = if columns == 1 && rows == 1 {
        view.width
            .max(view.height)
            .next_power_of_two()
            .clamp(64, TILE_SIZE)
    } else {
        TILE_SIZE
    };
    (columns, rows, size)
}

/// The tile at grid position (`tx`, `ty`) in frame device pixels.
pub(crate) fn tile_area(tx: u32, ty: u32) -> Rect {
    Rect::new(
        (tx * TILE_SIZE) as f32,
        (ty * TILE_SIZE) as f32,
        TILE_SIZE as f32,
        TILE_SIZE as f32,
    )
}

/// The hash of what each tile of a `columns` × `rows` grid shows: the items whose device bounds
/// meet the tile, in order, each with the clips, transforms, opacity and blend groups it is drawn
/// in. A clip, transform or opacity group shows only through what is drawn in it, so it changes
/// only the tiles its content meets: a box that moves by a transform repaints the tiles it left and
/// the tiles it entered, not every tile (raster invalidation of the old and the new bounds of what
/// moved). Filter and mask groups, whose effect can reach past their content's bounds, change every
/// tile, as before.
pub(crate) fn tile_hashes(
    list: &DisplayList,
    item_hashes: &[u64],
    bounds: &[Rect],
    columns: u32,
    rows: u32,
) -> Vec<u64> {
    let mut tiles = vec![0u64; (columns * rows) as usize];
    let size = TILE_SIZE as f32;
    // (the hash of the groups open around an item, innermost last: what it is drawn through)
    let mut groups: Vec<u64> = vec![0];
    for (i, ((item, &h), b)) in list.items.iter().zip(item_hashes).zip(bounds).enumerate() {
        match item {
            Item::PushClipRect { .. }
            | Item::PushClipRoundedRect { .. }
            | Item::PushClipPath { .. }
            | Item::PushTransform { .. }
            | Item::PushOpacity { .. }
            | Item::PushPlus
            | Item::PushLayer { .. } => {
                let outer = *groups.last().unwrap_or(&0);
                groups.push(hash_combine(outer, h));
                continue;
            }
            Item::PopClip
            | Item::PopTransform
            | Item::PopOpacity
            | Item::PopPlus
            | Item::PopLayer => {
                if groups.len() > 1 {
                    groups.pop();
                }
                continue;
            }
            _ => {}
        }
        let h = hash_combine(*groups.last().unwrap_or(&0), h);
        // (a backdrop filter shows what is painted under it, which may be drawn in other tiles: a
        // change there changes it in each of its tiles)
        let h = if matches!(item, Item::BackdropFilter { .. }) {
            list.items[..i]
                .iter()
                .zip(item_hashes)
                .zip(bounds)
                .filter(|((under, _), ub)| under.is_drawing() && ub.intersects(b))
                .fold(h, |acc, ((_, &uh), _)| hash_combine(acc, uh))
        } else {
            h
        };
        if !item.is_drawing() {
            for t in tiles.iter_mut() {
                *t = hash_combine(*t, h);
            }
            continue;
        }
        if b.is_empty() {
            continue;
        }
        let range = |lo: f32, hi: f32, n: u32| {
            let a = (lo / size).floor().max(0.0) as u32;
            let z = ((hi / size).ceil().max(0.0) as u32).min(n);
            a..z
        };
        for ty in range(b.y, b.y + b.height, rows) {
            for tx in range(b.x, b.x + b.width, columns) {
                let t = &mut tiles[(ty * columns + tx) as usize];
                *t = hash_combine(*t, h);
            }
        }
    }
    tiles
}
