// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`DocumentStyles`]: layout's [`StyleSource`] over the style engine: element and pseudo-element styles
//! from stylo, anonymous box styles built by inheritance (CSS 2 §9.2.1.1, CSS Tables 3 §3.2), generated
//! content with counters resolved, and the replaced content of images and other replaced elements
//! (HTML §15.4).

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

use servo_arc::Arc as ServoArc;
use snowlark_dom::{local_name, ns, Dom, NodeId};
use snowlark_layout::style::{
    AnonymousBox, Color, Content, FormPart, Image, PseudoElement, ReplacedContent, Style,
    StyleSource,
};
use snowlark_style::{StyleData, StyleEngine};
use style::properties::{ComputedValues, StyleBuilder};
use style::selector_parser::PseudoElement as StyloPseudo;
use style::values::specified::box_::Display as StyloDisplay;

use crate::adapter::RenderStyle;
use crate::content::{needs_walk, pseudo_key, CounterWalk};
use crate::env::{LoadedImage, StyleEnv};

/// What the page says about a media element for its rendering (HTML §4.8.11, §15.4): the video's
/// size once its metadata is known, whether the poster or a frame shows, the frames' slot, and the
/// user agent's controls.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaState {
    /// The video's own width and height (none before the metadata, or for audio).
    pub natural: Option<(u32, u32)>,
    /// HTML's "show poster flag": the poster (or nothing) shows instead of a frame.
    pub show_poster: bool,
    /// The slot of the video's GPU frames.
    pub video: Option<snowlark_paint::VideoId>,
    /// The page's id of the protected player whose picture the element shows instead (its frames
    /// never reach the page: the compositor shows the player's own surface).
    pub protected: Option<u64>,
    pub controls: Option<snowlark_layout::media::MediaControls>,
}

/// The page's media element states and the frames copied to memory, by element.
type MediaMaps<'a> = (
    &'a HashMap<NodeId, MediaState>,
    &'a HashMap<NodeId, snowlark_paint::ImageId>,
);

/// Where an element's image stands.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ImageState {
    /// Requested, not arrived.
    Pending,
    Loaded(LoadedImage),
    /// No URL, a network error, or undecodable data.
    Failed,
}

pub(crate) fn stylo_pseudo(p: PseudoElement) -> Option<StyloPseudo> {
    Some(match p {
        PseudoElement::Before => StyloPseudo::Before,
        PseudoElement::After => StyloPseudo::After,
        PseudoElement::Marker => StyloPseudo::Marker,
        PseudoElement::FirstLetter => StyloPseudo::FirstLetter,
        PseudoElement::FirstLine => StyloPseudo::FirstLine,
        PseudoElement::Backdrop => StyloPseudo::Backdrop,
        PseudoElement::DetailsContent => StyloPseudo::DetailsContent,
        PseudoElement::Part(p) => stylo_part(p),
        PseudoElement::DetailsSummary => return None,
    })
}

/// The generated box a stylo pseudo-element is, for those a compositor animates apart
/// (`snowlark_layout::composited_key`): `::before`, `::after`, `::marker`.
pub(crate) fn layout_pseudo(p: &StyloPseudo) -> Option<PseudoElement> {
    Some(match p {
        StyloPseudo::Before => PseudoElement::Before,
        StyloPseudo::After => PseudoElement::After,
        StyloPseudo::Marker => PseudoElement::Marker,
        _ => return None,
    })
}

fn stylo_part(p: FormPart) -> StyloPseudo {
    match p {
        FormPart::ProgressInner => StyloPseudo::WebkitProgressInnerElement,
        FormPart::ProgressBar => StyloPseudo::WebkitProgressBar,
        FormPart::ProgressValue => StyloPseudo::WebkitProgressValue,
        FormPart::MeterInner => StyloPseudo::WebkitMeterInnerElement,
        FormPart::MeterBar => StyloPseudo::WebkitMeterBar,
        FormPart::MeterOptimumValue => StyloPseudo::WebkitMeterOptimumValue,
        FormPart::MeterSuboptimumValue => StyloPseudo::WebkitMeterSuboptimumValue,
        FormPart::MeterEvenLessGoodValue => StyloPseudo::WebkitMeterEvenLessGoodValue,
    }
}

/// Anonymous box styles by (parent values address, kind), each with the parent it was made from.
type AnonymousCache = HashMap<(usize, u8), (ServoArc<ComputedValues>, RenderStyle)>;

/// The style source of one document for one box tree build.
pub struct DocumentStyles<'a> {
    dom: &'a Dom,
    engine: &'a StyleEngine,
    data: &'a StyleData,
    env: Arc<StyleEnv>,
    images: &'a HashMap<NodeId, ImageState>,
    /// Form controls' state from the page (values, checkedness, carets).
    controls: Option<&'a HashMap<NodeId, crate::controls::ControlState>>,
    /// The document's top layer, bottom first.
    top_layer: &'a [NodeId],
    /// The document's fullscreen element.
    fullscreen: Option<NodeId>,
    embed_types: Option<&'a HashMap<NodeId, String>>,
    /// The documents shown by the `object` and `embed` elements' navigables.
    embedded_documents: Option<&'a HashMap<NodeId, NodeId>>,
    /// Media elements' state from the page, and the frames copied to memory.
    media: Option<MediaMaps<'a>>,
    /// The canvases whose frames reach the compositor on the GPU, by element.
    canvas_videos: Option<&'a HashMap<NodeId, snowlark_paint::VideoId>>,
    /// Anonymous box styles by (parent values address, kind); the entry holds the parent so the address
    /// stays its own.
    anonymous: RefCell<AnonymousCache>,
    generated: OnceCell<HashMap<(NodeId, u8), Arc<Content>>>,
    may_style_markers: OnceCell<bool>,
    /// A size query container's style was handed to layout (CSS Contain 3 §2).
    query_containers: Cell<bool>,
    /// The `clipPath`s elements' `clip-path: url()` name.
    clip_references: Option<&'a HashMap<NodeId, crate::renderer::ClipUse>>,
    filter_references: Option<&'a HashMap<NodeId, Option<Arc<crate::renderer::FilterReferences>>>>,
    /// The elements' last remembered sizes (CSS Sizing 4 5.2.1), from the renderer.
    remembered: Option<&'a HashMap<NodeId, (Option<f32>, Option<f32>)>>,
    /// The elements' implicit anchor elements, from the renderer.
    implicit_anchors: Option<&'a HashMap<NodeId, NodeId>>,
    /// An element with `contain-intrinsic-size: auto` was handed to layout.
    auto_intrinsic: Cell<bool>,
    /// HTML "PDF viewer supported": PDF `object`s and `embed`s show a nested document.
    pdf_viewer: bool,
    /// [`crate::adapter::CustomScrollbars`] of the renderer (none: each is cascaded).
    custom_scrollbars: Option<&'a crate::adapter::CustomScrollbars>,
}

/// The URL an `object` (`data`) or `embed` (`src`) element would load, when it has one and is not
/// inside a media element (HTML §4.8.6, §4.8.7: such elements show nothing of their own).
fn embedded_url(dom: &Dom, el: NodeId) -> Option<&str> {
    let object = dom.is_html_element_named(el, &local_name!("object"));
    if !object && !dom.is_html_element_named(el, &local_name!("embed")) {
        return None;
    }
    let url = dom
        .get_attribute(el, if object { "data" } else { "src" })
        .filter(|u| !u.trim().is_empty())?;
    if dom.ancestors(el).any(|a| {
        dom.is_html_element_named(a, &local_name!("video"))
            || dom.is_html_element_named(a, &local_name!("audio"))
    }) {
        return None;
    }
    Some(url)
}

/// The type of an `object`'s or `embed`'s resource (HTML §4.8.7 "determine the resource type"):
/// `known` is its response's Content-Type essence once that arrived. Before, the type is a guess
/// that chooses how to fetch it: the essence of the `type` attribute, else what the URL's path
/// extension names for the image types.
fn embedded_type(dom: &Dom, el: NodeId, url: &str, known: Option<&str>) -> Option<String> {
    if let Some(t) = known {
        return Some(t.to_owned());
    }
    if let Some(t) = dom.get_attribute(el, "type") {
        let t = t.trim().to_ascii_lowercase();
        if !t.is_empty() {
            return Some(t.split(';').next().unwrap_or("").trim().to_owned());
        }
    }
    let path = url.split(['?', '#']).next().unwrap_or("");
    let ext = path.rsplit_once('.')?.1.to_ascii_lowercase();
    let essence = match ext.as_str() {
        "png" | "apng" => "image/png",
        "gif" => "image/gif",
        "jpg" | "jpeg" | "jpe" | "jfif" | "pjpeg" | "pjp" => "image/jpeg",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" | "cur" => "image/x-icon",
        "avif" => "image/avif",
        _ => return None,
    };
    Some(essence.to_owned())
}

/// Whether an `object` or `embed` element shows a nested document (HTML §4.8.6, §4.8.7): it has a
/// URL (`data`, `src`), is not inside a media element, and has no `type` or a document type, and
/// does not show an image ([`embedded_image`]). Snowlark has no plugins, but with `pdf_viewer` (HTML
/// "PDF viewer supported") a PDF type shows the PDF viewer, which the engine runs as a nested document
/// (as Chromium runs its viewer in a frame); an `object` otherwise represents its fallback content, an
/// `embed` nothing.
pub fn embeds_document(dom: &Dom, el: NodeId, known: Option<&str>, pdf_viewer: bool) -> bool {
    let Some(url) = embedded_url(dom, el) else {
        return false;
    };
    match embedded_type(dom, el, url, known) {
        None => true,
        Some(t) => {
            matches!(
                t.as_str(),
                "text/html" | "application/xhtml+xml" | "image/svg+xml" | "text/plain"
            ) || t.ends_with("+xml")
                || t == "text/xml"
                || t == "application/xml"
                || (pdf_viewer && is_pdf_type(&t))
        }
    }
}

/// The natural dimensions of an outermost `svg` element (SVG 2 §8.2, CSS Images 3 §4.1): its
/// `width` and `height` in pixels and its `viewBox`'s ratio; a missing length follows from the
/// other and the ratio, and with none of them it is 300×150. (width, height, ratio)
pub fn svg_root_natural(dom: &Dom, el: NodeId) -> (Option<f32>, Option<f32>, Option<f32>) {
    let attr_px = |n: &str| -> Option<f32> {
        let v = dom.get_attribute(el, n)?.trim();
        let v = v.strip_suffix("px").unwrap_or(v);
        v.parse::<f32>().ok().filter(|v| v.is_finite() && *v >= 0.0)
    };
    let ratio = dom.get_attribute(el, "viewBox").and_then(|v| {
        let n: Vec<f32> = v
            .split(|c: char| c == ',' || c.is_ascii_whitespace())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect();
        (n.len() == 4 && n[2] > 0.0 && n[3] > 0.0).then(|| n[2] / n[3])
    });
    let (w, h) = match (attr_px("width"), attr_px("height"), ratio) {
        (None, None, None) => (Some(300.0), Some(150.0)),
        (Some(w), None, Some(r)) => (Some(w), Some(w / r)),
        (None, Some(h), Some(r)) => (Some(h * r), Some(h)),
        other => (other.0, other.1),
    };
    (w, h, ratio)
}

/// The root `svg` element of `doc`, when it is an SVG document's (its document element is an
/// `svg` in the SVG namespace).
pub fn svg_document_root(dom: &Dom, doc: NodeId) -> Option<NodeId> {
    let root = dom.document_element(doc)?;
    let e = dom.element(root)?;
    (e.name().ns == ns!(svg) && e.name().local == local_name!("svg")).then_some(root)
}

/// HTML §8.9.1.6: the PDF viewer MIME types ("application/pdf", "text/pdf").
pub fn is_pdf_type(essence: &str) -> bool {
    essence.eq_ignore_ascii_case("application/pdf") || essence.eq_ignore_ascii_case("text/pdf")
}

/// The URL of the image an `object` or `embed` element represents (HTML §4.8.7: an object whose
/// resource type is an image type other than SVG represents that image, which `object-fit` and
/// `object-position` place like an `img`'s; `embed` shows image types the same way, as browsers
/// do), or `None`. `known` is the resource's Content-Type essence once it arrived.
pub fn embedded_image<'d>(dom: &'d Dom, el: NodeId, known: Option<&str>) -> Option<&'d str> {
    let url = embedded_url(dom, el)?;
    let t = embedded_type(dom, el, url, known)?;
    (t.starts_with("image/") && t != "image/svg+xml").then_some(url)
}

impl<'a> DocumentStyles<'a> {
    pub fn new(
        dom: &'a Dom,
        engine: &'a StyleEngine,
        data: &'a StyleData,
        env: Arc<StyleEnv>,
        images: &'a HashMap<NodeId, ImageState>,
    ) -> Self {
        DocumentStyles {
            dom,
            engine,
            data,
            env,
            images,
            controls: None,
            top_layer: &[],
            fullscreen: None,
            embed_types: None,
            embedded_documents: None,
            media: None,
            canvas_videos: None,
            anonymous: RefCell::new(HashMap::new()),
            generated: OnceCell::new(),
            may_style_markers: OnceCell::new(),
            query_containers: Cell::new(false),
            clip_references: None,
            filter_references: None,
            remembered: None,
            implicit_anchors: None,
            auto_intrinsic: Cell::new(false),
            pdf_viewer: false,
            custom_scrollbars: None,
        }
    }

    /// The renderer's custom scroll bars by element, reused while nothing that decides them changed.
    pub(crate) fn with_custom_scrollbars(
        mut self,
        cache: &'a crate::adapter::CustomScrollbars,
    ) -> Self {
        self.custom_scrollbars = Some(cache);
        self
    }

    /// Whether the embedder views PDFs inline (HTML "PDF viewer supported").
    pub fn with_pdf_viewer(mut self, on: bool) -> Self {
        self.pdf_viewer = on;
        self
    }

    /// The elements' last remembered sizes.
    pub fn with_remembered(
        mut self,
        remembered: &'a HashMap<NodeId, (Option<f32>, Option<f32>)>,
    ) -> Self {
        self.remembered = Some(remembered);
        self
    }

    /// The elements' implicit anchor elements (CSS Anchor Positioning 1 §2.4.1).
    pub fn with_implicit_anchors(mut self, anchors: &'a HashMap<NodeId, NodeId>) -> Self {
        self.implicit_anchors = Some(anchors);
        self
    }

    /// Whether an element with `contain-intrinsic-size: auto` was among the styles handed out.
    pub fn auto_intrinsic_seen(&self) -> bool {
        self.auto_intrinsic.get()
    }

    /// What elements' `clip-path: url()` name.
    pub(crate) fn with_filter_references(
        mut self,
        refs: &'a HashMap<NodeId, Option<Arc<crate::renderer::FilterReferences>>>,
    ) -> Self {
        self.filter_references = Some(refs);
        self
    }

    pub(crate) fn with_clip_references(
        mut self,
        clips: &'a HashMap<NodeId, crate::renderer::ClipUse>,
    ) -> Self {
        self.clip_references = Some(clips);
        self
    }

    /// The page's form control state, overriding the defaults from attributes.
    pub fn with_controls(
        mut self,
        controls: &'a HashMap<NodeId, crate::controls::ControlState>,
    ) -> Self {
        self.controls = Some(controls);
        self
    }

    /// The document's top layer (from the page: modal dialogs, open popovers).
    /// The Content-Type essences of the `object` and `embed` resources that arrived.
    pub fn with_embed_types(mut self, types: &'a HashMap<NodeId, String>) -> Self {
        self.embed_types = Some(types);
        self
    }

    fn embed_type(&self, el: NodeId) -> Option<&str> {
        self.embed_types?.get(&el).map(String::as_str)
    }

    /// The documents shown by the `object` and `embed` elements' navigables, by element.
    pub fn with_embedded_documents(mut self, docs: &'a HashMap<NodeId, NodeId>) -> Self {
        self.embedded_documents = Some(docs);
        self
    }

    /// The root `svg` element of the SVG document an `object` or `embed` shows.
    fn embedded_svg_root(&self, el: NodeId) -> Option<NodeId> {
        let doc = *self.embedded_documents?.get(&el)?;
        self.dom.is_alive(doc).then_some(())?;
        svg_document_root(self.dom, doc)
    }

    pub fn with_top_layer(mut self, top_layer: &'a [NodeId]) -> Self {
        self.top_layer = top_layer;
        self
    }

    pub fn with_fullscreen(mut self, fullscreen: Option<NodeId>) -> Self {
        self.fullscreen = fullscreen;
        self
    }

    /// The page's media element state and the frames copied to memory.
    pub fn with_media(
        mut self,
        media: &'a HashMap<NodeId, MediaState>,
        frames: &'a HashMap<NodeId, snowlark_paint::ImageId>,
    ) -> Self {
        self.media = Some((media, frames));
        self
    }

    /// The canvases whose frames reach the compositor on the GPU (shared textures, not a
    /// bitmap): each paints as its frames' video item.
    pub fn with_canvas_videos(
        mut self,
        videos: &'a HashMap<NodeId, snowlark_paint::VideoId>,
    ) -> Self {
        self.canvas_videos = Some(videos);
        self
    }

    /// A media element's content (HTML §15.4.? "video", §4.8.11 controls): a video's natural size
    /// is its own once known, else its poster's, else none (the 300×150 default object size); it
    /// shows its poster until a frame shows. An `<audio>` with `controls` is its controls (300×54).
    fn media_content(&self, el: NodeId, video: bool) -> Option<ReplacedContent> {
        let state = self.media.and_then(|(m, _)| m.get(&el));
        let frame_image = self.media.and_then(|(_, f)| f.get(&el)).copied();
        let controls = state.and_then(|s| s.controls.clone()).or_else(|| {
            self.dom.get_attribute(el, "controls").is_some().then(|| {
                snowlark_layout::media::MediaControls {
                    paused: true,
                    duration: f64::NAN,
                    volume: 1.0,
                    audio: !video,
                    visible: true,
                    ..Default::default()
                }
            })
        });
        if !video {
            controls.as_ref()?;
            return Some(ReplacedContent {
                natural_width: Some(300.0),
                natural_height: Some(54.0),
                media: Some(Arc::new(snowlark_layout::media::MediaContent {
                    video: None,
                    protected: None,
                    frame_image: None,
                    controls,
                    canvas: false,
                })),
                ..ReplacedContent::default()
            });
        }
        let poster = match self.images.get(&el) {
            Some(ImageState::Loaded(i)) => Some(self.shown(i, el)),
            _ => None,
        };
        let show_frame =
            state.is_some_and(|s| !s.show_poster && (s.video.is_some() || s.protected.is_some()));
        let (w, h, ratio) = match (state.and_then(|s| s.natural), poster) {
            (Some((w, h)), _) if w > 0 && h > 0 => (Some(w as f32), Some(h as f32), None),
            // (the poster frame's natural dimensions, an SVG poster's ratio among them)
            (_, Some(p)) => (p.natural.width, p.natural.height, p.natural.ratio()),
            // No natural dimensions (nor ratio): CSS 2 §10.3.2's 300×150 default applies to each
            // axis on its own (csswg-drafts#12053).
            _ => (None, None, None),
        };
        Some(ReplacedContent {
            image: if show_frame {
                None
            } else {
                poster.map(|p| p.id)
            },
            natural_width: w,
            natural_height: h,
            natural_ratio: ratio,
            control: None,
            media: Some(Arc::new(snowlark_layout::media::MediaContent {
                video: if show_frame {
                    state.and_then(|s| s.video)
                } else {
                    None
                },
                protected: if show_frame {
                    state.and_then(|s| s.protected)
                } else {
                    None
                },
                frame_image: if show_frame { frame_image } else { None },
                controls,
                canvas: false,
            })),
            generated: None,
            svg_root: false,
        })
    }

    fn handle(&self, values: ServoArc<ComputedValues>) -> RenderStyle {
        RenderStyle::new(values, self.env.clone())
    }

    fn raw_pseudo(
        &self,
        element: NodeId,
        pseudo: PseudoElement,
    ) -> Option<ServoArc<ComputedValues>> {
        let p = stylo_pseudo(pseudo)?;
        if pseudo == PseudoElement::Marker {
            // CSS Lists 3 §2: only list items have a ::marker.
            let primary = self.data.styles(element)?;
            if !primary.get_box().display.is_list_item() {
                return None;
            }
        }
        self.engine.pseudo_style(self.dom, self.data, element, &p)
    }

    /// Whether generated content that depends on the rest of the document (counters, quotes,
    /// `attr()`) was resolved for this source.
    /// Whether a size query container (`container-type: size`/`inline-size`) was among the element
    /// styles handed out.
    pub fn query_containers_seen(&self) -> bool {
        self.query_containers.get()
    }

    pub fn generated_seen(&self) -> bool {
        self.generated.get().is_some_and(|g| !g.is_empty())
    }

    fn generated(&self) -> &HashMap<(NodeId, u8), Arc<Content>> {
        self.generated.get_or_init(|| {
            let doc = self.engine.document();
            let Some(root) = self.dom.document_element(doc) else {
                return HashMap::new();
            };
            let element_style = |n: NodeId| self.data.styles_arc(n);
            let pseudo_style = |n: NodeId, p: PseudoElement| self.raw_pseudo(n, p);
            let make = |v: ServoArc<ComputedValues>| self.handle(v);
            let mut walk = CounterWalk::new(self.dom, &element_style, &pseudo_style, &make);
            walk.run(root);
            walk.out
        })
    }

    /// Whether `el` fetches its image in CORS mode (HTML "CORS settings attributes").
    fn cors_mode(&self, el: NodeId) -> bool {
        self.dom.get_attribute(el, "crossorigin").is_some()
    }

    /// Whether `el`'s `image-orientation` is `from-image` (CSS Images 3 §5.3), the initial value.
    /// A loaded image as `el` shows it: its `image-orientation`, its used color scheme.
    fn shown(&self, image: &LoadedImage, el: NodeId) -> LoadedImage {
        let dark = self.data.styles_arc(el).is_some_and(|v| {
            crate::adapter::used_color_scheme(&v) == snowlark_layout::style::ColorScheme::Dark
        });
        image
            .oriented(self.orients_from_image(el), self.cors_mode(el))
            .for_scheme(dark)
    }

    /// HTML §15.4.4: an `img` that represents its `alt` text (its image failed for good, and the
    /// `alt` attribute is not empty) and has no dimensions of its own (`width` and `height` both
    /// `auto`, the dimension attributes among them) is a non-replaced element showing that text.
    /// With dimensions it stays a replaced box of that size, as other engines draw it. HTML §15.5:
    /// an `input` in the Image Button state that does not represent an image is rendered the same
    /// way, with its `alt` text.
    fn img_alt_text(&self, el: NodeId) -> Option<String> {
        let img = self.dom.is_html_element_named(el, &local_name!("img"));
        let image_button = !img
            && self.dom.is_html_element_named(el, &local_name!("input"))
            && self
                .dom
                .get_attribute(el, "type")
                .is_some_and(|t| t.eq_ignore_ascii_case("image"));
        let shows_alt = if img {
            matches!(self.images.get(&el), Some(ImageState::Failed))
        } else {
            image_button && !matches!(self.images.get(&el), Some(ImageState::Loaded(_)))
        };
        if !shows_alt {
            return None;
        }
        let alt = self
            .dom
            .get_attribute(el, "alt")
            .filter(|a| !a.is_empty())?;
        let values = self.data.styles_arc(el)?;
        let position = values.get_position();
        if !position.width.is_auto() || !position.height.is_auto() {
            return None;
        }
        Some(alt.to_owned())
    }

    fn orients_from_image(&self, el: NodeId) -> bool {
        use style::properties::longhands::image_orientation::computed_value::T;
        self.data
            .styles_arc(el)
            .is_none_or(|v| v.get_inherited_box().image_orientation == T::FromImage)
    }

    /// HTML §15.4.4 for `img`, the natural size of `canvas`/`video`, the `svg` root's from its
    /// attributes.
    fn replaced(&self, el: NodeId) -> Option<ReplacedContent> {
        let e = self.dom.element(el)?;
        let name = e.name();
        // CSS Generated Content 3 §1.1: `content: <image>` on an element (an element's content
        // replaced by one image) makes it a replaced element showing the image, once available.
        if let Some(values) = self.data.styles_arc(el) {
            use style::values::generics::counters::{GenericContent, GenericContentItem};
            if let GenericContent::Items(items) = &values.get_counters().content {
                if let [GenericContentItem::Image(image)] = &items.items[..items.alt_start] {
                    return match self.handle(values.clone()).image(image) {
                        Image::None => None,
                        Image::Raster { id, natural } => {
                            Some(ReplacedContent::with_natural(id, natural))
                        }
                        other => {
                            let natural = other.natural();
                            Some(ReplacedContent {
                                natural_width: natural.width,
                                natural_height: natural.height,
                                natural_ratio: natural.ratio,
                                generated: Some(other),
                                ..ReplacedContent::default()
                            })
                        }
                    };
                }
            }
        }
        let attr_px = |n: &str| -> Option<f32> {
            let v = self.dom.get_attribute(el, n)?.trim();
            let v = v.strip_suffix("px").unwrap_or(v);
            v.parse::<f32>().ok().filter(|v| v.is_finite() && *v >= 0.0)
        };
        if name.ns == ns!(html) {
            match name.local {
                local_name!("img") => {
                    if self.img_alt_text(el).is_some() {
                        return None;
                    }
                    return Some(match self.images.get(&el) {
                        Some(ImageState::Loaded(i)) => {
                            let i = self.shown(i, el);
                            ReplacedContent::with_natural(i.id, i.natural)
                        }
                        // An image that is not available has no natural dimensions; with none
                        // given by CSS it is 0×0 (it represents nothing, or its alt text, which
                        // is not rendered here).
                        _ => ReplacedContent {
                            natural_width: Some(0.0),
                            natural_height: Some(0.0),
                            ..ReplacedContent::default()
                        },
                    });
                }
                local_name!("input")
                | local_name!("select")
                | local_name!("textarea")
                | local_name!("progress")
                | local_name!("meter") => {
                    let values = self.data.styles_arc(el)?;
                    if self.has_form_parts(el) {
                        // Not a native control: a box drawn by its parts.
                        return None;
                    }
                    // `::placeholder`'s color (the UA sheet's grey unless the page styles it), for
                    // the controls that show a placeholder (resolving it is a cascade).
                    let grey = Color::rgb(0x75, 0x75, 0x75);
                    let placeholder_color = if self.dom.get_attribute(el, "placeholder").is_some() {
                        self.engine
                            .pseudo_style(self.dom, self.data, el, &StyloPseudo::Placeholder)
                            .map_or(grey, |p| self.handle(p).color())
                    } else {
                        grey
                    };
                    let control = crate::controls::control(
                        self.dom,
                        el,
                        &values,
                        placeholder_color,
                        self.controls.and_then(|c| c.get(&el)),
                    )?;
                    return Some(ReplacedContent {
                        control: Some(Arc::new(control)),
                        ..ReplacedContent::default()
                    });
                }
                local_name!("video") => return self.media_content(el, true),
                local_name!("audio") => return self.media_content(el, false),
                local_name!("canvas") => {
                    // HTML §4.12.5: a canvas's natural size is its width and height attributes,
                    // 300×150 by default. A canvas that drew shows its bitmap, or its frames on
                    // the GPU (a WebGL canvas presenting into shared textures).
                    let video = self.canvas_videos.and_then(|v| v.get(&el)).copied();
                    let image = match self.images.get(&el) {
                        Some(ImageState::Loaded(i)) if video.is_none() => Some(i.id),
                        _ => None,
                    };
                    let media = video.map(|video| {
                        Arc::new(snowlark_layout::media::MediaContent {
                            video: Some(video),
                            canvas: true,
                            ..Default::default()
                        })
                    });
                    return Some(ReplacedContent {
                        image,
                        natural_width: Some(attr_px("width").unwrap_or(300.0)),
                        natural_height: Some(attr_px("height").unwrap_or(150.0)),
                        natural_ratio: None,
                        control: None,
                        media,
                        generated: None,
                        svg_root: false,
                    });
                }
                local_name!("object") | local_name!("embed")
                    if embedded_image(self.dom, el, self.embed_type(el)).is_some() =>
                {
                    // The image it represents, sized and fitted as an `img`'s (one that has not
                    // loaded has the 300x150 default object size). An `object` whose image cannot
                    // be rendered represents its fallback content (HTML §4.8.7 "fallback").
                    return match self.images.get(&el) {
                        Some(ImageState::Loaded(i)) => {
                            let i = self.shown(i, el);
                            Some(ReplacedContent::with_natural(i.id, i.natural))
                        }
                        Some(ImageState::Failed) if name.local == local_name!("object") => None,
                        _ => Some(ReplacedContent::default()),
                    };
                }
                local_name!("object") | local_name!("embed")
                    if !embeds_document(self.dom, el, self.embed_type(el), self.pdf_viewer) =>
                {
                    return None;
                }
                // An `object` or `embed` showing an SVG document has the natural dimensions of its
                // root `svg` element (an iframe never takes its content's).
                local_name!("embed") | local_name!("object")
                    if self.embedded_svg_root(el).is_some() =>
                {
                    let root = self.embedded_svg_root(el)?;
                    let (w, h, ratio) = svg_root_natural(self.dom, root);
                    return Some(ReplacedContent {
                        natural_width: w,
                        natural_height: h,
                        natural_ratio: ratio,
                        ..ReplacedContent::default()
                    });
                }
                local_name!("iframe")
                | local_name!("frame")
                | local_name!("embed")
                | local_name!("object") => {
                    // No natural dimensions: CSS 2 §10.3.2's 300×150 default applies.
                    return Some(ReplacedContent::default());
                }
                _ => return None,
            }
        }
        if name.ns == ns!(svg) && name.local == local_name!("svg") {
            // An outermost svg element (its parent is not SVG) is replaced (SVG 2 §8.2).
            let parent_is_svg = self
                .dom
                .parent_element(el)
                .and_then(|p| self.dom.element(p))
                .is_some_and(|p| p.name().ns == ns!(svg));
            if parent_is_svg {
                return None;
            }
            let (w, h, ratio) = svg_root_natural(self.dom, el);
            // Its subtree, rendered as an SVG image (crate::inline_svg).
            let image = match self.images.get(&el) {
                Some(ImageState::Loaded(i)) => Some(i.id),
                _ => None,
            };
            return Some(ReplacedContent {
                image,
                natural_width: w,
                natural_height: h,
                natural_ratio: ratio,
                svg_root: true,
                ..ReplacedContent::default()
            });
        }
        None
    }
}

impl StyleSource for DocumentStyles<'_> {
    type Style = RenderStyle;

    fn element_style(&self, element: NodeId) -> Option<RenderStyle> {
        let values = self.data.styles_arc(element)?;
        if !values.get_box().container_type.is_empty() {
            self.query_containers.set(true);
        }
        let mut style = self.handle(values);
        style.animations = self.engine.animation_flags(element);
        if let Some(clip) = self.clip_references.and_then(|c| c.get(&element)) {
            style.clip_reference = clip.reference.clone();
        }
        if let Some(refs) = self.filter_references.and_then(|c| c.get(&element)) {
            style.filter_references = refs.clone();
        }
        style.implicit_anchor = self.implicit_anchors.and_then(|m| m.get(&element).copied());
        // CSS Anchor Positioning 1 §6.1: an absolutely positioned box's position options.
        let p = style.values.get_position();
        if !p.position_try_fallbacks.value.is_none()
            && style.values.get_box().position.is_absolutely_positioned()
        {
            let options = self
                .engine
                .position_try_styles(self.dom, self.data, element);
            if !options.is_empty() {
                style.position_try = Some(options.into());
            }
        }
        // CSS Sizing 4 5.2.1: `contain-intrinsic-size: auto` and the element's last remembered size.
        if style.intrinsic_size_auto() != (false, false) {
            self.auto_intrinsic.set(true);
            style.remembered = self.remembered.and_then(|m| m.get(&element).copied());
        }
        if crate::adapter::shows_scrollbars(&style.values) {
            style.custom_scrollbar = match self.custom_scrollbars {
                Some(cache) => cache.get(self.engine, self.dom, self.data, &self.env, element),
                None => crate::adapter::custom_scrollbar(
                    self.engine,
                    self.dom,
                    self.data,
                    &self.env,
                    element,
                ),
            };
        }
        Some(style)
    }

    fn pseudo_style(&self, element: NodeId, pseudo: PseudoElement) -> Option<RenderStyle> {
        let values = self.raw_pseudo(element, pseudo)?;
        let content = if needs_walk(&values) {
            self.generated()
                .get(&(element, pseudo_key(pseudo)))
                .cloned()
        } else {
            None
        };
        let animations =
            stylo_pseudo(pseudo).map_or(0, |p| self.engine.pseudo_animation_flags(element, &p));
        Some(RenderStyle {
            values,
            env: self.env.clone(),
            content,
            clip_reference: None,
            filter_references: None,
            remembered: None,
            view: snowlark_layout::style::Flip::IDENTITY,
            animations,
            custom_scrollbar: None,
            anchored: None,
            implicit_anchor: None,
            position_try: None,
        })
    }

    fn first_letter_style(&self, element: NodeId, parent: &RenderStyle) -> Option<RenderStyle> {
        let mut own = self.pseudo_style(element, PseudoElement::FirstLetter)?;
        let primary = self.data.styles(element)?;
        if !std::ptr::eq(primary, &*parent.values) {
            own.values = self.engine.reparented_pseudo_style(
                self.dom,
                self.data,
                element,
                &StyloPseudo::FirstLetter,
                &own.values,
                &parent.values,
            );
        }
        Some(own)
    }

    fn on_first_line(
        &self,
        element: Option<NodeId>,
        style: &RenderStyle,
        parent: &RenderStyle,
    ) -> RenderStyle {
        let mut s = style.clone();
        s.values = self.engine.first_line_style(
            self.dom,
            self.data,
            element,
            &style.values,
            &parent.values,
        );
        s
    }

    fn anonymous_style(&self, parent: &RenderStyle, kind: AnonymousBox) -> RenderStyle {
        let (key, display) = match kind {
            AnonymousBox::Block => (0u8, StyloDisplay::Block),
            AnonymousBox::Table { inline: false } => (1, StyloDisplay::Table),
            AnonymousBox::Table { inline: true } => (2, StyloDisplay::InlineTable),
            AnonymousBox::TableRow => (3, StyloDisplay::TableRow),
            AnonymousBox::TableCell => (4, StyloDisplay::TableCell),
        };
        let mut cache = self.anonymous.borrow_mut();
        let address = &*parent.values as *const ComputedValues as usize;
        if let Some((_, s)) = cache.get(&(address, key)) {
            return s.clone();
        }
        // Inherited properties from the parent, everything else initial, `display` as the box needs.
        let stylist = self.engine.stylist();
        let mut builder = StyleBuilder::for_inheritance(
            stylist.device(),
            Some(stylist),
            Some(&parent.values),
            None,
        );
        builder.set_display(display);
        let style = self.handle(builder.build());
        cache.insert((address, key), (parent.values.clone(), style.clone()));
        style
    }

    fn may_style_markers(&self) -> bool {
        *self
            .may_style_markers
            .get_or_init(|| self.engine.may_style_markers())
    }

    fn replaced_content(&self, element: NodeId) -> Option<ReplacedContent> {
        self.replaced(element)
    }

    fn alt_text(&self, element: NodeId) -> Option<String> {
        self.img_alt_text(element)
    }

    fn top_layer(&self) -> &[NodeId] {
        self.top_layer
    }

    fn fullscreen_element(&self) -> Option<NodeId> {
        self.fullscreen
    }

    fn details_parts(&self, element: NodeId) -> Option<snowlark_layout::style::DetailsParts> {
        if !self
            .dom
            .is_html_element_named(element, &local_name!("details"))
        {
            return None;
        }
        let dom = self.dom;
        let summary = dom
            .element_children(element)
            .find(|&c| dom.is_html_element_named(c, &local_name!("summary")));
        Some(snowlark_layout::style::DetailsParts {
            summary,
            content: dom
                .children(element)
                .filter(|&c| Some(c) != summary)
                .collect(),
            open: dom.get_attribute(element, "open").is_some(),
        })
    }

    fn has_form_parts(&self, element: NodeId) -> bool {
        use style::values::specified::box_::Appearance;
        let dom = self.dom;
        (dom.is_html_element_named(element, &local_name!("progress"))
            || dom.is_html_element_named(element, &local_name!("meter")))
            && self
                .data
                .styles(element)
                .is_some_and(|s| s.get_box().appearance == Appearance::None)
    }

    /// HTML 15.5.14-15: the internal tree of a `progress` or `meter` element, an inner part holding
    /// the bar, holding the value part. The element sets the value part's inline size, as a style
    /// attribute would: a progress bar's position (zero while indeterminate), a meter's value
    /// between its minimum and maximum; a meter's value part is the pseudo-element of the gauge
    /// region the value is in.
    fn form_parts(&self, element: NodeId) -> Option<Vec<(FormPart, RenderStyle)>> {
        use snowlark_layout::style::{ControlKind, MeterLevel};
        if !self.has_form_parts(element) {
            return None;
        }
        let (parts, fraction) = if self
            .dom
            .is_html_element_named(element, &local_name!("progress"))
        {
            let position = crate::controls::progress_position(self.dom, element);
            (
                [
                    FormPart::ProgressInner,
                    FormPart::ProgressBar,
                    FormPart::ProgressValue,
                ],
                position.unwrap_or(0.0),
            )
        } else {
            let ControlKind::Meter { fraction, level } =
                crate::controls::meter_kind(self.dom, element)
            else {
                return None;
            };
            let value = match level {
                MeterLevel::Optimum => FormPart::MeterOptimumValue,
                MeterLevel::Suboptimum => FormPart::MeterSuboptimumValue,
                MeterLevel::EvenLessGood => FormPart::MeterEvenLessGoodValue,
            };
            ([FormPart::MeterInner, FormPart::MeterBar, value], fraction)
        };
        let inline = format!(
            "inline-size: {}%; block-size: 100%",
            f64::from(fraction) * 100.0
        );
        let mut parent = self.data.styles_arc(element)?;
        let mut out = Vec::with_capacity(parts.len());
        for (i, part) in parts.into_iter().enumerate() {
            let values = self.engine.part_style(
                self.dom,
                self.data,
                element,
                &stylo_part(part),
                &parent,
                (i == parts.len() - 1).then_some(inline.as_str()),
            )?;
            parent = values.clone();
            out.push((part, self.handle(values)));
        }
        Some(out)
    }

    fn flat_children(&self, parent: NodeId) -> Option<Vec<NodeId>> {
        if let Some(shadow) = self.dom.shadow_root(parent) {
            return Some(self.dom.children(shadow).collect());
        }
        self.data.slotted_nodes(parent).map(<[NodeId]>::to_vec)
    }
}
