// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A WebGL canvas's frames to the compositor without a readback (a renderer process's window).
//!
//! The browser makes shared textures for the canvas as it does for a video of the renderer's
//! (`snowlark_paint::video`: [`VideoStore::canvas_export`] gives the canvas an id among the
//! renderer's videos), the WebGL service copies each frame into a free one on the GPU
//! ([`Backend::present_shared`]), and the canvas paints as that video's item
//! ([`snowlark_paint::Item::Video`], transparent): the compositor draws its newest frame over the
//! tiles, and neither a readback nor a repaint of the canvas's tiles happens per frame. A canvas the
//! compositor cannot draw as such a quad where the page put it (in an opacity, filter or mask group,
//! a shaped clip, or turned: [`DisplayList::video_quads`]) leaves the shared textures for good, and
//! shows the last frame presented there as its bitmap from then on, as before.

use std::sync::Arc;

use snowlark_dom::NodeId;
use snowlark_gl::{SharedPresent, SharedTargets};
use snowlark_paint::video::VideoExport;
use snowlark_paint::{DisplayList, Item, VideoId};

use super::{to_frame, CanvasOwner, Engine};
use snowlark_bindings::rt::Handle;

/// A canvas's way to the browser's shared textures.
pub struct SharedOut {
    video: VideoId,
    export: Arc<dyn VideoExport>,
    /// The pool the service has, and its size.
    generation: u32,
    width: u32,
    height: u32,
}

fn trace(what: &str) {
    if std::env::var("SNOWLARK_TRACE").is_ok_and(|v| v == "1") {
        eprintln!("[webgl] {what}");
    }
}

impl Engine {
    /// Presents the canvas `el`'s context `h` into the browser's shared textures. False when its
    /// frame must be read back instead (no renderer process's store, a context the service cannot
    /// copy, no textures, or the canvas left them).
    pub(super) fn present_webgl_shared(&self, h: Handle, el: NodeId) -> bool {
        let (id, attrs, w, hh, off, has) = self.gl_ctx(h, |c| {
            (
                c.id,
                c.attributes,
                c.width.max(1),
                c.height.max(1),
                c.shared_off,
                c.shared.as_ref().map(|s| (s.width, s.height)),
            )
        });
        if off || !attrs.webgl2 || !attrs.premultiplied_alpha {
            return false;
        }
        if has != Some((w, hh)) && !self.webgl_share(h, el, w, hh) {
            return false;
        }
        let presented = match self.gl_backend() {
            Some(mut b) => b.present_shared(id),
            None => return false,
        };
        match presented {
            SharedPresent::Presented(index) => {
                let (video, export, generation) = self.gl_ctx(h, |c| {
                    let s = c.shared.as_ref().expect("shared above");
                    (s.video, s.export.clone(), s.generation)
                });
                export.frame(video, generation, index);
                self.with_node_renderer(el, |r, st| {
                    if !r.has_canvas_video(el) {
                        r.set_canvas_video(el, Some(video));
                        st.dirty = true;
                    }
                });
                true
            }
            SharedPresent::Busy => {
                // (every texture is still the browser's: the frame waits for the next update, its
                // drawing buffer untouched)
                let mut gl = self.gl.borrow_mut();
                let owner = CanvasOwner::Element(el);
                if !gl.dirty.contains(&owner) {
                    gl.dirty.push(owner);
                }
                drop(gl);
                self.render.borrow_mut().dirty = true;
                true
            }
            SharedPresent::Unavailable => {
                trace("a canvas's frame does not reach the shared textures: read back");
                self.webgl_leave_shared(h, el);
                false
            }
        }
    }

    /// Gives the context `h` shared textures of `w` × `hh` (the first, or after a new size). False
    /// when there are none for it: it reads back from then on.
    fn webgl_share(&self, h: Handle, el: NodeId, w: u32, hh: u32) -> bool {
        let id = self.gl_ctx(h, |c| c.id);
        let had = self.gl_ctx(h, |c| {
            c.shared.as_ref().map(|s| (s.video, s.export.clone()))
        });
        let had_textures = had.is_some();
        let (video, export) = match had {
            Some(v) => v,
            None => {
                let videos = self.media.borrow().config.videos.clone();
                match videos.and_then(|v| v.canvas_export()) {
                    Some(v) => v,
                    None => {
                        self.gl_ctx(h, |c| c.shared_off = true);
                        return false;
                    }
                }
            }
        };
        let opened = export.pool(video, w, hh).and_then(|pool| {
            let targets = SharedTargets {
                width: w,
                height: hh,
                handles: pool.handles,
            };
            let ok = self
                .gl_backend()
                .is_some_and(|mut b| b.share_into(id, Some(targets)));
            ok.then_some(pool.generation)
        });
        match opened {
            Some(generation) => {
                self.gl_ctx(h, |c| {
                    c.shared = Some(SharedOut {
                        video,
                        export,
                        generation,
                        width: w,
                        height: hh,
                    })
                });
                true
            }
            None => {
                trace("no shared textures for a canvas: read back");
                if had_textures {
                    self.webgl_leave_shared(h, el);
                } else {
                    // (a pool the browser made, if any, goes with the id)
                    export.removed(video);
                    self.gl_ctx(h, |c| c.shared_off = true);
                }
                false
            }
        }
    }

    /// The canvas `el` leaves the shared textures for good: it shows the last frame presented there
    /// as its bitmap, and its frames are read back from now on.
    fn webgl_leave_shared(&self, h: Handle, el: NodeId) {
        let (id, attrs, shared) = self.gl_ctx(h, |c| {
            c.shared_off = true;
            (c.id, c.attributes, c.shared.take())
        });
        let Some(s) = shared else {
            return;
        };
        let last = self.gl_backend().and_then(|mut b| {
            let last = b.read_presented(id);
            b.share_into(id, None);
            last
        });
        s.export.removed(s.video);
        self.with_node_renderer(el, |r, st| {
            r.set_canvas_video(el, None);
            st.dirty = true;
        });
        if let Some(rb) = last {
            self.show_canvas_frame(el, Some(to_frame(rb, &attrs)));
        }
        self.render.borrow_mut().dirty = true;
    }

    /// After a paint: the canvases whose frames `list` puts where a compositor cannot draw them as
    /// quads leave the shared textures (they show their bitmap from the next update).
    pub(crate) fn canvases_off_quads(&self, list: &DisplayList) {
        let shared: Vec<(VideoId, NodeId, Handle)> = {
            let gl = self.gl.borrow();
            gl.contexts
                .iter()
                .filter_map(|(&el, &h)| {
                    let v = self.gl_ctx(h, |c| c.shared.as_ref().map(|s| s.video))?;
                    Some((v, el, h))
                })
                .collect()
        };
        if shared.is_empty() {
            return;
        }
        for (i, ok) in list.video_quads() {
            if ok {
                continue;
            }
            if let Item::Video {
                video,
                transparent: true,
                ..
            } = list.items[i]
            {
                if let Some(&(_, el, h)) = shared.iter().find(|(v, _, _)| *v == video) {
                    trace("a canvas's frames are where a quad cannot show them: read back");
                    self.webgl_leave_shared(h, el);
                }
            }
        }
    }
}
