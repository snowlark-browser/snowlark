// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The page drives the renderer (`snowlark-render`): each rendered document gets a [`Renderer`] over the
//! engine's shared DOM: the top-level browsing context's active document, and the active document of
//! every child navigable whose container (`iframe`, `frame`, `object`, `embed`) is in a rendered
//! document. The DOM's reactions reach every renderer as they are drained
//! ([`Engine::render_dom_changed`]; each keeps its own document's), `<link>` sheets the loader fetched
//! are handed to the renderer of their document, and the event loop's "update the rendering" step
//! (HTML §8.1.7.3) runs the resize and scroll steps of each document, the animation frame callbacks
//! and then each renderer's style and layout update in tree order ([`Engine::update_the_rendering`]):
//! a frame's viewport is its container's content box in its parent's layout. Script-driven reads
//! (geometry, `getComputedStyle`) force style and layout with [`Engine::with_layout`].
//!
//! Painting nests: a frame's display list (its viewport scrolled by its own scroll position) is
//! painted into its container's content box in the parent's list ([`Engine::paint`]); the renderers
//! of one page share one image store so the lists can nest. Input finds the innermost document under
//! a point ([`Engine::hit_element`]).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use snowlark_bindings::rt::Cx;
use snowlark_dom::{NodeId, Reaction};
use snowlark_net::NetClient;
use snowlark_paint::{DisplayList, ImageStore, Point, Rect, Size, TextEngine};
use snowlark_render::{Renderer, TimelineClock};
use snowlark_style::DeviceConfig;

use crate::engine::{node_handle, Engine};
use crate::event_loop::TaskSource;

/// What the page needs to render its documents.
#[derive(Clone)]
pub struct RenderSetup {
    /// The network the renderer fetches images, fonts and `@import`s through (the page's own when
    /// `None`).
    pub net: Option<NetClient>,
    /// The text engine (web fonts register here).
    pub text: Arc<TextEngine>,
    /// Device pixels per CSS px.
    pub device_pixel_ratio: f32,
    /// The width (CSS px) of the classic vertical scroll bar the embedder shows at the top-level
    /// viewport's edge when the document overflows it (0: overlay scroll bars, which take no room).
    pub scrollbar_width: f32,
    /// Logs resource arrivals.
    pub trace: bool,
    /// The user prefers a dark color scheme (`prefers-color-scheme`).
    pub dark: bool,
}

impl RenderSetup {
    /// A setup with its own text engine, at 1 device pixel per CSS px.
    pub fn new() -> Result<RenderSetup, String> {
        Ok(RenderSetup {
            net: None,
            text: Arc::new(TextEngine::new().map_err(|e| e.to_string())?),
            device_pixel_ratio: 1.0,
            scrollbar_width: 0.0,
            trace: false,
            dark: false,
        })
    }
}

/// A rendered document's state (the top-level document's lives in `Engine::render`, the frames' in
/// `Engine::frame_renders`).
#[derive(Default)]
pub struct RenderState {
    pub setup: Option<RenderSetup>,
    /// The document's renderer.
    /// (boxed: borrowing it moves it out of the state and back, a pointer rather than the renderer)
    pub renderer: Option<Box<Renderer>>,
    pub document: Option<NodeId>,
    /// The frame element showing the document (`None` for the top-level document).
    pub container: Option<NodeId>,
    /// The viewport (the initial containing block), CSS px.
    pub viewport: Size,
    /// How far the viewport is scrolled.
    pub scroll: Point,
    /// Scroll positions of scroll containers.
    pub element_scroll: std::collections::HashMap<NodeId, Point>,
    /// The document may look different: a rendering update is due.
    pub dirty: bool,
    /// A rendering update produced new fragments: paint again.
    pub needs_paint: bool,
    /// The viewport changed size since the last rendering update (HTML "run the resize steps").
    pub resized: bool,
    /// HTML "pending scroll event targets": the document (for the viewport) or elements.
    pub pending_scroll: Vec<NodeId>,
    /// CSSOM View "pending scrollend event targets": scrolls that completed.
    pub pending_scrollend: Vec<NodeId>,
    /// Smooth scrolls in progress (CSSOM View §4 "perform a scroll" with smooth behavior).
    pub(crate) smooth: Vec<crate::engine::view::SmoothScroll>,
    /// Script scrolled the viewport to this position; the embedder moves its view there.
    pub scroll_request: Option<Point>,
    /// The images of the page's renderers (made with the top-level one).
    pub images: Option<Arc<ImageStore>>,
    /// How the renderers' document timelines read the time (the top-level state's, for every
    /// renderer; `None`: the monotonic clock).
    pub clock: Option<TimelineClock>,
    /// The embedder hides the page (the top-level state's; see [`Engine::set_shown`]).
    pub hidden: bool,
    /// The visibility state of documents whose "update the visibility state" ran or is queued (the
    /// top-level state's; see [`Engine::document_visible`]): true for "visible".
    pub visibility: rustc_hash::FxHashMap<NodeId, bool>,
}

impl RenderState {
    /// The part of the document the user sees, for its animated images: the viewport, or nothing
    /// while the page is hidden.
    fn animation_view(&self, hidden: bool) -> Option<Rect> {
        Some(if hidden {
            Rect::ZERO
        } else {
            Rect::new(
                self.scroll.x,
                self.scroll.y,
                self.viewport.width,
                self.viewport.height,
            )
        })
    }

    fn wants_update(&self) -> bool {
        self.dirty
            || self.resized
            || !self.pending_scroll.is_empty()
            || !self.pending_scrollend.is_empty()
            || !self.smooth.is_empty()
            || self.renderer.as_ref().is_some_and(|r| r.animating())
    }
}

/// Trades what is a document's own between two rendering states (the page-wide setup, images,
/// clock and visibility stay with the top-level state): [`Engine::begin_view`].
fn swap_document_state(a: &mut RenderState, b: &mut RenderState) {
    std::mem::swap(&mut a.renderer, &mut b.renderer);
    std::mem::swap(&mut a.document, &mut b.document);
    std::mem::swap(&mut a.container, &mut b.container);
    std::mem::swap(&mut a.viewport, &mut b.viewport);
    std::mem::swap(&mut a.scroll, &mut b.scroll);
    std::mem::swap(&mut a.element_scroll, &mut b.element_scroll);
    std::mem::swap(&mut a.dirty, &mut b.dirty);
    std::mem::swap(&mut a.needs_paint, &mut b.needs_paint);
    std::mem::swap(&mut a.resized, &mut b.resized);
    std::mem::swap(&mut a.pending_scroll, &mut b.pending_scroll);
    std::mem::swap(&mut a.pending_scrollend, &mut b.pending_scrollend);
    std::mem::swap(&mut a.smooth, &mut b.smooth);
    std::mem::swap(&mut a.scroll_request, &mut b.scroll_request);
}

/// A hit in a document: the topmost element and the point in its document's viewport.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DocumentHit {
    pub element: NodeId,
    pub point: Point,
    /// For a replaced element (a form control): the point from its content box's origin.
    pub content: Option<Point>,
    /// Whether the point is on text, not just in `element`.
    pub text: bool,
}

impl Engine {
    /// The renderer of the top-level document, with the rendering state (`None` without rendering).
    pub fn with_renderer<R>(
        &self,
        f: impl FnOnce(&mut Renderer, &mut RenderState) -> R,
    ) -> Option<R> {
        let mut st = self.render.borrow_mut();
        let mut r = st.renderer.take()?;
        let out = f(&mut r, &mut st);
        st.renderer = Some(r);
        Some(out)
    }

    /// The renderer of a rendered document (the top-level one or a frame's).
    pub fn with_doc_renderer<R>(
        &self,
        doc: NodeId,
        f: impl FnOnce(&mut Renderer, &mut RenderState) -> R,
    ) -> Option<R> {
        if self.render.borrow().document == Some(doc) {
            return self.with_renderer(f);
        }
        let mut frames = self.frame_renders.borrow_mut();
        let st = frames.get_mut(&doc)?;
        let mut r = st.renderer.take()?;
        let out = f(&mut r, st);
        st.renderer = Some(r);
        Some(out)
    }

    /// The renderer of the document `node` is in.
    pub(crate) fn with_node_renderer<R>(
        &self,
        node: NodeId,
        f: impl FnOnce(&mut Renderer, &mut RenderState) -> R,
    ) -> Option<R> {
        let doc = {
            let dom = self.dom();
            if !dom.is_alive(node) {
                return None;
            }
            dom.node_document(node)
        };
        self.with_doc_renderer(doc, f)
    }

    /// Runs `f` with every renderer (the top-level document's first).
    pub(crate) fn for_each_renderer(&self, mut f: impl FnMut(&mut Renderer, &mut RenderState)) {
        self.with_renderer(&mut f);
        let mut frames = self.frame_renders.borrow_mut();
        for st in frames.values_mut() {
            if let Some(mut r) = st.renderer.take() {
                f(&mut r, st);
                st.renderer = Some(r);
            }
        }
    }

    /// Whether `doc` is a rendered document (the top-level one or a frame's).
    pub(crate) fn is_rendered_document(&self, doc: NodeId) -> bool {
        self.render.borrow().document == Some(doc) || self.frame_renders.borrow().contains_key(&doc)
    }

    /// The top-level rendered document.
    pub(crate) fn rendered_top(&self) -> Option<NodeId> {
        self.render.borrow().document
    }

    /// The frame element whose content `doc` is, for a rendered frame document.
    pub(crate) fn rendered_container(&self, doc: NodeId) -> Option<NodeId> {
        self.frame_renders
            .borrow()
            .get(&doc)
            .and_then(|s| s.container)
    }

    /// The rendered frame documents shown by elements of `doc`, in tree order of their containers:
    /// (container, document).
    pub(crate) fn rendered_frames_of(&self, doc: NodeId) -> Vec<(NodeId, NodeId)> {
        let frames = self.frame_renders.borrow();
        if frames.is_empty() {
            return Vec::new();
        }
        let dom = self.dom();
        let mut out: Vec<(NodeId, NodeId)> = frames
            .iter()
            .filter_map(|(&d, st)| {
                let c = st.container?;
                (dom.is_alive(c) && dom.is_connected(c) && dom.node_document(c) == doc)
                    .then_some((c, d))
            })
            .collect();
        if out.len() > 1 {
            // (by the containers' ancestors, not a walk of the document: this runs several times
            // per rendering update)
            out.sort_by(|(a, _), (b, _)| dom.shadow_including_tree_order(*a, *b));
        }
        out
    }

    /// Every rendered document in tree order (a document before the frames it shows): the page's
    /// own top-level document's tree, then that of each other top-level traversable it hosts (an
    /// auxiliary browsing context), oldest first.
    /// The shown document (the page's own, or the view's while one is selected:
    /// [`Engine::begin_view`]) and the frame documents inside it, in tree order: what the find box
    /// searches.
    pub(crate) fn shown_documents(&self) -> Vec<NodeId> {
        self.rendered_top()
            .map(|top| self.rendered_tree(top))
            .unwrap_or_default()
    }

    pub(crate) fn rendered_documents(&self) -> Vec<NodeId> {
        let Some(top) = self.rendered_top() else {
            return Vec::new();
        };
        let mut out = self.rendered_tree(top);
        let mut popups: Vec<(usize, NodeId)> = {
            let frames = self.frame_renders.borrow();
            frames
                .iter()
                .filter(|(_, s)| s.container.is_none())
                .filter_map(|(&d, _)| Some((self.context_of_document(d)?, d)))
                .collect()
        };
        popups.sort_unstable();
        for (_, d) in popups {
            out.extend(self.rendered_tree(d));
        }
        out
    }

    /// The rendered document `root` and the frame documents inside it, in tree order.
    fn rendered_tree(&self, root: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(d) = stack.pop() {
            out.push(d);
            let kids = self.rendered_frames_of(d);
            stack.extend(kids.into_iter().rev().map(|(_, k)| k));
        }
        out
    }

    fn new_renderer(&self, st: &RenderState, viewport: Size) -> Option<Renderer> {
        let setup = st.setup.clone()?;
        let net = setup.net.clone().or(self.hooks.borrow().net.clone())?;
        let screen = self.hooks.borrow().viewport;
        let config = DeviceConfig {
            viewport: (viewport.width, viewport.height),
            // (the top-level viewport is the screen frames report; before it is known, this one)
            screen: (screen != (0, 0)).then_some((screen.0 as f32, screen.1 as f32)),
            device_pixel_ratio: setup.device_pixel_ratio,
            scripting: true,
            input: self.hooks.borrow().input,
            color_scheme: if setup.dark {
                snowlark_style::ColorScheme::Dark
            } else {
                snowlark_style::ColorScheme::Light
            },
            ..DeviceConfig::default()
        };
        let mut r = Renderer::new(net, setup.text.clone(), config);
        if let Some(clock) = st.clock {
            r.set_timeline_clock(clock);
        }
        r.set_trace(setup.trace);
        r.set_external_link_sheets(true);
        r.set_pdf_viewer(self.pdf_viewer_supported());
        let waker = self.waker();
        r.set_notify(Some(Arc::new(move || waker.wake())));
        Some(r)
    }

    /// The top-level browsing context's active document became `doc`: a renderer for it (and the
    /// previous document's frames are no longer rendered).
    pub(crate) fn render_attach(&self, doc: NodeId) {
        self.end_view();
        // (the top-level traversables this page hosts besides its own keep theirs)
        let popups: Vec<NodeId> = self
            .frame_renders
            .borrow()
            .iter()
            .filter(|(_, s)| s.container.is_none())
            .map(|(&d, _)| d)
            .collect();
        let keep: rustc_hash::FxHashSet<NodeId> = popups
            .into_iter()
            .flat_map(|d| self.rendered_tree(d))
            .collect();
        self.frame_renders
            .borrow_mut()
            .retain(|d, _| keep.contains(d));
        let mut st = self.render.borrow_mut();
        let Some(mut r) = self.new_renderer(&st, st.viewport) else {
            return;
        };
        // (the embedder's classic scroll bar is at the top-level viewport's edge only)
        r.set_scrollbar_width(st.setup.as_ref().map_or(0.0, |s| s.scrollbar_width));
        let images = st
            .images
            .get_or_insert_with(|| Arc::new(ImageStore::new()))
            .clone();
        // The old renderer frees its images before the new one adds any.
        st.renderer = None;
        r.share_images(images);
        r.set_pdf_viewer(self.pdf_viewer_supported() && !self.plugins_sandboxed(doc));
        r.attach(self.dom.clone(), doc);
        // (its timeline counts from the event loop's time origin, as rAF and performance.now do)
        r.set_time_origin(self.event_loop.borrow().origin);
        st.renderer = Some(Box::new(r));
        st.document = Some(doc);
        st.scroll = Point::ZERO;
        st.element_scroll.clear();
        st.pending_scroll.clear();
        st.pending_scrollend.clear();
        st.smooth.clear();
        st.scroll_request = None;
        st.dirty = true;
        st.needs_paint = true;
        drop(st);
        self.controls_resync.set(true);
        // (its fetches are the document's: its request client, which Resource Timing reports them
        // under, and its policy)
        self.render_policy_changed(doc);
    }

    /// A child navigable's active document became `doc`, shown by `container`: a renderer for it
    /// when the container is in a rendered document.
    pub(crate) fn render_attach_frame(&self, doc: NodeId, container: NodeId, old: Option<NodeId>) {
        if let Some(o) = old {
            self.frame_renders.borrow_mut().remove(&o);
        }
        let parent = self.dom().node_document(container);
        if !self.is_rendered_document(parent) {
            return;
        }
        let (r, images) = {
            let top = self.render.borrow();
            let Some(images) = top.images.clone() else {
                return;
            };
            let viewport = Size::new(300.0, 150.0);
            (self.new_renderer(&top, viewport), images)
        };
        let Some(mut r) = r else {
            return;
        };
        r.share_images(images);
        r.set_pdf_viewer(self.pdf_viewer_supported() && !self.plugins_sandboxed(doc));
        r.attach(self.dom.clone(), doc);
        // (its timeline counts from the event loop's time origin, as rAF and performance.now do)
        r.set_time_origin(self.event_loop.borrow().origin);
        let st = RenderState {
            renderer: Some(Box::new(r)),
            document: Some(doc),
            container: Some(container),
            viewport: Size::new(300.0, 150.0),
            dirty: true,
            needs_paint: true,
            resized: false,
            ..Default::default()
        };
        self.frame_renders.borrow_mut().insert(doc, st);
        self.controls_resync.set(true);
        self.render.borrow_mut().dirty = true;
        self.set_embedded_document(container, Some(doc));
        // (its fetches are the document's: its request client, which Resource Timing reports them
        // under, and its policy)
        self.render_policy_changed(doc);
    }

    /// Tells the renderer of an `object`'s or `embed`'s document which document the element's
    /// navigable shows (an SVG document's root gives the element its natural dimensions).
    fn set_embedded_document(&self, container: NodeId, doc: Option<NodeId>) {
        let embeds = {
            let dom = self.dom();
            dom.is_alive(container)
                && (dom.is_html_element_named(container, &snowlark_dom::local_name!("object"))
                    || dom.is_html_element_named(container, &snowlark_dom::local_name!("embed")))
        };
        if embeds {
            self.with_node_renderer(container, |r, st| {
                r.set_embedded_document(container, doc);
                st.dirty = true;
            });
        }
    }

    /// The active document of a top-level traversable this page hosts besides its own (an
    /// auxiliary browsing context) became `doc`: a renderer for it, with that traversable's
    /// viewport, when the page renders.
    pub(crate) fn render_attach_popup(
        &self,
        doc: NodeId,
        viewport: (f32, f32),
        old: Option<NodeId>,
    ) {
        if old.is_some() && self.view.get().is_some_and(|(d, _)| Some(d) == old) {
            self.end_view();
        }
        if let Some(o) = old {
            self.frame_renders.borrow_mut().remove(&o);
        }
        let viewport = Size::new(viewport.0, viewport.1);
        let (r, images) = {
            let top = self.render.borrow();
            let Some(images) = top.images.clone() else {
                return;
            };
            (self.new_renderer(&top, viewport), images)
        };
        let Some(mut r) = r else {
            return;
        };
        r.share_images(images);
        r.set_pdf_viewer(self.pdf_viewer_supported() && !self.plugins_sandboxed(doc));
        r.attach(self.dom.clone(), doc);
        r.set_time_origin(self.event_loop.borrow().origin);
        let st = RenderState {
            renderer: Some(Box::new(r)),
            document: Some(doc),
            container: None,
            viewport,
            dirty: true,
            needs_paint: true,
            resized: false,
            ..Default::default()
        };
        self.frame_renders.borrow_mut().insert(doc, st);
        self.controls_resync.set(true);
        self.render_policy_changed(doc);
    }

    /// Makes the hosted top-level traversable `window` the one the top-level calls address (its
    /// paint, viewport, scrolling, hit testing and input), until [`Engine::end_view`]: its rendering
    /// state and the page's own top-level document's trade places. False when it has none.
    pub fn begin_view(&self, window: crate::engine::HostedWindow) -> bool {
        let Some(doc) = self.hosted_window_document(window) else {
            return false;
        };
        if let Some((current, _)) = self.view.get() {
            if current == doc {
                return true;
            }
            self.end_view();
        }
        let Some(top) = self.rendered_top() else {
            return false;
        };
        let mut frames = self.frame_renders.borrow_mut();
        let Some(mut st) = frames.remove(&doc) else {
            return false;
        };
        swap_document_state(&mut self.render.borrow_mut(), &mut st);
        frames.insert(top, st);
        self.view.set(Some((doc, top)));
        true
    }

    /// Ends [`Engine::begin_view`]: the page's own top-level document's rendering state is back in
    /// place. (Any change of either document's renderer ends it first.)
    pub fn end_view(&self) {
        let Some((doc, top)) = self.view.take() else {
            return;
        };
        let mut frames = self.frame_renders.borrow_mut();
        let Some(mut st) = frames.remove(&top) else {
            return;
        };
        swap_document_state(&mut self.render.borrow_mut(), &mut st);
        frames.insert(doc, st);
    }

    /// Child navigables were destroyed: their documents (and the frames inside them) are no longer
    /// rendered.
    pub(crate) fn render_detach_documents(&self, docs: &[NodeId]) {
        if self
            .view
            .get()
            .is_some_and(|(d, t)| docs.contains(&d) || docs.contains(&t))
        {
            self.end_view();
        }
        let mut frames = self.frame_renders.borrow_mut();
        let mut any = false;
        let mut containers = Vec::new();
        for d in docs {
            if let Some(st) = frames.remove(d) {
                any = true;
                containers.extend(st.container);
            }
        }
        drop(frames);
        for c in containers {
            self.set_embedded_document(c, None);
        }
        if any {
            let mut st = self.render.borrow_mut();
            st.dirty = true;
            st.needs_paint = true;
        }
    }

    /// The DOM's reactions, as they are drained: the renderers' style invalidation and box tree
    /// changes.
    pub(crate) fn render_dom_changed(&self, reactions: &[Reaction]) {
        self.content_dom_changed(reactions);
        let mut settled = Vec::new();
        self.for_each_renderer(|r, st| {
            r.dom_changed(reactions);
            st.dirty = true;
            // (asked only when a sheet waits: this runs after every batch of DOM changes)
            if r.sheets_pending() {
                settled.extend(r.take_sheets_settled());
            }
        });
        for (el, outcome) in settled {
            self.queue_sheet_event(el, outcome);
        }
    }

    /// `slot.assign()` changed a slot's manually assigned nodes.
    pub(crate) fn render_slot_assigned(&self, slot: NodeId, nodes: Vec<NodeId>) {
        self.with_node_renderer(slot, |r, st| {
            r.slot_assigned(slot, nodes);
            st.dirty = true;
        });
    }

    /// A shadow root was attached to `host`.
    pub(crate) fn render_shadow_attached(&self, host: NodeId) {
        self.with_node_renderer(host, |r, st| {
            r.shadow_root_attached(host);
            st.dirty = true;
        });
    }

    /// A `<link rel=stylesheet>`'s sheet arrived (or failed): HTML "process the linked resource".
    /// Returns whether a renderer took it (and reports, through [`Engine::queue_sheet_event`],
    /// when the sheet's critical subresources are in).
    pub(crate) fn render_link_sheet(
        &self,
        link: NodeId,
        sheet: Option<(String, url::Url)>,
    ) -> bool {
        let (taken, settled) = self
            .with_node_renderer(link, |r, st| {
                st.dirty = true;
                (r.link_sheet_loaded(link, sheet), r.take_sheets_settled())
            })
            .unwrap_or_default();
        for (el, outcome) in settled {
            self.queue_sheet_event(el, outcome);
        }
        taken
    }

    /// A `<link>` stopped being a stylesheet link of the document.
    pub(crate) fn render_link_removed(&self, link: NodeId) {
        self.with_node_renderer(link, |r, st| {
            r.link_sheet_removed(link);
            st.dirty = true;
        });
    }

    /// Applies the renderers' arrived subresources. Returns whether any arrived.
    pub(crate) fn render_pump(&self) -> bool {
        let mut any = false;
        let mut settled = Vec::new();
        self.for_each_renderer(|r, st| {
            if r.pump().applied > 0 {
                st.dirty = true;
                any = true;
                settled.extend(r.take_sheets_settled());
            }
        });
        for (el, outcome) in settled {
            self.queue_sheet_event(el, outcome);
        }
        self.queue_image_events();
        // (an `object` or `embed` fetched as an image whose response is a document: HTML §4.8.7)
        let mut redecided = Vec::new();
        self.for_each_renderer(|r, _| redecided.extend(r.take_embed_redecided()));
        for (el, essence) in redecided {
            self.html.borrow_mut().embed_types.insert(el, essence);
            self.queue_task(
                crate::event_loop::TaskSource::DomManipulation,
                move |e, cx| e.frame_inserted(cx, el),
            );
        }
        // (HTML §4.8.7: an `object` fires `error` when its resource fails; an `embed` none)
        let mut settled = Vec::new();
        self.for_each_renderer(|r, _| settled.extend(r.take_embed_settled()));
        for (el, loaded) in settled {
            if loaded {
                self.queue_element_task_fire(el, "load", false, false);
            } else if self
                .dom()
                .is_html_element_named(el, &snowlark_dom::local_name!("object"))
            {
                self.queue_element_task_fire(el, "error", false, false);
            }
        }
        any
    }

    /// The image URLs the renderers fetched: the `img` elements' requests for them complete.
    fn queue_image_events(&self) {
        let mut arrived = Vec::new();
        let mut uses = Vec::new();
        self.for_each_renderer(|r, _| {
            arrived.extend(r.take_image_url_events());
            uses.extend(r.take_use_events());
        });
        self.image_urls_arrived(arrived);
        // SVG 2 §16.1.7: a `use` element's reference to another document was processed.
        for (el, ok) in uses {
            self.queue_element_task_fire(el, if ok { "load" } else { "error" }, false, false);
        }
    }

    /// Whether a renderer has fetches in flight.
    pub(crate) fn render_loading(&self) -> bool {
        let mut any = false;
        self.for_each_renderer(|r, _| any |= r.pending() > 0);
        any
    }

    /// HTML "render-blocked": the top-level document gets no rendering update (no animation frame
    /// callbacks either) while its render-blocking stylesheets load or it has no body yet.
    pub(crate) fn render_blocked(&self) -> bool {
        self.render
            .borrow()
            .renderer
            .as_ref()
            .is_some_and(|r| r.render_blocked())
    }

    /// Whether a rendering update is wanted (besides animation frame callbacks): a document changed,
    /// or an animated image in view shows another frame.
    pub(crate) fn render_wanted(&self) -> bool {
        let top = self.render.borrow();
        let hidden = top.hidden;
        let wanted = |s: &RenderState| {
            s.wants_update()
                || s.renderer
                    .as_ref()
                    .is_some_and(|r| r.image_frame_due(s.animation_view(hidden)))
        };
        wanted(&top)
            || self.frame_renders.borrow().values().any(wanted)
            || self.fullscreen_events_pending()
    }

    /// When an animated image in view next shows another frame (HTML §4.8.4.3; images out of view
    /// and those of a hidden page wait, as Chrome pauses them): the event loop wakes then.
    pub(crate) fn image_frame_wakeup(&self) -> Option<std::time::Instant> {
        let top = self.render.borrow();
        if top.hidden {
            return None;
        }
        let next = |s: &RenderState| {
            s.renderer
                .as_ref()?
                .next_image_frame(s.animation_view(false))
        };
        let frames = self.frame_renders.borrow();
        std::iter::once(next(&top))
            .chain(frames.values().map(next))
            .flatten()
            .min()
    }

    /// The embedder shows the page, or hides it (a background tab): the system visibility state of
    /// the page's traversable (HTML §7.1.1). A hidden page has no rendering opportunities, so HTML
    /// §8.1.7.3 "update the rendering" leaves its documents out: no animation frame callbacks,
    /// animations, resize or intersection observations run for it and its animated images decode no
    /// frames ([`Engine::page_hidden`]); timers and tasks run as before. Each active document's
    /// visibility state follows in a task on the user interaction task source ("update the
    /// visibility state", which fires `visibilitychange`).
    pub fn set_shown(&self, shown: bool) {
        let docs = self.active_documents();
        {
            let mut top = self.render.borrow_mut();
            if top.hidden == !shown {
                return;
            }
            // Until its task runs, each document keeps the visibility state it has now.
            let was_visible = !top.hidden;
            top.visibility.retain(|d, _| docs.contains(d));
            for &d in &docs {
                top.visibility.entry(d).or_insert(was_visible);
            }
            top.hidden = !shown;
            if shown {
                // (what changed while hidden gets its rendering update now)
                top.dirty = true;
            }
        }
        for doc in docs {
            self.queue_task(TaskSource::UserInteraction, move |e, cx| {
                e.update_visibility_state(cx, doc, shown);
            });
        }
    }

    /// What asks for this rendering update, for the profile (`SNOWLARK_PROFILE=1`): the conditions
    /// [`Engine::frame_due`] tests, with the CSS animations that keep ticking by name.
    fn frame_reasons(&self) -> String {
        let mut why = Vec::new();
        let raf = self.event_loop.borrow().raf.len();
        if raf > 0 {
            why.push(format!("animation frame callbacks {raf}"));
        }
        let hidden = self.render.borrow().hidden;
        let mut state = |label: String, s: &RenderState| {
            let mut parts: Vec<String> = Vec::new();
            for (on, what) in [
                (s.dirty, "changed"),
                (s.resized, "resized"),
                (!s.pending_scroll.is_empty(), "scroll events"),
                (!s.pending_scrollend.is_empty(), "scrollend events"),
                (!s.smooth.is_empty(), "smooth scroll"),
            ] {
                if on {
                    parts.push(what.into());
                }
            }
            if let Some(r) = &s.renderer {
                let (names, transitions) = r.ticking_animations();
                if !names.is_empty() {
                    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
                    for n in &names {
                        *counts.entry(n.as_str()).or_default() += 1;
                    }
                    let mut by_count: Vec<_> = counts.into_iter().collect();
                    by_count.sort_by_key(|e| std::cmp::Reverse(e.1));
                    let list: Vec<String> = by_count
                        .iter()
                        .take(6)
                        .map(|(n, c)| format!("{n} x{c}"))
                        .collect();
                    parts.push(format!(
                        "css animations {} ({})",
                        names.len(),
                        list.join(", ")
                    ));
                }
                if transitions > 0 {
                    parts.push(format!("css transitions {transitions}"));
                }
                if names.is_empty() && transitions == 0 && r.animating() {
                    parts.push("css animation events".into());
                }
                if r.image_frame_due(s.animation_view(hidden)) {
                    parts.push("animated image frame".into());
                }
            }
            if !parts.is_empty() {
                why.push(format!("{label}: {}", parts.join(", ")));
            }
        };
        state("top".into(), &self.render.borrow());
        for (i, s) in self.frame_renders.borrow().values().enumerate() {
            state(format!("frame {}", i + 1), s);
        }
        for (on, what) in [
            (self.intersection_pending(), "intersection observers"),
            (self.resize_pending(), "resize observers"),
            (self.event_timing_pending(), "event timing"),
            (self.animations_need_frame(), "script animations"),
        ] {
            if on {
                why.push(what.into());
            }
        }
        if why.is_empty() {
            "nothing (an update asked for directly)".into()
        } else {
            why.join("; ")
        }
    }

    /// The embedder hides the page: its documents have no rendering opportunities.
    pub(crate) fn page_hidden(&self) -> bool {
        self.render.borrow().hidden
    }

    /// The active documents of the page's browsing contexts (the top-level one and its frames').
    fn active_documents(&self) -> Vec<NodeId> {
        let frames = self.frames.borrow();
        frames
            .list
            .iter()
            .filter(|c| !c.discarded)
            .map(|c| c.document)
            .collect()
    }

    /// A document's visibility state is "visible" (HTML §7.1.1 "visibility state"): what its last
    /// "update the visibility state" set; for a document made since, its traversable's system
    /// visibility state when it is an active document ("create and initialize a Document object"),
    /// and "hidden" for any other (documents of `DOMParser`, `createHTMLDocument`...).
    pub(crate) fn document_visible(&self, doc: NodeId) -> bool {
        let top = self.render.borrow();
        if let Some(&visible) = top.visibility.get(&doc) {
            return visible;
        }
        let hidden = top.hidden;
        drop(top);
        !hidden && self.is_active_document(doc)
    }

    /// HTML §6.2 "update the visibility state" of `doc` to visible or hidden: nothing when it is
    /// that already; otherwise it becomes that, a `visibility-state` performance entry is queued and
    /// `visibilitychange` fires at the document, bubbling. (The screen orientation and view
    /// transition steps are not implemented.)
    pub(crate) fn update_visibility_state(&self, cx: &mut Cx, doc: NodeId, visible: bool) {
        if !self.dom().is_alive(doc) || self.document_visible(doc) == visible {
            return;
        }
        // (a timeline made now starts with the state before this one)
        let window = self.window_of_document(doc);
        if let Some(w) = window {
            self.performance_of(w);
        }
        self.render.borrow_mut().visibility.insert(doc, visible);
        if let Some(w) = window {
            self.queue_visibility_state_entry(w, visible, self.now());
        }
        self.fire_event(cx, node_handle(doc), "visibilitychange", true, false);
        if visible {
            self.resume_device_enumeration(cx, doc);
        }
    }

    /// How the renderers' document timelines read the time, from now on and for renderers made
    /// later (default: the monotonic clock). `--render --animation-time` holds it.
    pub fn set_timeline_clock(&self, clock: TimelineClock) {
        self.render.borrow_mut().clock = Some(clock);
        self.for_each_renderer(|r, _| r.set_timeline_clock(clock));
    }

    /// Whether a rendering update has events to fire: a `resize` or `scroll` (HTML "run the resize
    /// steps" and "run the scroll steps" after the embedder resized or scrolled the viewport).
    pub(crate) fn render_events_pending(&self) -> bool {
        // (CSS animation events a scan has to find: a restyle started, canceled, seeked or paused
        // an animation; running ones are not work, they would never settle)
        let pending = |s: &RenderState| {
            s.document.is_some()
                && s.renderer.as_ref().is_some_and(|r| {
                    s.resized
                        || !s.pending_scroll.is_empty()
                        || !s.pending_scrollend.is_empty()
                        || !s.smooth.is_empty()
                        || r.css_events_due()
                })
        };
        pending(&self.render.borrow()) || self.frame_renders.borrow().values().any(pending)
    }

    /// Whether any rendered document may look different.
    pub(crate) fn render_dirty(&self) -> bool {
        self.render.borrow().dirty || self.frame_renders.borrow().values().any(|s| s.dirty)
    }

    /// A frame's viewport follows its container's content box in its parent's layout (laid out
    /// first when due). Returns whether it changed.
    pub(crate) fn sync_frame_viewport(&self, doc: NodeId) -> bool {
        let Some(container) = self.rendered_container(doc) else {
            return false;
        };
        let size = self
            .with_layout(container, |r, _| {
                r.geometry(container).map(|g| {
                    Size::new(
                        g.content_box.width.max(0.0).floor(),
                        g.content_box.height.max(0.0).floor(),
                    )
                })
            })
            .flatten()
            .unwrap_or(Size::ZERO);
        let screen = self.hooks.borrow().viewport;
        let mut frames = self.frame_renders.borrow_mut();
        let Some(st) = frames.get_mut(&doc) else {
            return false;
        };
        if let Some(r) = st.renderer.as_mut() {
            r.set_screen((screen.0 as f32, screen.1 as f32));
        }
        if st.viewport == size {
            return false;
        }
        st.viewport = size;
        st.resized = true;
        st.dirty = true;
        st.needs_paint = true;
        if let Some(r) = st.renderer.as_mut() {
            r.set_viewport(size);
        }
        true
    }

    /// CSS Color Adjust 1 §2.3 (w3c/csswg-drafts#4772): a frame's document takes its container's used
    /// color scheme as its preferred color scheme (the container's document's style brought up to
    /// date first, and its own container's before it). Returns whether it changed.
    pub(crate) fn sync_frame_color_scheme(&self, doc: NodeId) -> bool {
        // (a read from inside a renderer callback finds the renderers taken: nothing to do then)
        if self.render.try_borrow_mut().is_err() || self.frame_renders.try_borrow_mut().is_err() {
            return false;
        }
        let Some(container) = self.rendered_container(doc) else {
            return false;
        };
        // (`with_style` brings the container's document, and its own container's, up to date)
        let used = self
            .with_style(container, |r| r.used_color_scheme(container))
            .flatten();
        let scheme = match used {
            Some(snowlark_layout::style::ColorScheme::Dark) => snowlark_style::ColorScheme::Dark,
            Some(snowlark_layout::style::ColorScheme::Light) => snowlark_style::ColorScheme::Light,
            // (no box: the user's preference, as for the top-level document)
            None => {
                if self.render.borrow().setup.as_ref().is_some_and(|s| s.dark) {
                    snowlark_style::ColorScheme::Dark
                } else {
                    snowlark_style::ColorScheme::Light
                }
            }
        };
        let mut frames = self.frame_renders.borrow_mut();
        let Some(st) = frames.get_mut(&doc) else {
            return false;
        };
        let Some(r) = st.renderer.as_mut() else {
            return false;
        };
        if r.color_scheme() == scheme {
            return false;
        }
        r.set_color_scheme(scheme);
        st.dirty = true;
        st.needs_paint = true;
        true
    }

    /// Brings style and layout up to date for a script's read of `node` (forced layout: render-blocking
    /// is ignored and the timeline does not move) and runs `f` with the renderer. `None` without
    /// rendering, or when `node` is not in a rendered document.
    pub(crate) fn with_layout<R>(
        &self,
        node: NodeId,
        f: impl FnOnce(&mut Renderer, &RenderState) -> R,
    ) -> Option<R> {
        let doc = {
            let dom = self.dom();
            if !dom.is_alive(node) {
                return None;
            }
            if dom.is_document(node) {
                node
            } else {
                dom.node_document(node)
            }
        };
        if !self.is_rendered_document(doc) {
            return None;
        }
        self.sync_frame_viewport(doc);
        self.sync_frame_color_scheme(doc);
        // (script animations' current values: the model takes effect immediately)
        self.sync_animation_styles();
        self.sync_form_states(None, crate::engine::forms::FormSync::Connected);
        // (a control's state set by script, such as a textarea's value, decides its box: the read
        // measures it, not what the last rendering update showed)
        if !self.controls_dirty.borrow().is_empty() {
            self.sync_controls();
        }
        let now = self.document_time(doc);
        self.with_doc_renderer(doc, |r, st| {
            let probe = self.profile.as_ref().map(|_| (Instant::now(), r.stats()));
            // (CSS animations at the time script sees: rendering updates may not have run for them)
            if let Some(now) = now {
                r.advance_timeline_for_script(now / 1000.0);
            }
            r.layout_for_script(st.viewport)?;
            if let (Some(p), Some((t, before))) = (&self.profile, probe) {
                p.forced("layout", t.elapsed(), &before, &r.stats());
            }
            Some(f(r, st))
        })
        .flatten()
    }

    /// Brings computed styles up to date for a script's read of `node` (no layout) and runs `f` with
    /// the renderer. `None` without rendering, or when `node` is not in a rendered document.
    pub(crate) fn with_style<R>(
        &self,
        node: NodeId,
        f: impl FnOnce(&mut Renderer) -> R,
    ) -> Option<R> {
        let mut now = None;
        if let Some(doc) = {
            let dom = self.dom();
            dom.is_alive(node).then(|| dom.node_document(node))
        } {
            // (a frame's media queries follow its viewport, its container's size in the parent)
            self.sync_frame_viewport(doc);
            self.sync_frame_color_scheme(doc);
            if self.is_rendered_document(doc) {
                now = self.document_time(doc);
            }
        }
        self.sync_animation_styles();
        self.sync_form_states(None, crate::engine::forms::FormSync::Connected);
        self.with_node_renderer(node, |r, _| {
            let probe = self.profile.as_ref().map(|_| (Instant::now(), r.stats()));
            // (CSS animations at the time script sees: rendering updates may not have run for them)
            if let Some(now) = now {
                r.advance_timeline_for_script(now / 1000.0);
            }
            if r.needs_restyle() {
                r.restyle_for_script();
            }
            if let (Some(p), Some((t, before))) = (&self.profile, probe) {
                p.forced("style", t.elapsed(), &before, &r.stats());
            }
            f(r)
        })
    }

    /// The device pixel ratio changed (a monitor's scale, a page zoom): `resolution` media queries
    /// and `devicePixelRatio` follow; images with `srcset` choose again.
    pub fn set_device_pixel_ratio(&self, dpr: f32) {
        {
            let mut st = self.render.borrow_mut();
            if let Some(s) = st.setup.as_mut() {
                if s.device_pixel_ratio == dpr {
                    return;
                }
                s.device_pixel_ratio = dpr;
            }
        }
        self.for_each_renderer(|r, st| {
            r.set_device_pixel_ratio(dpr);
            st.dirty = true;
            st.resized = true;
        });
        let mut st = self.render.borrow_mut();
        st.dirty = true;
        st.resized = true;
    }

    /// The embedder's classic vertical scroll bar for the top-level viewport is `width` CSS px wide
    /// (0: overlay scroll bars): layout leaves it out of the initial containing block when it shows
    /// ([`snowlark_render::Renderer::set_scrollbar_width`]).
    pub fn set_scrollbar_width(&self, width: f32) {
        let mut st = self.render.borrow_mut();
        if let Some(s) = st.setup.as_mut() {
            if s.scrollbar_width == width {
                return;
            }
            s.scrollbar_width = width;
        }
        if let Some(r) = st.renderer.as_mut() {
            r.set_scrollbar_width(width);
        }
        st.dirty = true;
    }

    /// Whether the top-level document's last layout showed the classic vertical scroll bar.
    pub fn vertical_scrollbar(&self) -> bool {
        self.render
            .borrow()
            .renderer
            .as_ref()
            .is_some_and(|r| r.vertical_scrollbar())
    }

    /// How the top-level viewport's scroll bar is drawn: its width in the last layout and the root
    /// element's used color scheme and `scrollbar-color`
    /// ([`snowlark_render::Renderer::viewport_scrollbar`]).
    pub fn viewport_scrollbar(&self) -> snowlark_render::ViewportScrollbar {
        self.render
            .borrow()
            .renderer
            .as_ref()
            .map(|r| r.viewport_scrollbar())
            .unwrap_or_default()
    }

    /// The canvas surface's color: the root element's used color scheme decides it
    /// ([`snowlark_render::Renderer::canvas_color`]).
    pub fn canvas_color(&self) -> snowlark_paint::Color {
        self.render
            .borrow()
            .renderer
            .as_ref()
            .map_or(snowlark_paint::Color::WHITE, |r| r.canvas_color())
    }

    /// The color a viewer shows where the page is not drawn (yet): the canvas background's color
    /// over the canvas surface's ([`snowlark_render::Renderer::page_background`]).
    pub fn page_background(&self) -> snowlark_paint::Color {
        self.render
            .borrow()
            .renderer
            .as_ref()
            .map_or(snowlark_paint::Color::WHITE, |r| r.page_background())
    }

    /// The viewport changed size.
    pub fn set_viewport(&self, viewport: Size) {
        let mut st = self.render.borrow_mut();
        if st.viewport == viewport {
            return;
        }
        st.viewport = viewport;
        st.resized = true;
        st.dirty = true;
        if let Some((doc, _)) = self.view.get() {
            // A hosted window's view: its own viewport, which its next documents keep too.
            if let Some(r) = st.renderer.as_mut() {
                r.set_viewport(viewport);
            }
            drop(st);
            if let Some(id) = self.context_of_document(doc) {
                self.frames.borrow_mut().list[id].popup_viewport =
                    Some((viewport.width, viewport.height));
            }
            return;
        }
        if let Some(r) = st.renderer.as_mut() {
            r.set_screen((viewport.width, viewport.height));
            r.set_viewport(viewport);
        }
        drop(st);
        self.hooks.borrow_mut().viewport = (viewport.width as i32, viewport.height as i32);
    }

    /// Where the viewport and its window sit on the screen (the window moved, resized or zoomed).
    pub fn set_screen_placement(&self, placement: crate::engine::window::ScreenPlacement) {
        self.hooks.borrow_mut().screen = placement;
    }

    /// HTML §8.1.7.3 "update the rendering": for each rendered document in tree order, the resize
    /// steps and the scroll steps; the animation frame callbacks; then style and layout of each
    /// ("update the rendering or user interface"), a parent before the frames it shows. Returns
    /// whether the rendering changed (paint again).
    pub fn update_the_rendering(&self, cx: &mut Cx) -> bool {
        if self.profile.is_some() {
            eprintln!("[frame] why: {}", self.frame_reasons());
        }
        self.render_pump();
        self.drain_resource_timings();
        // The pointer move held for this frame, first, before the frame's other steps (the HTML
        // text has no step for it: browsers dispatch frame-aligned input here).
        self.dispatch_held_move(cx);
        // A still pointer over content that moved or went: the boundary events of what is under it
        // now, before the frame's other steps.
        self.recompute_hover(cx);
        let docs = self.rendered_documents();
        for &d in docs.iter().skip(1) {
            self.sync_frame_viewport(d);
            self.sync_frame_color_scheme(d);
        }
        // "Flush autofocus candidates" for each top-level document (the page's and its popups').
        for &top in &docs {
            if self.parent_document(top).is_none() {
                self.flush_autofocus_candidates(cx, top);
            }
        }
        let mut any_resized = false;
        // Smooth scrolls move on to this frame's positions (their `scroll` events follow).
        let now = self.now();
        self.step_smooth_scrolls(cx, now);
        for &doc in &docs {
            // "Run the resize steps": a `resize` event at the window when the viewport changed size.
            let resized = self
                .with_doc_renderer(doc, |_, st| std::mem::take(&mut st.resized))
                .unwrap_or(false);
            if resized {
                any_resized = true;
                if let Some(w) = self.window_of_document(doc) {
                    self.fire_event(cx, w, "resize", false, false);
                    // CSSOM View §13.1 "run the resize steps": the visual viewport's size changed
                    // with it (no pinch zoom): `resize` at its VisualViewport too.
                    if let Some(vv) = self.visual_viewport_existing(w) {
                        self.fire_event(cx, vv, "resize", false, false);
                    }
                }
            }
            // "Run the scroll steps": `scroll` at each pending target (bubbling at a document).
            let targets = self
                .with_doc_renderer(doc, |_, st| std::mem::take(&mut st.pending_scroll))
                .unwrap_or_default();
            for t in targets {
                if !self.dom().is_alive(t) {
                    continue;
                }
                let is_doc = self.dom().is_document(t);
                self.fire_event(cx, node_handle(t), "scroll", is_doc, false);
            }
            // CSSOM View: `scrollend` at each target whose scroll completed (bubbling at a
            // document).
            let ended = self
                .with_doc_renderer(doc, |_, st| std::mem::take(&mut st.pending_scrollend))
                .unwrap_or_default();
            for t in ended {
                if !self.dom().is_alive(t) {
                    continue;
                }
                let is_doc = self.dom().is_document(t);
                self.fire_event(cx, node_handle(t), "scrollend", is_doc, false);
            }
        }
        if any_resized {
            self.microtask_checkpoint(cx);
        }
        // "Evaluate media queries and report changes."
        self.report_media_query_changes(cx);
        // "Update animations and send events" at the frame's time, which the animation frame
        // callbacks get too (the top-level document's clock decides it).
        // (the begin frame's time in the window: Blink's `PageAnimator::ServiceScriptedAnimations`
        // with the `BeginFrameArgs::frame_time`)
        let page_now = self.take_frame_time();
        let frame_now = match self.rendered_top() {
            Some(top) => self
                .with_doc_renderer(top, |r, _| r.clock_at(page_now / 1000.0) * 1000.0)
                .unwrap_or(page_now),
            None => page_now,
        };
        self.update_animations_and_send_events(cx, frame_now);
        // "Run the fullscreen steps" for each document, in tree order.
        if self.fullscreen_events_pending() {
            for &doc in &docs {
                self.run_fullscreen_steps(cx, doc);
            }
        }
        // "Run the animation frame callbacks."
        self.run_animation_frames_at(cx, frame_now);
        self.microtask_checkpoint(cx);
        // "Update the rendering or user interface of doc": canvases that drew, then style and layout.
        self.flush_canvases();
        self.sync_media_rendering(cx);
        self.sync_form_states(Some(cx), crate::engine::forms::FormSync::Connected);
        self.sync_controls();
        // Content blocking: generic hiding for new classes and ids, procedural filters.
        self.content_filter_pass(cx);
        self.sync_animation_styles();
        let docs = self.rendered_documents();
        for (i, &doc) in docs.iter().enumerate() {
            if i > 0 {
                self.sync_frame_viewport(doc);
            }
            let hidden = self.render.borrow().hidden;
            let laid_out = self
                .with_doc_renderer(doc, |r, st| {
                    st.dirty = false;
                    r.set_image_animation_view(st.animation_view(hidden));
                    r.set_frame_time(r.clock_at(page_now / 1000.0));
                    if r.update_rendering(st.viewport).is_some() {
                        st.needs_paint = true;
                        true
                    } else {
                        // Render-blocked: try again at the next opportunity.
                        st.dirty = true;
                        false
                    }
                })
                .unwrap_or(false);
            // A one-line field scrolls to keep its caret in view: its box is laid out again.
            if laid_out && self.keep_caret_in_view(doc) {
                if let Some(el) = self.focused_element(doc) {
                    if let Some(state) = self.control_render_state(el, doc) {
                        self.with_doc_renderer(doc, |r, st| {
                            r.set_control_state(el, state);
                            r.layout(st.viewport);
                        });
                    }
                }
            }
        }
        // The selection's paint, with the highlight colors of the styles just computed.
        self.sync_selection_paint();
        // (the canvas frames queued before style and layout)
        self.finish_canvas_frames();
        // Paint Timing "mark paint timing", and Event Timing's pending entries end here.
        for &doc in &docs {
            self.mark_paint_timing(doc);
        }
        self.dispatch_pending_event_timings();
        self.queue_image_events();
        // Lazy media and images near the viewport after this layout start loading.
        self.media_after_layout(cx);
        self.images_after_layout(cx);
        self.frames_after_layout(cx);
        // CSS Sizing 4 5.2.1: the last remembered sizes, recorded where observations are gathered.
        for doc in self.rendered_documents() {
            self.with_layout(doc, |r, _| r.record_remembered_sizes());
        }
        // Resize Observer: gather and broadcast active observations, deepest first.
        self.update_resize_observations(cx);
        // HTML: the focus fixup, for each document.
        self.focus_fixup(cx, &self.rendered_documents());
        // Intersection Observer: "run the update intersection observations steps".
        self.update_intersection_observations(cx);
        // CSS Font Loading "update the font loading status".
        self.update_fonts_ready(cx);
        self.update_font_waiters(cx);
        if let Some(p) = &self.profile {
            let stats: Vec<_> = docs
                .iter()
                .enumerate()
                .filter_map(|(i, &d)| {
                    // (a frame by its document's URL: which page an update was for)
                    let label = if i == 0 {
                        "top".to_string()
                    } else {
                        format!("frame {i} {}", self.dom().document_url(d))
                    };
                    self.with_doc_renderer(d, |r, _| (d, label, r.stats(), r.timings()))
                })
                .collect();
            eprint!("{}", p.rendering_update(&stats));
        }
        self.compositor_changes_handed_over();
        self.render.borrow().needs_paint
            || self.frame_renders.borrow().values().any(|s| s.needs_paint)
    }

    /// Form controls' page state (dirty values, checkedness, selectedness, the focused field's
    /// caret) for the renderers: the controls whose state or attributes changed since the last
    /// update (`controls_dirty`), the focused ones and those focused at the last update, and every
    /// control with state once a renderer was made.
    pub(crate) fn sync_controls(&self) {
        if self.rendered_top().is_none() {
            return;
        }
        let mut els: Vec<NodeId> = self.controls_dirty.borrow_mut().drain().collect();
        if self.controls_resync.replace(false) {
            els.extend(self.html.borrow().controls.keys().copied());
        }
        let mut focused = Vec::new();
        for doc in self.rendered_documents() {
            if let Some(f) = self.focused_element(doc) {
                focused.push(f);
            }
        }
        els.extend(focused.iter().copied());
        els.extend(self.controls_focused.replace(focused));
        // (controls of documents not rendered wait for theirs: one adopted into a rendered document
        // shows its state there)
        let mut waiting: Vec<NodeId> = Vec::new();
        let mut seen = rustc_hash::FxHashSet::default();
        for el in els {
            // (`:checked` follows checkedness and selectedness; an option's depends on its select's
            // other options)
            if self
                .dom()
                .is_html_element_named(el, &snowlark_dom::local_name!("select"))
            {
                for o in self.options_of(el) {
                    self.sync_checked_state(o);
                }
            } else {
                self.sync_checked_state(el);
            }
            // An option's state shows in its select.
            let marked = el;
            let (el, doc) = {
                let dom = self.dom();
                if !dom.is_alive(el) {
                    continue;
                }
                let doc = dom.node_document(el);
                if dom.is_html_element_named(el, &snowlark_dom::local_name!("option")) {
                    match dom.ancestors(el).find(|&a| {
                        dom.is_html_element_named(a, &snowlark_dom::local_name!("select"))
                    }) {
                        Some(s) => (s, doc),
                        None => continue,
                    }
                } else {
                    (el, doc)
                }
            };
            if !self.is_rendered_document(doc) {
                waiting.push(marked);
                continue;
            }
            if !seen.insert(el) {
                continue;
            }
            if let Some(state) = self.control_render_state(el, doc) {
                self.with_doc_renderer(doc, |r, st| {
                    if r.set_control_state(el, state) {
                        st.needs_paint = true;
                    }
                });
            }
        }
        // (what the reads above marked is up to date)
        let mut dirty = self.controls_dirty.borrow_mut();
        dirty.clear();
        dirty.extend(waiting);
    }

    fn control_render_state(
        &self,
        el: NodeId,
        doc: NodeId,
    ) -> Option<snowlark_render::controls::ControlState> {
        let (input, textarea, select) = {
            let dom = self.dom();
            (
                dom.is_html_element_named(el, &snowlark_dom::local_name!("input")),
                dom.is_html_element_named(el, &snowlark_dom::local_name!("textarea")),
                dom.is_html_element_named(el, &snowlark_dom::local_name!("select")),
            )
        };
        let mut state = snowlark_render::controls::ControlState::default();
        if select {
            let options = self.options_of(el);
            let dom = self.dom();
            state.selected = Some(
                options
                    .iter()
                    .map(|&o| self.option_selected_in(&dom, o))
                    .collect(),
            );
            return Some(state);
        }
        if !input && !textarea {
            return None;
        }
        let kind = self.input_type(el);
        if matches!(kind.as_deref(), Some("checkbox" | "radio")) {
            state.checked = Some(self.checkedness(el));
            state.indeterminate = self.indeterminate(el);
            return Some(state);
        }
        if self.ctl(el, |c| c.value.is_some()) {
            state.value = Some(self.editor_value(el));
        }
        if self.focused_element(doc) == Some(el) && self.is_text_control(el) {
            let (s, e, d) = self.ctl(el, |c| c.selection);
            state.editing = Some((s, e, if d == "backward" { s } else { e }));
            state.scroll_x = self.ctl(el, |c| c.scroll_x);
            state.composition = self.composition_range(el);
        }
        Some(state)
    }

    /// Takes the "paint again" flag (of any rendered document: frames paint into the top-level
    /// document's list). While the canvas frames of the last rendering update are on their way,
    /// none is taken: the paint shows them with the rest of that update
    /// ([`Engine::canvas_paint_deadline`]).
    pub fn take_needs_paint(&self) -> bool {
        // (the top-level document shown and its frames: a hosted window's are its view's)
        let Some(top) = self.rendered_top() else {
            return false;
        };
        if self.canvas_paint_held() {
            return false;
        }
        let tree = self.rendered_tree(top);
        let mut any = std::mem::take(&mut self.render.borrow_mut().needs_paint);
        let mut frames = self.frame_renders.borrow_mut();
        for d in &tree[1..] {
            if let Some(st) = frames.get_mut(d) {
                any |= std::mem::take(&mut st.needs_paint);
            }
        }
        any
    }

    /// Script scrolled the viewport since the last call: where to.
    pub fn take_scroll_request(&self) -> Option<Point> {
        self.render.borrow_mut().scroll_request.take()
    }

    /// The embedder scrolled the viewport (the user did): a `scroll` event follows at the next
    /// rendering update (CSSOM View §13.1 "scroll the viewport" → "perform a scroll").
    pub fn viewport_scrolled(&self, to: Point) {
        let mut st = self.render.borrow_mut();
        if st.scroll == to {
            return;
        }
        st.scroll = to;
        if let Some(doc) = st.document {
            if !st.pending_scroll.contains(&doc) {
                st.pending_scroll.push(doc);
            }
        }
    }

    /// The display list of the top-level document's viewport scrolled to `origin` (which may differ
    /// from the viewport's scroll position: the embedder rasterizes a band around it), for `scale`
    /// device pixels per CSS px, with the frames' documents painted into their containers. Lays out
    /// first when nothing was laid out yet. `None` without rendering, and while the document is
    /// render-blocked (its first frame waits for its `<head>` stylesheets or the timeout).
    pub fn paint(&self, origin: Point, scale: f32) -> Option<DisplayList> {
        let doc = self.rendered_top()?;
        let list = self.paint_document(doc, Some(origin), scale);
        // (what a compositor runs from this frame on: `docs/compositor-animations.md`, S3)
        if list.is_some() {
            self.note_compositor_frame();
        }
        list
    }

    fn paint_document(
        &self,
        doc: NodeId,
        origin: Option<Point>,
        scale: f32,
    ) -> Option<DisplayList> {
        let mut embeds = HashMap::new();
        for (container, child) in self.rendered_frames_of(doc) {
            if let Some(list) = self.paint_document(child, None, scale) {
                embeds.insert(container, list);
            }
        }
        self.with_doc_renderer(doc, |r, st| {
            // HTML §8.1.7.3: a render-blocked document has had no rendering update; nothing of it
            // is shown yet (a layout a script forced has no business on the screen either).
            if r.render_blocked() {
                return None;
            }
            if r.fragments().is_none() {
                // (asked before the first rendering update: lay out now)
                r.layout(st.viewport)?;
            }
            let scroll = origin.unwrap_or(st.scroll);
            Some(r.paint_with_embeds(
                st.viewport,
                scroll,
                scale,
                (!embeds.is_empty()).then_some(&embeds),
                // (the top-level document's viewport is the one the embedder scrolls)
                origin.is_some(),
            ))
        })
        .flatten()
    }

    /// The topmost element at a viewport point of the top-level document, descending into the
    /// frames it hits (their documents' elements, at the point in their viewports).
    pub(crate) fn hit_element(&self, p: Point) -> Option<DocumentHit> {
        let mut doc = self.rendered_top()?;
        let mut point = p;
        for _ in 0..32 {
            let hit = self.view_hit(doc, point)?;
            let (element, content, text) = (hit.element, hit.content, hit.text);
            let child = self
                .rendered_frames_of(doc)
                .into_iter()
                .find(|(c, _)| *c == element)
                .map(|(_, d)| d);
            match (child, content) {
                (Some(d), Some(at)) => {
                    let viewport = self
                        .with_doc_renderer(d, |_, st| st.viewport)
                        .unwrap_or(Size::ZERO);
                    if at.x < 0.0 || at.y < 0.0 || at.x >= viewport.width || at.y >= viewport.height
                    {
                        // (the container's padding or border)
                        return Some(DocumentHit {
                            element,
                            point,
                            content,
                            text,
                        });
                    }
                    doc = d;
                    point = at;
                }
                _ => {
                    return Some(DocumentHit {
                        element,
                        point,
                        content,
                        text,
                    })
                }
            }
        }
        None
    }

    /// The viewports of the frames the user can scroll and the scroll containers inside frames, as
    /// the embedder's scrollers (padding box or frame content box in top-level document
    /// coordinates, scroll position, range): (node, rect, scroll, max). A frame's viewport is
    /// scrolled through its container element's node.
    pub fn frame_scrollers(&self) -> Vec<(NodeId, snowlark_paint::Rect, Point, Point)> {
        let mut out = Vec::new();
        let Some(top) = self.rendered_top() else {
            return out;
        };
        self.collect_frame_scrollers(top, Point::ZERO, &mut out);
        // From the top-level viewport to top-level document coordinates.
        let top_scroll = self.render.borrow().scroll;
        for s in &mut out {
            s.1 = s.1.translate(top_scroll.x, top_scroll.y);
        }
        out
    }

    /// `origin`: where `doc`'s viewport origin is in the top-level viewport.
    fn collect_frame_scrollers(
        &self,
        doc: NodeId,
        origin: Point,
        out: &mut Vec<(NodeId, snowlark_paint::Rect, Point, Point)>,
    ) {
        for (container, child) in self.rendered_frames_of(doc) {
            // The container's content box in its document's viewport, then in the top-level one.
            let rect = self
                .with_layout(container, |r, st| {
                    let g = r.geometry(container)?;
                    let s = crate::engine::view::scroll_shift(r, st, &self.dom(), container);
                    Some(g.content_box.translate(-s.x, -s.y))
                })
                .flatten();
            let Some(rect) = rect else {
                continue;
            };
            let rect = rect.translate(origin.x, origin.y);
            // (positions from the start of the range, as for scroll containers)
            let (real, scroll, max) = self
                .with_layout(child, |r, st| {
                    let (min, max) = crate::engine::view::viewport_range(r, st);
                    (
                        st.scroll,
                        Point::new(st.scroll.x - min.x, st.scroll.y - min.y),
                        Point::new(max.x - min.x, max.y - min.y),
                    )
                })
                .unwrap_or((Point::ZERO, Point::ZERO, Point::ZERO));
            // HTML §15.4.4 / §15.6: `scrolling=no` (or `off`, `noscroll`) frames are not scrolled
            // by the user.
            let no_scrolling = self
                .dom()
                .get_attribute(container, "scrolling")
                .is_some_and(|v| {
                    let v = v.trim();
                    ["no", "off", "noscroll"]
                        .iter()
                        .any(|k| v.eq_ignore_ascii_case(k))
                });
            if (max.x >= 1.0 || max.y >= 1.0) && !no_scrolling {
                out.push((container, rect, scroll, max));
            }
            // The child's scroll containers (in its document coordinates), in the top-level
            // viewport.
            let shift = Point::new(rect.x - real.x, rect.y - real.y);
            for (n, r, s, m) in self.user_scroll_containers_of(child) {
                let r = r.translate(shift.x, shift.y).intersect(&rect);
                if !r.is_empty() {
                    out.push((n, r, s, m));
                }
            }
            self.collect_frame_scrollers(child, Point::new(rect.x, rect.y), out);
        }
    }
}
