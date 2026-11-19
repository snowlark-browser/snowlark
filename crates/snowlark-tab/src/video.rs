// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Video layers: each `<video>` frame in a tab's display list ([`Item::Video`]) becomes a quad the
//! compositor fills with the video's latest GPU frame at the display's rate, with a "patch" over it:
//! what the page paints over the video (its controls, boxes overlapping it), rasterized into a
//! small transparent frame of its own and drawn after the frame. The tiles under the quad hold the
//! video's box as painted (black where the frame goes), so a frame that is late or missing shows
//! black, as a video element does before its first frame. This is the C# engine's design
//! (`Gpu/PageRasterizer.cs`).

use snowlark_paint::{
    DisplayList, Item, Matrix, Rect, Sources, TileRasterizer, TiledFrame, VideoId, Viewport,
};

/// A video in a raster's device coordinates (the list's, at the raster's scale).
pub struct VideoQuad {
    pub video: VideoId,
    /// Where the frame goes (its `object-fit` rectangle).
    pub dest: Rect,
    /// What of it shows (its content box, cut by the clips around it and the raster's bounds).
    pub clip: Rect,
    /// What the page paints over the video, and where its pixel (0, 0) is.
    pub patch: Option<(TiledFrame, (i32, i32))>,
}

/// The video items of a list: (index, video, device dest, device visible rect). Videos under a
/// transform that is not a translation or scale are left to the tiles.
pub fn video_items(list: &DisplayList, scale: f32) -> Vec<(usize, VideoId, Rect, Rect)> {
    let mut out = Vec::new();
    if !list.items.iter().any(|i| matches!(i, Item::Video { .. })) {
        return out;
    }
    let mut transforms = vec![Matrix::scale(scale, scale)];
    let mut clips = vec![Rect::infinite()];
    for (i, item) in list.items.iter().enumerate() {
        let m = *transforms.last().unwrap();
        let c = *clips.last().unwrap();
        match item {
            Item::PushClipRect { rect }
            | Item::PushClipRoundedRect { rect, .. }
            | Item::PushClipPath { bounds: rect, .. } => {
                clips.push(m.apply_rect(rect).intersect(&c));
            }
            Item::PushMask { bounds, .. } => clips.push(m.apply_rect(bounds).intersect(&c)),
            Item::PopClip | Item::PopMask => {
                clips.pop();
            }
            Item::PushTransform { matrix } => transforms.push(matrix.then(&m)),
            Item::PopTransform => {
                transforms.pop();
            }
            Item::Video {
                video, dest, clip, ..
            } => {
                if !m.is_axis_aligned() {
                    continue;
                }
                let d = m.apply_rect(dest);
                let visible = m.apply_rect(clip).intersect(&d).intersect(&c);
                if !visible.is_empty() {
                    out.push((i, *video, d, visible));
                }
            }
            _ => {}
        }
    }
    out
}

/// What paints over the video at `index`: the list without the drawing items up to it (nor any
/// video), keeping its groups balanced.
pub fn patch_list(list: &DisplayList, index: usize) -> DisplayList {
    let mut patch = list.clone();
    patch.items = list
        .items
        .iter()
        .enumerate()
        .filter(|(i, item)| {
            !matches!(item, Item::Video { .. }) && (*i > index || !item.is_drawing())
        })
        .map(|(_, item)| *item)
        .collect();
    patch
}

/// The list to raster the tiles under the videos from: `list` without the drawing items after a
/// video that lie wholly inside the part of it that shows (at `scale`), which the video's patch
/// draws over its frame, so a change to them is rastered once, in the patch, and not again into
/// the tiles where the frame hides it (what overlaps a video is a layer of its own, with tiles of its
/// own). Items in a filter group stay (a filter can spread them past their bounds).
/// None when nothing is left out.
pub fn under_videos(list: &DisplayList, scale: f32) -> Option<DisplayList> {
    let videos = video_items(list, scale);
    if videos.is_empty() {
        return None;
    }
    let mut item_bounds = Vec::new();
    list.item_bounds(&Matrix::scale(scale, scale), &mut item_bounds);
    let inside = |outer: &Rect, b: &Rect| {
        b.x >= outer.x
            && b.y >= outer.y
            && b.right() <= outer.right()
            && b.bottom() <= outer.bottom()
    };
    let mut filters = 0u32;
    let mut keep = vec![true; list.items.len()];
    let mut dropped = false;
    for (i, (item, b)) in list.items.iter().zip(&item_bounds).enumerate() {
        match item {
            Item::PushFilter { .. } => filters += 1,
            Item::PopFilter => filters = filters.saturating_sub(1),
            _ => {}
        }
        if filters > 0 || !item.is_drawing() || matches!(item, Item::Video { .. }) || b.is_empty() {
            continue;
        }
        if videos
            .iter()
            .any(|(index, _, _, visible)| *index < i && inside(visible, b))
        {
            keep[i] = false;
            dropped = true;
        }
    }
    if !dropped {
        return None;
    }
    let mut under = list.clone();
    let mut i = 0;
    under.items.retain(|_| {
        i += 1;
        keep[i - 1]
    });
    Some(under)
}

/// Where a video's patch keeps its tiles from one raster to the next ([`TileRasterizer::raster_piece`]):
/// by the video and the piece of the frame it is in (`piece`: 0 for the full frame).
fn patch_slot(video: VideoId, piece: u64) -> u64 {
    (0x5649_4445_4F00_0000 ^ u64::from(video.0)).rotate_left(23)
        ^ piece.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// The video quads of a list rasterized into `view` (with pixel (0, 0) at `origin` of the list's
/// device coordinates), with their patches rasterized by `raster`. A patch keeps its tiles for the
/// next raster under a slot of its video's and `piece`'s (the frame's piece the list is: 0 for the
/// full frame), so only its tiles that changed are painted again.
pub fn quads(
    raster: &mut TileRasterizer,
    list: &DisplayList,
    sources: Sources,
    view: &Viewport,
    origin: (i32, i32),
    piece: u64,
) -> Vec<VideoQuad> {
    let mut out = Vec::new();
    let bounds = Rect::new(
        origin.0 as f32,
        origin.1 as f32,
        view.width as f32,
        view.height as f32,
    );
    let items = video_items(list, view.scale);
    if items.is_empty() {
        return out;
    }
    let mut item_bounds = Vec::new();
    list.item_bounds(&Matrix::scale(view.scale, view.scale), &mut item_bounds);
    for (index, video, dest, visible) in items {
        let visible = visible.intersect(&bounds);
        if visible.is_empty() {
            continue;
        }
        let x0 = visible.x.floor();
        let y0 = visible.y.floor();
        let w = ((visible.x + visible.width).ceil() - x0).max(1.0) as u32;
        let h = ((visible.y + visible.height).ceil() - y0).max(1.0) as u32;
        // Only when something after the video draws over it.
        let over = list
            .items
            .iter()
            .zip(&item_bounds)
            .enumerate()
            .any(|(i, (item, b))| {
                i > index
                    && item.is_drawing()
                    && !matches!(item, Item::Video { .. })
                    && b.intersects(&visible)
            });
        let patch = if over {
            let patch_list = patch_list(list, index);
            let patch_view = Viewport {
                width: w,
                height: h,
                scale: view.scale,
                background: snowlark_paint::Color::TRANSPARENT,
            };
            raster
                .raster_piece(
                    &patch_list,
                    sources,
                    &patch_view,
                    (x0 as i32, y0 as i32),
                    patch_slot(video, piece),
                )
                .ok()
                .map(|f| (f, (x0 as i32, y0 as i32)))
        } else {
            None
        };
        out.push(VideoQuad {
            video,
            dest,
            clip: visible,
            patch,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use snowlark_paint::{Color, DisplayListBuilder, ImageId};

    #[test]
    fn videos_and_what_paints_over_them() {
        let mut b = DisplayListBuilder::new();
        b.fill_rect(Rect::new(0.0, 0.0, 400.0, 400.0), Color::WHITE);
        b.push_clip_rect(Rect::new(0.0, 0.0, 300.0, 300.0));
        b.push_transform(Matrix::translation(10.0, 20.0));
        b.video(
            VideoId(7),
            Some(ImageId(1)),
            Rect::new(0.0, 0.0, 320.0, 180.0),
            Rect::new(0.0, 0.0, 320.0, 180.0),
        );
        b.fill_rect(Rect::new(0.0, 150.0, 320.0, 30.0), Color::BLACK);
        b.pop_transform();
        b.pop_clip();
        let list = b.build();
        let v = video_items(&list, 2.0);
        assert_eq!(v.len(), 1);
        let (index, id, dest, visible) = v[0];
        assert_eq!(id, VideoId(7));
        assert_eq!(dest, Rect::new(20.0, 40.0, 640.0, 360.0));
        // (cut by the clip at 300×300 CSS px: 600 device px)
        assert_eq!(visible, Rect::new(20.0, 40.0, 580.0, 360.0));
        let patch = patch_list(&list, index);
        assert!(patch.validate().is_ok());
        let drawn: Vec<_> = patch.items.iter().filter(|i| i.is_drawing()).collect();
        assert_eq!(drawn.len(), 1);
        assert!(matches!(drawn[0], Item::FillRect { color, .. } if *color == Color::BLACK));
    }

    /// What lies wholly over the shown part of a video is left out of the tiles under it (its patch
    /// draws it); what reaches past it, what is under the video, and what a filter can spread stay.
    #[test]
    fn the_tiles_under_a_video_leave_out_what_its_patch_draws() {
        let mut b = DisplayListBuilder::new();
        b.fill_rect(Rect::new(0.0, 0.0, 400.0, 400.0), Color::WHITE);
        b.fill_rect(Rect::new(20.0, 20.0, 10.0, 10.0), Color::BLACK);
        b.video(
            VideoId(3),
            None,
            Rect::new(0.0, 0.0, 320.0, 180.0),
            Rect::new(0.0, 0.0, 320.0, 180.0),
        );
        let controls = Color::rgb(1, 2, 3);
        b.fill_rect(Rect::new(10.0, 150.0, 100.0, 20.0), controls);
        b.fill_rect(Rect::new(300.0, 170.0, 50.0, 50.0), Color::BLACK);
        let list = b.build();
        let under = under_videos(&list, 1.5).expect("something left out");
        assert!(under.validate().is_ok());
        assert_eq!(under.items.len(), list.items.len() - 1);
        assert!(!under
            .items
            .iter()
            .any(|i| matches!(i, Item::FillRect { color, .. } if *color == controls)));
        // (nothing over the video: nothing to leave out)
        let mut b = DisplayListBuilder::new();
        b.video(
            VideoId(3),
            None,
            Rect::new(0.0, 0.0, 320.0, 180.0),
            Rect::new(0.0, 0.0, 320.0, 180.0),
        );
        b.fill_rect(Rect::new(300.0, 170.0, 50.0, 50.0), Color::BLACK);
        assert!(under_videos(&b.build(), 1.0).is_none());
    }

    #[test]
    fn rotated_videos_stay_in_the_tiles() {
        let mut b = DisplayListBuilder::new();
        b.push_transform(Matrix::rotation(0.5));
        b.video(
            VideoId(1),
            None,
            Rect::new(0.0, 0.0, 10.0, 10.0),
            Rect::new(0.0, 0.0, 10.0, 10.0),
        );
        b.pop_transform();
        assert!(video_items(&b.build(), 1.0).is_empty());
    }
}
