// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `img` element's processing model (HTML §4.8.4.3): image requests (the current request and
//! the pending request, their states and natural sizes), "update the image data" on the element's
//! relevant mutations, the "list of available images", lazy loading (`loading=lazy`, resumed when
//! the element nears the viewport), and the `load`/`error` events, `complete`, `currentSrc`,
//! `naturalWidth`/`naturalHeight` and `decode()` that follow from them.
//!
//! The images themselves are fetched and decoded by the document's renderer, keyed by URL (the
//! `img` elements, CSS images and `<picture>` share them): the requests here ask it for a URL
//! ([`Renderer::request_image_url`]) and follow its arrivals.

use snowlark_bindings::rt::{ExceptionName, JsPromise};
use snowlark_dom::{local_name, ns, NodeId};
use snowlark_render::ImageStatus;

use super::{exception, Engine};
use crate::event_loop::TaskSource;

/// An image request's state (HTML §4.8.4.3 "image request").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestState {
    Unavailable,
    PartiallyAvailable,
    CompletelyAvailable,
    Broken,
}

/// An image request.
#[derive(Clone, Debug)]
pub struct ImageRequest {
    pub state: RequestState,
    /// Its current URL (empty for none).
    pub url: String,
    /// The image's natural width and height in image pixels, once available.
    pub natural: (Option<f32>, Option<f32>),
    /// Its natural aspect ratio (width / height), when it has one.
    pub ratio: Option<f32>,
    /// The pixel density the source was selected with.
    pub density: f32,
    /// The fetch waits for the lazy load resumption steps.
    pub lazy: bool,
    /// The request delays its document's load event (it was not lazy loaded when made).
    pub delays_load: bool,
}

impl ImageRequest {
    fn new(url: String, density: f32) -> ImageRequest {
        ImageRequest {
            state: RequestState::Unavailable,
            url,
            natural: (None, None),
            ratio: None,
            density,
            lazy: false,
            delays_load: true,
        }
    }
}

impl Default for ImageRequest {
    fn default() -> Self {
        ImageRequest::new(String::new(), 1.0)
    }
}

/// An `img` element's image data.
#[derive(Default)]
pub struct ImgData {
    pub current: ImageRequest,
    pub pending: Option<ImageRequest>,
    /// Bumped by each run of "update the image data": an older run's continuation, task or
    /// promise sees it changed and stops.
    pub generation: u32,
    /// `decode()` promises waiting for the current request.
    pub decodes: Vec<(u32, JsPromise)>,
    /// A run of "update the image data" waits for its microtask (the image is not complete then).
    pub updating: bool,
    /// The newest run of "update the image data" has its "maybe omit events" flag set.
    pub omit_events: bool,
    /// The element's "last auto-sizes width" (HTML "parse a sizes attribute"), in CSS px.
    pub auto_width: Option<f32>,
    /// While it allows auto-sizes: it was being rendered, with `auto_width` as its concrete
    /// object size width, at the last selection or layout (so the next layout can tell when it
    /// starts being rendered or that width changes).
    pub auto_rendered: bool,
}

/// The engine's image elements.
#[derive(Default)]
pub struct Images {
    pub elements: rustc_hash::FxHashMap<NodeId, ImgData>,
    /// Runs of "update the image data" waiting for their microtask ("await a stable state").
    pub updates: Vec<(NodeId, u32)>,
    /// Image preloads (`link` elements) that take an image their document is fetching already,
    /// and the URL they wait for.
    pub preload_waiters: Vec<(NodeId, String)>,
    /// `decode()` calls waiting for their microtask, which checks the image.
    /// Each with the run of "update the image data" and the request URL at the call.
    pub decode_checks: Vec<(NodeId, (u32, String), JsPromise)>,
}

/// Whether an attribute change is one of an `img` element's relevant mutations: `src`, `srcset`
/// or `sizes` set, changed or removed; the `crossorigin` or `referrerpolicy` attribute's state
/// changed. (`width` is not one: a change of the width it lays out at is, for an image that
/// allows auto-sizes, which the next layout sees.)
pub(crate) fn is_relevant_change(name: &str, old: Option<&str>, new: Option<&str>) -> bool {
    // (the CORS settings attribute's states: none, anonymous (also empty and invalid), credentials)
    let cors = |v: Option<&str>| match v {
        None => 0,
        Some(v) if v.eq_ignore_ascii_case("use-credentials") => 2,
        Some(_) => 1,
    };
    // (the referrer policy attribute's state: a valid keyword, ASCII lowercase, or empty)
    let policy = |v: Option<&str>| {
        const POLICIES: [&str; 8] = [
            "no-referrer",
            "no-referrer-when-downgrade",
            "same-origin",
            "origin",
            "strict-origin",
            "origin-when-cross-origin",
            "strict-origin-when-cross-origin",
            "unsafe-url",
        ];
        v.map(str::to_ascii_lowercase)
            .filter(|v| POLICIES.contains(&v.as_str()))
            .unwrap_or_default()
    };
    match name {
        "src" | "srcset" | "sizes" => true,
        "crossorigin" => cors(old) != cors(new),
        "referrerpolicy" => policy(old) != policy(new),
        _ => false,
    }
}

impl Engine {
    fn img<R>(&self, el: NodeId, f: impl FnOnce(&mut ImgData) -> R) -> R {
        f(self.images.borrow_mut().elements.entry(el).or_default())
    }

    /// Whether the element "uses srcset or picture": it has a `srcset` attribute, or a `picture`
    /// parent.
    fn uses_srcset_or_picture(&self, el: NodeId) -> bool {
        let dom = self.dom();
        dom.attr(el, &ns!(), &local_name!("srcset")).is_some()
            || dom
                .parent_element(el)
                .is_some_and(|p| dom.is_html_element_named(p, &local_name!("picture")))
    }

    /// The renderer's view of `url` for `el`'s document (`None` without a renderer).
    fn image_status(&self, el: NodeId, url: &str, start: bool) -> Option<ImageStatus> {
        self.with_node_renderer(el, |r, st| {
            if start {
                st.dirty = true;
            }
            r.request_image_url(url, start)
        })
    }

    /// HTML §4.8.4.3.4 "update the image data" for `el` (`restart_animations` aside).
    pub(crate) fn update_the_image_data(&self, cx: &mut snowlark_bindings::rt::Cx, el: NodeId) {
        self.update_the_image_data_with(cx, el, false);
    }

    /// "Update the image data", with its "maybe omit events" flag: a run that selects the
    /// source the current request already shows fires no event.
    fn update_the_image_data_with(
        &self,
        cx: &mut snowlark_bindings::rt::Cx,
        el: NodeId,
        omit_events: bool,
    ) {
        if !self.dom().is_alive(el) {
            return;
        }
        // 1. (an element of a document without a browsing context has no image data)
        let doc = self.dom().node_document(el);
        if !self.is_active_document(doc) {
            return;
        }
        let gen = self.img(el, |d| {
            d.generation = d.generation.wrapping_add(1);
            d.updating = false;
            d.omit_events = omit_events;
            d.generation
        });
        // (any decode() waiting on the old request rejects; a run that may keep the current
        // request leaves them to it)
        if !omit_events {
            self.reject_decodes(el, gen);
        }
        // (no source at all: the image data is cleared at once, without an event)
        if !self.uses_srcset_or_picture(el)
            && self.dom().attr(el, &ns!(), &local_name!("src")).is_none()
        {
            self.img(el, |d| {
                d.pending = None;
                d.current = ImageRequest {
                    state: RequestState::Broken,
                    ..ImageRequest::default()
                };
            });
            self.sync_img_renderer(el);
            self.settle_decodes(el);
            return;
        }
        // 3-5. The "list of available images": a plain `src` already decoded completes now
        // (not for a CORS request: the list is keyed by the CORS settings too).
        let cors = self
            .dom()
            .attr(el, &ns!(), &snowlark_dom::LocalName::from("crossorigin"))
            .is_some();
        if !cors && !self.uses_srcset_or_picture(el) {
            let src = self
                .dom()
                .attr(el, &ns!(), &local_name!("src"))
                .map(str::to_owned)
                .filter(|s| !s.is_empty());
            if let Some(src) = src {
                if let Some(url) = self.encoding_parse_url(el, &src) {
                    let url = url.to_string();
                    if let Some(ImageStatus::Loaded {
                        width,
                        height,
                        ratio,
                    }) = self.image_status(el, &url, false)
                    {
                        let same = self.img(el, |d| d.current.url == url);
                        self.img(el, |d| {
                            d.pending = None;
                            d.current = ImageRequest {
                                state: RequestState::CompletelyAvailable,
                                url,
                                natural: (width, height),
                                ratio,
                                density: 1.0,
                                lazy: false,
                                delays_load: true,
                            };
                        });
                        self.sync_img_renderer(el);
                        // ("if maybe omit events is not set or previousURL is not urlString")
                        if !(omit_events && same) {
                            self.queue_img_event(el, gen, true);
                        }
                        self.settle_decodes(el);
                        return;
                    }
                }
            }
        }
        // 6. The rest runs after a microtask.
        self.img(el, |d| d.updating = true);
        self.images.borrow_mut().updates.push((el, gen));
        self.queue_internal_microtask(cx, super::reactions::Microtask::ImageData);
    }

    /// The microtask of "update the image data" (steps 7 onward) for the runs waiting for it.
    pub(crate) fn img_microtask(&self) {
        let updates = std::mem::take(&mut self.images.borrow_mut().updates);
        for (el, gen) in updates {
            if self.dom().is_alive(el) {
                self.update_the_image_data_continue(el, gen);
            }
        }
    }

    fn update_the_image_data_continue(&self, el: NodeId, gen: u32) {
        // 7. (a later run supersedes this one)
        if self.img(el, |d| d.generation) != gen {
            return;
        }
        let omit_events = self.img(el, |d| {
            d.updating = false;
            d.omit_events
        });
        // 8. Select an image source.
        let auto = self.auto_sizes_width(el);
        let selected = match self.with_node_renderer(el, |r, _| r.select_image_source(el, auto)) {
            Some(Some(s)) => s,
            _ => {
                // (no renderer: the `src` alone, at density 1)
                let src = self
                    .dom()
                    .attr(el, &ns!(), &local_name!("src"))
                    .map(str::to_owned)
                    .filter(|s| !s.trim().is_empty());
                src.map(|s| {
                    let s = s.trim_matches(|c: char| c.is_ascii_whitespace());
                    match self.encoding_parse_url(el, s) {
                        Some(u) => (u.to_string(), 1.0),
                        None => (s.to_owned(), 1.0),
                    }
                })
            }
        };
        // 9. No source: the request is broken (and `error` fires when there was something to use).
        let Some((url, density)) = selected else {
            self.img(el, |d| {
                d.pending = None;
                d.current = ImageRequest {
                    state: RequestState::Broken,
                    ..ImageRequest::default()
                };
            });
            self.sync_img_renderer(el);
            // (an empty `srcset` or a `picture` with nothing to select fires nothing, as other
            // engines do)
            let has_source = {
                let dom = self.dom();
                dom.attr(el, &ns!(), &local_name!("src")).is_some()
                    || dom
                        .attr(el, &ns!(), &local_name!("srcset"))
                        .is_some_and(|s| !s.trim().is_empty())
            };
            if has_source {
                self.queue_img_event(el, gen, false);
            }
            self.settle_decodes(el);
            return;
        };
        // 10. A source that is not a URL: the request is broken, with that as its URL.
        if url::Url::parse(&url).is_err() {
            self.img(el, |d| {
                d.pending = None;
                d.current = ImageRequest {
                    state: RequestState::Broken,
                    url: url.clone(),
                    ..ImageRequest::default()
                };
            });
            self.sync_img_renderer(el);
            self.queue_img_event(el, gen, false);
            self.settle_decodes(el);
            return;
        }
        // 11. The same URL is already on its way.
        let same = self.img(el, |d| {
            d.pending.as_ref().is_some_and(|p| p.url == url)
                || (d.current.url == url && d.current.state == RequestState::PartiallyAvailable)
        });
        if same {
            return;
        }
        // (a run that may omit events and selects what the current request shows keeps it)
        let shown = omit_events
            && self.img(el, |d| {
                let keep = d.pending.is_none()
                    && d.current.url == url
                    && d.current.state == RequestState::CompletelyAvailable;
                if keep {
                    d.current.density = density;
                }
                keep
            });
        if shown {
            self.sync_img_renderer(el);
            self.settle_decodes(el);
            return;
        }
        // 12-14. A new request: the current one if that shows nothing, else the pending one.
        let lazy = self.will_lazy_load(el);
        self.img(el, |d| {
            let mut request = ImageRequest::new(url.clone(), density);
            request.lazy = lazy;
            // 23. "delay load event": only for an image that is not lazy loaded.
            request.delays_load = !lazy;
            if matches!(
                d.current.state,
                RequestState::Unavailable | RequestState::Broken
            ) {
                d.pending = None;
                d.current = request;
            } else {
                d.pending = Some(request);
            }
        });
        self.sync_img_renderer(el);
        // 23-24. A lazy image waits for the lazy load resumption steps; others fetch now.
        if !lazy {
            self.fetch_image_request(el, gen);
        }
    }

    /// HTML "will lazy load element steps": `loading=lazy` in a document where scripting is
    /// enabled.
    fn will_lazy_load(&self, el: NodeId) -> bool {
        self.dom()
            .attr(el, &ns!(), &snowlark_dom::LocalName::from("loading"))
            .is_some_and(|l| l.eq_ignore_ascii_case("lazy"))
    }

    /// Starts the fetch of `el`'s newest request (or completes it when the image is there).
    fn fetch_image_request(&self, el: NodeId, gen: u32) {
        let url = self.img(el, |d| match &mut d.pending {
            Some(p) => {
                p.lazy = false;
                p.url.clone()
            }
            None => {
                d.current.lazy = false;
                d.current.url.clone()
            }
        });
        match self.image_status(el, &url, true) {
            Some(ImageStatus::Fetching) => {}
            Some(status) => self.image_request_done(el, gen, &url, status),
            None => self.image_request_done(el, gen, &url, ImageStatus::Failed),
        }
    }

    /// The renderers' image URLs that completed: the requests waiting for them complete.
    pub(crate) fn image_urls_arrived(&self, arrived: Vec<(String, ImageStatus)>) {
        if arrived.is_empty() {
            return;
        }
        let links: Vec<(NodeId, bool)> = {
            let mut images = self.images.borrow_mut();
            let mut done = Vec::new();
            images.preload_waiters.retain(|(el, url)| {
                match arrived.iter().find(|(u, _)| u == url) {
                    Some((_, status)) => {
                        done.push((*el, matches!(status, ImageStatus::Loaded { .. })));
                        false
                    }
                    None => true,
                }
            });
            done
        };
        for (el, ok) in links {
            if self.dom().is_alive(el) {
                self.queue_element_task_fire(el, if ok { "load" } else { "error" }, false, false);
            }
        }
        let waiting: Vec<(NodeId, u32, String)> = self
            .images
            .borrow()
            .elements
            .iter()
            .filter_map(|(&el, d)| {
                let r = d.pending.as_ref().unwrap_or(&d.current);
                (r.state == RequestState::Unavailable && !r.lazy && !r.url.is_empty())
                    .then(|| (el, d.generation, r.url.clone()))
            })
            .collect();
        for (el, gen, url) in waiting {
            if let Some((_, status)) = arrived.iter().find(|(u, _)| *u == url) {
                self.image_request_done(el, gen, &url, *status);
            }
        }
    }

    /// A request of `el` for `url` completed with `status`.
    fn image_request_done(&self, el: NodeId, gen: u32, url: &str, status: ImageStatus) {
        let ok = matches!(status, ImageStatus::Loaded { .. });
        let applied = self.img(el, |d| {
            let pending = d.pending.as_ref().is_some_and(|p| p.url == url);
            if !pending && d.current.url != url {
                return false;
            }
            // ("upgrade the pending request to the current request")
            if pending {
                d.current = d.pending.take().expect("pending");
            }
            match status {
                ImageStatus::Loaded {
                    width,
                    height,
                    ratio,
                } => {
                    d.current.state = RequestState::CompletelyAvailable;
                    d.current.natural = (width, height);
                    d.current.ratio = ratio;
                }
                _ => {
                    d.current.state = RequestState::Broken;
                    d.current.natural = (None, None);
                    d.current.ratio = None;
                }
            }
            true
        });
        if applied {
            self.sync_img_renderer(el);
            if let ImageStatus::Loaded {
                width: Some(w),
                height: Some(h),
                ..
            } = status
            {
                // (an image document's own image: its title and look, before `load`)
                let size = (w.round() as u32, h.round() as u32);
                self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                    if e.dom().is_alive(el) && e.img(el, |d| d.generation) == gen {
                        e.image_document_loaded(cx, el, size);
                    }
                });
            }
            self.queue_img_event(el, gen, ok);
            self.settle_decodes(el);
        }
    }

    /// The renderer shows the current request's image (none while it is broken).
    fn sync_img_renderer(&self, el: NodeId) {
        let (request, delays_load) = self.img(el, |d| {
            let c = &d.current;
            (
                (c.state != RequestState::Broken && !c.url.is_empty())
                    .then(|| (c.url.clone(), c.density)),
                d.pending.as_ref().unwrap_or(c).delays_load,
            )
        });
        self.with_node_renderer(el, |r, st| {
            r.set_img_delays_load(el, delays_load);
            r.set_img_request(el, request);
            st.dirty = true;
        });
    }

    /// Queues the element task that fires `load` or `error` (unless a later run superseded it).
    fn queue_img_event(&self, el: NodeId, gen: u32, ok: bool) {
        self.queue_task(TaskSource::DomManipulation, move |e, cx| {
            if !e.dom().is_alive(el) || e.img(el, |d| d.generation) != gen {
                return;
            }
            e.fire_event(
                cx,
                super::node_handle(el),
                if ok { "load" } else { "error" },
                false,
                false,
            );
        });
    }

    /// HTML "allows auto-sizes": `loading=lazy`, and a `sizes` value that is `auto` or starts
    /// with `auto,` (ASCII case-insensitive).
    fn allows_auto_sizes(&self, el: NodeId) -> bool {
        if !self.will_lazy_load(el) {
            return false;
        }
        let dom = self.dom();
        dom.attr(el, &ns!(), &local_name!("sizes"))
            .is_some_and(|s| {
                let b = s.as_bytes();
                b.len() >= 4
                    && b[..4].eq_ignore_ascii_case(b"auto")
                    && (b.len() == 4 || b[4] == b',')
            })
    }

    /// `el`'s concrete object size width in CSS px while it is being rendered (as of the last
    /// layout): its content box's width.
    fn rendered_width(&self, el: NodeId) -> Option<f32> {
        if !self.dom().is_connected(el) {
            return None;
        }
        self.with_node_renderer(el, |r, _| r.geometry(el).map(|g| g.content_box.width))
            .flatten()
    }

    /// What an `auto` source size of `el` stands for (HTML "parse a sizes attribute"): when it
    /// allows auto-sizes, its concrete object size width while it is being rendered (which
    /// becomes its last auto-sizes width), else its last auto-sizes width; `None` otherwise.
    fn auto_sizes_width(&self, el: NodeId) -> Option<f32> {
        if !self.allows_auto_sizes(el) {
            return None;
        }
        // (an element that is not connected has no last auto-sizes width: removal resets it)
        if !self.dom().is_connected(el) {
            return self.img(el, |d| {
                d.auto_rendered = false;
                d.auto_width = None;
                None
            });
        }
        let width = self.rendered_width(el);
        self.img(el, |d| {
            d.auto_rendered = width.is_some();
            if width.is_some() {
                d.auto_width = width;
            }
            d.auto_width
        })
    }

    /// HTML "lazy load resumption steps" for the lazy images near their viewport after a layout,
    /// and for those that stopped being lazy. First, an `img` that allows auto-sizes and started
    /// being rendered, or whose concrete object size width changed, updates its image data (a
    /// relevant mutation, with "maybe omit events"); its request then waits for the next layout.
    pub(crate) fn images_after_layout(&self, cx: &mut snowlark_bindings::rt::Cx) {
        let autos: Vec<NodeId> = self
            .images
            .borrow()
            .elements
            .keys()
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
            .filter(|&el| self.dom().is_alive(el) && self.allows_auto_sizes(el))
            .collect();
        let mut changed = Vec::new();
        for el in autos {
            let width = self.rendered_width(el);
            let moved = self.img(el, |d| {
                let moved = width.is_some() && (!d.auto_rendered || d.auto_width != width);
                d.auto_rendered = width.is_some();
                if moved {
                    d.auto_width = width;
                }
                moved
            });
            if moved {
                changed.push(el);
            }
        }
        for &el in &changed {
            self.update_the_image_data_with(cx, el, true);
        }
        let lazy: Vec<(NodeId, u32)> = self
            .images
            .borrow()
            .elements
            .iter()
            .filter_map(|(&el, d)| {
                let r = d.pending.as_ref().unwrap_or(&d.current);
                (r.lazy && r.state == RequestState::Unavailable).then_some((el, d.generation))
            })
            .collect();
        for (el, gen) in lazy {
            if !self.dom().is_alive(el) || changed.contains(&el) {
                continue;
            }
            if !self.will_lazy_load(el) || self.near_viewport(el) {
                self.fetch_image_request(el, gen);
            }
        }
    }

    /// Whether `el` is rendered within the lazy load root margin of its document's viewport (as
    /// of the last layout): a viewport's size around it, at least 1250 CSS px, as Chromium.
    pub(crate) fn near_viewport(&self, el: NodeId) -> bool {
        self.with_node_renderer(el, |r, st| {
            r.geometry(el).is_some_and(|g| {
                let b = g.border_box;
                let (mx, my) = (
                    st.viewport.width.max(1250.0),
                    st.viewport.height.max(1250.0),
                );
                let (x, y) = (b.x - st.scroll.x, b.y - st.scroll.y);
                x + b.width >= -mx
                    && x <= st.viewport.width + mx
                    && y + b.height >= -my
                    && y <= st.viewport.height + my
            })
        })
        .unwrap_or(false)
    }

    /// The `loading` attribute changed: a request that waited for lazy loading and is no longer
    /// lazy fetches now ("lazy load resumption steps").
    pub(crate) fn img_loading_changed(&self, el: NodeId) {
        if self.will_lazy_load(el) {
            return;
        }
        let waiting = self.images.borrow().elements.get(&el).and_then(|d| {
            let r = d.pending.as_ref().unwrap_or(&d.current);
            r.lazy.then_some(d.generation)
        });
        if let Some(gen) = waiting {
            self.fetch_image_request(el, gen);
        }
    }

    /// Insertion steps for the image elements: an `img` (or a `source` of a `picture`) inserted.
    /// An `img` whose image data was never updated (made by the parser, cloned, or created with
    /// its attributes) updates it now, and one inserted into a `picture` selects again. Only the
    /// root of an inserted subtree counts for a `picture` (a `picture` inserted with its children
    /// keeps their selection, as other engines do), and a `source` changes only the `img`
    /// elements after it.
    pub(crate) fn img_inserted(
        &self,
        cx: &mut snowlark_bindings::rt::Cx,
        node: NodeId,
        root: bool,
    ) {
        let (is_img, is_source, in_picture) = {
            let dom = self.dom();
            (
                dom.is_html_element_named(node, &local_name!("img")),
                dom.is_html_element_named(node, &local_name!("source")),
                dom.parent_element(node)
                    .is_some_and(|p| dom.is_html_element_named(p, &local_name!("picture"))),
            )
        };
        if is_img {
            let known = self.images.borrow().elements.contains_key(&node);
            if (in_picture && root) || !known {
                self.update_the_image_data(cx, node);
            }
        } else if is_source && in_picture && root {
            self.picture_changed(cx, node);
        }
    }

    /// Removing steps: an `img` leaving a `picture`, or a `source` leaving one (for the `img`
    /// elements that followed it, `old_next` on). A removed `img` forgets its last auto-sizes
    /// width (the removing steps; a `moveBefore()` runs the moving steps, which keep it).
    pub(crate) fn img_removed(
        &self,
        cx: &mut snowlark_bindings::rt::Cx,
        node: NodeId,
        old_parent: Option<NodeId>,
        old_next: Option<NodeId>,
    ) {
        let Some(parent) = old_parent else { return };
        if self.dom().is_html_element_named(node, &local_name!("img")) && !self.svg.borrow().moving
        {
            if let Some(d) = self.images.borrow_mut().elements.get_mut(&node) {
                d.auto_width = None;
                d.auto_rendered = false;
            }
        }
        let (is_img, is_source, picture) = {
            let dom = self.dom();
            (
                dom.is_html_element_named(node, &local_name!("img")),
                dom.is_html_element_named(node, &local_name!("source")),
                dom.is_alive(parent) && dom.is_html_element_named(parent, &local_name!("picture")),
            )
        };
        if !picture {
            return;
        }
        if is_img {
            self.update_the_image_data(cx, node);
        } else if is_source {
            let imgs: Vec<NodeId> = {
                let dom = self.dom();
                std::iter::successors(old_next.filter(|&n| dom.parent(n) == Some(parent)), |&n| {
                    dom.next_sibling(n)
                })
                .filter(|&c| dom.is_html_element_named(c, &local_name!("img")))
                .collect()
            };
            for img in imgs {
                self.update_the_image_data(cx, img);
            }
        }
    }

    /// A `source` of a `picture` was inserted or changed: the `img` elements after it in the
    /// `picture` select their source again.
    pub(crate) fn picture_changed(&self, cx: &mut snowlark_bindings::rt::Cx, source: NodeId) {
        let imgs: Vec<NodeId> = {
            let dom = self.dom();
            let Some(parent) = dom.parent_element(source) else {
                return;
            };
            if !dom.is_html_element_named(parent, &local_name!("picture")) {
                return;
            }
            dom.element_children(parent)
                .skip_while(|&c| c != source)
                .filter(|&c| dom.is_html_element_named(c, &local_name!("img")))
                .collect()
        };
        for img in imgs {
            self.update_the_image_data(cx, img);
        }
    }

    // ----- what script reads --------------------------------------------------------------------------

    /// `img.complete` (HTML §4.8.4.4).
    pub(crate) fn img_complete(&self, el: NodeId) -> bool {
        let (src, srcset) = {
            let dom = self.dom();
            (
                dom.attr(el, &ns!(), &local_name!("src")).map(str::to_owned),
                dom.attr(el, &ns!(), &local_name!("srcset")).is_some(),
            )
        };
        if !srcset && src.as_deref().is_none_or(str::is_empty) {
            return true;
        }
        let images = self.images.borrow();
        let Some(d) = images.elements.get(&el) else {
            return false;
        };
        !d.updating
            && d.pending.is_none()
            && matches!(
                d.current.state,
                RequestState::CompletelyAvailable | RequestState::Broken
            )
    }

    /// `img.currentSrc`: the current request's current URL.
    pub(crate) fn img_current_src(&self, el: NodeId) -> String {
        self.images
            .borrow()
            .elements
            .get(&el)
            .map(|d| d.current.url.clone())
            .unwrap_or_default()
    }

    /// The density-corrected natural size of the current request's image, if it is available.
    pub(crate) fn img_natural_size(&self, el: NodeId) -> Option<(f64, f64)> {
        let images = self.images.borrow();
        let d = images.elements.get(&el)?;
        if !matches!(
            d.current.state,
            RequestState::CompletelyAvailable | RequestState::PartiallyAvailable
        ) {
            return None;
        }
        let density = if d.current.density > 0.0 {
            d.current.density
        } else {
            1.0
        };
        let (w, h) = d.current.natural;
        Some(natural_dimensions(
            w.map(|w| w / density),
            h.map(|h| h / density),
            d.current.ratio,
        ))
    }

    // ----- decode() --------------------------------------------------------------------------------------

    /// `img.decode()` (HTML §4.8.4.4).
    pub(crate) fn img_decode(
        &self,
        cx: &mut snowlark_bindings::rt::Cx,
        el: NodeId,
    ) -> snowlark_bindings::rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(snowlark_bindings::rt::Error::Pending)?;
        // 2. The checks run in a microtask (after an adoption or a source change made in the
        // same script).
        let called = self.img(el, |d| {
            (
                d.generation,
                d.pending.as_ref().unwrap_or(&d.current).url.clone(),
            )
        });
        self.images
            .borrow_mut()
            .decode_checks
            .push((el, called, p.clone()));
        self.queue_internal_microtask(cx, super::reactions::Microtask::ImageDecode);
        Ok(p)
    }

    /// The microtask of `decode()` (step 2): a document that is not fully active or a broken
    /// current request rejects; an available one resolves; otherwise the promise waits for it.
    pub(crate) fn img_decode_microtask(&self, cx: &mut snowlark_bindings::rt::Cx) {
        let checks = std::mem::take(&mut self.images.borrow_mut().decode_checks);
        for (el, called, p) in checks {
            if !self.dom().is_alive(el) {
                continue;
            }
            let doc = self.dom().node_document(el);
            let (state, gen, updating, url) = self.img(el, |d| {
                (
                    if d.pending.is_some() {
                        RequestState::Unavailable
                    } else {
                        d.current.state
                    },
                    d.generation,
                    d.updating,
                    d.pending.as_ref().unwrap_or(&d.current).url.clone(),
                )
            });
            // (an element "update the image data" never ran for, with no source: its current
            // request is broken, as that run would have left it)
            let no_source = state == RequestState::Unavailable
                && !updating
                && !self.uses_srcset_or_picture(el)
                && self.dom().attr(el, &ns!(), &local_name!("src")).is_none();
            // (a run of "update the image data" that started after the call changes or mutates
            // the current request the promise waits for)
            // (one that found the same image available already leaves it as it was)
            let superseded =
                gen != called.0 && !(state == RequestState::CompletelyAvailable && url == called.1);
            if !self.is_active_document(doc)
                || state == RequestState::Broken
                || no_source
                || superseded
            {
                p.reject(
                    cx,
                    exception(ExceptionName::EncodingError, "The image cannot be decoded"),
                );
            } else if state == RequestState::CompletelyAvailable {
                p.resolve(cx, ());
            } else {
                self.img(el, |d| d.decodes.push((gen, p)));
            }
        }
    }

    /// The current request settled: the `decode()` promises waiting for it settle.
    fn settle_decodes(&self, el: NodeId) {
        let (ok, pending, decodes) = self.img(el, |d| {
            (
                d.current.state == RequestState::CompletelyAvailable,
                d.pending.is_some(),
                std::mem::take(&mut d.decodes),
            )
        });
        if decodes.is_empty() {
            return;
        }
        if pending {
            self.img(el, |d| d.decodes = decodes);
            return;
        }
        self.queue_task(TaskSource::DomManipulation, move |_, cx| {
            for (_, p) in decodes {
                if ok {
                    p.resolve(cx, ());
                } else {
                    p.reject(
                        cx,
                        exception(ExceptionName::EncodingError, "The image cannot be decoded"),
                    );
                }
            }
        });
    }

    /// A new run of "update the image data" rejects the `decode()` promises of earlier runs.
    fn reject_decodes(&self, el: NodeId, gen: u32) {
        let old: Vec<JsPromise> = self.img(el, |d| {
            let (old, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut d.decodes)
                .into_iter()
                .partition(|(g, _)| *g != gen);
            d.decodes = keep;
            old.into_iter().map(|(_, p)| p).collect()
        });
        if old.is_empty() {
            return;
        }
        self.queue_task(TaskSource::DomManipulation, move |_, cx| {
            for p in old {
                p.reject(
                    cx,
                    exception(
                        ExceptionName::EncodingError,
                        "The image's source changed before it decoded",
                    ),
                );
            }
        });
    }
}

/// `naturalWidth` and `naturalHeight` of an available image (HTML §4.8.4.4, CSS Images 3 §5.2):
/// its density-corrected natural dimensions, with a missing one from the natural aspect ratio or
/// the default object size (300x150), into which an image with only a ratio is fit.
fn natural_dimensions(w: Option<f32>, h: Option<f32>, ratio: Option<f32>) -> (f64, f64) {
    const DEFAULT: (f32, f32) = (300.0, 150.0);
    let ratio = ratio.filter(|r| *r > 0.0 && r.is_finite());
    let (w, h) = match (w, h, ratio) {
        (Some(w), Some(h), _) => (w, h),
        (Some(w), None, Some(r)) => (w, w / r),
        (Some(w), None, None) => (w, DEFAULT.1),
        (None, Some(h), Some(r)) => (h * r, h),
        (None, Some(h), None) => (DEFAULT.0, h),
        (None, None, Some(r)) if r >= DEFAULT.0 / DEFAULT.1 => (DEFAULT.0, DEFAULT.0 / r),
        (None, None, Some(r)) => (DEFAULT.1 * r, DEFAULT.1),
        (None, None, None) => DEFAULT,
    };
    (w as f64, h as f64)
}

#[cfg(test)]
mod tests {
    use super::natural_dimensions;

    #[test]
    fn missing_natural_dimensions_come_from_the_ratio_or_the_default_size() {
        assert_eq!(
            natural_dimensions(Some(20.0), Some(10.0), None),
            (20.0, 10.0)
        );
        assert_eq!(natural_dimensions(None, None, None), (300.0, 150.0));
        assert_eq!(natural_dimensions(Some(40.0), None, None), (40.0, 150.0));
        assert_eq!(natural_dimensions(None, Some(40.0), None), (300.0, 40.0));
        assert_eq!(
            natural_dimensions(Some(40.0), None, Some(2.0)),
            (40.0, 20.0)
        );
        assert_eq!(
            natural_dimensions(None, Some(40.0), Some(2.0)),
            (80.0, 40.0)
        );
        // Only a ratio: contained in 300x150.
        assert_eq!(natural_dimensions(None, None, Some(3.0)), (300.0, 100.0));
        assert_eq!(natural_dimensions(None, None, Some(1.0)), (150.0, 150.0));
    }
}
