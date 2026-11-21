// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! How painted content is split into pieces a compositor places as the viewport scrolls
//! (CSS Positioned Layout 3 §3.4, sticky positioning): a sticky box clipped by boxes that scroll
//! with the document is a piece of its own, drawn through their clips.

use std::sync::Arc;
use std::time::Duration;

use snowlark_net::{NetConfig, NetThread, Url};
use snowlark_paint::{Item, LayerKind, Point, Rect, Size, StateOp, TextEngine};
use snowlark_render::Renderer;
use snowlark_style::DeviceConfig;

const VIEWPORT: Size = Size {
    width: 400.0,
    height: 300.0,
};

fn split(body: &str) -> snowlark_paint::SplitList {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut r = Renderer::new(
        net.client(),
        Arc::new(TextEngine::new().unwrap()),
        DeviceConfig {
            viewport: (VIEWPORT.width, VIEWPORT.height),
            scripting: false,
            ..Default::default()
        },
    );
    let html = format!("<!DOCTYPE html><style>body {{ margin: 0 }}</style>{body}");
    let url = Url::parse("http://layers.test/").unwrap();
    r.load_html(html.as_bytes(), &url, None);
    assert!(r.render_until_settled(Duration::from_secs(20)).complete);
    r.update_rendering(VIEWPORT);
    let list = r.paint_with_embeds(VIEWPORT, Point::ZERO, 1.0, None, true);
    list.split_layers()
}

/// A sticky box inside two `overflow: clip` sections (a scroll-driven media section's shape): a
/// sticky piece drawn through the sections' clip, which stays with the document while the piece
/// moves within it, and the page is not pinned (painted again at every scroll position, its sticky
/// content off by the distance scrolled until then).
#[test]
fn a_sticky_box_in_clips_that_scroll_is_a_piece_drawn_through_them() {
    let split = split(
        "<div style='height: 100px'></div>
         <section style='overflow: clip; height: 900px'>
           <div style='overflow: clip; margin: 0 20px'>
             <div style='height: 600px'>
               <div style='position: sticky; top: 0; height: 200px; background: red'>
                 <div style='position: relative; overflow: clip; height: 100px'>
                   <div style='position: relative; height: 300px; background: blue'></div>
                 </div>
               </div>
             </div>
           </div>
         </section>",
    );
    assert!(!split.pinned);
    let sticky: Vec<_> = split
        .layers
        .iter()
        .filter(|l| matches!(l.kind, Some(LayerKind::Sticky(_))))
        .collect();
    // (the box, and each positioned box in it that paints something, a member of the page's
    // stacking context)
    assert_eq!(sticky.len(), 2);
    for piece in &sticky {
        // (the inner section's clip: its padding box, inside the outer one's)
        assert_eq!(
            piece.state.0,
            [StateOp::Clip(Rect::new(20.0, 100.0, 360.0, 600.0))]
        );
    }
    let fills = |piece: &snowlark_paint::SplitLayer| -> Vec<Rect> {
        piece
            .list
            .items
            .iter()
            .filter_map(|i| match i {
                Item::FillRect { rect, .. } => Some(*rect),
                _ => None,
            })
            .collect()
    };
    let clips = |piece: &snowlark_paint::SplitLayer| -> Vec<Rect> {
        piece
            .list
            .items
            .iter()
            .filter_map(|i| match i {
                Item::PushClipRect { rect } => Some(*rect),
                _ => None,
            })
            .collect()
    };
    // The sticky box, with no clip of the sections in its own list.
    assert_eq!(fills(sticky[0]), [Rect::new(20.0, 100.0, 360.0, 200.0)]);
    assert!(clips(sticky[0]).is_empty());
    // The blue box, in the clip of its parent, which moves with the sticky box.
    let blue = sticky
        .iter()
        .find(|p| fills(p) == [Rect::new(20.0, 100.0, 360.0, 300.0)])
        .expect("the blue box's piece");
    assert_eq!(clips(blue), [Rect::new(20.0, 100.0, 360.0, 100.0)]);
}

/// The same box in a clip that a transform moves stays where it is painted, as before: the page is
/// pinned.
#[test]
fn a_sticky_box_in_a_transformed_clip_stays_pinned() {
    let split = split(
        "<section style='overflow: clip; height: 900px; transform: translateX(1px)'>
           <div style='height: 600px'>
             <div style='position: sticky; top: 0; height: 200px; background: red'></div>
           </div>
         </section>",
    );
    assert!(split.pinned);
}
