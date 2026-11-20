// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`Renderer`]: one document from bytes to pixels. It shares the DOM with whoever changes it (the page
//! crate's engine, or nobody for a document it parsed itself) and borrows it for each call; it owns the
//! style engine and its side table, the box tree, and the document's subresource loads. The page crate
//! drives it with [`Renderer::pump`] from its event loop, headless callers with
//! [`Renderer::render_until_settled`].

use std::cell::{Ref, RefCell, RefMut};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use servo_arc::Arc as ServoArc;
use snowlark_dom::{local_name, ns, Dom, NodeData, NodeId, ParseOptions, Reaction};
use snowlark_layout::{
    style::NaturalSize, BoxDamage, BoxGeometry, ElementChange, Fragment, LayoutStats, LayoutTree,
    PaintOptions, TreeChanges, UpdateStats,
};
use snowlark_net::{Client, NetClient, Observer, Url};
use snowlark_paint::{
    Color, DisplayList, DisplayListBuilder, HeadlessRenderer, ImageStore, Pixmap, Point, Size,
    Sources, TextEngine, Viewport,
};
use snowlark_style::{
    DeviceConfig, FontFace, FontFaceSource, ImportLoader, ImportRequest, ImportResponse,
    RestyleStats, StyleChange, StyleData, StyleEngine,
};
use style::properties::ComputedValues;
use style::values::computed as c;

use crate::adapter::RenderStyle;
use crate::env::{FontAliases, ImageTable, LoadedImage, StyleEnv, WebFace};
use crate::fetch::{decode_stylesheet, Arrival, Fetcher, Purpose};
use crate::metrics::TextMetrics;
use crate::source::{stylo_pseudo, DocumentStyles, ImageState};
use crate::srcset;
use crate::svg::{SvgImage, SvgImages, SvgStats};
use crate::used::NodeUsedValues;

/// Cumulative time per phase, in milliseconds.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timings {
    pub parse_ms: f64,
    /// Restyles, including resource discovery after each.
    pub style_ms: f64,
    /// Box tree builds.
    pub build_ms: f64,
    pub layout_ms: f64,
    /// Display list building.
    pub paint_ms: f64,
    /// Rasterization and PNG encoding.
    pub raster_ms: f64,
    /// Applying arrivals (stylesheet parsing, image decoding, font registration).
    pub resources_ms: f64,
    pub restyles: u32,
    pub layouts: u32,
}

/// What the style engine, the box tree and layout did, summed over the renderer's lifetime (for the
/// CLI's statistics and regression checks).
#[derive(Clone, Copy, Debug, Default)]
pub struct RenderStats {
    /// Elements whose styles restyles recomputed.
    pub restyled: u64,
    /// Elements the restyle traversals visited, of them the ones that ran selector matching, and
    /// the styles they took from the style sharing cache.
    pub traversed: u64,
    pub matched: u64,
    pub shared: u64,
    /// The restyle records by what their change required (the strongest of an element's and its
    /// pseudo-elements' changes).
    pub unchanged: u64,
    pub repaint: u64,
    pub relayout: u64,
    pub reconstruct: u64,
    /// Box tree updates: full builds and incremental updates.
    pub full_builds: u64,
    pub incremental_updates: u64,
    /// What the incremental updates did (boxes built, restyled in place...).
    pub update: UpdateStats,
    /// Boxes laid out again because a web font they name arrived.
    pub font_boxes: u64,
    /// Inline svgs written out to make their image.
    pub svgs_serialized: u64,
    /// Inline svgs looked at for a change without writing them out (drawn as paint operations:
    /// [`crate::svg_paint::fingerprint`]).
    pub svgs_fingerprinted: u64,
    /// Inline svgs recorded as display list paint operations ([`crate::svg_paint`]), and those
    /// that could not be (drawn as images).
    pub svgs_recorded: u64,
    pub svgs_not_recorded: u64,
    /// Boxes laid out, and boxes whose cached fragment was reused.
    pub laid_out: u64,
    pub reused: u64,
    /// Of `laid_out`, boxes laid out again in the same pass.
    pub relaid: u64,
    /// Text runs shaped, and taken from the shaping cache.
    pub shaped: u64,
    pub shape_cache_hits: u64,
    /// Words of the runs shaped that DirectWrite shaped, and that came from the word cache.
    pub words_shaped: u64,
    pub words_cached: u64,
    /// Flex and grid containers whose fragment was built, and their algorithm runs (measurements
    /// and layouts; `snowlark_layout::LayoutStats`).
    pub containers_built: u64,
    pub algorithm_runs: u64,
    /// Stylesheet changes the style engine took (each restyles the document and its shadow trees:
    /// [`snowlark_style::StyleEngine::sheets_generation`]).
    pub sheet_changes: u64,
    /// Walks of the fragment tree that read the size query containers' sizes (one per layout that
    /// changed the tree, on a page with a container).
    pub query_size_reads: u64,
}

/// An inline svg's paint operations ([`crate::svg_paint`]) for its markup's hash (an image id
/// can be reused once a changed svg's old image is removed) and the content box size.
struct SvgRecording {
    hash: u64,
    size: (f32, f32),
    recording: SvgRecordingResult,
}

type SvgRecordingResult = Result<Arc<DisplayList>, crate::svg_paint::Unsupported>;

/// An SVG root's natural width, height and ratio ([`crate::source::svg_root_natural`]).
type SvgNatural = (Option<f32>, Option<f32>, Option<f32>);

/// Whether [`Renderer::dom_changed`] takes a reaction. The DOM holds every document of the event
/// loop: only `document`'s changes count.
///
/// A change inside a tree outside the document (a subtree built before it is inserted, a clone) has
/// nothing to restyle, discover or lay out: its insertion, when it comes, is a connected one that
/// prepares and discovers the whole subtree. Removals are kept (the removed node is out of the
/// document now, but its styles and sheets go), and so are adoptions, and attribute changes while an
/// `object` or `embed` has a decided type (`embeds`).
///
/// (inlined into the filter: it runs for every DOM change, most often one at a time)
#[inline(always)]
fn renders_reaction(dom: &Dom, document: NodeId, embeds: bool, r: &Reaction) -> bool {
    let n = match r {
        Reaction::AttributeChanged { element, .. } => *element,
        Reaction::Inserted { node, .. }
        | Reaction::Removed { node, .. }
        | Reaction::Adopted { node, .. }
        | Reaction::Cloned { copy: node, .. }
        | Reaction::DataReplaced { node, .. }
        | Reaction::TextSplit { node, .. }
        | Reaction::TextMerged { into: node, .. } => *node,
        Reaction::ChildrenChanged { parent } => *parent,
    };
    if !dom.is_alive(n) || dom.node_document(n) != document {
        return false;
    }
    match r {
        Reaction::Removed { .. } | Reaction::Adopted { .. } => true,
        Reaction::AttributeChanged { .. } if embeds => true,
        _ => dom.is_connected(n),
    }
}

/// A content box size as a map key (its bits: sizes compare equal only when they are).
fn size_key(size: (f32, f32)) -> (u32, u32) {
    (size.0.to_bits(), size.1.to_bits())
}

/// What [`Renderer::pump`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PumpResult {
    /// Fetches that completed and were applied.
    pub applied: usize,
    /// Fetches still in flight.
    pub pending: usize,
}

/// How [`Renderer::render_until_settled`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settled {
    /// No fetch was in flight at the end.
    pub complete: bool,
    /// Fetches still in flight when the timeout hit.
    pub pending: usize,
    /// Fetches started for this document so far.
    pub requests: usize,
}

/// How the document timeline reads the time at each rendering update ([`Renderer::update_rendering`]).
/// Times are since the document's time origin (when [`Renderer::load_html`] or [`Renderer::attach`]
/// took the document). Whatever the clock says, the timeline never moves back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimelineClock {
    /// The monotonic clock (a browser).
    Monotonic,
    /// The monotonic clock, held once it reaches the given time (`--render --animation-time`).
    MonotonicUntil(Duration),
    /// A manual clock: the timeline reads this time (tests, deterministic captures).
    Manual(Duration),
}

/// HTML's render-blocking timeout ("an implementation-defined timeout value") when none is set.
pub const DEFAULT_RENDER_BLOCKING_TIMEOUT: Duration = Duration::from_secs(5);

/// A subresource request count by kind, for the CLI's summary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceCounts {
    pub sheets: usize,
    pub imports: usize,
    pub images: usize,
    pub fonts: usize,
    pub failed: usize,
}

#[derive(Clone, Debug)]
enum ImportEntry {
    Pending,
    Loaded { text: String, url: Url },
    Failed,
}

/// The `@import` loader: answers from what has arrived and records what is missing, so parsing never
/// blocks on the network. When the missing sheets arrive, the sheets that import them are parsed
/// again.
#[derive(Default)]
struct ImportCache {
    entries: Mutex<HashMap<String, ImportEntry>>,
    missing: Mutex<Vec<Url>>,
}

impl ImportLoader for ImportCache {
    fn load(&self, request: &ImportRequest<'_>) -> ImportResponse {
        let mut entries = self.entries.lock().unwrap();
        match entries.get(request.url.as_str()) {
            Some(ImportEntry::Loaded { text, url }) => ImportResponse::Css {
                text: text.clone(),
                url: url.clone(),
            },
            Some(_) => ImportResponse::Failed,
            None => {
                entries.insert(request.url.as_str().to_owned(), ImportEntry::Pending);
                self.missing.lock().unwrap().push(request.url.clone());
                ImportResponse::Failed
            }
        }
    }
}

/// CSS Cascade 5 §2.1 "fetch an `@import`": the response's Content-Type must be `text/css` (none, and
/// the unknown type, are let through, as for a `link`'s sheet), except in quirks mode for a
/// response of the document's origin (HTML's "quirks mode stylesheet" rule).
fn import_is_css(doc: &Doc, head: &snowlark_net::ResponseHead, url: &Url) -> bool {
    let essence = snowlark_net::mime::extract_mime_type(&head.headers).map(|m| m.essence());
    if essence
        .as_deref()
        .is_none_or(|e| matches!(e, "text/css" | "application/x-unknown-content-type"))
    {
        return true;
    }
    let dom = doc.dom.borrow();
    let response_url = head.url().unwrap_or(url);
    dom.quirks_mode(doc.document) == snowlark_dom::QuirksMode::Quirks
        && response_url.origin() == doc.url.origin()
}

/// The state of `sheet`'s `@import` tree: `None` while an import is still being fetched (or has
/// arrived but its importer was not parsed again yet), else whether every import loaded.
fn import_tree_state(
    entries: &HashMap<String, ImportEntry>,
    sheet: &snowlark_style::Sheet,
) -> Option<bool> {
    let mut ok = true;
    for (url, child) in sheet.imports() {
        let Some(url) = url else {
            continue;
        };
        match entries.get(url.as_str()) {
            // (not requested: deeper than the import limit)
            None => {}
            Some(ImportEntry::Pending) => return None,
            Some(ImportEntry::Failed) => ok = false,
            Some(ImportEntry::Loaded { .. }) => ok &= import_tree_state(entries, &child?)?,
        }
    }
    Some(ok)
}

/// A `<link rel=stylesheet>`'s sheet as fetched.
struct LinkSheet {
    text: Option<(String, Url)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum UrlImage {
    Fetching,
    /// Decoded (a bitmap at its pixel size), or an SVG image's placeholder.
    Loaded(LoadedImage),
    Failed,
}

/// An `@font-face` rule's load.
struct FaceLoad {
    face: FontFace,
    /// The `src` entry being tried; past the end when all failed.
    source: usize,
    loaded: bool,
    /// Whether a rule that applies still declares this face (faces stay in the table so that
    /// fetches in flight keep their index).
    active: bool,
    /// What the face registered with the text engine, once loaded.
    web: Option<WebFace>,
    /// A `FontFace` a script made (not declared by a style sheet).
    script: bool,
    /// Whether loading has begun (a style sheet's faces begin at once; a script's when asked).
    started: bool,
    /// Whether it is in `document.fonts` (a script's face that is not takes no part in matching).
    in_use: bool,
}

/// How far a face's loading has come (CSS Font Loading 3 `FontFaceLoadStatus`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaceState {
    Unloaded,
    Loading,
    Loaded,
    Error,
}

/// A face the document has: its descriptors as CSS text and its state.
#[derive(Clone, Debug)]
pub struct FaceInfo {
    pub id: usize,
    pub family: String,
    pub style: String,
    pub weight: String,
    pub stretch: String,
    pub unicode_range: String,
    pub state: FaceState,
    pub script: bool,
}

/// The state of the loaded document.
struct Doc {
    /// Elements' last remembered sizes (CSS Sizing 4 5.2.1: `contain-intrinsic-size: auto`).
    remembered: HashMap<NodeId, (Option<f32>, Option<f32>)>,
    /// An element with `contain-intrinsic-size: auto` has been styled: the sizes are worth recording.
    auto_intrinsic_seen: bool,
    /// The DOM, shared with the page (borrowed for each call, never across one).
    dom: Rc<RefCell<Dom>>,
    document: NodeId,
    url: Url,
    engine: StyleEngine,
    data: StyleData,
    layout: LayoutTree<RenderStyle>,
    /// The shaping cache of the document's canvas text: its origin's on this thread, apart from
    /// layout's (a long document fills that, and canvas labels would push its runs and words out).
    canvas_text: Arc<std::sync::Mutex<snowlark_paint::TextCache>>,
    env: Arc<StyleEnv>,
    fetcher: Fetcher,
    imports: Arc<ImportCache>,
    links: HashMap<NodeId, LinkSheet>,
    /// `<img>` elements: the chosen URL and its density, and their state.
    img_src: HashMap<NodeId, Option<(String, f32)>>,
    /// The `img` elements whose current request does not delay the load event (HTML: a request
    /// made while the element would lazy load).
    img_no_load_delay: HashSet<NodeId>,
    img_state: HashMap<NodeId, ImageState>,
    /// The Content-Type essences of the `object` and `embed` resources that arrived (HTML §4.8.7).
    embed_types: HashMap<NodeId, String>,
    /// The documents the `object` and `embed` elements' navigables show, and the natural
    /// dimensions each took from an SVG document's root when its box was last built.
    embedded_documents: HashMap<NodeId, NodeId>,
    embedded_natural: HashMap<NodeId, Option<SvgNatural>>,
    /// The `object` and `embed` elements fetched as images whose response turned out a document,
    /// with its Content-Type essence.
    embed_redecided: Vec<(NodeId, String)>,
    /// The `object` and `embed` elements whose image finished loading, and whether it did: the page
    /// fires their events.
    embed_settled: Vec<(NodeId, bool)>,
    /// The `link` and `style` elements whose sheet was made and whose `load` or `error` waits for
    /// its critical subresources (its `@import`s, at any depth).
    sheets_awaiting: Vec<NodeId>,
    /// The IDs `offset-path: url()` named: the shapes [`Doc::read_motion_paths`] reads.
    motion_refs: HashSet<String>,
    /// The `img` elements whose image the page's image requests decide ([`Renderer::set_img_request`]):
    /// the walk does not select theirs.
    page_imgs: HashSet<NodeId>,
    /// The `loading=lazy` videos whose poster the page let load ([`Renderer::release_poster`]).
    posters_released: HashSet<NodeId>,
    /// Every image URL requested, by absolute URL.
    images_by_url: HashMap<String, UrlImage>,
    /// CSS image URLs whose arrival changes boxes (list-style-image, `content: url()`), not only
    /// painting.
    box_images: HashSet<String>,
    faces: Vec<FaceLoad>,
    /// `local()` faces found while discovering, added to the font table after.
    /// The active faces in cascade order (later wins among faces of equal descriptors).
    face_order: Vec<usize>,
    /// Outermost inline `svg` elements: the hash of their serialized markup and the SVG image made
    /// from it (see [`crate::inline_svg`]).
    inline_svgs: HashMap<NodeId, (u64, Option<snowlark_paint::ImageId>)>,
    /// Outermost inline `svg` elements drawn as paint operations with no markup written: their
    /// fingerprints ([`crate::svg_paint::fingerprint`]), whose keys are their `inline_svgs` hashes.
    svg_prints: HashMap<NodeId, crate::svg_paint::Fingerprint>,
    /// Outermost inline `svg` elements whose paint operations could not be recorded: written out
    /// as markup (the image path) until a recording of them succeeds.
    svg_markup: rustc_hash::FxHashSet<NodeId>,
    /// The elements with a `clip-path: url()` and what they name ([`crate::clip_ref`]).
    clip_references: HashMap<NodeId, ClipUse>,
    /// The elements with a `url()` in `filter` or `backdrop-filter` and what they name
    /// ([`crate::filter_ref`]); `None` until it is read.
    filter_references: HashMap<NodeId, Option<Arc<FilterReferences>>>,
    /// The images `feImage`s of those filters refer to by URL: an arrival reads the filters again.
    filter_image_urls: HashSet<String>,
    /// Whether the `filter: url()` references are to be read again (an `feImage`'s image came).
    filters_stale: bool,
    /// The elements `feImage`s draw, as SVG images, by a hash of their markup.
    fe_images: HashMap<u64, snowlark_paint::ImageId>,
    /// The paint operations of inline svgs ([`crate::svg_paint`]) by their image and the content box
    /// size they were recorded for; `None` when the svg is drawn as its image (and why).
    svg_recordings: HashMap<snowlark_paint::ImageId, SvgRecording>,
    /// Inline svgs with an image whose content changed during a script's read: made again at the
    /// next [`Renderer::discover`] that is not for one (see [`Renderer::layout_for_script`]).
    svgs_waiting: rustc_hash::FxHashSet<NodeId>,
    /// The URLs inline `svg` elements' `<image>` elements show (fetched once each), and the bytes
    /// of those that arrived (see [`crate::inline_svg`]).
    svg_image_urls: HashSet<String>,
    svg_image_bytes: HashMap<String, Arc<Vec<u8>>>,
    /// The documents `use` elements refer to, by URL without the fragment: each is fetched once.
    svg_resources: HashMap<String, crate::svg_resource::SvgResource>,
    /// The resource each `use` element last reported on, and the reports for the page: (element,
    /// whether the resource loaded).
    use_reported: HashMap<NodeId, String>,
    use_events: Vec<(NodeId, bool)>,
    /// What the last walk of every element for images saw: the viewport, the device pixel ratio
    /// and the base URL (while these stay, only the elements that changed since are looked at
    /// again: `discover_nodes`, `discover_roots` and the restyled ones).
    images_walked: Option<((f32, f32), f32, Url)>,
    /// Elements whose attributes or children changed since the last discovery.
    discover_nodes: rustc_hash::FxHashSet<NodeId>,
    /// Inserted subtrees since the last discovery.
    discover_roots: Vec<NodeId>,
    /// The restyle records the last discovery looked at (the log's length then).
    restyles_seen: usize,
    /// The sheets generation whose `@font-face` rules the last discovery read.
    faces_generation: Option<u64>,
    /// The faces changed during the last restyle: it runs once more.
    faces_restyle: bool,
    /// The document's base URL, until a node is inserted or removed or a `base` changes.
    base: RefCell<Option<Url>>,
    /// A `link` or `style` may have been inserted or changed since `discover_links` last looked.
    links_dirty: bool,
    needs_restyle: bool,
    needs_build: bool,
    /// DOM changes and box regenerations (an image arrived) for the next box tree update; restyle
    /// records are added from the style engine's log then.
    changes: TreeChanges,
    /// The nodes in `changes.text`, which holds each once.
    text_changed: rustc_hash::FxHashSet<NodeId>,
    /// `dom_changed`'s sets of inserted nodes and structurally changed parents, kept between
    /// calls (empty) so most calls, which report one or two nodes, allocate nothing.
    batch_sets: (rustc_hash::FxHashSet<NodeId>, rustc_hash::FxHashSet<NodeId>),
    /// Web font families that arrived since the last box tree update: boxes using them take the new
    /// font table and are laid out again.
    fonts_arrived: Vec<String>,
    /// Generated content with counters, quotes or `attr()` has been seen: changes elsewhere can
    /// change it, so structural and counter changes rebuild the whole tree.
    generated_seen: bool,
    /// A size query container (CSS Contain 3 §2) has had boxes: layouts record container sizes.
    query_containers_seen: bool,
    /// The fragment tree the query container sizes were last read from: a layout that changed
    /// nothing returns the same tree, whose sizes are already recorded. (Weak, so the allocation,
    /// and with it the address, stays taken while this is kept.)
    query_sizes_from: Option<std::sync::Weak<Fragment<RenderStyle>>>,
    fragments: Option<Arc<Fragment<RenderStyle>>>,
    /// The initial containing block of the last layout: the viewport without the classic vertical
    /// scroll bar when one shows.
    last_viewport: Option<Size>,
    /// The last layout showed the viewport's vertical scroll bar (the next one starts from that).
    vertical_scrollbar: bool,
    /// The width of the gutter the last layout left for it (`scrollbar-width` on the root decides it).
    vertical_scrollbar_width: f32,
    scroll: HashMap<NodeId, Point>,
    requests: usize,
    counts: ResourceCounts,
    /// When the document was taken: the zero of the render-blocking timeout.
    origin: Instant,
    /// The zero of its timeline: `origin`, or the page's time origin
    /// ([`Renderer::set_time_origin`]).
    timeline_origin: Instant,
    /// HTML "render-blocking element set": the `<link rel=stylesheet>` elements whose sheets
    /// the first rendering waits for.
    render_blocking: HashSet<NodeId>,
    /// A sheet of the parsed `<head>` (a `<style>` or a render-blocking `<link>`) exists, so `@import`s
    /// in flight (their critical subresources) block rendering too.
    import_blockers: Vec<NodeId>,
    /// The document has had its first rendering update; it is never render-blocked again.
    rendered: bool,
    /// The images showing `<canvas>` bitmaps.
    canvases: HashMap<NodeId, snowlark_paint::ImageId>,
    /// Form controls' state from the page.
    control_states: HashMap<NodeId, crate::controls::ControlState>,
    /// The top layer (CSS Position 4 §4.1) the page keeps for this document, bottom first.
    top_layer: Vec<NodeId>,
    /// The document's fullscreen element (Fullscreen API), from the page.
    fullscreen: Option<NodeId>,
    /// Media elements' state from the page, and their frames copied to memory (headless renders).
    media: HashMap<NodeId, crate::source::MediaState>,
    video_frames: HashMap<NodeId, snowlark_paint::ImageId>,
    /// The page's selection and editing caret, to paint.
    selection: Option<snowlark_layout::TextSelection>,
    /// Find in page: the matches to highlight (a browser feature, set by the embedder; none in
    /// headless modes).
    find: Option<snowlark_layout::find::FindHighlights>,
    /// The page's Content Security Policy check of `<style>` elements (see [`StyleBlockCheck`]).
    style_check: Option<StyleBlockCheck>,
    /// Image URLs whose request completed since the page last asked (the page's `<img>` image
    /// requests follow them).
    image_url_events: Vec<(String, ImageStatus)>,
    /// The animated images of the document (`<img>`, CSS images), by their image in the store.
    animations: HashMap<snowlark_paint::ImageId, ImageAnimation>,
}

/// An animated image a document shows (HTML §4.8.4.3, "an animated image"): its frames follow the
/// document timeline, the clock CSS animations use, from the time it was first painted. Every user
/// of one image (all `<img>` elements and CSS images of one URL) shows the same frame, as in
/// browsers.
struct ImageAnimation {
    animation: Arc<snowlark_paint::image::Animation>,
    /// The timeline time (s) the animation started: its first paint (with a held or manual clock,
    /// the time origin, so that a capture at a time does not depend on how long loading took).
    start: Option<f64>,
    /// The frame the store shows.
    frame: usize,
    /// Where the last paint drew it, in document coordinates (empty: not painted, or clipped away).
    painted: Vec<snowlark_paint::Rect>,
    /// The last paint drew it in a fixed or sticky layer (on the screen wherever it scrolls).
    layered: bool,
    /// The decoder of an animation whose frames are decoded as they show
    /// ([`snowlark_paint::image::Animation::encoded`]), made at its first frame change.
    decoder: Option<snowlark_paint::image::FrameDecoder>,
    /// Its frames stopped decoding: it stays on the frame it shows.
    failed: bool,
}

impl ImageAnimation {
    /// Whether it shows in `view` (document coordinates; `None`: everything counts as shown, an
    /// empty view nothing).
    fn shown(&self, view: Option<snowlark_paint::Rect>) -> bool {
        match view {
            None => true,
            Some(v) if v.is_empty() => false,
            Some(v) => self.layered || self.painted.iter().any(|r| r.intersects(&v)),
        }
    }
}

/// Whether a cross-origin image response shares itself with a document at `doc` through CORS
/// (`Access-Control-Allow-Origin` of `*` or the document's origin): it is CORS-same-origin for
/// `image-orientation` where it was asked for in CORS mode. (Images are fetched once per URL, so
/// the element's `crossorigin` attribute decides that when it is shown:
/// [`LoadedImage::oriented`].)
fn cors_shared(doc: &Url, headers: &snowlark_net::HeaderList) -> bool {
    headers
        .get("access-control-allow-origin")
        .is_some_and(|v| v.trim() == "*" || v.trim() == doc.origin().ascii_serialization())
}

/// Whether an image fetched from `url` is same-origin with a document at `doc` (a `data:` image is
/// the document's own).
fn same_origin(doc: &Url, url: &Url) -> bool {
    url.scheme() == "data" || url.origin() == doc.origin()
}

/// What an image URL's request reached: the image's natural size once fetched and decoded (in
/// image pixels; an SVG image may have none), or a failure.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ImageStatus {
    Fetching,
    Loaded {
        width: Option<f32>,
        height: Option<f32>,
        /// The natural aspect ratio (width / height), when there is one.
        ratio: Option<f32>,
    },
    Failed,
}

impl ImageStatus {
    fn of(state: UrlImage) -> ImageStatus {
        match state {
            UrlImage::Fetching => ImageStatus::Fetching,
            UrlImage::Failed => ImageStatus::Failed,
            UrlImage::Loaded(image) => ImageStatus::Loaded {
                width: image.natural.width,
                height: image.natural.height,
                ratio: image.natural.ratio(),
            },
        }
    }
}

/// Whether an `img` defers its image until the page resumes it (HTML §4.8.4.3.? "will lazy load
/// element steps", the page's): `loading=lazy` and a source the page has not requested.
fn lazy_img(dom: &Dom, n: NodeId) -> bool {
    dom.is_html_element_named(n, &local_name!("img"))
        && dom
            .get_attribute(n, "loading")
            .is_some_and(|l| l.eq_ignore_ascii_case("lazy"))
}

/// Whether a `video` defers its poster until the page lets it load ([`Renderer::release_poster`]):
/// `loading=lazy` (HTML lazy loading of media elements).
fn lazy_video(dom: &Dom, n: NodeId) -> bool {
    dom.is_html_element_named(n, &local_name!("video"))
        && dom
            .get_attribute(n, "loading")
            .is_some_and(|l| l.eq_ignore_ascii_case("lazy"))
}

/// What the page's request policy gives a document's fetches (sheets, `@import`s, images, fonts):
/// the document's request client (its origin, CSP list, referrer policy and ancestors, which
/// `snowlark-net` checks each request and redirect against) and an observer of the fetches' events on
/// the network thread (where the page takes the CSP violations to report).
#[derive(Clone)]
pub struct FetchPolicy {
    pub client: Arc<Client>,
    pub observer: Option<Observer>,
}

/// HTML §4.2.6 "update a `style` block" steps 6-7, the page's: whether a `<style>` element's sheet may
/// be created (CSP's inline check of its text, which reports a violation itself). Not set: always.
pub type StyleBlockCheck = std::rc::Rc<dyn Fn(&Dom, NodeId) -> bool>;

/// "Update a `style` block" with the page's CSP check: a blocked element has no sheet. Returns
/// whether an HTML `style` element got a sheet, whose `load` or `error` is then due (step 9).
fn update_style_block(
    engine: &mut StyleEngine,
    data: &StyleData,
    check: Option<&StyleBlockCheck>,
    dom: &Dom,
    el: NodeId,
) -> bool {
    if check.is_some_and(|c| dom.is_connected(el) && !c(dom, el)) {
        engine.remove_sheets_of(data, el);
        return false;
    }
    engine.update_style_block(dom, data, el).is_some()
        && dom.is_html_element_named(el, &local_name!("style"))
}

/// Renders one document: style, layout and paint with subresource loading. See the crate docs.
pub struct Renderer {
    net: NetClient,
    text: Arc<TextEngine>,
    images: Arc<ImageStore>,
    /// The bitmaps this renderer added to `images` (freed with the document or the renderer: the
    /// store may be shared with other renderers).
    owned_images: Vec<snowlark_paint::ImageId>,
    /// SVG images and their rasters (in `images`).
    svg: SvgImages,
    config: DeviceConfig,
    /// Counts the changes of what media queries are evaluated against (the device configuration,
    /// the document): see [`Renderer::media_generation`].
    media_generation: u64,
    /// The width (CSS px) of the classic vertical scroll bar the embedder shows at the viewport's
    /// edge when the document overflows it; 0 for overlay scroll bars, which take no room.
    scrollbar_width: f32,
    /// HTML "PDF viewer supported" (PDF `object`s and `embed`s show a nested document).
    pdf_viewer: bool,
    fonts: Arc<RwLock<FontAliases>>,
    image_table: Arc<ImageTable>,
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
    doc: Option<Box<Doc>>,
    timings: Timings,
    stats: RenderStats,
    /// Style and layout are being brought up to date for a script's read.
    script_read: bool,
    trace: bool,
    clock: TimelineClock,
    render_blocking_timeout: Duration,
    /// `<link rel=stylesheet>` sheets are fetched by the caller (the page's loader, which fires their
    /// `load` and `error` events) and handed over with [`Renderer::link_sheet_loaded`].
    external_link_sheets: bool,
    style_check: Option<StyleBlockCheck>,
    rendering_updates: u64,
    /// The part of the document the user sees (document coordinates) as of the last
    /// [`Renderer::set_image_animation_view`]: animated images outside it decode no frames.
    animation_view: Option<snowlark_paint::Rect>,
    /// The time (s) the next rendering update moves the timeline to, when the page chose it (so its
    /// animation frame callbacks, its script animations and this document's CSS animations agree).
    frame_time: Option<f64>,
}

impl Drop for Renderer {
    /// The images go with the renderer (its store may live on, shared with other renderers).
    fn drop(&mut self) {
        self.free_images();
    }
}

/// Clears the axes of a last remembered size (inline, block) in which `values` has no
/// `contain-intrinsic-size: auto` (CSS Sizing 4 5.2.1).
fn forget_lost_auto(size: &mut (Option<f32>, Option<f32>), values: &ComputedValues) {
    use style::values::generics::box_::GenericContainIntrinsicSize as I;
    let p = values.get_position();
    let auto = |v: &style::values::computed::ContainIntrinsicSize| {
        matches!(v, I::AutoLength(_) | I::AutoNone)
    };
    let (inline_auto, block_auto) = if values.writing_mode.is_horizontal() {
        (
            auto(&p.contain_intrinsic_width),
            auto(&p.contain_intrinsic_height),
        )
    } else {
        (
            auto(&p.contain_intrinsic_height),
            auto(&p.contain_intrinsic_width),
        )
    };
    if !inline_auto {
        size.0 = None;
    }
    if !block_auto {
        size.1 = None;
    }
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// HTML §2.3.2: a token list attribute contains `token`, ASCII case-insensitively.
fn has_token(value: &str, token: &str) -> bool {
    value
        .split_ascii_whitespace()
        .any(|t| t.eq_ignore_ascii_case(token))
}

/// Whether box generation or layout may read an attribute of `el` directly, rather than through
/// its computed style: replaced and embedded content, form controls, table spans, list numbering,
/// `details`, slots, SVG and MathML, and elements whose parents read their children's attributes
/// (`option`, `source`, `col`, `li`...). The attributes of the other HTML elements (sectioning,
/// grouping and text-level elements, and autonomous custom elements) reach their boxes only
/// through style: presentational hints, `lang`, `dir`, `hidden` and attribute selectors, which the
/// style engine invalidates and whose restyle reports the damage; or through `attr()` in generated
/// content, which the caller rules out.
fn boxes_read_attributes(dom: &snowlark_dom::Dom, el: NodeId) -> bool {
    if *dom.namespace(el) != ns!(html) {
        return true;
    }
    let local: &str = dom.local_name(el);
    let plain = matches!(
        local,
        "html"
            | "body"
            | "div"
            | "span"
            | "p"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "nav"
            | "main"
            | "aside"
            | "search"
            | "hgroup"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "address"
            | "blockquote"
            | "figure"
            | "figcaption"
            | "pre"
            | "dl"
            | "dt"
            | "dd"
            | "form"
            | "label"
            | "a"
            | "em"
            | "strong"
            | "b"
            | "i"
            | "u"
            | "s"
            | "small"
            | "big"
            | "code"
            | "kbd"
            | "samp"
            | "var"
            | "cite"
            | "abbr"
            | "dfn"
            | "time"
            | "data"
            | "mark"
            | "sub"
            | "sup"
            | "bdi"
            | "bdo"
            | "del"
            | "ins"
            | "center"
            | "font"
            | "nobr"
            | "tt"
            | "strike"
            | "br"
            | "wbr"
            | "hr"
    );
    // (a valid custom element name has a hyphen; no HTML element's name does)
    !(plain || local.contains('-'))
}

impl Doc {
    /// The shapes `offset-path: url()` names, as they are now: read for each paint, since a change
    /// of their attributes restyles no one who follows them.
    fn read_motion_paths(&self) {
        if self.motion_refs.is_empty() {
            return;
        }
        let dom = self.dom.borrow();
        let Ok(mut paths) = self.env.motion_paths.write() else {
            return;
        };
        paths.clear();
        for id in &self.motion_refs {
            if let Some(d) = crate::inline_svg::shape_path_by_id(&dom, self.document, id) {
                paths.insert(id.clone(), d);
            }
        }
    }
}

impl Renderer {
    /// A renderer fetching through `net`, shaping with `text`, styling for `config` (its
    /// `font_metrics` is set to the text engine's when empty).
    pub fn new(net: NetClient, text: Arc<TextEngine>, mut config: DeviceConfig) -> Renderer {
        let fonts = Arc::new(RwLock::new(FontAliases::default()));
        if config.font_metrics.is_none() {
            config.font_metrics = Some(Arc::new(TextMetrics::new(
                text.clone(),
                fonts.clone(),
                config.default_font_size,
                config.default_monospace_font_size,
            )));
        }
        Renderer {
            net,
            svg: SvgImages::with_text(text.clone()),
            text,
            images: Arc::new(ImageStore::new()),
            owned_images: Vec::new(),
            config,
            media_generation: 0,
            scrollbar_width: 0.0,
            pdf_viewer: false,
            fonts,
            image_table: Arc::new(ImageTable::default()),
            notify: None,
            doc: None,
            timings: Timings::default(),
            stats: RenderStats::default(),
            script_read: false,
            trace: false,
            clock: TimelineClock::Monotonic,
            render_blocking_timeout: DEFAULT_RENDER_BLOCKING_TIMEOUT,
            external_link_sheets: false,
            style_check: None,
            rendering_updates: 0,
            animation_view: None,
            frame_time: None,
        }
    }

    /// How the document timeline reads the time (default [`TimelineClock::Monotonic`]).
    pub fn set_timeline_clock(&mut self, clock: TimelineClock) {
        self.clock = clock;
    }

    /// HTML's render-blocking timeout: past this time since the document's time origin, pending
    /// render-blocking sheets no longer hold the first rendering (default
    /// [`DEFAULT_RENDER_BLOCKING_TIMEOUT`]).
    pub fn set_render_blocking_timeout(&mut self, timeout: Duration) {
        self.render_blocking_timeout = timeout;
    }

    /// Whether the caller fetches `<link rel=stylesheet>` sheets (see
    /// [`Renderer::link_sheet_loaded`]); the renderer still decides which of them block rendering.
    pub fn set_external_link_sheets(&mut self, external: bool) {
        self.external_link_sheets = external;
    }

    /// A `<link rel=stylesheet>`'s sheet the caller fetched arrived (`Some` with its decoded text and
    /// final URL) or failed (`None`): HTML "process the linked resource". It is applied (replacing an
    /// earlier sheet of the same link) and the link no longer blocks rendering. Returns whether the
    /// link's `load` or `error` is now awaited ([`Renderer::take_sheets_settled`] reports it).
    pub fn link_sheet_loaded(&mut self, owner: NodeId, sheet: Option<(String, Url)>) -> bool {
        let Some(doc) = self.doc.as_deref_mut() else {
            return false;
        };
        doc.render_blocking.remove(&owner);
        {
            let dom = doc.dom.borrow();
            if !dom.is_alive(owner) || dom.node_document(owner) != doc.document {
                return false;
            }
        }
        let awaited = sheet.is_some();
        match sheet {
            Some((text, url)) => {
                Self::apply_link_sheet(doc, owner, &text, &url);
                doc.links.insert(
                    owner,
                    LinkSheet {
                        text: Some((text, url)),
                    },
                );
                doc.sheets_awaiting.push(owner);
            }
            None => {
                doc.engine.remove_sheets_of(&doc.data, owner);
                doc.links.insert(owner, LinkSheet { text: None });
                doc.needs_restyle = true;
            }
        }
        let t = Instant::now();
        self.start_missing_imports();
        self.timings.resources_ms += ms(t);
        awaited
    }

    /// Forgets a `<link>`'s sheet (its `href` or `rel` changed: the caller fetches again, or it is no
    /// longer a stylesheet link).
    pub fn link_sheet_removed(&mut self, owner: NodeId) {
        if let Some(doc) = self.doc.as_deref_mut() {
            doc.engine.remove_sheets_of(&doc.data, owner);
            doc.links.remove(&owner);
            doc.render_blocking.remove(&owner);
            doc.needs_restyle = true;
        }
    }

    /// The request policy of the attached document's fetches from now on (call after
    /// [`Renderer::attach`]; a new document starts with a client made from its URL alone).
    pub fn set_fetch_policy(&mut self, policy: FetchPolicy) {
        if let Some(d) = self.doc.as_deref_mut() {
            d.fetcher.set_policy(policy.client, policy.observer);
        }
    }

    /// The image URLs whose request completed since the last call, with what it reached.
    /// The `use` elements whose external reference was processed since the last call: (element,
    /// whether the resource loaded).
    pub fn take_use_events(&mut self) -> Vec<(NodeId, bool)> {
        self.doc
            .as_deref_mut()
            .map(|d| std::mem::take(&mut d.use_events))
            .unwrap_or_default()
    }

    pub fn take_image_url_events(&mut self) -> Vec<(String, ImageStatus)> {
        self.doc
            .as_deref_mut()
            .map(|d| std::mem::take(&mut d.image_url_events))
            .unwrap_or_default()
    }

    /// Whether the page's current image request for an `img` delays the document's load event
    /// (HTML §4.8.4.3.4 "delay load event": not for one made while the element would lazy load).
    pub fn set_img_delays_load(&mut self, node: NodeId, delays: bool) {
        if let Some(doc) = self.doc.as_deref_mut() {
            if delays {
                doc.img_no_load_delay.remove(&node);
            } else {
                doc.img_no_load_delay.insert(node);
            }
        }
    }

    /// The page's current image request for an `img` changed (HTML §4.8.4.3: an `img` shows its
    /// current request's image): its URL and pixel density, or none. The element shows that from
    /// now on, whatever the walk would select.
    pub fn set_img_request(&mut self, node: NodeId, request: Option<(String, f32)>) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        let connected = {
            let dom = doc.dom.borrow();
            if !dom.is_alive(node) || dom.node_document(node) != doc.document {
                return;
            }
            dom.is_connected(node)
        };
        doc.page_imgs.insert(node);
        if doc.img_src.get(&node) == Some(&request) && doc.img_state.contains_key(&node) {
            return;
        }
        let state = match &request {
            None => ImageState::Failed,
            Some((url, density)) => match doc.images_by_url.get(url.as_str()) {
                Some(s) => Self::img_state_of(*s, *density),
                None => ImageState::Pending,
            },
        };
        doc.img_src.insert(node, request);
        doc.img_state.insert(node, state);
        if connected {
            doc.changes.restyled.push(ElementChange::reconstruct(node));
        }
    }

    /// Whether the page let `node`'s poster load ([`Renderer::release_poster`]).
    pub fn poster_released(&self, node: NodeId) -> bool {
        self.doc
            .as_deref()
            .is_some_and(|d| d.posters_released.contains(&node))
    }

    /// DOM's move (`moveBefore`) moved `node` (see [`snowlark_style::StyleEngine::node_moved`]).
    pub fn node_moved(&mut self, node: NodeId) {
        if let Some(doc) = self.doc.as_deref_mut() {
            doc.engine.node_moved(node);
        }
    }

    /// A `loading=lazy` video's lazy load resumed (the page's): its poster loads from now on.
    pub fn release_poster(&mut self, node: NodeId) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        if !doc.posters_released.insert(node) {
            return;
        }
        let Some(Some((url, density))) = doc.img_src.get(&node).cloned() else {
            return;
        };
        if !matches!(doc.img_state.get(&node), Some(ImageState::Pending)) {
            return;
        }
        Self::request_image(doc, &url);
        let state = Self::img_state_of(doc.images_by_url[url.as_str()], density);
        doc.img_state.insert(node, state);
        doc.changes.restyled.push(ElementChange::reconstruct(node));
    }

    /// HTML §4.8.4.3.6 "select an image source" for an `img` of the attached document: the
    /// absolute URL and its pixel density (`None` without a source); `None` without a document.
    /// `auto` is the size an `auto` source size stands for: the page's, when the `img` allows
    /// auto-sizes (its concrete object size width, or its last auto-sizes width), else `None`.
    pub fn select_image_source(
        &self,
        img: NodeId,
        auto: Option<f32>,
    ) -> Option<Option<(String, f32)>> {
        let doc = self.doc.as_deref()?;
        let dom = doc.dom.borrow();
        if !dom.is_alive(img) {
            return Some(None);
        }
        let base = Self::base_url(doc);
        let device = doc.engine.stylist().device();
        let quirks = doc.engine.stylist().quirks_mode();
        let url_data = style::stylesheets::UrlExtraData::from(base.clone());
        Some(select_img_source(
            &dom,
            img,
            &base,
            device,
            quirks,
            &url_data,
            self.config.device_pixel_ratio,
            auto,
        ))
    }

    /// Where the request of the image at `url` stands; with `start`, fetches it first if it was
    /// never requested (once per URL: the document's `<img>` elements and CSS images share it).
    /// A URL never requested is `Fetching` without `start`.
    /// What this document's list of available images holds for `url` (HTML §4.8.4.3.2), fetched
    /// for one of its elements or style properties: `None` when nothing asked for it yet.
    pub fn available_image(&self, url: &str) -> Option<ImageStatus> {
        let doc = self.doc.as_deref()?;
        doc.images_by_url.get(url).map(|s| ImageStatus::of(*s))
    }

    pub fn request_image_url(&mut self, url: &str, start: bool) -> ImageStatus {
        let Some(doc) = self.doc.as_deref_mut() else {
            return ImageStatus::Failed;
        };
        if start {
            Self::request_image(doc, url);
        }
        doc.images_by_url
            .get(url)
            .map_or(ImageStatus::Fetching, |s| ImageStatus::of(*s))
    }

    /// The page's CSP check of `style` attributes, for the attached document.
    pub fn set_style_attribute_check(
        &mut self,
        check: Option<snowlark_style::StyleAttributeCheck>,
    ) {
        if let Some(d) = self.doc.as_deref_mut() {
            d.engine.set_style_attribute_check(check);
            d.needs_restyle = true;
        }
    }

    /// The page's CSP check of `<style>` elements, for this document and the ones attached later.
    pub fn set_style_block_check(&mut self, check: Option<StyleBlockCheck>) {
        if let Some(d) = self.doc.as_deref_mut() {
            d.style_check = check.clone();
        }
        self.style_check = check;
    }

    /// Logs resource arrivals and phase decisions to stderr.
    pub fn set_trace(&mut self, trace: bool) {
        self.trace = trace;
    }

    /// Called (on the network thread) whenever a fetch makes progress: wake the event loop, which then
    /// calls [`Renderer::pump`].
    pub fn set_notify(&mut self, notify: Option<Arc<dyn Fn() + Send + Sync>>) {
        if let Some(d) = &mut self.doc {
            d.fetcher.set_notify(notify.clone());
        }
        self.notify = notify;
    }

    pub fn text(&self) -> &Arc<TextEngine> {
        &self.text
    }

    pub fn images(&self) -> &Arc<ImageStore> {
        &self.images
    }

    /// Uses `store` for this renderer's images (the documents of one page's frames share one, so
    /// their display lists nest). Call before loading a document.
    pub fn share_images(&mut self, store: Arc<ImageStore>) {
        self.free_images();
        self.images = store;
    }

    fn free_images(&mut self) {
        for id in self.owned_images.drain(..) {
            self.images.remove(id);
        }
        self.svg.clear(&self.images);
    }

    pub fn device_config(&self) -> &DeviceConfig {
        &self.config
    }

    pub fn timings(&self) -> Timings {
        self.timings
    }

    /// SVG image rasterizations and raster cache hits so far.
    pub fn svg_stats(&self) -> SvgStats {
        self.svg.stats()
    }

    /// The document's DOM (`None` before [`Renderer::load_html`] or [`Renderer::attach`]), borrowed.
    pub fn dom(&self) -> Option<Ref<'_, Dom>> {
        self.doc.as_ref().map(|d| d.dom.borrow())
    }

    /// The shared DOM itself.
    pub fn dom_rc(&self) -> Option<&Rc<RefCell<Dom>>> {
        self.doc.as_ref().map(|d| &d.dom)
    }

    /// The Document node.
    pub fn document(&self) -> Option<NodeId> {
        self.doc.as_ref().map(|d| d.document)
    }

    /// The style engine and its side table (for `getComputedStyle` and the CSSOM).
    pub fn style(&self) -> Option<(&StyleEngine, &StyleData)> {
        self.doc.as_ref().map(|d| (&d.engine, &d.data))
    }

    /// Subresource counts so far.
    pub fn resource_counts(&self) -> ResourceCounts {
        self.doc.as_ref().map(|d| d.counts).unwrap_or_default()
    }

    /// Parses `bytes` as an HTML document at `url` (HTML §13.2.3 encoding sniffing with the
    /// Content-Type `charset`), adds its `<style>` sheets and starts fetching its `<link>` sheets.
    /// Scripts don't run; `noscript` follows [`DeviceConfig::scripting`].
    pub fn load_html(&mut self, bytes: &[u8], url: &Url, transport_charset: Option<&str>) {
        let t = Instant::now();
        let mut input = snowlark_net::encoding::SniffInput::new(bytes);
        input.transport_charset = transport_charset;
        let (encoding, _) = snowlark_net::encoding::sniff_html_encoding(&input);
        let mut dom = Dom::new();
        let document = dom.parse_html_document(
            bytes,
            encoding,
            ParseOptions {
                scripting_enabled: self.config.scripting,
                ..ParseOptions::default()
            },
        );
        dom.set_document_url(document, url.as_str());
        if dom.encoding(document) == encoding_rs::UTF_8 && encoding != encoding_rs::UTF_8 {
            dom.set_encoding(document, encoding);
        }
        self.timings.parse_ms += ms(t);
        self.attach(Rc::new(RefCell::new(dom)), document);
    }

    /// Renders `document` of a DOM shared with the caller, at its document URL. The caller reports
    /// its changes to the DOM with [`Renderer::dom_changed`]; the renderer borrows the DOM in each of
    /// its calls (so none may be made while the caller holds a mutable borrow).
    pub fn attach(&mut self, dom_rc: Rc<RefCell<Dom>>, document: NodeId) {
        let t = Instant::now();
        let dom = dom_rc.borrow();
        let url = Url::parse(dom.document_url(document))
            .unwrap_or_else(|_| Url::parse("about:blank").unwrap());
        let mut engine = StyleEngine::new(&dom, document, self.config.clone());
        let imports = Arc::new(ImportCache::default());
        engine.set_import_loader(Some(imports.clone()));
        let data = StyleData::new();
        let mut fetcher = Fetcher::new(self.net.clone(), &url);
        fetcher.set_notify(self.notify.clone());
        *self.fonts.write().unwrap() = FontAliases::default();
        self.image_table = Arc::new(ImageTable::default());
        self.free_images();
        let env = Arc::new(StyleEnv {
            fonts: FontAliases::default(),
            images: self.image_table.clone(),
            motion_paths: Default::default(),
        });
        drop(dom);
        let mut doc = Box::new(Doc {
            dom: dom_rc,
            document,
            url,
            engine,
            data,
            layout: LayoutTree::new(),
            canvas_text: Default::default(),
            env,
            fetcher,
            imports,
            links: HashMap::new(),
            img_src: HashMap::new(),
            img_no_load_delay: HashSet::new(),
            img_state: HashMap::new(),
            page_imgs: HashSet::new(),
            posters_released: HashSet::new(),
            embed_types: HashMap::new(),
            embedded_documents: HashMap::new(),
            embedded_natural: HashMap::new(),
            embed_redecided: Vec::new(),
            embed_settled: Vec::new(),
            sheets_awaiting: Vec::new(),
            motion_refs: HashSet::new(),
            images_by_url: HashMap::new(),
            box_images: HashSet::new(),
            faces: Vec::new(),
            face_order: Vec::new(),
            inline_svgs: HashMap::new(),
            svg_prints: HashMap::new(),
            svg_markup: Default::default(),
            clip_references: HashMap::new(),
            filter_references: HashMap::new(),
            filter_image_urls: HashSet::new(),
            filters_stale: false,
            fe_images: HashMap::new(),
            svg_recordings: HashMap::new(),
            svgs_waiting: Default::default(),
            svg_image_urls: HashSet::new(),
            svg_image_bytes: HashMap::new(),
            svg_resources: HashMap::new(),
            use_reported: HashMap::new(),
            use_events: Vec::new(),
            images_walked: None,
            discover_nodes: Default::default(),
            discover_roots: Vec::new(),
            restyles_seen: 0,
            faces_generation: None,
            faces_restyle: false,
            base: RefCell::new(None),
            links_dirty: true,
            needs_restyle: true,
            needs_build: true,
            changes: TreeChanges::default(),
            text_changed: Default::default(),
            batch_sets: Default::default(),
            fonts_arrived: Vec::new(),
            generated_seen: false,
            query_containers_seen: false,
            query_sizes_from: None,
            auto_intrinsic_seen: false,
            remembered: HashMap::new(),
            fragments: None,
            last_viewport: None,
            vertical_scrollbar: false,
            vertical_scrollbar_width: 0.0,
            scroll: HashMap::new(),
            requests: 0,
            counts: ResourceCounts::default(),
            origin: Instant::now(),
            timeline_origin: Instant::now(),
            render_blocking: HashSet::new(),
            import_blockers: Vec::new(),
            rendered: false,
            canvases: HashMap::new(),
            media: HashMap::new(),
            video_frames: HashMap::new(),
            control_states: HashMap::new(),
            top_layer: Vec::new(),
            fullscreen: None,
            selection: None,
            find: None,
            style_check: self.style_check.clone(),
            image_url_events: Vec::new(),
            animations: HashMap::new(),
        });
        // Documents of one origin on this thread shape through one text cache (never across origins:
        // snowlark_paint::text_cache explains why).
        let origin = doc.url.origin().ascii_serialization();
        let cache = self.text.shared_text_cache(&origin);
        doc.layout.set_text_cache(cache);
        // (an opaque origin's canvas text gets a cache of its own; no origin contains a space)
        if !origin.is_empty() && origin != "null" {
            doc.canvas_text = self.text.shared_text_cache(&format!("{origin} canvas"));
        }
        {
            let dom_rc = doc.dom.clone();
            let dom = dom_rc.borrow();
            let styles: Vec<NodeId> = dom
                .shadow_including_descendants(doc.document)
                .filter(|&n| dom.is_style_element(n))
                .collect();
            for el in styles {
                if update_style_block(
                    &mut doc.engine,
                    &doc.data,
                    doc.style_check.as_ref(),
                    &dom,
                    el,
                ) {
                    doc.sheets_awaiting.push(el);
                }
            }
        }
        self.doc = Some(doc);
        self.media_generation += 1;
        self.discover_links();
        self.start_missing_imports();
        self.timings.style_ms += ms(t);
    }

    /// The document's base URL (HTML §2.4.1): the first `<base href>`, else the document's URL.
    fn base_url(doc: &Doc) -> Url {
        if let Some(u) = &*doc.base.borrow() {
            return u.clone();
        }
        let u = Self::find_base_url(doc);
        *doc.base.borrow_mut() = Some(u.clone());
        u
    }

    fn find_base_url(doc: &Doc) -> Url {
        let dom = doc.dom.borrow();
        if !dom.has_connected_base(doc.document) {
            return doc.url.clone();
        }
        if let Some(root) = dom.document_element(doc.document) {
            for n in dom.descendants(root) {
                if dom.is_html_element_named(n, &local_name!("base")) {
                    if let Some(href) = dom.get_attribute(n, "href") {
                        // ("set the frozen base URL": the first `base` with an `href`; one that
                        // does not parse, or is a `data:` or `javascript:` URL, is the fallback)
                        return doc
                            .url
                            .join(href.trim())
                            .ok()
                            .filter(|u| !matches!(u.scheme(), "data" | "javascript"))
                            .unwrap_or_else(|| doc.url.clone());
                    }
                }
            }
        }
        doc.url.clone()
    }

    /// HTML §4.2.4 `<link rel=stylesheet>`: fetch each one not yet requested (alternate sheets and
    /// disabled links are not applied, a non-CSS `type` is not fetched).
    fn discover_links(&mut self) {
        let external = self.external_link_sheets;
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        // (nothing inserted or changed that could be a new link since the last look)
        if !doc.links_dirty {
            return;
        }
        doc.links_dirty = false;
        let dom_rc = doc.dom.clone();
        let dom = dom_rc.borrow();
        let base = Self::base_url(doc);
        let Some(root) = dom.document_element(doc.document) else {
            return;
        };
        // HTML "block rendering": an element joins the render-blocking element set only while
        // the document allows adding render-blocking elements (a text/html document whose body
        // element is null). A `link` or `style` the parser created is implicitly potentially
        // render-blocking, and the parser created those that precede the body in tree order before
        // the body existed; one a script inserted is potentially render-blocking with
        // `blocking=render`, and blocks only when the body is null now.
        let body = dom.body(doc.document);
        let allows_adding = dom.is_html_document(doc.document) && body.is_none();
        let rendered = doc.rendered;
        let mut before_body = !rendered;
        let mut found = Vec::new();
        for n in dom.descendants(root) {
            if Some(n) == body {
                before_body = false;
            }
            let blocks = |n: NodeId| {
                let parsed =
                    dom.element_flags(n) & snowlark_dom::element_flags::PARSER_INSERTED != 0;
                !rendered
                    && if parsed {
                        before_body
                    } else {
                        allows_adding
                            && dom
                                .get_attribute(n, "blocking")
                                .is_some_and(|b| has_token(b, "render"))
                    }
            };
            if dom.is_style_element(n) && !doc.import_blockers.contains(&n) && blocks(n) {
                // (its `@import`s are critical subresources)
                doc.import_blockers.push(n);
            }
            if !dom.is_html_element_named(n, &local_name!("link")) || doc.links.contains_key(&n) {
                continue;
            }
            let attr = |name| dom.get_attribute(n, name);
            let rel = attr("rel").unwrap_or("");
            if !has_token(rel, "stylesheet") || has_token(rel, "alternate") {
                continue;
            }
            if attr("disabled").is_some() {
                continue;
            }
            if attr("type")
                .is_some_and(|t| !t.trim().is_empty() && !t.trim().eq_ignore_ascii_case("text/css"))
            {
                continue;
            }
            let Some(href) = attr("href").map(str::trim).filter(|h| !h.is_empty()) else {
                continue;
            };
            let Ok(url) = base.join(href) else {
                continue;
            };
            // HTML link type "stylesheet", the linked resource fetch setup steps: "If el's
            // media attribute's value matches the environment and el is potentially render-blocking,
            // then block rendering on el."
            let blocking = blocks(n)
                && srcset::media_matches(
                    doc.engine.stylist().device(),
                    doc.engine.stylist().quirks_mode(),
                    &style::stylesheets::UrlExtraData::from(base.clone()),
                    attr("media").unwrap_or(""),
                );
            // The request's render-blocking flag (Resource Timing's renderBlockingStatus) also
            // counts a `blocking=render` link inserted into the head later, as Chromium does.
            let request_blocking = blocking
                || (attr("blocking").is_some_and(|b| has_token(b, "render"))
                    && dom
                        .head(doc.document)
                        .is_some_and(|head| dom.ancestors(n).any(|a| a == head)));
            found.push((n, url, blocking, request_blocking));
        }
        for (n, url, blocking, request_blocking) in found {
            if blocking {
                doc.render_blocking.insert(n);
                doc.import_blockers.push(n);
            }
            doc.links.insert(n, LinkSheet { text: None });
            if !external {
                doc.fetcher
                    .start_blocking(url, Purpose::Sheet(n), request_blocking);
                doc.requests += 1;
                doc.counts.sheets += 1;
            }
        }
    }

    fn start_missing_imports(&mut self) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        let missing: Vec<Url> = std::mem::take(&mut *doc.imports.missing.lock().unwrap());
        // (an `@import` of a sheet that blocks the first rendering blocks it too)
        let blocking = !doc.rendered && {
            let dom = doc.dom.borrow();
            doc.import_blockers
                .iter()
                .any(|&n| dom.is_alive(n) && dom.is_connected(n))
        };
        for url in missing {
            if !doc.fetcher.is_fetching(&url) {
                doc.fetcher.start_blocking(url, Purpose::Import, blocking);
                doc.requests += 1;
                doc.counts.imports += 1;
            }
        }
    }

    /// Parses every author sheet again (an `@import` arrived).
    fn reparse_sheets(doc: &mut Doc) {
        let owners: Vec<NodeId> = doc.links.keys().copied().collect();
        for owner in owners {
            let Some((text, url)) = doc.links[&owner].text.clone() else {
                continue;
            };
            Self::apply_link_sheet(doc, owner, &text, &url);
        }
        // The `<style>` sheets that import (parsing them again keeps no CSSOM change of theirs, as
        // a browser's would not either once the import arrived).
        let dom_rc = doc.dom.clone();
        let dom = dom_rc.borrow();
        let styles: Vec<NodeId> = dom
            .shadow_including_descendants(doc.document)
            .filter(|&n| dom.is_style_element(n))
            .filter(|&n| dom.child_text_content(n).contains("@import"))
            .collect();
        for el in styles {
            update_style_block(
                &mut doc.engine,
                &doc.data,
                doc.style_check.as_ref(),
                &dom,
                el,
            );
        }
        doc.needs_restyle = true;
    }

    fn apply_link_sheet(doc: &mut Doc, owner: NodeId, text: &str, url: &Url) {
        let dom_rc = doc.dom.clone();
        let dom = dom_rc.borrow();
        let media = dom.get_attribute(owner, "media").unwrap_or("").to_owned();
        let sheet = doc.engine.parse_sheet(&doc.data, text, url, &media);
        doc.engine.remove_sheets_of(&doc.data, owner);
        doc.engine.add_sheet(&dom, &doc.data, owner, sheet);
        doc.needs_restyle = true;
    }

    /// Applies the fetches that completed: a stylesheet is parsed and added (restyle), an image is
    /// decoded (its box is rebuilt, or the next paint shows a background), a font is registered
    /// (boxes rebuilt with the new font). Never blocks.
    pub fn pump(&mut self) -> PumpResult {
        let Some(doc) = self.doc.as_deref_mut() else {
            return PumpResult::default();
        };
        let arrivals = doc.fetcher.poll();
        let n = arrivals.len();
        if n > 0 {
            let t = Instant::now();
            for a in arrivals {
                self.apply(a);
            }
            self.start_missing_imports();
            self.timings.resources_ms += ms(t);
        }
        PumpResult {
            applied: n,
            pending: self.doc.as_ref().map_or(0, |d| d.fetcher.len()),
        }
    }

    fn apply(&mut self, a: Arrival) {
        let trace = self.trace;
        let doc = self.doc.as_deref_mut().expect("a document");
        if trace {
            eprintln!(
                "[render] {:?} {} in {} ms: {}",
                a.purpose,
                a.url,
                a.elapsed.as_millis(),
                match &a.result {
                    Ok((_, b)) => format!("{} B", b.len()),
                    Err(e) => e.clone(),
                }
            );
        }
        if a.result.is_err() {
            doc.counts.failed += 1;
        }
        match a.purpose {
            Purpose::Sheet(owner) => {
                // "Process the linked resource", on success or failure: "Unblock rendering on el"
                // (its `@import`s, the critical subresources, block on their own: see
                // `Renderer::render_blocked`).
                doc.render_blocking.remove(&owner);
                let Ok((head, body)) = a.result else {
                    return;
                };
                let final_url = head.url().cloned().unwrap_or_else(|| a.url.clone());
                let charset = snowlark_net::encoding::legacy_extract_encoding(&head.headers);
                let env = doc.dom.borrow().encoding(doc.document);
                let (text, _) = decode_stylesheet(&body, charset, Some(env));
                Self::apply_link_sheet(doc, owner, &text, &final_url);
                if let Some(l) = doc.links.get_mut(&owner) {
                    l.text = Some((text, final_url));
                }
            }
            Purpose::Import => {
                let entry = match a.result {
                    Ok((head, _)) if !import_is_css(doc, &head, &a.url) => ImportEntry::Failed,
                    Ok((head, body)) => {
                        let final_url = head.url().cloned().unwrap_or_else(|| a.url.clone());
                        let charset =
                            snowlark_net::encoding::legacy_extract_encoding(&head.headers);
                        let env = doc.dom.borrow().encoding(doc.document);
                        let (text, _) = decode_stylesheet(&body, charset, Some(env));
                        ImportEntry::Loaded {
                            text,
                            url: final_url,
                        }
                    }
                    Err(_) => ImportEntry::Failed,
                };
                doc.imports
                    .entries
                    .lock()
                    .unwrap()
                    .insert(a.url.as_str().to_owned(), entry);
                // Parse again once every import in flight has arrived.
                let still = doc
                    .imports
                    .entries
                    .lock()
                    .unwrap()
                    .values()
                    .any(|e| matches!(e, ImportEntry::Pending));
                if !still {
                    Self::reparse_sheets(doc);
                }
            }
            Purpose::SvgResource | Purpose::SvgStyleResource => {
                use crate::svg_resource::{style_key, ResourceDocument, SvgResource};
                let key = if a.purpose == Purpose::SvgStyleResource {
                    style_key(a.url.as_str())
                } else {
                    a.url.as_str().to_owned()
                };
                let config = self.config.clone();
                let state = a
                    .result
                    .ok()
                    .and_then(|(head, body)| {
                        let html = snowlark_net::mime::extract_mime_type(&head.headers)
                            .is_some_and(|m| m.essence() == "text/html");
                        ResourceDocument::parse(&body, &a.url, html, config)
                    })
                    .map_or(SvgResource::Failed, |r| SvgResource::Loaded(Rc::new(r)));
                doc.svg_resources.insert(key, state);
                // (the `svg` elements that refer to it are made again, and the `use` elements report)
                doc.images_walked = None;
                doc.needs_restyle = true;
            }
            Purpose::SvgImage => {
                // (an inline `svg` shows it: its image is made again with the bytes)
                if let Ok((_, body)) = a.result {
                    doc.svg_image_bytes
                        .insert(a.url.as_str().to_owned(), Arc::new(body));
                    doc.images_walked = None;
                    doc.needs_restyle = true;
                }
            }
            Purpose::Image => {
                let key = a.url.as_str().to_owned();
                let mut response_type = None;
                let state = match a.result {
                    Ok((head, body)) => {
                        response_type = snowlark_net::mime::extract_mime_type(&head.headers)
                            .map(|m| m.essence().to_ascii_lowercase());
                        let svg = response_type
                            .as_deref()
                            .is_some_and(crate::svg::is_svg_mime);
                        let clock = self.clock;
                        // (`image-orientation: none` shows a same-origin image's stored pixels:
                        // CSS Images 3 §5.3)
                        let same = same_origin(&doc.url, &a.url);
                        let keep_raw = same || cors_shared(&doc.url, &head.headers);
                        let mut add = |image: snowlark_paint::image::DecodedImage| {
                            let (width, height) = (image.width(), image.height());
                            let animation = image.animation.clone();
                            let raw = image.unoriented().filter(|_| keep_raw);
                            let id = self.images.add(image);
                            self.owned_images.push(id);
                            if let Some(animation) = animation {
                                Self::animation_added(doc, id, animation, clock);
                            }
                            let mut loaded = LoadedImage::bitmap(id, width as f32, height as f32);
                            if let Some(raw) = raw {
                                let n = NaturalSize::fixed(raw.width() as f32, raw.height() as f32);
                                let raw_id = self.images.add(raw);
                                self.owned_images.push(raw_id);
                                loaded.unoriented = Some((raw_id, n));
                                loaded.cors_only = !same;
                            }
                            loaded
                        };
                        let loaded = if let Some(pixels) = a.decoded {
                            pixels.map(&mut add)
                        } else if svg {
                            // An SVG image: parsed now (as a document, which is styled), rasterized
                            // when painted at a size.
                            SvgImage::parse(&body).ok().map(|image| {
                                let natural = image.natural();
                                let now = doc.engine.timeline_time();
                                let document = crate::svg::SvgDocument::parse(&body, &a.url, now);
                                let dark = crate::svg::SvgDocument::parse(&body, &a.url, now);
                                let id = self.svg.add_with_document(&self.images, image, document);
                                let mut loaded = LoadedImage::new(id, natural);
                                loaded.dark = dark
                                    .and_then(|d| self.svg.add_dark_variant(&self.images, id, d));
                                loaded
                            })
                        } else {
                            snowlark_paint::image::decode(&body).ok().map(add)
                        };
                        match loaded {
                            Some(image) => UrlImage::Loaded(image),
                            None => {
                                if trace {
                                    eprintln!("[render] {}: undecodable image", a.url);
                                }
                                doc.counts.failed += 1;
                                UrlImage::Failed
                            }
                        }
                    }
                    Err(_) => UrlImage::Failed,
                };
                doc.images_by_url.insert(key.clone(), state);
                doc.image_url_events
                    .push((key.clone(), ImageStatus::of(state)));
                if let UrlImage::Loaded(image) = state {
                    self.image_table.insert(key.clone(), image);
                }
                if doc.box_images.contains(&key) {
                    doc.needs_build = true;
                }
                if doc.filter_image_urls.contains(&key) {
                    doc.filters_stale = true;
                    doc.needs_restyle = true;
                }
                // The <img> elements showing it.
                let users: Vec<(NodeId, f32)> = doc
                    .img_src
                    .iter()
                    .filter_map(|(n, s)| match s {
                        Some((u, d)) if *u == key => Some((*n, *d)),
                        _ => None,
                    })
                    .collect();
                for (n, density) in users {
                    doc.img_state.insert(n, Self::img_state_of(state, density));
                    doc.changes.restyled.push(ElementChange::reconstruct(n));
                    // HTML §4.8.7: the response's type decides what an `object` or `embed` shows;
                    // what was fetched as an image on a guess may be a document.
                    let embed = {
                        let dom = doc.dom.borrow();
                        dom.is_html_element_named(n, &local_name!("object"))
                            || dom.is_html_element_named(n, &local_name!("embed"))
                    };
                    if !embed {
                        continue;
                    }
                    if let Some(t) = &response_type {
                        doc.embed_types.insert(n, t.clone());
                        if !t.starts_with("image/") || t == "image/svg+xml" {
                            doc.embed_redecided.push((n, t.clone()));
                            continue;
                        }
                    }
                    doc.embed_settled
                        .push((n, matches!(state, UrlImage::Loaded(_))));
                }
            }
            Purpose::Font { face, source } => {
                let loaded = match a.result {
                    Ok((_, body)) => crate::woff::to_sfnt(&body)
                        .ok()
                        .and_then(|sfnt| self.text.add_font_data(&sfnt).ok()),
                    Err(_) => None,
                };
                match loaded {
                    Some(internal) => {
                        let f = &mut doc.faces[face];
                        f.loaded = true;
                        f.web = Some(WebFace::described(
                            internal,
                            &f.face.weight,
                            &f.face.style,
                            &f.face.stretch,
                        ));
                        let family = f.face.family.clone();
                        Self::rebuild_fonts(&self.fonts, doc);
                        doc.env = Arc::new(StyleEnv {
                            fonts: self.fonts.read().unwrap().clone(),
                            images: self.image_table.clone(),
                            motion_paths: doc.env.motion_paths.clone(),
                        });
                        doc.fonts_arrived.push(family);
                        // Font-relative units (ex, ch) may change.
                        doc.engine.sheets_changed();
                        doc.needs_restyle = true;
                    }
                    None => {
                        if trace {
                            eprintln!(
                                "[render] font source {source} of {} unusable",
                                doc.faces[face].face.family
                            );
                        }
                        Self::next_font_source(&self.text, doc, face, source + 1);
                    }
                }
            }
        }
    }

    /// An animated image arrived: its animation starts at its first paint (see [`ImageAnimation`]).
    fn animation_added(
        doc: &mut Doc,
        id: snowlark_paint::ImageId,
        animation: Arc<snowlark_paint::image::Animation>,
        clock: TimelineClock,
    ) {
        let start = (!matches!(clock, TimelineClock::Monotonic)).then_some(0.0);
        doc.animations.insert(
            id,
            ImageAnimation {
                animation,
                start,
                frame: 0,
                painted: Vec::new(),
                layered: false,
                decoder: None,
                failed: false,
            },
        );
    }

    fn img_state_of(state: UrlImage, density: f32) -> ImageState {
        match state {
            UrlImage::Fetching => ImageState::Pending,
            UrlImage::Failed => ImageState::Failed,
            UrlImage::Loaded(image) => ImageState::Loaded(image.at_density(density)),
        }
    }

    /// Brings the document's faces up to date with its style sheets (a no-op when they have not
    /// changed since): known faces keep their load, new ones start theirs, faces no rule declares
    /// any more stop counting, and the cascade order of the rest is registered.
    fn reconcile_faces(text: &TextEngine, fonts: &RwLock<FontAliases>, doc: &mut Doc) -> bool {
        let generation = doc.engine.sheets_generation();
        if doc.faces_generation == Some(generation) {
            return false;
        }
        {
            doc.faces_generation = Some(generation);
            let declared = doc.engine.font_faces(&doc.data);
            // The faces now declared, in cascade order: those already known keep their load, the
            // new ones start theirs, the ones no rule declares any more stop counting.
            let mut used = vec![false; doc.faces.len()];
            let mut order = Vec::with_capacity(declared.len());
            for face in declared {
                let known = (0..doc.faces.len()).find(|&i| {
                    doc.faces[i].active
                        && !doc.faces[i].script
                        && !used[i]
                        && doc.faces[i].face == face
                });
                if let Some(i) = known {
                    used[i] = true;
                    order.push(i);
                    continue;
                }
                doc.faces.push(FaceLoad {
                    face,
                    source: 0,
                    loaded: false,
                    active: true,
                    web: None,
                    script: false,
                    started: true,
                    in_use: true,
                });
                used.push(true);
                let i = doc.faces.len() - 1;
                order.push(i);
                Self::next_font_source(text, doc, i, 0);
            }
            for (i, f) in doc.faces.iter_mut().enumerate() {
                if f.script {
                    // (faces a script added come after those the style sheets declare)
                    if f.active && f.in_use {
                        order.push(i);
                    }
                } else if !used[i] {
                    f.active = false;
                }
            }
            let changed = doc.face_order != order;
            if changed {
                // The families of the faces that came, went or moved: their text is shaped again
                // with the faces that now apply, though its computed style may be the same (a
                // descriptor changed through `CSSFontFaceRule.style`, a rule inserted or removed).
                for &i in doc.face_order.iter().chain(&order) {
                    let family = &doc.faces[i].face.family;
                    if !doc
                        .fonts_arrived
                        .iter()
                        .any(|f| f.eq_ignore_ascii_case(family))
                    {
                        doc.fonts_arrived.push(family.clone());
                    }
                }
            }
            doc.face_order = order;
            Self::rebuild_fonts(fonts, doc);
            if changed {
                // Shaping depends on the faces (and their order): measure again.
                doc.engine.sheets_changed();
                doc.needs_restyle = true;
                doc.faces_restyle = true;
                doc.faces_generation = Some(doc.engine.sheets_generation());
            }
        }
        true
    }

    /// Registers the loaded active faces with the font table in cascade order.
    fn rebuild_fonts(fonts: &RwLock<FontAliases>, doc: &Doc) {
        let mut table = FontAliases::default();
        for (position, &i) in doc.face_order.iter().enumerate() {
            let f = &doc.faces[i];
            let family = table.family_mut(&f.face.family);
            if let Some(web) = &f.web {
                family.faces.push(WebFace {
                    order: position,
                    ..web.clone()
                });
            }
        }
        *fonts.write().unwrap() = table;
    }

    /// Starts fetching source `from` (or the next usable one) of face `face`.
    fn next_font_source(text: &TextEngine, doc: &mut Doc, face: usize, from: usize) {
        let f = &mut doc.faces[face];
        let mut i = from;
        while let Some(src) = f.face.sources.get(i) {
            match src {
                FontFaceSource::Url { url, format } => {
                    // CSS Fonts 4 §4.3: skip formats this engine can't load.
                    let ok = format.as_deref().is_none_or(|fmt| {
                        let fmt = fmt.to_ascii_lowercase();
                        ["woff2", "woff", "truetype", "opentype"]
                            .iter()
                            .any(|k| fmt.contains(k))
                    });
                    if ok {
                        if let Ok(u) = Url::parse(url) {
                            f.source = i;
                            doc.fetcher.start(u, Purpose::Font { face, source: i });
                            doc.requests += 1;
                            doc.counts.fonts += 1;
                            return;
                        }
                    }
                }
                FontFaceSource::Local(name) => {
                    // `local()`: the installed family of that name stands in for the face; with
                    // none installed the source fails.
                    if !text.has_family(name) {
                        i += 1;
                        continue;
                    }
                    f.source = i;
                    f.loaded = true;
                    f.web = Some(WebFace::described(
                        name.clone(),
                        &f.face.weight,
                        &f.face.style,
                        &f.face.stretch,
                    ));
                    return;
                }
            }
            i += 1;
        }
        f.source = i;
    }

    /// Finds the images, `@font-face` faces and `<link>` sheets the styled document needs and starts
    /// fetching the new ones.
    /// Finds the subresources the styles and the DOM need: `@font-face` sources, and the images of
    /// `url()` values and `<img>` elements (walking every element: skipped when nothing was
    /// `restyled` and the DOM and the device are as the last walk saw them).
    fn discover(&mut self, restyled: bool) {
        self.discover_links();
        let dpr = self.config.device_pixel_ratio;
        let viewport = self.config.viewport;
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        let base = Self::base_url(doc);
        // @font-face (read again only after the sheets changed).
        Self::reconcile_faces(&self.text, &self.fonts, doc);
        doc.env = Arc::new(StyleEnv {
            fonts: self.fonts.read().unwrap().clone(),
            images: self.image_table.clone(),
            motion_paths: doc.env.motion_paths.clone(),
        });
        // Images. A walk over every element when the viewport, the device pixel ratio or the base
        // URL changed (`srcset` chooses again, relative URLs resolve again); otherwise only the
        // elements that may have changed what they need: those restyled, those whose attributes or
        // children changed, and inserted subtrees. (A walk per restyle made every forced style
        // update cost the document's size.)
        let dom_rc = doc.dom.clone();
        let dom = dom_rc.borrow();
        let walk = (viewport, dpr, base.clone());
        let full = doc.images_walked.as_ref() != Some(&walk) || doc.data.restyles().1;
        let (log, _) = doc.data.restyles();
        let new_restyles = &log[doc.restyles_seen.min(log.len())..];
        // (a script's read makes no image: only what it can observe, an svg's natural size, counts)
        let script_read = self.script_read && !full;
        if !full
            && !restyled
            && new_restyles.is_empty()
            && doc.discover_nodes.is_empty()
            && doc.discover_roots.is_empty()
            && (script_read || doc.svgs_waiting.is_empty())
            && !doc.filters_stale
        {
            return;
        }
        let Some(root) = dom.document_element(doc.document) else {
            return;
        };
        doc.images_walked = Some(walk);
        let device = doc.engine.stylist().device();
        let quirks = doc.engine.stylist().quirks_mode();
        let url_data = style::stylesheets::UrlExtraData::from(base.clone());
        let mut css_urls: Vec<(String, bool)> = Vec::new();
        let mut img_changes: Vec<(NodeId, Option<(String, f32)>)> = Vec::new();
        let mut svg_changes: Vec<(NodeId, u64, crate::inline_svg::Serialized)> = Vec::new();
        // (svgs drawn as paint operations, with no markup: docs/svg-paint.md step 1)
        let mut print_changes: Vec<(NodeId, crate::svg_paint::Fingerprint)> = Vec::new();
        // (the svgs that changed themselves, not only what is inside them: their attributes give
        // the natural size, which layout reads)
        let mut svg_roots: rustc_hash::FxHashSet<NodeId> = Default::default();
        // (a change a `clip-path: url()` could see: a `clipPath` is in a `defs` or has an id, so a
        // change in one is a change other svgs can reference)
        let mut clips_changed = full;
        let ours = |n: NodeId| {
            dom.is_alive(n)
                && matches!(dom.data(n), NodeData::Element(_))
                && dom.is_connected(n)
                && dom.node_document(n) == doc.document
        };
        let elements: Vec<NodeId> = if full {
            // (shadow trees included: their images, inline svgs and style URLs are the page's too)
            dom.shadow_including_descendants(root)
                .chain(std::iter::once(root))
                .filter(|&n| matches!(dom.data(n), NodeData::Element(_)))
                .collect()
        } else {
            let mut set: rustc_hash::FxHashSet<NodeId> = Default::default();
            let mut out = Vec::new();
            let mut sprites = false;
            let mut add = |n: NodeId, out: &mut Vec<NodeId>, sprites: &mut bool| {
                if !ours(n) {
                    return;
                }
                // (a change inside an inline `svg` changes its image: the outermost `svg` is
                // serialized again; a change that other svgs can reference, inside an element
                // with an `id`, `defs` or `symbol`, or in an svg that is not rendered, as sprite
                // sheets are, serializes them all again)
                let n = match crate::inline_svg::outermost_svg_of(&dom, n) {
                    Some(svg) => {
                        let display_none = doc
                            .data
                            .styles(svg)
                            .is_none_or(|s| s.get_box().display.is_none());
                        if display_none || crate::inline_svg::referable(&dom, n, svg) {
                            *sprites = true;
                        }
                        if n == svg {
                            svg_roots.insert(svg);
                        }
                        svg
                    }
                    None => n,
                };
                if set.insert(n) {
                    out.push(n);
                }
            };
            for r in new_restyles {
                add(r.element, &mut out, &mut sprites);
            }
            for &n in &doc.discover_nodes {
                add(n, &mut out, &mut sprites);
            }
            for &r in &doc.discover_roots {
                if ours(r) {
                    for n in dom.shadow_including_inclusive_descendants(r) {
                        if matches!(dom.data(n), NodeData::Element(_)) {
                            add(n, &mut out, &mut sprites);
                        }
                    }
                }
            }
            if !script_read {
                for &n in &doc.svgs_waiting {
                    add(n, &mut out, &mut sprites);
                }
            }
            if sprites {
                clips_changed = true;
                for &svg in doc.inline_svgs.keys() {
                    if ours(svg) && set.insert(svg) {
                        out.push(svg);
                    }
                }
            }
            out
        };
        doc.restyles_seen = log.len();
        doc.discover_nodes.clear();
        doc.discover_roots.clear();
        if !script_read {
            doc.svgs_waiting.clear();
        }
        for n in elements {
            if let Some(id) = doc.data.styles(n).and_then(crate::motion::url_fragment) {
                doc.motion_refs.insert(id);
            }
            let Some(values) = doc.data.styles_arc(n) else {
                continue;
            };
            collect_style_urls(&values, &mut css_urls);
            if clip_url(&values).is_some() && !doc.clip_references.contains_key(&n) {
                doc.clip_references.insert(n, ClipUse::default());
                clips_changed = true;
            }
            if !filter_urls(&values).is_empty() && !doc.filter_references.contains_key(&n) {
                doc.filter_references.insert(n, None);
                clips_changed = true;
            }
            for p in [
                snowlark_layout::style::PseudoElement::Before,
                snowlark_layout::style::PseudoElement::After,
            ] {
                if let Some(pv) =
                    stylo_pseudo(p).and_then(|sp| doc.engine.pseudo_style(&dom, &doc.data, n, &sp))
                {
                    collect_style_urls(&pv, &mut css_urls);
                }
            }
            if dom.is_html_element_named(n, &local_name!("img")) && !doc.page_imgs.contains(&n) {
                let chosen =
                    select_img_source(&dom, n, &base, device, quirks, &url_data, dpr, None);
                if doc.img_src.get(&n) != Some(&chosen) {
                    img_changes.push((n, chosen));
                }
            }
            // HTML §4.8.9: a video's poster frame, the `poster` attribute's URL.
            if dom.is_html_element_named(n, &local_name!("video")) {
                let chosen = dom
                    .get_attribute(n, "poster")
                    .filter(|p| !p.trim().is_empty())
                    .and_then(|p| base.join(p.trim()).ok())
                    .map(|u| (u.to_string(), 1.0));
                if doc.img_src.get(&n) != Some(&chosen) {
                    img_changes.push((n, chosen));
                }
            }
            // HTML §4.8.7: an `object` or `embed` showing an image fetches it as an image, once it
            // is being rendered.
            let known = doc.embed_types.get(&n).map(String::as_str);
            let rendered = || {
                std::iter::successors(Some(n), |&a| dom.parent(a))
                    .filter(|&a| dom.element(a).is_some())
                    .all(|a| {
                        doc.data
                            .styles(a)
                            .is_some_and(|v| !v.get_box().display.is_none())
                    })
            };
            if let Some(url) = crate::source::embedded_image(&dom, n, known).filter(|_| rendered())
            {
                // (HTML §4.8.6, §4.8.7: "encoding-parsing a URL", the query in the document's
                // encoding)
                let encoding = dom.encoding(dom.node_document(n));
                let chosen = crate::encoding_parse_url(&base, encoding, url.trim())
                    .map(|u| (u.to_string(), 1.0));
                if doc.img_src.get(&n) != Some(&chosen) {
                    img_changes.push((n, chosen));
                }
            }
            if crate::inline_svg::is_outermost_svg(&dom, n) {
                if script_read && doc.inline_svgs.contains_key(&n) && !svg_roots.contains(&n) {
                    doc.svgs_waiting.insert(n);
                    continue;
                }
                doc.svgs_waiting.remove(&n);
                if !doc.svg_markup.contains(&n) {
                    self.stats.svgs_fingerprinted += 1;
                    if let Some(print) =
                        crate::svg_paint::fingerprint(&dom, &doc.data, n, &doc.url, &base)
                    {
                        // (unchanged: the old fingerprint and its key stay)
                        let same = doc.svg_prints.get(&n).is_some_and(|old| {
                            print.draws_like(old)
                                && doc.inline_svgs.get(&n).map(|e| e.0) == Some(old.key())
                        });
                        if !same {
                            print_changes.push((n, print));
                        }
                        continue;
                    }
                }
                doc.svg_prints.remove(&n);
                self.stats.svgs_serialized += 1;
                let svg = {
                    let external = crate::inline_svg::External::new(&doc.svg_resources, &doc.url);
                    crate::inline_svg::serialize(&dom, &doc.data, n, &base, &external)
                };
                // SVG 2 §16.1.7: the documents its `use` elements refer to, each fetched once.
                for url in svg.uses.iter().map(|u| &u.1).chain(&svg.resources) {
                    use crate::svg_resource::{reference_allowed, SvgResource};
                    if doc.svg_resources.contains_key(url) {
                        continue;
                    }
                    let state = match Url::parse(url) {
                        Ok(u) if reference_allowed(&doc.url, &u) => {
                            doc.fetcher.start(u, Purpose::SvgResource);
                            doc.requests += 1;
                            SvgResource::Pending
                        }
                        _ => SvgResource::Failed,
                    };
                    doc.svg_resources.insert(url.clone(), state);
                }
                for url in &svg.style_resources {
                    use crate::svg_resource::{style_key, SvgResource};
                    let key = style_key(url);
                    if !doc.svg_resources.contains_key(&key) {
                        let state = match Url::parse(url) {
                            Ok(u) => {
                                doc.fetcher.start(u, Purpose::SvgStyleResource);
                                doc.requests += 1;
                                SvgResource::Pending
                            }
                            _ => SvgResource::Failed,
                        };
                        doc.svg_resources.insert(key, state);
                    }
                }
                // Their `load` and `error` events: once per reference that was processed.
                // (a `use` removed and collected since is gone, whichever svg it was in)
                doc.use_reported.retain(|el, _| {
                    dom.is_alive(*el)
                        && (crate::inline_svg::outermost_svg_of(&dom, *el) != Some(n)
                            || svg.uses.iter().any(|u| u.0 == *el))
                });
                for (el, url) in &svg.uses {
                    use crate::svg_resource::SvgResource;
                    let ok = match doc.svg_resources.get(url) {
                        Some(SvgResource::Loaded(_)) => true,
                        Some(SvgResource::Failed) => false,
                        _ => continue,
                    };
                    if doc.use_reported.get(el) != Some(url) {
                        doc.use_reported.insert(*el, url.clone());
                        doc.use_events.push((*el, ok));
                    }
                }
                // (its `<image>`s are the document's images: an arrival makes it again)
                for url in &svg.images {
                    if doc.svg_image_urls.insert(url.clone()) {
                        if let Ok(u) = Url::parse(url) {
                            doc.fetcher.start(u, Purpose::SvgImage);
                            doc.requests += 1;
                            doc.counts.images += 1;
                        }
                    }
                }
                let hash = {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    svg.markup.hash(&mut h);
                    for url in &svg.images {
                        doc.svg_image_bytes.contains_key(url).hash(&mut h);
                    }
                    h.finish()
                };
                if doc.inline_svgs.get(&n).map(|e| e.0) != Some(hash) {
                    svg_changes.push((n, hash, svg));
                }
            }
        }
        // Inline SVG: an image per outermost `svg` element, made again when its markup changes;
        // the element's replaced box is built again with the new image.
        // (gone: removed, moved into another `svg`, or in a `display: none` subtree now)
        let gone: Vec<NodeId> = doc
            .inline_svgs
            .keys()
            .filter(|&&n| {
                !(ours(n)
                    && crate::inline_svg::is_outermost_svg(&dom, n)
                    && doc.data.styles_arc(n).is_some())
            })
            .copied()
            .collect();
        for n in gone {
            if let Some((_, Some(id))) = doc.inline_svgs.remove(&n) {
                self.svg.remove(&self.images, id);
            }
            doc.svg_prints.remove(&n);
            doc.svg_markup.remove(&n);
            doc.img_state.remove(&n);
        }
        let svgs_changed = !svg_changes.is_empty() || !print_changes.is_empty();
        for (n, print) in print_changes {
            if let Some((_, Some(old))) = doc.inline_svgs.remove(&n) {
                self.svg.remove(&self.images, old);
            }
            // (the root's attributes as the markup would have them: crate::inline_svg writes the
            // outermost element's attributes, animated values first)
            let attr = |name: &str| {
                dom.animated_attr(n, name)
                    .or_else(|| dom.attr(n, &ns!(), &name.into()))
            };
            let image = crate::svg::SvgImage::without_markup(
                attr("width"),
                attr("height"),
                attr("viewBox"),
            );
            let natural = image.natural();
            let id = self.svg.add(&self.images, image);
            doc.img_state
                .insert(n, ImageState::Loaded(LoadedImage::new(id, natural)));
            doc.inline_svgs.insert(n, (print.key(), Some(id)));
            doc.svg_prints.insert(n, print);
            doc.changes.restyled.push(ElementChange::reconstruct(n));
        }
        for (n, hash, svg) in svg_changes {
            if let Some((_, Some(old))) = doc.inline_svgs.remove(&n) {
                self.svg.remove(&self.images, old);
            }
            let images: HashMap<String, Arc<Vec<u8>>> = svg
                .images
                .iter()
                .filter_map(|u| Some((u.clone(), doc.svg_image_bytes.get(u)?.clone())))
                .collect();
            let image = crate::svg::SvgImage::parse(svg.markup.as_bytes())
                .ok()
                .map(|i| i.with_images(images));
            let id = image.map(|img| {
                let natural = img.natural();
                let id = self.svg.add(&self.images, img);
                doc.img_state
                    .insert(n, ImageState::Loaded(LoadedImage::new(id, natural)));
                id
            });
            if id.is_none() {
                doc.img_state.insert(n, ImageState::Failed);
            }
            doc.inline_svgs.insert(n, (hash, id));
            doc.changes.restyled.push(ElementChange::reconstruct(n));
        }
        // `clip-path: url()`: what each names is resolved again when the `clipPath` (as written
        // for it) changes, and the element's box is built again with the new geometry.
        if clips_changed || svgs_changed {
            let users: Vec<NodeId> = doc.clip_references.keys().copied().collect();
            for n in users {
                let url = ours(n)
                    .then(|| doc.data.styles_arc(n))
                    .flatten()
                    .and_then(|v| clip_url(&v));
                let Some(url) = url else {
                    doc.clip_references.remove(&n);
                    continue;
                };
                // The `clipPath` named, then the one its own `clip-path` names, and so on.
                let mut levels: Vec<(String, bool)> = Vec::new();
                let mut chain: Vec<NodeId> = Vec::new();
                let find = |from: NodeId, url: &str| {
                    crate::clip_ref::local_id(url)
                        .and_then(|id| crate::clip_ref::find_clip_path(&dom, from, id))
                };
                let mut next = find(n, &url);
                while let Some(c) = next.filter(|c| !chain.contains(c)) {
                    if chain.len() >= crate::clip_ref::MAX_LEVELS {
                        break;
                    }
                    chain.push(c);
                    let external = crate::inline_svg::External::new(&doc.svg_resources, &doc.url);
                    levels.push((
                        crate::inline_svg::clip_path_markup(&dom, &doc.data, c, &base, &external),
                        crate::clip_ref::bounding_box_units(&dom, c),
                    ));
                    next = doc
                        .data
                        .styles_arc(c)
                        .and_then(|v| clip_url(&v))
                        .and_then(|u| find(c, &u));
                }
                let hash = {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    levels.hash(&mut h);
                    h.finish()
                };
                let Some(entry) = doc.clip_references.get_mut(&n) else {
                    continue;
                };
                if entry.resolved && entry.hash == hash {
                    continue;
                }
                entry.resolved = true;
                entry.hash = hash;
                entry.reference = (!levels.is_empty()).then(|| {
                    let text = levels.iter().any(|(m, _)| m.contains("<text"));
                    let fonts = text.then(|| self.svg.fonts());
                    Arc::new(crate::clip_ref::resolve(&levels, fonts))
                });
                doc.changes.restyled.push(ElementChange::reconstruct(n));
            }
        }
        // `filter: url()`: each URL's `<filter>` is read again when what is written for it may
        // have changed (or an `feImage`'s image came), and the element's box is built again when
        // it did.
        if clips_changed || svgs_changed || std::mem::take(&mut doc.filters_stale) {
            use crate::filter_ref::{ImageHref, ImageSource};
            let users: Vec<NodeId> = doc.filter_references.keys().copied().collect();
            // (the images `feImage`s refer to by URL, and the element images still drawn)
            let mut image_urls: Vec<String> = Vec::new();
            let mut fe_used: HashSet<u64> = HashSet::new();
            for n in users {
                let urls = ours(n)
                    .then(|| doc.data.styles_arc(n))
                    .flatten()
                    .map(|v| filter_urls(&v))
                    .unwrap_or_default();
                if urls.is_empty() {
                    doc.filter_references.remove(&n);
                    continue;
                }
                let mut refs: FilterReferences = Vec::with_capacity(urls.len());
                for url in urls {
                    let found = crate::clip_ref::local_id(&url)
                        .and_then(|id| crate::filter_ref::find_filter(&dom, n, id));
                    let Some(f) = found else {
                        refs.push((url, None));
                        continue;
                    };
                    let styles = |e: NodeId| doc.data.styles_arc(e);
                    let lengths = crate::filter_ref::Lengths {
                        device: doc.engine.stylist().device(),
                        quirks: doc.engine.stylist().quirks_mode(),
                        base: &base,
                    };
                    let mut filter = crate::filter_ref::read(&dom, f, &styles, &lengths);
                    // Filter Effects 1 §9.15: what each `feImage` draws (nothing until its image
                    // is there).
                    let hrefs: Vec<(usize, ImageHref)> =
                        filter.image_hrefs().map(|(i, h)| (i, h.clone())).collect();
                    for (i, href) in hrefs {
                        let source = match href {
                            ImageHref::Url(u) => {
                                let source = match doc.images_by_url.get(&u) {
                                    Some(UrlImage::Loaded(img)) => {
                                        match (img.natural.width, img.natural.height) {
                                            (Some(width), Some(height)) => {
                                                Some(ImageSource::Sized {
                                                    image: img.id,
                                                    width,
                                                    height,
                                                })
                                            }
                                            _ => Some(ImageSource::Fill(img.id)),
                                        }
                                    }
                                    _ => None,
                                };
                                image_urls.push(u);
                                source
                            }
                            ImageHref::Element(e) => {
                                let external =
                                    crate::inline_svg::External::new(&doc.svg_resources, &doc.url);
                                let markup = crate::inline_svg::fe_image_markup(
                                    &dom, &doc.data, e, &base, &external,
                                );
                                let hash = {
                                    use std::hash::{Hash, Hasher};
                                    let mut h = std::collections::hash_map::DefaultHasher::new();
                                    markup.hash(&mut h);
                                    h.finish()
                                };
                                fe_used.insert(hash);
                                let id = match doc.fe_images.get(&hash) {
                                    Some(id) => Some(*id),
                                    None => crate::svg::SvgImage::parse(markup.as_bytes())
                                        .ok()
                                        .map(|img| {
                                            let id = self.svg.add(&self.images, img);
                                            doc.fe_images.insert(hash, id);
                                            id
                                        }),
                                };
                                id.map(ImageSource::Fill)
                            }
                        };
                        filter.set_image_source(i, source);
                    }
                    refs.push((url, Some(Arc::new(filter))));
                }
                let entry = doc.filter_references.get_mut(&n).expect("a user");
                if entry.as_deref() == Some(&refs) {
                    continue;
                }
                *entry = Some(Arc::new(refs));
                doc.changes.restyled.push(ElementChange::reconstruct(n));
            }
            // (an element image no filter draws any more is dropped)
            let images = &self.images;
            let svg = &mut self.svg;
            doc.fe_images.retain(|hash, id| {
                let keep = fe_used.contains(hash);
                if !keep {
                    svg.remove(images, *id);
                }
                keep
            });
            for url in image_urls {
                doc.filter_image_urls.insert(url.clone());
                Self::request_image_from(doc, &url, false);
            }
        }
        for (url, affects_boxes) in css_urls {
            if affects_boxes {
                doc.box_images.insert(url.clone());
            }
            Self::request_image_from(doc, &url, true);
        }
        for (n, chosen) in img_changes {
            let state = match &chosen {
                None => ImageState::Failed,
                // (a lazy image waits for the page to request its source, a lazy video's poster
                // for the page to let it load)
                Some((url, _))
                    if !doc.images_by_url.contains_key(url)
                        && (lazy_img(&dom, n)
                            || (lazy_video(&dom, n) && !doc.posters_released.contains(&n))) =>
                {
                    ImageState::Pending
                }
                Some((url, density)) => {
                    Self::request_image(doc, url);
                    let state = doc.images_by_url[url.as_str()];
                    Self::img_state_of(state, *density)
                }
            };
            // (one that had this image already: its load is over)
            if !matches!(state, ImageState::Pending)
                && (dom.is_html_element_named(n, &local_name!("object"))
                    || dom.is_html_element_named(n, &local_name!("embed")))
            {
                doc.embed_settled
                    .push((n, !matches!(state, ImageState::Failed)));
            }
            doc.img_src.insert(n, chosen);
            doc.img_state.insert(n, state);
            doc.changes.restyled.push(ElementChange::reconstruct(n));
        }
    }

    fn request_image(doc: &mut Doc, url: &str) {
        Self::request_image_from(doc, url, false)
    }

    /// [`Self::request_image`]; `css` for an image a style property refers to.
    fn request_image_from(doc: &mut Doc, url: &str, css: bool) {
        if doc.images_by_url.contains_key(url) {
            return;
        }
        match Url::parse(url) {
            Ok(u) => {
                doc.images_by_url.insert(url.to_owned(), UrlImage::Fetching);
                if css {
                    doc.fetcher.start_css_image(u);
                } else {
                    doc.fetcher.start(u, Purpose::Image);
                }
                doc.requests += 1;
                doc.counts.images += 1;
            }
            Err(_) => {
                doc.images_by_url.insert(url.to_owned(), UrlImage::Failed);
            }
        }
    }

    /// The document `el`'s navigable shows (`None`: it has none any more), for an `object` or
    /// `embed`: an SVG document's root gives the element its natural dimensions.
    pub fn set_embedded_document(&mut self, el: NodeId, document: Option<NodeId>) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        let old = match document {
            Some(d) => doc.embedded_documents.insert(el, d),
            None => doc.embedded_documents.remove(&el),
        };
        if old != document && doc.embedded_natural.remove(&el).flatten().is_some() {
            doc.needs_restyle = true;
            doc.changes.restyled.push(ElementChange::reconstruct(el));
        }
    }

    /// Before a layout: an `object` or `embed` whose SVG document's root now gives other natural
    /// dimensions (it was parsed, its attributes changed, or the document went) builds its box
    /// again.
    fn check_embedded_documents(&mut self) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        if doc.embedded_documents.is_empty() && doc.embedded_natural.is_empty() {
            return;
        }
        let dom_rc = doc.dom.clone();
        let dom = dom_rc.borrow();
        let mut changed: Vec<NodeId> = Vec::new();
        for (&el, &d) in &doc.embedded_documents {
            let now = (dom.is_alive(el) && dom.is_alive(d))
                .then(|| crate::source::svg_document_root(&dom, d))
                .flatten()
                .map(|root| crate::source::svg_root_natural(&dom, root));
            if doc.embedded_natural.get(&el).copied().flatten() != now {
                changed.push(el);
                doc.embedded_natural.insert(el, now);
            }
        }
        doc.embedded_natural
            .retain(|el, _| doc.embedded_documents.contains_key(el));
        for el in changed {
            if dom.is_alive(el) {
                doc.needs_restyle = true;
                doc.changes.restyled.push(ElementChange::reconstruct(el));
            }
        }
    }

    /// `el`'s navigable received a response of an image type: `el` shows the image itself (HTML
    /// §4.8.7), fetched and fitted as an image.
    pub fn set_embed_type(&mut self, el: NodeId, essence: &str) {
        if let Some(doc) = self.doc.as_deref_mut() {
            doc.embed_types.insert(el, essence.to_owned());
            doc.discover_nodes.insert(el);
            doc.needs_restyle = true;
            doc.changes.restyled.push(ElementChange::reconstruct(el));
        }
    }

    /// The `object` and `embed` elements whose response (fetched as an image) made them show a
    /// document: the page makes their navigables.
    pub fn take_embed_redecided(&mut self) -> Vec<(NodeId, String)> {
        self.doc
            .as_deref_mut()
            .map_or_else(Vec::new, |d| std::mem::take(&mut d.embed_redecided))
    }

    /// The `object` and `embed` elements whose image finished loading, and whether it did.
    pub fn take_embed_settled(&mut self) -> Vec<(NodeId, bool)> {
        self.doc
            .as_deref_mut()
            .map_or_else(Vec::new, |d| std::mem::take(&mut d.embed_settled))
    }

    /// HTML "update a `style` block" step 9 and the `link` element's "process the linked
    /// resource" for a style sheet: whether a sheet made since it was last reported still waits
    /// for its critical subresources (its `@import`s, at any depth); such a sheet delays its
    /// document's load event. (Inlined: the page asks after every batch of DOM changes.)
    #[inline]
    pub fn sheets_pending(&self) -> bool {
        self.doc
            .as_ref()
            .is_some_and(|d| !d.sheets_awaiting.is_empty())
    }

    /// The owners whose sheet and critical subresources have all been obtained (see
    /// [`Renderer::sheets_pending`]), and whether every one of them loaded (`Some(true)`: `load`)
    /// or not (`Some(false)`: `error`). An owner that lost its sheet meanwhile is reported with
    /// `None` (no event).
    pub fn take_sheets_settled(&mut self) -> Vec<(NodeId, Option<bool>)> {
        let Some(d) = self.doc.as_deref_mut() else {
            return Vec::new();
        };
        if d.sheets_awaiting.is_empty() {
            return Vec::new();
        }
        let entries = d.imports.entries.lock().unwrap();
        let mut settled = Vec::new();
        let mut seen = std::collections::HashSet::new();
        d.sheets_awaiting.retain(|&owner| {
            if !seen.insert(owner) {
                return false;
            }
            let Some(sheet) = d.engine.sheet_of(owner) else {
                settled.push((owner, None));
                return false;
            };
            match import_tree_state(&entries, &sheet) {
                Some(ok) => {
                    settled.push((owner, Some(ok)));
                    false
                }
                None => true,
            }
        });
        settled
    }

    /// A sheet's rules changed through the CSSOM (`insertRule`, `deleteRule`, a rule's declarations,
    /// `disabled`): the cascade is rebuilt at the next restyle.
    pub fn stylesheets_changed(&mut self) {
        if let Some(d) = self.doc.as_deref_mut() {
            d.engine.sheets_changed();
            d.needs_restyle = true;
        }
    }

    /// Adds a user-origin style sheet made of `css` (the content blocker's cosmetic filters). It is
    /// none of the document's sheets: the CSSOM does not list it.
    pub fn add_user_css(&mut self, css: &str) {
        if let Some(d) = self.doc.as_deref_mut() {
            d.engine.add_user_css(&d.data, css);
            d.needs_restyle = true;
        }
    }

    /// Parses a constructed `CSSStyleSheet` of this document (CSSOM §6.4: its constructor
    /// document), under this document's style lock so it can be adopted here.
    pub fn parse_constructed_sheet(
        &self,
        css: &str,
        base: &Url,
        media: &str,
    ) -> Option<snowlark_style::Sheet> {
        let d = self.doc.as_deref()?;
        Some(snowlark_style::parse_with_lock_of(
            css, base, media, &d.data,
        ))
    }

    /// CSSOM §6.4: the adopted style sheets of `scope` (this document or one of its shadow roots)
    /// are now `sheets`, after its own sheets in the cascade. Sheets parsed under another
    /// document's lock cannot join this cascade and are left out.
    pub fn set_adopted_sheets(&mut self, scope: NodeId, sheets: Vec<snowlark_style::Sheet>) {
        let Some(d) = self.doc.as_deref_mut() else {
            return;
        };
        let sheets: Vec<_> = sheets
            .into_iter()
            .filter(|s| s.uses_lock_of(&d.data))
            .collect();
        let dom_rc = d.dom.clone();
        let dom = dom_rc.borrow();
        d.engine.set_adopted_sheets(&dom, &d.data, scope, sheets);
        d.needs_restyle = true;
    }

    /// Whether a restyle is due (a sheet arrived, the DOM changed).
    pub fn needs_restyle(&self) -> bool {
        self.doc.as_ref().is_some_and(|d| d.needs_restyle)
    }

    /// Marks the document as needing a restyle and a box tree rebuild (after DOM changes the caller
    /// made through [`Renderer::dom_mut`]).
    pub fn invalidate(&mut self) {
        if let Some(d) = &mut self.doc {
            d.needs_restyle = true;
            d.needs_build = true;
        }
    }

    /// Reports DOM changes to the style engine: the reactions the page layer drained from the DOM
    /// (`Dom::take_reactions`) after changing it through [`Renderer::dom_mut`]. The elements they
    /// affect restyle at the next restyle, where transitions can start.
    ///
    /// Insertions and removals regenerate the boxes of the parent's container at the next layout;
    /// character data changes update text boxes in place; attribute changes restyle (`class`, `id`,
    /// `style`, `data-*`, `aria-*`) or, for attributes that boxes read (`colspan`, `value`, `src`,
    /// form control state...), regenerate the element's boxes.
    pub fn dom_changed(&mut self, reactions: &[Reaction]) {
        // (descendants of inserted or removed nodes: see `wants_descendant_reaction`)
        if reactions.is_empty() {
            return;
        }
        let Some(d) = self.doc.as_deref_mut() else {
            return;
        };
        let dom_rc = d.dom.clone();
        let dom = dom_rc.borrow();
        let document = d.document;
        let embeds = !d.embed_types.is_empty();
        // (references: a batch that also changed other documents is not copied)
        let reactions: smallvec::SmallVec<[&Reaction; 16]> = reactions
            .iter()
            .filter(|r| renders_reaction(&dom, document, embeds, r))
            .collect();
        if reactions.is_empty() {
            d.engine.batch_done();
            return;
        }
        let reactions = &reactions[..];
        d.engine
            .process_reactions(&dom, &mut d.data, reactions.iter().copied());
        d.needs_restyle = true;
        let is = |n: NodeId, name: &snowlark_dom::LocalName| {
            dom.is_html_element_named(n, name)
                || (*name == local_name!("style") && snowlark_style::is_style_element(&dom, n))
        };
        // What the next discovery of subresources looks at again (see `discover`).
        for r in reactions {
            match r {
                Reaction::Inserted {
                    node,
                    connected: true,
                    root,
                    ..
                } => {
                    // A new `link` (inserted descendants have reactions of their own, see
                    // `wants_descendant_reaction`), a new base URL, or, before the first rendering,
                    // anything (a parser-inserted `style` before the body blocks rendering): only
                    // then can the walk in `discover_links` find something new. (A walk per
                    // insertion cost the document's size on every DOM-building update.)
                    if !d.rendered
                        || is(*node, &local_name!("link"))
                        || is(*node, &local_name!("base"))
                        || dom.is_style_element(*node)
                    {
                        d.links_dirty = true;
                    }
                    *d.base.get_mut() = None;
                    if *root {
                        d.discover_roots.push(*node);
                    }
                    // (a `source` chooses for its `picture`'s `img`)
                    if let Some(p) = dom.parent(*node).filter(|&p| dom.is_element(p)) {
                        if is(*node, &local_name!("source")) {
                            d.discover_roots.push(p);
                        } else {
                            d.discover_nodes.insert(p);
                        }
                    }
                }
                Reaction::Removed {
                    node,
                    old_parent,
                    was_connected: true,
                    ..
                } => {
                    *d.base.get_mut() = None;
                    if let Some(p) = old_parent.filter(|&p| dom.is_alive(p) && dom.is_element(p)) {
                        if is(*node, &local_name!("source")) {
                            d.discover_roots.push(p);
                        } else {
                            d.discover_nodes.insert(p);
                        }
                    }
                }
                Reaction::AttributeChanged { element, .. } => {
                    // (a new resource for an `object` or `embed`: its response decides again)
                    // (its keys hash with SipHash: no hashing when there are none)
                    if !d.embed_types.is_empty() {
                        d.embed_types.remove(element);
                    }
                    if is(*element, &local_name!("link")) || dom.is_style_element(*element) {
                        d.links_dirty = true;
                    }
                    if is(*element, &local_name!("base")) {
                        *d.base.get_mut() = None;
                    }
                    // (a disconnected element is looked at with its subtree when it is inserted:
                    // see below)
                    if dom.is_connected(*element) {
                        d.discover_nodes.insert(*element);
                    }
                    if is(*element, &local_name!("source")) {
                        if let Some(p) = dom.parent_element(*element) {
                            d.discover_roots.push(p);
                        }
                    }
                }
                Reaction::ChildrenChanged { parent } if dom.is_connected(*parent) => {
                    d.discover_nodes.insert(*parent);
                }
                _ => {}
            }
        }
        // Changes inside a disconnected subtree (an element built before it is inserted, as most
        // script-built content is) are neither discovered nor kept for the box tree: nothing of it
        // is rendered, and its insertion (`discover_roots`, the new parent's children) looks at the
        // whole subtree then. Keeping them made both sets grow by every element built between two
        // rendering updates, and, once generated content was seen, any text entry rebuilt every box.
        let (mut inserted, mut structural) = std::mem::take(&mut d.batch_sets);
        // The nodes this batch inserted, so each insertion finds out in constant time whether its
        // parent came with it.
        for r in reactions {
            if let Reaction::Inserted { node, .. } = r {
                inserted.insert(*node);
            }
        }
        // (an element whose ancestor was inserted in this batch is new too: a reaction filter can
        // leave descendants out)
        let new_in_batch = |el: NodeId| {
            !inserted.is_empty() && dom.inclusive_ancestors(el).any(|a| inserted.contains(&a))
        };
        let mut relink: Vec<(NodeId, String, Url)> = Vec::new();
        let mut style_blocks: Vec<NodeId> = Vec::new();
        for r in reactions {
            match r {
                Reaction::Inserted {
                    node,
                    connected,
                    root,
                    ..
                } => {
                    if *connected && dom.is_style_element(*node) {
                        style_blocks.push(*node);
                    }
                    if !*connected || !*root {
                        continue;
                    }
                    if let Some(p) = dom.parent(*node) {
                        if !inserted.contains(&p) && structural.insert(p) {
                            d.changes.children.push(p);
                        }
                    }
                }
                Reaction::Removed {
                    node,
                    old_parent,
                    was_connected,
                    ..
                } => {
                    // HTML §4.2.6 "update a style block" and the link's sheet removal: a removed
                    // owner's sheets go.
                    if dom.is_style_element(*node) || is(*node, &local_name!("link")) {
                        d.engine.remove_sheets_of(&d.data, *node);
                        if d.links.remove(node).is_some() {
                            d.render_blocking.remove(node);
                        }
                    }
                    if let (Some(p), true) = (old_parent, *was_connected) {
                        if structural.insert(*p) {
                            d.changes.children.push(*p);
                        }
                    }
                }
                Reaction::AttributeChanged {
                    element,
                    local_name,
                    namespace,
                    ..
                } => {
                    let name: &str = local_name;
                    if namespace.is_none() {
                        if dom.is_style_element(*element) && matches!(name, "media" | "type") {
                            style_blocks.push(*element);
                        }
                        // HTML "blocking" attribute: removing `render` from a script-inserted
                        // element's (not implicitly potentially render-blocking) unblocks it.
                        if name == "blocking"
                            && dom.element_flags(*element)
                                & snowlark_dom::element_flags::PARSER_INSERTED
                                == 0
                            && !dom
                                .get_attribute(*element, "blocking")
                                .is_some_and(|b| has_token(b, "render"))
                        {
                            d.render_blocking.remove(element);
                            d.import_blockers.retain(|n| n != element);
                        }
                        if is(*element, &local_name!("link")) && name == "media" {
                            if let Some(l) = d.links.get(element) {
                                if let Some((text, url)) = l.text.clone() {
                                    // (parsed again below, once the DOM borrow is released)
                                    relink.push((*element, text, url));
                                }
                            }
                        }
                    }
                    // (`slot` moves the element to another slot of its parent's shadow tree)
                    let styling = namespace.is_none()
                        && (matches!(name, "class" | "id" | "style")
                            || !d.generated_seen
                                && (name.starts_with("data-")
                                    || name.starts_with("aria-")
                                    || name != "slot" && !boxes_read_attributes(&dom, *element)));
                    // (an element inserted in this batch gets its boxes from the insertion; a
                    // disconnected one has none)
                    if !styling && dom.is_connected(*element) && !new_in_batch(*element) {
                        d.changes.restyled.push(ElementChange::attributes(*element));
                    }
                }
                Reaction::ChildrenChanged { parent } if dom.is_style_element(*parent) => {
                    style_blocks.push(*parent);
                }
                _ => {}
            }
        }
        for r in reactions {
            if let Reaction::ChildrenChanged { parent } = r {
                if dom.is_connected(*parent)
                    && !structural.contains(parent)
                    && d.text_changed.insert(*parent)
                {
                    d.changes.text.push(*parent);
                }
            }
        }
        // (kept, emptied, for the next call; a big batch's sets are not)
        for set in [&mut inserted, &mut structural] {
            if set.capacity() > 1024 {
                *set = Default::default();
            } else {
                set.clear();
            }
        }
        d.batch_sets = (inserted, structural);
        style_blocks.dedup();
        for el in style_blocks {
            if update_style_block(&mut d.engine, &d.data, d.style_check.as_ref(), &dom, el) {
                d.sheets_awaiting.push(el);
            }
        }
        // A `<link>` the parser inserts is found at once, so that it blocks rendering while the
        // body is still null (render-blocked documents get no restyle that would find it).
        let new_link = !d.rendered
            && reactions.iter().any(|r| {
                matches!(r, Reaction::Inserted { node, connected: true, .. } if is(*node, &local_name!("link")))
            });
        drop(dom);
        for (owner, text, url) in relink {
            Self::apply_link_sheet(d, owner, &text, &url);
        }
        if new_link {
            self.discover_links();
        }
    }

    /// The DOM, to change (the caller then reports the changes with [`Renderer::dom_changed`], or
    /// to the style engine itself and calls [`Renderer::invalidate`]).
    pub fn dom_mut(&mut self) -> Option<RefMut<'_, Dom>> {
        self.doc.as_ref().map(|d| d.dom.borrow_mut())
    }

    /// Brings computed styles up to date and discovers the subresources they need: a style change
    /// event (CSS Transitions 1 §3) at the timeline's current time, which does not move. This is what
    /// script-driven style reads (`getComputedStyle`) force; it ignores render-blocking, and
    /// transitions can start from it, as the spec counts every style change event, not only those
    /// of rendering updates. Frames come from [`Renderer::update_rendering`].
    pub fn restyle(&mut self) -> RestyleStats {
        self.restyle_inner(None)
    }

    /// [`Renderer::restyle`] for a script's read: see [`Renderer::layout_for_script`].
    pub fn restyle_for_script(&mut self) -> RestyleStats {
        self.script_read = true;
        let stats = self.restyle();
        self.script_read = false;
        stats
    }

    /// Makes the images of the inline svgs that script reads left waiting.
    pub fn make_waiting_svgs(&mut self) {
        if self
            .doc
            .as_deref()
            .is_some_and(|d| !d.svgs_waiting.is_empty())
        {
            self.discover(false);
        }
    }

    /// The restyle, with the timeline moved to `now` (seconds) first when given (a rendering update).
    fn restyle_inner(&mut self, now: Option<f64>) -> RestyleStats {
        let stats = self.restyle_pass(now);
        // Finding the document's faces changed (a layer reordered, a face removed) restyles at
        // once, so that this update already lays text out with the faces that now apply.
        if self.doc.as_deref().is_some_and(|d| d.faces_restyle) {
            if let Some(d) = self.doc.as_deref_mut() {
                d.faces_restyle = false;
            }
            let again = self.restyle_pass(now);
            return RestyleStats {
                restyled: stats.restyled + again.restyled,
                ..again
            };
        }
        stats
    }

    fn restyle_pass(&mut self, now: Option<f64>) -> RestyleStats {
        let t = Instant::now();
        let Some(doc) = self.doc.as_deref_mut() else {
            return RestyleStats::default();
        };
        let dom_rc = doc.dom.clone();
        let dom = dom_rc.borrow();
        let stats = match now {
            Some(now) => doc.engine.restyle_at(&dom, &mut doc.data, now),
            None => doc.engine.restyle(&dom, &mut doc.data),
        };
        doc.needs_restyle = false;
        drop(dom);
        self.stats.restyled += u64::from(stats.restyled);
        self.stats.traversed += u64::from(stats.traversed);
        self.stats.matched += u64::from(stats.matched);
        self.stats.shared += u64::from(stats.shared);
        self.discover(stats.restyled > 0);
        self.start_missing_imports();
        self.timings.style_ms += ms(t);
        self.timings.restyles += 1;
        stats
    }

    /// HTML "render-blocked": the document's render-blocking element set is non-empty (here: a
    /// render-blocking `<link>`'s sheet, or an `@import` of a `<head>` sheet, is in flight) or the
    /// document allows adding render-blocking elements (a text/html document without a body), and the
    /// render-blocking timeout has not passed since the time origin. HTML's "update the rendering"
    /// skips a render-blocked document. Once the document has rendered it is never render-blocked
    /// again: the set cannot grow once the body exists and time only moves on (a script removing the
    /// body is not taken as blocking again).
    pub fn render_blocked(&self) -> bool {
        let Some(doc) = self.doc.as_deref() else {
            return false;
        };
        if doc.rendered || doc.origin.elapsed() >= self.render_blocking_timeout {
            return false;
        }
        let dom = doc.dom.borrow();
        // "Whenever a render-blocking element el becomes browsing-context disconnected ... unblock
        // rendering on el."
        let links = doc
            .render_blocking
            .iter()
            .any(|&n| dom.is_alive(n) && dom.is_connected(n));
        // (the `@import`s of a render-blocking sheet block while its owner is connected)
        let imports = doc
            .import_blockers
            .iter()
            .any(|&n| dom.is_alive(n) && dom.is_connected(n))
            && doc
                .imports
                .entries
                .lock()
                .unwrap()
                .values()
                .any(|e| matches!(e, ImportEntry::Pending));
        let allows_adding = dom.is_html_document(doc.document) && dom.body(doc.document).is_none();
        links || imports || allows_adding
    }

    /// The document timeline's current time.
    pub fn timeline_time(&self) -> Duration {
        self.doc.as_deref().map_or(Duration::ZERO, |d| {
            Duration::from_secs_f64(d.engine.timeline_time().max(0.0))
        })
    }

    /// Whether a CSS transition or animation is in its delay or active phase, so the next frame
    /// looks different: the caller schedules another [`Renderer::update_rendering`].
    /// The CSS animations (by name, one per element animated) and the number of transitions that
    /// keep the document's rendering updating (profiling).
    pub fn ticking_animations(&self) -> (Vec<String>, usize) {
        self.doc
            .as_deref()
            .map(|d| d.engine.ticking_animations())
            .unwrap_or_default()
    }

    /// Whether the next frame looks different or has events to send: an animation or transition
    /// the page runs itself is in its delay or active phase, or a CSS animation event may come.
    /// Animations a compositor runs ask only for the frames at which they change
    /// ([`Renderer::compositor_next_change`]).
    pub fn animating(&self) -> bool {
        self.doc
            .as_deref()
            .is_some_and(|d| d.engine.animations_need_page_ticks() || d.engine.css_events_pending())
    }

    /// The timeline time (s) of the next change of an animation a compositor runs that the page
    /// must service (Blink's `DocumentTimeline::ScheduleNextService`): it starts, ends, or, when
    /// `iteration_events` (a listener for `animationiteration` exists), iterates.
    pub fn compositor_next_change(&self, iteration_events: bool) -> Option<f64> {
        self.doc
            .as_deref()
            .and_then(|d| d.engine.compositor_next_change(iteration_events))
    }

    /// Whether a scan for CSS animation events is due ([`snowlark_style::StyleEngine::css_events_due`]).
    pub fn css_events_due(&self) -> bool {
        self.doc
            .as_deref()
            .is_some_and(|d| d.engine.css_events_due())
    }

    /// Whether a compositor runs the script animation effect `id`
    /// ([`snowlark_style::StyleEngine::script_effect_on_compositor`]).
    pub fn script_effect_on_compositor(&self, id: u64) -> bool {
        self.doc
            .as_deref()
            .is_some_and(|d| d.engine.script_effect_on_compositor(id))
    }

    /// Whether the CSS animation or transition `key` names runs on a compositor.
    pub fn css_on_compositor(&self, key: &snowlark_style::web_animations::CssKey) -> bool {
        self.doc
            .as_deref()
            .is_some_and(|d| d.engine.css_key_on_compositor(key))
    }

    /// Moves the timeline to `now` (s) for a script's read between rendering updates (Blink's
    /// `DocumentAnimations::UpdateAnimationTimingIfNeeded`): while animations run, their values,
    /// states and events follow the time script sees (the page's animation clock), also when no
    /// rendering update has run for them (a compositor runs them). The restyle is a style change
    /// event at that time. The first rendering update too: its animation events and frame
    /// callbacks run at the frame's time before its restyle (before it, the page's time is this
    /// timeline's, so nothing moves).
    pub fn advance_timeline_for_script(&mut self, now: f64) {
        let Some(doc) = self.doc.as_deref() else {
            return;
        };
        if now <= doc.engine.timeline_time() || !doc.engine.animations_active() {
            return;
        }
        self.restyle_inner(Some(now));
    }

    /// The time (s) this document's clock reads when the monotonic clock reads `monotonic`: the page
    /// gives its own time so all its documents and its animation frame callbacks share one; the
    /// clock holds or replaces it (`--render --animation-time`).
    pub fn clock_at(&self, monotonic: f64) -> f64 {
        match self.clock {
            TimelineClock::Monotonic => monotonic,
            TimelineClock::MonotonicUntil(limit) => monotonic.min(limit.as_secs_f64()),
            TimelineClock::Manual(t) => t.as_secs_f64(),
        }
    }

    /// The document's time origin: its timeline and clocks count from `origin` (the page's, so its
    /// animation frame callbacks, `performance.now()` and the timeline agree) instead of from when
    /// the document was attached.
    pub fn set_time_origin(&mut self, origin: Instant) {
        if let Some(doc) = self.doc.as_deref_mut() {
            doc.timeline_origin = origin;
        }
    }

    /// The time (s) the next rendering update moves the timeline to (instead of the clock's time).
    pub fn set_frame_time(&mut self, now: f64) {
        self.frame_time = Some(now);
    }

    /// Web Animations: the script animations in effect in this document, sampled (see
    /// [`snowlark_style::StyleEngine::set_script_effects`]). Returns the previous samples (a buffer).
    /// Whether script animations (Web Animations) affect this document's style.
    pub fn has_script_effects(&self) -> bool {
        self.doc
            .as_deref()
            .is_some_and(|d| d.engine.has_script_effects())
    }

    pub fn set_script_effects(
        &mut self,
        samples: Vec<snowlark_style::web_animations::EffectSample>,
    ) -> Vec<snowlark_style::web_animations::EffectSample> {
        let Some(doc) = self.doc.as_deref_mut() else {
            return samples;
        };
        if samples.is_empty() && !doc.engine.has_script_effects() {
            return samples;
        }
        let dom_rc = doc.dom.clone();
        let dom = dom_rc.borrow();
        let old = doc.engine.set_script_effects(&dom, &mut doc.data, samples);
        doc.needs_restyle = true;
        old
    }

    /// The events of this document's CSS animations and transitions whose phase changed by timeline
    /// time `now` (s) (see [`snowlark_style::StyleEngine::css_animation_events`]).
    pub fn css_animation_events(
        &mut self,
        now: f64,
    ) -> Vec<snowlark_style::web_animations::CssAnimationEvent> {
        let Some(doc) = self.doc.as_deref_mut() else {
            return Vec::new();
        };
        let dom_rc = doc.dom.clone();
        let dom = dom_rc.borrow();
        doc.engine.css_animation_events(&dom, &doc.data, now)
    }

    /// This document's CSS animations and transitions at timeline time `now` (s) (see
    /// [`snowlark_style::StyleEngine::css_timings`]).
    pub fn css_timings(&self, now: f64) -> Vec<snowlark_style::web_animations::CssTiming> {
        let Some(doc) = self.doc.as_deref() else {
            return Vec::new();
        };
        let dom = doc.dom.borrow();
        doc.engine.css_timings(&dom, now)
    }

    /// The keyframes of an element's CSS animation (see
    /// [`snowlark_style::StyleEngine::css_animation_keyframes`]).
    pub fn css_animation_keyframes(
        &self,
        element: NodeId,
        name: &str,
    ) -> Vec<snowlark_style::web_animations::RawKeyframe> {
        let Some(doc) = self.doc.as_deref() else {
            return Vec::new();
        };
        let dom = doc.dom.borrow();
        doc.engine
            .css_animation_keyframes(&dom, &doc.data, element, name)
    }

    /// Script controls a CSS animation or transition (see [`snowlark_style::StyleEngine::css_control`]).
    pub fn css_control(
        &mut self,
        key: &snowlark_style::web_animations::CssKey,
        now: f64,
        local: Option<f64>,
        paused: bool,
    ) -> bool {
        let Some(doc) = self.doc.as_deref_mut() else {
            return false;
        };
        let dom_rc = doc.dom.clone();
        let dom = dom_rc.borrow();
        let found = doc
            .engine
            .css_control(&dom, &mut doc.data, key, now, local, paused);
        if found {
            doc.needs_restyle = true;
        }
        found
    }

    /// The time the clock gives the timeline now, in seconds.
    fn clock_time(&self) -> f64 {
        let elapsed = self
            .doc
            .as_deref()
            .map_or(Duration::ZERO, |d| d.timeline_origin.elapsed());
        match self.clock {
            TimelineClock::Monotonic => elapsed,
            TimelineClock::MonotonicUntil(limit) => elapsed.min(limit),
            TimelineClock::Manual(t) => t,
        }
        .as_secs_f64()
    }

    /// The style part of HTML's "update the rendering" for this document: nothing while it is
    /// render-blocked (returns false); otherwise "update animations and send events" (the timeline
    /// moves to the clock's time) and the restyle, a style change event at that time.
    fn update_style_for_rendering(&mut self) -> bool {
        // (the head's links block the first rendering update: they are found before it)
        if self.doc.as_deref().is_some_and(|d| !d.rendered) {
            self.discover_links();
        }
        if self.render_blocked() {
            return false;
        }
        let now = self.frame_time.take().unwrap_or_else(|| self.clock_time());
        let Some(doc) = self.doc.as_deref_mut() else {
            return false;
        };
        let first = !doc.rendered;
        doc.rendered = true;
        if first || doc.needs_restyle || doc.engine.animations_active() {
            self.restyle_inner(Some(now));
        } else {
            // Nothing to restyle: the timeline still moves.
            let dom_rc = doc.dom.clone();
            doc.engine.restyle_at(&dom_rc.borrow(), &mut doc.data, now);
        }
        self.make_waiting_svgs();
        self.advance_image_animations();
        true
    }

    /// Animated images show the frame of the timeline's time (the frames of an animation decoded
    /// as it shows only while it is in the view: [`Renderer::set_image_animation_view`]).
    fn advance_image_animations(&mut self) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        if doc.animations.is_empty() {
            return;
        }
        let now = doc.engine.timeline_time();
        let view = self.animation_view;
        for (&id, a) in &mut doc.animations {
            let Some(start) = a.start else {
                continue;
            };
            let index = a.animation.timing.at((now - start) * 1000.0).index;
            if index == a.frame || a.failed {
                continue;
            }
            match &a.animation.encoded {
                None => self.images.set_frame(id, index),
                Some(bytes) => {
                    if !a.shown(view) {
                        continue;
                    }
                    if a.decoder.is_none() {
                        a.decoder = snowlark_paint::image::FrameDecoder::new(bytes.clone()).ok();
                    }
                    match a.decoder.as_mut().map(|d| d.frame(index)) {
                        Some(Ok(frame)) => self.images.show_frame(id, frame),
                        _ => {
                            a.failed = true;
                            continue;
                        }
                    }
                }
            }
            a.frame = index;
        }
    }

    /// The part of the document (document coordinates) the user sees, for animated images: the
    /// frames of one decoded as it shows ([`snowlark_paint::image::ANIMATION_FRAMES_BYTES`]) are
    /// decoded only while it is painted there. `None` (the default): everything counts as seen; an
    /// empty rectangle: nothing does (a hidden tab).
    pub fn set_image_animation_view(&mut self, view: Option<snowlark_paint::Rect>) {
        self.animation_view = view;
    }

    /// When (timeline time, s) the next animated image painted in `view` (as
    /// [`Renderer::set_image_animation_view`] takes it) changes frame. Images painted nowhere in
    /// it are paused as far as rendering updates go: Chrome too stops animating images out of view.
    fn next_image_frame_time(&self, view: Option<snowlark_paint::Rect>) -> Option<f64> {
        let doc = self.doc.as_deref()?;
        let now = doc.engine.timeline_time();
        doc.animations
            .values()
            .filter(|a| !a.failed && a.shown(view))
            .filter_map(|a| {
                let start = a.start?;
                let next = a.animation.timing.at((now - start) * 1000.0).next?;
                Some(start + next / 1000.0)
            })
            .min_by(f64::total_cmp)
    }

    /// Whether an animated image painted in `view` shows another frame at the clock's time: a
    /// rendering update is due. (An SVG image with running animations changes with every frame of
    /// the page's own clock, as the document's CSS animations do: there is no next time to wait for.)
    pub fn image_frame_due(&self, view: Option<snowlark_paint::Rect>) -> bool {
        let time = self.clock_time();
        self.svg.frame_due(view, time)
            || self.next_image_frame_time(view).is_some_and(|t| time >= t)
    }

    /// When an animated image painted in `view` shows its next frame (`None`: none will, or the
    /// clock does not move by itself).
    pub fn next_image_frame(&self, view: Option<snowlark_paint::Rect>) -> Option<Instant> {
        let t = self.next_image_frame_time(view)?;
        let doc = self.doc.as_deref()?;
        match self.clock {
            TimelineClock::Monotonic => {}
            TimelineClock::MonotonicUntil(limit) if t <= limit.as_secs_f64() => {}
            _ => return None,
        }
        Some(doc.timeline_origin + Duration::from_secs_f64(t.max(0.0)))
    }

    /// HTML's "update the rendering" for this document, once per frame: `None` while the document is
    /// render-blocked (nothing is shown and its timeline does not move); otherwise the timeline moves
    /// to the clock's time (transitions and animations advance), styles are brought up to date and
    /// the document is laid out in `viewport`. Paint the frame with [`Renderer::paint`] after a
    /// `Some`, and ask for another frame while [`Renderer::animating`].
    pub fn update_rendering(&mut self, viewport: Size) -> Option<Arc<Fragment<RenderStyle>>> {
        self.set_viewport(viewport);
        if !self.update_style_for_rendering() {
            return None;
        }
        self.rendering_updates += 1;
        self.layout(viewport)
    }

    /// How many rendering updates ran (not render-blocked): after one, the `<img>` elements of the
    /// document as it was then have their requests.
    pub fn rendering_updates(&self) -> u64 {
        self.rendering_updates
    }

    /// Whether an `<img>` element's image is still being fetched or decoded (HTML: it delays its
    /// document's load event).
    pub fn images_pending(&self) -> bool {
        self.doc.as_ref().is_some_and(|d| {
            d.img_src.iter().any(|(node, src)| {
                src.as_ref().is_some_and(|(url, _)| {
                    !d.img_no_load_delay.contains(node)
                        && matches!(d.images_by_url.get(url.as_str()), Some(UrlImage::Fetching))
                })
            })
        })
    }

    /// Whether any image the document requested, for an element or for its styles (backgrounds,
    /// list markers, `content`, borders, masks...), is still being fetched or decoded.
    pub fn any_image_pending(&mut self) -> bool {
        self.make_waiting_svgs();
        self.doc.as_ref().is_some_and(|d| {
            // (SVG 2 §16.1.5: unresolved references of `use` elements hold the load event back too)
            d.svg_resources
                .values()
                .any(|r| matches!(r, crate::svg_resource::SvgResource::Pending))
                || {
                    // (the images only lazy loaded `img` elements requested do not count)
                    let lazy_only: HashSet<&str> = if d.img_no_load_delay.is_empty() {
                        HashSet::new()
                    } else {
                        let url_of = |n: &NodeId| {
                            d.img_src
                                .get(n)
                                .and_then(|s| s.as_ref())
                                .map(|(u, _)| u.as_str())
                        };
                        let delaying: HashSet<&str> = d
                            .img_src
                            .keys()
                            .filter(|n| !d.img_no_load_delay.contains(n))
                            .filter_map(url_of)
                            .collect();
                        d.img_no_load_delay
                            .iter()
                            .filter_map(url_of)
                            .filter(|u| !delaying.contains(u))
                            .collect()
                    };
                    d.images_by_url.iter().any(|(url, i)| {
                        matches!(i, UrlImage::Fetching) && !lazy_only.contains(url.as_str())
                    })
                }
        })
    }

    /// Changes the device pixel ratio (`resolution` media queries, `srcset` choices).
    pub fn set_device_pixel_ratio(&mut self, dpr: f32) {
        if self.config.device_pixel_ratio == dpr {
            return;
        }
        self.config.device_pixel_ratio = dpr;
        self.media_generation += 1;
        if let Some(doc) = self.doc.as_deref_mut() {
            doc.engine.set_device(&doc.data, self.config.clone());
            doc.needs_restyle = true;
        }
    }

    /// Changes the preferred color scheme (`prefers-color-scheme` media queries).
    pub fn set_color_scheme(&mut self, scheme: snowlark_style::ColorScheme) {
        if self.config.color_scheme == scheme {
            return;
        }
        self.config.color_scheme = scheme;
        self.media_generation += 1;
        if let Some(doc) = self.doc.as_deref_mut() {
            doc.engine.set_device(&doc.data, self.config.clone());
            doc.needs_restyle = true;
        }
    }

    /// The width (CSS px) of the classic vertical scroll bar the embedder shows when the document
    /// overflows the viewport vertically (0: overlay scroll bars). Layout then takes the viewport
    /// without it as the initial containing block (CSS Overflow 3 §3.1, CSSOM View's "viewport width
    /// excluding the size of a rendered scroll bar"); media queries and viewport units keep the whole
    /// viewport.
    pub fn set_scrollbar_width(&mut self, width: f32) {
        self.scrollbar_width = width.max(0.0);
    }

    /// Whether the embedder views PDFs inline (HTML "PDF viewer supported").
    pub fn set_pdf_viewer(&mut self, on: bool) {
        self.pdf_viewer = on;
    }

    /// Whether the last layout showed the viewport's classic vertical scroll bar.
    pub fn vertical_scrollbar(&self) -> bool {
        self.vertical_scrollbar_width() > 0.0
    }

    /// The width (CSS px) of the viewport's classic vertical scroll bar in the last layout: 0 when
    /// none shows, thinner than [`Renderer::set_scrollbar_width`]'s for the root's
    /// `scrollbar-width: thin` (CSS Scrollbars 1 §3).
    pub fn vertical_scrollbar_width(&self) -> f32 {
        match self.doc.as_deref() {
            Some(d) if self.scrollbar_width > 0.0 && d.vertical_scrollbar => {
                d.vertical_scrollbar_width
            }
            _ => 0.0,
        }
    }

    /// How the viewport's scroll bar is drawn, from the root element's style: CSS Scrollbars 1
    /// applies the root's `scrollbar-color` and `scrollbar-width` to the viewport (not the body's),
    /// and CSS Color Adjust 1 §2.4 has scroll bars follow the used color scheme (the root's, for
    /// the viewport's, as Chrome propagates it). `width` is the last layout's bar (0: none shows).
    pub fn viewport_scrollbar(&self) -> ViewportScrollbar {
        let width = self.vertical_scrollbar_width();
        let Some(values) = self.root_style() else {
            return ViewportScrollbar {
                width,
                ..ViewportScrollbar::default()
            };
        };
        ViewportScrollbar {
            width,
            color_scheme: crate::adapter::used_color_scheme(&values),
            colors: crate::adapter::scrollbar_colors(&values),
        }
    }

    /// CSS Color Adjust 1 §2.4: the root element's used color scheme sets the canvas surface's
    /// color, which shows where the canvas background (the root's or body's `background`) is
    /// transparent: white, or Chrome's #121212 (the `Canvas` system color) for a dark scheme.
    pub fn canvas_color(&self) -> Color {
        canvas_color(
            self.root_style()
                .map_or(snowlark_layout::style::ColorScheme::Light, |v| {
                    crate::adapter::used_color_scheme(&v)
                }),
        )
    }

    /// The color a viewer shows where the page is not drawn (yet): the canvas background's
    /// `background-color` (CSS Backgrounds 3 §2.11: the root's, or the body's) over the canvas
    /// surface's color, as of the last layout. Opaque.
    pub fn page_background(&self) -> Color {
        let surface = self.canvas_color();
        let color = self
            .doc
            .as_deref()
            .and_then(|d| d.layout.canvas_background_color());
        match color {
            Some(c) if c.a == 255 => c,
            Some(c) if c.a > 0 => {
                let a = c.a as f32 / 255.0;
                let mix = |over: u8, under: u8| {
                    (over as f32 * a + under as f32 * (1.0 - a)).round() as u8
                };
                Color::rgb(
                    mix(c.r, surface.r),
                    mix(c.g, surface.g),
                    mix(c.b, surface.b),
                )
            }
            _ => surface,
        }
    }

    /// The used color scheme of `element` as of the last restyle (`None` when it has no style).
    pub fn used_color_scheme(
        &self,
        element: NodeId,
    ) -> Option<snowlark_layout::style::ColorScheme> {
        let doc = self.doc.as_deref()?;
        doc.data
            .styles(element)
            .map(crate::adapter::used_color_scheme)
    }

    /// The preferred color scheme `prefers-color-scheme` and the used color schemes follow.
    pub fn color_scheme(&self) -> snowlark_style::ColorScheme {
        self.config.color_scheme
    }

    /// The document element's computed values.
    fn root_style(&self) -> Option<ServoArc<ComputedValues>> {
        let doc = self.doc.as_deref()?;
        let dom = doc.dom.borrow();
        let root = dom.document_element(doc.document)?;
        doc.data.styles_arc(root)
    }

    /// The viewport without the classic scroll bar the last layout showed (the initial containing
    /// block); `viewport` minus nothing before a layout.
    pub fn layout_viewport(&self, viewport: Size) -> Size {
        Size::new(
            (viewport.width - self.vertical_scrollbar_width()).max(0.0),
            viewport.height,
        )
    }

    /// Changes the viewport: media queries and viewport units are evaluated again at the next
    /// restyle.
    /// The output device's screen in CSS pixels (the top-level viewport; see
    /// [`DeviceConfig::screen`]).
    pub fn set_screen(&mut self, screen: (f32, f32)) {
        if self.config.screen == Some(screen) {
            return;
        }
        self.config.screen = Some(screen);
        self.media_generation += 1;
        if let Some(doc) = self.doc.as_deref_mut() {
            doc.engine.set_device(&doc.data, self.config.clone());
            doc.needs_restyle = true;
        }
    }

    pub fn set_viewport(&mut self, viewport: Size) {
        if self.config.viewport == (viewport.width, viewport.height) {
            return;
        }
        self.config.viewport = (viewport.width, viewport.height);
        self.media_generation += 1;
        if let Some(doc) = self.doc.as_deref_mut() {
            doc.engine.set_device(&doc.data, self.config.clone());
            doc.needs_restyle = true;
        }
    }

    /// Lays the document out in `viewport` (restyling first when due, as [`Renderer::restyle`] does:
    /// the timeline does not move and render-blocking is ignored), rebuilding the boxes that changed.
    /// Returns the root fragment. Script-driven geometry reads force this; frames come from
    /// [`Renderer::update_rendering`].
    ///
    /// Size query containers (CSS Contain 3 §2) get their content-box sizes from the layout; when one
    /// changed, the container's subtree is restyled against it (`@container` queries, container
    /// query length units) and laid out again, a few rounds at most for nested containers (size
    /// containment keeps a container's queried size independent of what its queries change).
    pub fn layout(&mut self, viewport: Size) -> Option<Arc<Fragment<RenderStyle>>> {
        const ROUNDS: usize = 4;
        let mut root = self.layout_once(viewport)?;
        for _ in 0..ROUNDS {
            if !self.update_query_containers() {
                break;
            }
            root = self.layout_once(viewport)?;
        }
        Some(root)
    }

    /// [`Renderer::layout`] for a script's read (`getBoundingClientRect`, `offsetWidth`...). An inline
    /// svg's image is only painted: a read leaves one that exists until the rendering update, so
    /// that a script alternating changes and reads inside an svg does not write it out each time.
    /// (What the svg's own attributes give layout, its natural size, is made at once.)
    pub fn layout_for_script(&mut self, viewport: Size) -> Option<Arc<Fragment<RenderStyle>>> {
        self.script_read = true;
        let root = self.layout(viewport);
        self.script_read = false;
        root
    }

    /// Records each size query container's content-box size from the last layout in its axes
    /// (CSS Contain 3 §2, §6: what `@container` size queries and container query length units of its
    /// descendants resolve against) and invalidates the subtrees of those whose size changed.
    /// Returns whether one changed.
    /// CSS Sizing 4 5.2.1, run in the rendering update's resize observation step: records, for each
    /// element with `contain-intrinsic-size: auto` in an axis, that is not size contained in it and
    /// has a principal box, the content box size of the box as its last remembered size in that axis;
    /// forgets those of elements that no longer have `auto`. The size belongs to the element, so it
    /// is kept while the element has no box.
    pub fn record_remembered_sizes(&mut self) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        if !doc.auto_intrinsic_seen {
            return;
        }
        let Some(root) = doc.fragments.clone() else {
            return;
        };
        use snowlark_layout::style::Style as _;
        let mut found: Vec<(NodeId, Option<f32>, Option<f32>)> = Vec::new();
        root.walk(&mut |f, _| {
            let Some(b) = f.as_box() else {
                return;
            };
            let (Some(node), None) = (b.node, b.pseudo) else {
                return;
            };
            let (auto_w, auto_h) = b.style.intrinsic_size_auto();
            if !auto_w && !auto_h {
                return;
            }
            let contained = b.style.phys_size_containment();
            let c = f.content_rect();
            // (remembered per logical axis: a change of writing mode keeps the inline size)
            let (w, h) = (
                (auto_w && !contained.width).then_some(c.width),
                (auto_h && !contained.height).then_some(c.height),
            );
            found.push(if b.style.values().writing_mode.is_horizontal() {
                (node, w, h)
            } else {
                (node, h, w)
            });
        });
        for (node, w, h) in found {
            if w.is_none() && h.is_none() {
                continue;
            }
            let e = doc.remembered.entry(node).or_insert((None, None));
            if w.is_some() {
                e.0 = w;
            }
            if h.is_some() {
                e.1 = h;
            }
        }
        // An element that no longer has `auto` in an axis forgets that axis.
        let data = &doc.data;
        let dom_rc = doc.dom.clone();
        let dom = dom_rc.borrow();
        doc.remembered.retain(|&node, size| {
            // (a disconnected element loses it)
            if !dom.is_alive(node) || !dom.is_connected(node) {
                return false;
            }
            if let Some(values) = data.styles(node) {
                forget_lost_auto(size, values);
            }
            size.0.is_some() || size.1.is_some()
        });
    }

    fn update_query_containers(&mut self) -> bool {
        let Some(doc) = self.doc.as_deref_mut() else {
            return false;
        };
        if !doc.query_containers_seen {
            return false;
        }
        let Some(root) = doc.fragments.clone() else {
            return false;
        };
        // An unchanged tree has the sizes recorded from it already. Without this, every geometry
        // read of a page with a container (each Intersection Observer target, twice, per frame)
        // walked the whole fragment tree. (Blink reads a container's size while laying the
        // container out, `BlockNode` → `StyleEngine::UpdateStyleAndLayoutTreeForContainer`, since
        // "[@container] Interleave style/layout on first pass", 2ac21f37: a clean layout reads
        // nothing.)
        if doc
            .query_sizes_from
            .as_ref()
            .is_some_and(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(&root)))
        {
            return false;
        }
        doc.query_sizes_from = Some(Arc::downgrade(&root));
        self.stats.query_size_reads += 1;
        let mut sizes: Vec<(NodeId, Option<f32>, Option<f32>)> = Vec::new();
        let mut seen = HashSet::new();
        root.walk(&mut |f, _| {
            let Some(b) = f.as_box() else {
                return;
            };
            let (Some(node), None) = (b.node, b.pseudo) else {
                return;
            };
            let Some((w, h)) = b.style.query_container_axes() else {
                return;
            };
            // (the first fragment of a box split across lines or columns)
            if !seen.insert(node) {
                return;
            }
            // A container whose principal box is not eligible for size containment (a non-atomic
            // inline, a table, an internal table or ruby box; CSS Contain 2 §3.1) has no size: its
            // queries are unknown.
            use snowlark_layout::style::{Display, DisplayInside, DisplayOutside, Style as _};
            let eligible = match b.style.display() {
                Display::Box {
                    inside: DisplayInside::Table | DisplayInside::Ruby,
                    ..
                }
                | Display::Internal(_) => false,
                Display::Box {
                    outside: DisplayOutside::Inline,
                    inside: DisplayInside::Flow,
                    ..
                } => matches!(b.content, snowlark_layout::BoxContent::Replaced { .. }),
                _ => true,
            };
            if !eligible {
                sizes.push((node, None, None));
                return;
            }
            let c = f.content_rect();
            sizes.push((node, w.then_some(c.width), h.then_some(c.height)));
        });
        let dom_rc = doc.dom.clone();
        let dom = dom_rc.borrow();
        let mut changed = false;
        for (node, w, h) in sizes {
            if doc.data.query_container_size(node) != Some((w, h)) {
                doc.data.set_query_container_size(node, w, h);
                doc.engine.invalidate_subtree(&dom, &mut doc.data, node);
                changed = true;
            }
        }
        doc.needs_restyle |= changed;
        changed
    }

    fn layout_once(&mut self, viewport: Size) -> Option<Arc<Fragment<RenderStyle>>> {
        self.set_viewport(viewport);
        self.check_embedded_documents();
        if self.needs_restyle() {
            self.restyle();
        }
        let t = Instant::now();
        let pdf_viewer = self.pdf_viewer;
        let doc = self.doc.as_deref_mut()?;
        let dom_rc = doc.dom.clone();
        let dom = dom_rc.borrow();
        let restyles = doc.data.take_restyles();
        // CSS Sizing 4 5.2.1: an element that lost `contain-intrinsic-size: auto` in an axis
        // forgets its last remembered size there at once.
        if !doc.remembered.is_empty() {
            for r in &restyles.elements {
                if let (Some(size), Some(values)) = (
                    doc.remembered.get_mut(&r.element),
                    doc.data.styles(r.element),
                ) {
                    forget_lost_auto(size, values);
                }
            }
        }
        doc.restyles_seen = 0;
        let styles = DocumentStyles::new(
            &dom,
            &doc.engine,
            &doc.data,
            doc.env.clone(),
            &doc.img_state,
        )
        .with_controls(&doc.control_states)
        .with_top_layer(&doc.top_layer)
        .with_fullscreen(doc.fullscreen)
        .with_media(&doc.media, &doc.video_frames)
        .with_embed_types(&doc.embed_types)
        .with_embedded_documents(&doc.embedded_documents)
        .with_pdf_viewer(pdf_viewer)
        .with_remembered(&doc.remembered)
        .with_clip_references(&doc.clip_references)
        .with_filter_references(&doc.filter_references);
        // The restyle records since the last update, as box tree changes.
        let mut counters = false;
        for r in &restyles.elements {
            match r.strongest() {
                StyleChange::Unchanged => self.stats.unchanged += 1,
                StyleChange::Paint | StyleChange::Repaint => self.stats.repaint += 1,
                StyleChange::Relayout => self.stats.relayout += 1,
                StyleChange::Reconstruct => self.stats.reconstruct += 1,
            }
            counters |= r.counters;
            let damage = match r.change {
                StyleChange::Reconstruct => BoxDamage::Reconstruct,
                StyleChange::Relayout => BoxDamage::Relayout,
                StyleChange::Repaint => BoxDamage::Repaint,
                StyleChange::Paint => BoxDamage::Paint,
                // (only the decorations it gives its contents)
                StyleChange::Unchanged if r.contents == StyleChange::Paint => BoxDamage::Paint,
                // stylo does not diff `::marker`: a list item restyled with equal values may still
                // have a new marker style.
                StyleChange::Unchanged
                    if r.contents != StyleChange::Unchanged
                        || doc
                            .data
                            .styles(r.element)
                            .is_some_and(|s| s.get_box().display.is_list_item()) =>
                {
                    BoxDamage::Repaint
                }
                StyleChange::Unchanged => continue,
            };
            doc.changes.restyled.push(
                ElementChange::restyled(r.element, damage, r.contents > StyleChange::Paint)
                    .decorations(r.contents == StyleChange::Paint),
            );
        }
        if !doc.fonts_arrived.is_empty() && !doc.needs_build {
            // Web fonts arrived: every handle takes the new font table; boxes naming an arrived
            // family are laid out again (their text reshaped, everything else reused).
            let families = std::mem::take(&mut doc.fonts_arrived);
            let env = doc.env.clone();
            let changed = doc.layout.restyle_all(|s| {
                let uses = s.names_family(&families);
                Some((s.with_env(env.clone()), uses))
            });
            self.stats.font_boxes += u64::from(changed);
        }
        doc.fonts_arrived.clear();
        let generated = doc.generated_seen
            && (counters || !doc.changes.children.is_empty() || !doc.changes.text.is_empty());
        if doc.needs_build || restyles.overflowed || generated {
            let allocated = doc.layout.boxes().allocated();
            doc.layout.build(&dom, doc.document, &styles);
            doc.needs_build = false;
            self.stats.full_builds += 1;
            self.stats.update.boxes_built += (doc.layout.boxes().allocated() - allocated) as u32;
        } else if !doc.changes.is_empty() {
            let u = doc.layout.update(&dom, doc.document, &styles, &doc.changes);
            if u.full_build {
                self.stats.full_builds += 1;
            } else {
                self.stats.incremental_updates += 1;
            }
            self.stats.update.add(&u);
        }
        doc.changes.clear();
        doc.text_changed.clear();
        doc.generated_seen |= styles.generated_seen();
        doc.query_containers_seen |= styles.query_containers_seen();
        doc.auto_intrinsic_seen |= styles.auto_intrinsic_seen();
        self.timings.build_ms += ms(t);
        let t = Instant::now();
        // The viewport's vertical scroll bar shows when the viewport's `overflow-y` is `scroll`, or
        // `auto` (`visible` computes to it there) and the content is taller than the viewport; it
        // takes its width from the initial containing block. Showing it can change the height, so
        // a layout that contradicts the assumption it started from is done again with the other
        // one, and a second contradiction (content that only overflows without the bar) keeps the
        // bar.
        // (CSS Scrollbars 1 §3: the root's `scrollbar-width` applies to the viewport: `none` hides
        // its bar, the viewport still scrolling; `thin` makes it thinner)
        let sb = viewport_scrollbar_width(
            self.scrollbar_width,
            dom.document_element(doc.document)
                .and_then(|root| doc.data.styles(root))
                .map_or(snowlark_layout::style::ScrollbarWidth::Auto, |v| {
                    crate::adapter::scrollbar_width(v)
                }),
        );
        let icb = |bar: bool| {
            Size::new(
                (viewport.width - if bar { sb } else { 0.0 }).max(0.0),
                viewport.height,
            )
        };
        let wants_bar = |layout: &snowlark_layout::LayoutTree<RenderStyle>,
                         root: &Fragment<RenderStyle>| {
            use snowlark_layout::style::Overflow;
            match layout.viewport_overflow().map(|o| o.1) {
                Some(Overflow::Scroll) => true,
                Some(Overflow::Hidden | Overflow::Clip) => false,
                _ => root.overflow.bottom() > viewport.height + 0.5,
            }
        };
        let mut bar = sb > 0.0 && doc.vertical_scrollbar;
        let mut root = doc.layout.layout(&self.text, icb(bar));
        let mut ls = doc.layout.stats();
        if sb > 0.0 && wants_bar(&doc.layout, &root) != bar {
            bar = !bar;
            root = doc.layout.layout(&self.text, icb(bar));
            sum_layout_stats(&mut ls, doc.layout.stats());
            if !bar && wants_bar(&doc.layout, &root) {
                bar = true;
                root = doc.layout.layout(&self.text, icb(bar));
                sum_layout_stats(&mut ls, doc.layout.stats());
            }
        }
        doc.vertical_scrollbar = bar;
        doc.vertical_scrollbar_width = sb;
        self.stats.laid_out += u64::from(ls.laid_out);
        self.stats.reused += u64::from(ls.reused);
        self.stats.relaid += u64::from(ls.relaid);
        self.stats.shaped += u64::from(ls.shaped);
        self.stats.shape_cache_hits += u64::from(ls.shape_cache_hits);
        self.stats.words_shaped += u64::from(ls.words_shaped);
        self.stats.words_cached += u64::from(ls.words_cached);
        self.stats.containers_built += u64::from(ls.containers_built);
        self.stats.algorithm_runs += u64::from(ls.algorithm_runs);
        doc.fragments = Some(root.clone());
        doc.last_viewport = Some(icb(bar));
        self.timings.layout_ms += ms(t);
        self.timings.layouts += 1;
        Some(root)
    }

    /// Restyle, box tree and layout statistics summed over the renderer's lifetime.
    pub fn stats(&self) -> RenderStats {
        let mut s = self.stats;
        s.sheet_changes = self
            .doc
            .as_ref()
            .map_or(0, |d| d.engine.sheets_generation());
        s
    }

    /// The last layout's statistics.
    pub fn layout_stats(&self) -> LayoutStats {
        self.doc
            .as_ref()
            .map(|d| d.layout.stats())
            .unwrap_or_default()
    }

    /// The principal writing mode and direction (CSS Writing Modes 4 §8): what the viewport
    /// scrolls by.
    pub fn principal_writing_mode(
        &self,
    ) -> Option<(
        snowlark_layout::style::WritingMode,
        snowlark_layout::style::Direction,
    )> {
        self.doc.as_ref()?.layout.principal_writing_mode()
    }

    /// The fragment tree of the last layout.
    pub fn fragments(&self) -> Option<&Arc<Fragment<RenderStyle>>> {
        self.doc.as_ref()?.fragments.as_ref()
    }

    /// The scroll position of a scroll container, for painting and hit testing.
    pub fn set_scroll(&mut self, node: NodeId, offset: Point) {
        if let Some(d) = &mut self.doc {
            d.scroll.insert(node, offset);
        }
    }

    /// The display list of the last layout for `viewport`, scrolled by `scroll`, for the device pixel
    /// ratio of the [`DeviceConfig`].
    pub fn paint(&mut self, viewport: Size, scroll: Point) -> DisplayList {
        self.paint_scaled(viewport, scroll, self.config.device_pixel_ratio)
    }

    /// [`Renderer::paint`] for a raster of `scale` device pixels per CSS px: the SVG images in the list
    /// are rasterized (or taken from the cache) at the device size they are drawn at.
    pub fn paint_scaled(&mut self, viewport: Size, scroll: Point, scale: f32) -> DisplayList {
        self.paint_with_embeds(viewport, scroll, scale, None, false)
    }

    /// [`Renderer::paint_scaled`] with nested documents' display lists painted into the content
    /// boxes of the elements that show them (frames), as [`PaintOptions::embeds`] describes. The
    /// nested lists must refer to this renderer's image store (see [`Renderer::share_images`]).
    /// With `layers`, fixed and sticky content is marked as layers for a compositor
    /// ([`PaintOptions::layers`]), and the list covers the embedder's band: from half a viewport
    /// above `scroll` to three and a half below (the rest is left out, see
    /// [`snowlark_layout::paint_part`]).
    /// The display list of the document's own content as [`Renderer::paint_with_embeds`] makes it,
    /// without its side effects (animated images' frame bookkeeping, SVG rasterization, paint
    /// timings): what a rendering update would paint, for Paint Timing.
    pub fn paint_probe(&self, viewport: Size, scroll: Point) -> DisplayList {
        let mut builder = DisplayListBuilder::new();
        if let Some(doc) = self.doc.as_deref() {
            if let Some(root) = &doc.fragments {
                let options = PaintOptions {
                    viewport,
                    viewport_scroll: scroll,
                    scroll: Some(&doc.scroll),
                    embeds: None,
                    selection: None,
                    find: None,
                    layers: false,
                    device_scale: 1.0,
                    composited: None,
                    animations: None,
                };
                snowlark_layout::paint_part(root, &self.text, &options, None, &mut builder);
            }
        }
        builder.build()
    }

    pub fn paint_with_embeds(
        &mut self,
        viewport: Size,
        scroll: Point,
        scale: f32,
        embeds: Option<&HashMap<NodeId, DisplayList>>,
        layers: bool,
    ) -> DisplayList {
        let t = Instant::now();
        let mut builder = DisplayListBuilder::new();
        if let Some(doc) = self.doc.as_deref() {
            doc.read_motion_paths();
            if let Some(root) = &doc.fragments {
                // (for a compositor: the elements with a transform or opacity animation in effect)
                let mut animated = Vec::new();
                if layers {
                    doc.engine.composited_animation_targets(&mut animated);
                }
                let composited = |n: NodeId| {
                    animated
                        .binary_search_by_key(&n.to_bits(), |a| a.to_bits())
                        .is_ok()
                };
                // (and the animations of theirs the compositor runs, on a timeline that follows
                // the monotonic clock: S2)
                let origin = snowlark_paint::Ticks::now().before(doc.timeline_origin.elapsed());
                let compositor_animations = |n: NodeId,
                                             reference: snowlark_paint::Rect,
                                             at: snowlark_paint::Point,
                                             out: &mut Vec<
                    snowlark_paint::layer_animation::LayerAnimation,
                >| {
                    if let Some(values) = doc.data.styles(n) {
                        crate::compositor_animations::collect(
                            (&doc.engine, &doc.data),
                            n,
                            values,
                            reference,
                            at,
                            origin,
                            out,
                        );
                    }
                };
                let options = PaintOptions {
                    viewport,
                    viewport_scroll: scroll,
                    scroll: Some(&doc.scroll),
                    embeds,
                    selection: doc.selection.as_ref(),
                    find: doc.find.as_ref(),
                    layers,
                    device_scale: scale,
                    composited: layers.then_some(&composited as &dyn Fn(NodeId) -> bool),
                    animations: layers.then_some(
                        &compositor_animations as &snowlark_layout::CompositorAnimations,
                    ),
                };
                // The embedder rasterizes a band of at most three viewports from `scroll`, or for
                // a list painted for exactly the viewport (one with pinned content) up to a viewport
                // above it and a viewport and a half below (and paints again before scrolling out
                // of it): what lies farther, with half a viewport of margin for shadows and other
                // ink outside the boxes, is left out.
                let cull = layers.then(|| {
                    let h = viewport.height.max(1.0);
                    snowlark_paint::Rect::new(-1.0e7, -1.5 * h, 2.0e7, 5.0 * h)
                });
                snowlark_layout::paint_part(root, &self.text, &options, cull, &mut builder);
                if let Some(b) = doc.selection.as_ref().and_then(|s| s.caret_box) {
                    builder.fill_rect(b, snowlark_paint::Color::BLACK);
                }
            }
        }
        let mut list = builder.build();
        if let Some(mut replaced) = self.record_inline_svgs(&list) {
            replaced.animations = std::mem::take(&mut list.animations);
            list = replaced;
        }
        // (SVG images on this document's timeline)
        let now = self
            .doc
            .as_deref()
            .map_or(0.0, |d| d.engine.timeline_time());
        let frame = crate::svg::SvgFrame {
            scale,
            now,
            viewport: snowlark_paint::Rect::new(0.0, 0.0, viewport.width, viewport.height),
            scroll,
        };
        self.svg.resolve(&mut list, &self.images, &frame);
        if let Some(doc) = self.doc.as_deref_mut() {
            Self::animations_painted(doc, &list, scroll);
            if layers {
                // What a compositor gets of the animations (the rest of the boxes are drawn as
                // painted): the page stops servicing those until their next change (S2).
                snowlark_paint::layer_animation::fit_budget(&mut list.animations);
                doc.engine
                    .set_on_compositor(list.animations.iter().filter_map(|a| {
                        let id = match a.property() {
                            snowlark_paint::layer_animation::Property::Transform => {
                                snowlark_style::LonghandId::Transform
                            }
                            snowlark_paint::layer_animation::Property::Opacity => {
                                snowlark_style::LonghandId::Opacity
                            }
                            snowlark_paint::layer_animation::Property::Filter => {
                                snowlark_style::LonghandId::Filter
                            }
                        };
                        Some((NodeId::from_bits(a.element)?, id))
                    }));
            }
        }
        self.timings.paint_ms += ms(t);
        list
    }

    /// `list` with the inline svgs it draws whole replaced by their paint operations
    /// ([`crate::svg_paint`]), recorded for the size they are drawn at when not recorded for it
    /// yet; `None` when it draws none (the others stay images, rasterized by [`crate::svg`]).
    fn record_inline_svgs(&mut self, list: &DisplayList) -> Option<DisplayList> {
        let doc = self.doc.as_deref_mut()?;
        if doc.inline_svgs.is_empty() {
            doc.svg_recordings.clear();
            return None;
        }
        let nodes: HashMap<snowlark_paint::ImageId, (NodeId, u64)> = doc
            .inline_svgs
            .iter()
            .filter_map(|(&n, &(hash, id))| Some((id?, (n, hash))))
            .collect();
        doc.svg_recordings
            .retain(|id, r| nodes.get(id).is_some_and(|(_, hash)| *hash == r.hash));
        // (image, its svg and markup hash, the size drawn at)
        type Wanted = (snowlark_paint::ImageId, (NodeId, u64), (f32, f32));
        let mut wanted: Vec<Wanted> = Vec::new();
        for item in &list.items {
            if let snowlark_paint::Item::Image {
                image, dest, src, ..
            } = *item
            {
                let size = (dest.width, dest.height);
                let Some(&n) = nodes.get(&image) else {
                    continue;
                };
                let known = doc
                    .svg_recordings
                    .get(&image)
                    .is_some_and(|r| r.size == size);
                if src.is_empty() && !known && !wanted.iter().any(|w| w.0 == image && w.2 == size) {
                    wanted.push((image, n, size));
                }
            }
        }
        if !wanted.is_empty() {
            // An svg whose markup and size equal another's draws the same (the markup is the
            // whole drawing: the outermost element carries every value it does not take from the
            // initial ones), so it takes that recording: a page's repeated icons record once.
            let mut same: HashMap<(u64, (u32, u32)), SvgRecordingResult> = doc
                .svg_recordings
                .values()
                .map(|r| ((r.hash, size_key(r.size)), r.recording.clone()))
                .collect();
            // An svg drawn without markup takes the recording of one whose fingerprint draws
            // alike (the same content and equal computed values) at the same size: (its content
            // hash, size) to (svg, key) of those recorded.
            type Alike = HashMap<(u64, (u32, u32)), Vec<(NodeId, u64)>>;
            let mut alike: Alike = HashMap::new();
            for (image, r) in &doc.svg_recordings {
                let Some(&(n, _)) = nodes.get(image) else {
                    continue;
                };
                if let Some(p) = doc.svg_prints.get(&n) {
                    alike
                        .entry((p.content(), size_key(r.size)))
                        .or_default()
                        .push((n, r.hash));
                }
            }
            let mut made = Vec::with_capacity(wanted.len());
            let mut recorded = (0, 0);
            // (the svgs recorded, and those not: an svg without markup gets it now)
            let mut outcomes: Vec<(NodeId, snowlark_paint::ImageId, bool)> = Vec::new();
            let query = self.svg_query();
            if query.is_none() {
                outcomes.extend(wanted.iter().map(|w| (w.1 .0, w.0, false)));
            }
            if let Some(q) = &query {
                let prints = &self.doc.as_deref()?.svg_prints;
                for (image, (n, hash), size) in wanted {
                    let sk = (hash, size_key(size));
                    let recording = match same.get(&sk) {
                        Some(r) => r.clone(),
                        None => {
                            let print = prints.get(&n);
                            let like = print.and_then(|p| {
                                alike
                                    .get(&(p.content(), sk.1))?
                                    .iter()
                                    .find(|(m, _)| prints.get(m).is_some_and(|o| p.draws_like(o)))
                                    .and_then(|&(_, key)| same.get(&(key, sk.1)))
                            });
                            let r = match like {
                                Some(r) => r.clone(),
                                None => {
                                    let r = crate::svg_paint::record(q, n, size).map(Arc::new);
                                    match r {
                                        Ok(_) => recorded.0 += 1,
                                        Err(_) => recorded.1 += 1,
                                    }
                                    r
                                }
                            };
                            if let Some(p) = print {
                                alike
                                    .entry((p.content(), sk.1))
                                    .or_default()
                                    .push((n, hash));
                            }
                            same.insert(sk, r.clone());
                            r
                        }
                    };
                    outcomes.push((n, image, recording.is_ok()));
                    made.push((
                        image,
                        SvgRecording {
                            hash,
                            size,
                            recording,
                        },
                    ));
                }
            }
            drop(query);
            self.stats.svgs_recorded += recorded.0;
            self.stats.svgs_not_recorded += recorded.1;
            let doc = self.doc.as_deref_mut()?;
            for (image, recording) in made {
                doc.svg_recordings.insert(image, recording);
            }
            let mut need: Vec<(NodeId, snowlark_paint::ImageId)> = Vec::new();
            for (n, image, ok) in outcomes {
                if ok {
                    doc.svg_markup.remove(&n);
                } else if !self.svg.has_markup(image) && !need.iter().any(|x| x.0 == n) {
                    need.push((n, image));
                }
            }
            if !need.is_empty() {
                self.write_svg_markup(&need);
            }
        }
        let doc = self.doc.as_deref()?;
        let recordings = &doc.svg_recordings;
        list.replace_images(&mut |image, dest| {
            let r = recordings.get(&image)?;
            let list = r.recording.as_ref().ok()?;
            (r.size == (dest.width, dest.height))
                .then(|| (&**list, snowlark_paint::Matrix::translation(dest.x, dest.y)))
        })
    }

    /// Gives the images of inline svgs drawn with no markup that could not be recorded after all
    /// their markup ([`crate::inline_svg::serialize`]), so they are drawn as images: (svg, image).
    /// Each is written out from then on, until a recording of it succeeds.
    fn write_svg_markup(&mut self, svgs: &[(NodeId, snowlark_paint::ImageId)]) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        let base = Self::base_url(doc);
        let dom = doc.dom.borrow();
        for &(n, image) in svgs {
            doc.svg_markup.insert(n);
            doc.svg_prints.remove(&n);
            if !dom.is_alive(n) {
                continue;
            }
            self.stats.svgs_serialized += 1;
            let svg = {
                let external = crate::inline_svg::External::new(&doc.svg_resources, &doc.url);
                crate::inline_svg::serialize(&dom, &doc.data, n, &base, &external)
            };
            // (what it fetches is started where svgs are written out as they change: a
            // fingerprint is taken only of svgs that fetch nothing, so this is for safety)
            if !(svg.images.is_empty()
                && svg.uses.is_empty()
                && svg.resources.is_empty()
                && svg.style_resources.is_empty())
            {
                doc.svgs_waiting.insert(n);
            }
            if let Ok(img) = crate::svg::SvgImage::parse(svg.markup.as_bytes()) {
                self.svg.set_markup(image, img);
            }
        }
    }

    /// Why an inline svg is drawn as an image rather than as paint operations, once a paint asked
    /// ([`crate::svg_paint::record`]); `None` when it is recorded, or not painted yet.
    pub fn inline_svg_unsupported(&self, svg: NodeId) -> Option<&'static str> {
        let doc = self.doc.as_deref()?;
        let id = doc.inline_svgs.get(&svg)?.1?;
        doc.svg_recordings
            .get(&id)?
            .recording
            .as_ref()
            .err()
            .copied()
    }

    /// Why the inline svgs that were painted as images were not recorded (one entry each).
    pub fn inline_svg_fallbacks(&self) -> Vec<&'static str> {
        let Some(doc) = self.doc.as_deref() else {
            return Vec::new();
        };
        doc.svg_recordings
            .values()
            .filter_map(|r| r.recording.as_ref().err().copied())
            .collect()
    }

    /// Where the animated images were painted (the list is in viewport coordinates, scrolled by
    /// `scroll`), and the start of the animations painted for the first time.
    fn animations_painted(doc: &mut Doc, list: &DisplayList, scroll: Point) {
        if doc.animations.is_empty() {
            return;
        }
        let mut found = Vec::new();
        list.image_bounds(&|id| doc.animations.contains_key(&id), &mut found);
        for a in doc.animations.values_mut() {
            a.painted.clear();
            a.layered = false;
        }
        let now = doc.engine.timeline_time();
        for (id, rect, layered) in found {
            let Some(a) = doc.animations.get_mut(&id) else {
                continue;
            };
            if rect.is_empty() && !layered {
                continue;
            }
            a.layered |= layered;
            if !rect.is_empty() {
                a.painted.push(rect.translate(scroll.x, scroll.y));
            }
            a.start.get_or_insert(now);
        }
    }

    /// The selection to paint (highlighted text, an editing host's caret); `None` for none.
    pub fn set_selection(&mut self, selection: Option<snowlark_layout::TextSelection>) {
        if let Some(d) = self.doc.as_deref_mut() {
            d.selection = selection;
        }
    }

    /// The matches of find in page to highlight; `None` for none. Whether the highlights changed.
    pub fn set_find(&mut self, find: Option<snowlark_layout::find::FindHighlights>) -> bool {
        match self.doc.as_deref_mut() {
            Some(d) if d.find != find => {
                d.find = find;
                true
            }
            _ => false,
        }
    }

    /// The find highlights painted now.
    pub fn find_highlights(&self) -> Option<&snowlark_layout::find::FindHighlights> {
        self.doc.as_deref()?.find.as_ref()
    }

    /// Which layout is the last: the fragment tree's address (a new layout has another).
    pub fn layout_id(&self) -> Option<usize> {
        Some(Arc::as_ptr(self.doc.as_ref()?.fragments.as_ref()?) as usize)
    }

    /// The rendered text of the last layout, for find in page (see [`snowlark_layout::find`]), and
    /// which layout it is of (the fragment tree's address: a new layout has another).
    pub fn find_items(&self) -> Option<(usize, Vec<snowlark_layout::find::FindItem>)> {
        let root = self.doc.as_ref()?.fragments.as_ref()?;
        Some((
            Arc::as_ptr(root) as usize,
            snowlark_layout::find::find_items(root),
        ))
    }

    /// The selection painted now.
    pub fn selection(&self) -> Option<&snowlark_layout::TextSelection> {
        self.doc.as_deref()?.selection.as_ref()
    }

    /// The caret position (text node, DOM byte offset) nearest a viewport point in the last layout,
    /// among the text nodes `keep` accepts (`snowlark_layout::caret_position`).
    pub fn caret_position(
        &self,
        viewport: Size,
        scroll: Point,
        point: Point,
        keep: &dyn Fn(NodeId) -> bool,
    ) -> Option<(NodeId, u32)> {
        let doc = self.doc.as_deref()?;
        let root = doc.fragments.as_ref()?;
        let options = PaintOptions {
            viewport,
            viewport_scroll: scroll,
            scroll: Some(&doc.scroll),
            embeds: None,
            selection: None,
            find: None,
            layers: false,
            device_scale: 1.0,
            composited: None,
            animations: None,
        };
        snowlark_layout::caret_position(root, point, &options, keep)
    }

    /// The caret rectangle (viewport CSS px) at a DOM byte offset of a text node in the last layout.
    pub fn caret_rect(
        &self,
        viewport: Size,
        scroll: Point,
        node: NodeId,
        byte: u32,
    ) -> Option<snowlark_paint::Rect> {
        let doc = self.doc.as_deref()?;
        let root = doc.fragments.as_ref()?;
        let options = PaintOptions {
            viewport,
            viewport_scroll: scroll,
            scroll: Some(&doc.scroll),
            embeds: None,
            selection: None,
            find: None,
            layers: false,
            device_scale: 1.0,
            composited: None,
            animations: None,
        };
        snowlark_layout::caret_rect(root, node, byte, &options)
    }

    /// The `::selection` colors of the text of each of `elements` (CSS Pseudo-Elements 4 §3): the
    /// highlight's background and the selected text's color (`None`: the text keeps its own).
    /// `None` for an element: no author style gives either, so the user agent's highlight colors
    /// apply (§3.3.1).
    ///
    /// Each of the two comes from the element's own `::selection` style when that declares it, else
    /// from its parent's highlight (§3.5); a background nothing declares beside a declared color is
    /// transparent, and `color: currentcolor` leaves the text's color alone (§3.6.3, with no
    /// highlight below this one).
    pub fn selection_colors(
        &self,
        elements: &[NodeId],
    ) -> Vec<Option<(snowlark_paint::Color, Option<snowlark_paint::Color>)>> {
        use snowlark_paint::Color;
        use snowlark_style::DeclaredColor;
        /// The background and the text color (`Some(None)`: declared, the text's own) that a
        /// highlight has from itself and its ancestors' highlights.
        type Inherited = (Option<Color>, Option<Option<Color>>);
        let Some(d) = self.doc.as_deref() else {
            return vec![None; elements.len()];
        };
        let dom = d.dom.borrow();
        let pseudo = style::selector_parser::PseudoElement::Selection;
        let mut known: std::collections::HashMap<NodeId, Inherited> = Default::default();
        let mut chain = Vec::new();
        elements
            .iter()
            .map(|&el| {
                // Up to an element already resolved (or the root), then back down.
                let mut inherited: Inherited = (None, None);
                let mut cur = Some(el);
                while let Some(e) = cur {
                    if let Some(&k) = known.get(&e) {
                        inherited = k;
                        break;
                    }
                    chain.push(e);
                    cur = snowlark_style::flat_parent(&dom, &d.data, e);
                }
                while let Some(e) = chain.pop() {
                    if let Some(values) = d.engine.pseudo_style(&dom, &d.data, e, &pseudo) {
                        let (color, background) =
                            snowlark_style::highlight_colors_declared(&d.data, &values);
                        let s = RenderStyle::new(values, d.env.clone());
                        match color {
                            DeclaredColor::No => {}
                            DeclaredColor::CurrentColor => inherited.1 = Some(None),
                            DeclaredColor::Value => {
                                inherited.1 = Some(Some(snowlark_layout::Style::color(&s)))
                            }
                        }
                        if background != DeclaredColor::No {
                            inherited.0 = Some(snowlark_layout::Style::background_color(&s));
                        }
                    }
                    known.insert(e, inherited);
                }
                match inherited {
                    (None, None) => None,
                    (background, color) => {
                        Some((background.unwrap_or(Color::TRANSPARENT), color.flatten()))
                    }
                }
            })
            .collect()
    }

    /// The border box and friends of `node`'s first box fragment (`getBoundingClientRect`,
    /// `offset*`, `client*`, `scroll*` start from it), unscrolled.
    pub fn geometry(&self, node: NodeId) -> Option<BoxGeometry> {
        self.doc.as_ref()?.layout.geometry(node)
    }

    /// Runs `f` with the first box fragment of `node` in the last layout (see
    /// [`snowlark_layout::LayoutTree::with_box_fragment`]).
    pub fn with_box_fragment<R>(
        &self,
        node: NodeId,
        f: impl FnOnce(&Fragment<RenderStyle>) -> R,
    ) -> Option<R> {
        let d = self.doc.as_ref()?;
        d.fragments.as_ref()?;
        d.layout.with_box_fragment(node, f)
    }

    /// [`Renderer::client_rects`] without transforms (the layout boxes `offset*` measure).
    pub fn layout_rects(&self, node: NodeId) -> Vec<snowlark_paint::Rect> {
        self.doc
            .as_ref()
            .map(|d| d.layout.layout_rects(node))
            .unwrap_or_default()
    }

    /// How far sticky positioning moves `node`'s box at these scroll positions (see
    /// [`snowlark_layout::LayoutTree::sticky_offset`]).
    pub fn sticky_offset(
        &self,
        node: NodeId,
        viewport: Size,
        scroll: snowlark_paint::Point,
        element_scroll: &HashMap<NodeId, snowlark_paint::Point>,
    ) -> snowlark_paint::Point {
        self.doc.as_ref().map_or(snowlark_paint::Point::ZERO, |d| {
            d.layout
                .sticky_offset(node, viewport, scroll, element_scroll)
        })
    }

    /// The border, padding and content boxes of `node`'s first box fragment, unscrolled (no
    /// scrollable overflow: see [`Renderer::geometry`]).
    pub fn box_rects(&self, node: NodeId) -> Option<snowlark_layout::BoxRects> {
        self.doc.as_ref()?.layout.box_rects(node)
    }

    /// The padding box of `node`'s first box fragment with its and its ancestors' transforms
    /// applied (where its content clip is), unscrolled.
    pub fn transformed_padding_box(&self, node: NodeId) -> Option<snowlark_paint::Rect> {
        self.doc.as_ref()?.layout.transformed_padding_box(node)
    }

    /// The border boxes of every fragment of `node` in the last layout (CSSOM View
    /// `getClientRects()`), unscrolled.
    pub fn client_rects(&self, node: NodeId) -> Vec<snowlark_paint::Rect> {
        self.doc
            .as_ref()
            .map(|d| d.layout.client_rects(node))
            .unwrap_or_default()
    }

    /// The rectangles of the part of text node `node` between DOM byte offsets `start` and `end`
    /// in the last layout (`Range.getClientRects()`), unscrolled.
    pub fn text_range_rects(
        &self,
        node: NodeId,
        start: u32,
        end: u32,
    ) -> Vec<snowlark_paint::Rect> {
        self.doc
            .as_ref()
            .map(|d| d.layout.text_range_rects(node, start, end))
            .unwrap_or_default()
    }

    /// The computed styles, the style environment and the last initial containing block, for the
    /// SVG geometry ([`crate::svg_geometry`]).
    /// The resource document and fragment a `use` element refers to, once it is there.
    pub(crate) fn svg_resource_of(
        &self,
        dom: &snowlark_dom::Dom,
        el: NodeId,
    ) -> Option<(Rc<crate::svg_resource::ResourceDocument>, Option<String>)> {
        use crate::svg_resource::SvgResource;
        let d = self.doc.as_deref()?;
        let href = dom
            .attr(el, &ns!(), &"href".into())
            .or_else(|| dom.attr(el, &ns!(xlink), &"href".into()))?;
        let mut url = Self::base_url(d).join(href.trim()).ok()?;
        let fragment = url
            .fragment()
            .map(snowlark_layout::svg::parse::percent_decoded);
        url.set_fragment(None);
        match d.svg_resources.get(url.as_str())? {
            SvgResource::Loaded(r) => Some((r.clone(), fragment)),
            _ => None,
        }
    }

    pub(crate) fn svg_doc(&self) -> Option<(&StyleData, &Arc<crate::env::StyleEnv>, Option<Size>)> {
        let d = self.doc.as_deref()?;
        Some((&d.data, &d.env, d.last_viewport))
    }

    /// `node`'s computed style as layout reads it (display, position, overflow...), if it has one.
    pub fn element_style(&self, node: NodeId) -> Option<RenderStyle> {
        let d = self.doc.as_deref()?;
        let values = d.data.styles_arc(node)?;
        Some(RenderStyle::new(values, d.env.clone()))
    }

    /// `node`'s parent in the flat tree (CSS Scoping 1 §2.2), as the boxes were built from it: its
    /// assigned slot, its parent element, or the host of the shadow root it is a child of.
    pub fn flat_parent(&self, dom: &Dom, node: NodeId) -> Option<NodeId> {
        match self.doc.as_deref() {
            Some(d) if dom.node_document(node) == d.document => {
                snowlark_style::flat_parent(dom, &d.data, node)
            }
            _ => {
                let p = dom.parent(node)?;
                if dom.is_shadow_root(p) {
                    return dom.host(p);
                }
                dom.is_element(p).then_some(p)
            }
        }
    }

    /// Layout's used values for CSSOM's resolved values of `node`
    /// (`snowlark_style::computed::resolved_property(.., Some(&used))`).
    pub fn used_values(&self, node: NodeId) -> Option<NodeUsedValues> {
        let doc = self.doc.as_deref()?;
        let dom = doc.dom.borrow();
        let values = doc.data.styles_arc(node)?;
        let Some(geometry) = doc.layout.box_rects(node) else {
            // (an SVG element has no CSS box; its `transform` still resolves to a matrix)
            let svg = dom.element(node).is_some_and(|e| e.name().ns == ns!(svg));
            return svg.then(|| NodeUsedValues::boxless(values));
        };
        // The containing block: the nearest ancestor with a box (for absolutely positioned boxes, the
        // nearest positioned one; for fixed, the viewport).
        let position = values.get_box().position;
        use style::values::computed::PositionProperty as P;
        let viewport = doc
            .last_viewport
            .unwrap_or(Size::new(self.config.viewport.0, self.config.viewport.1));
        let icb = snowlark_paint::Rect::new(0.0, 0.0, viewport.width, viewport.height);
        let mut cb = None;
        let mut a = dom.parent_element(node);
        while let Some(p) = a {
            if let Some(g) = doc.layout.box_rects(p) {
                let positioned = doc
                    .data
                    .styles(p)
                    .is_some_and(|s| s.get_box().position != P::Static);
                match position {
                    P::Absolute if !positioned => {}
                    P::Absolute => {
                        cb = Some(g.padding_box);
                        break;
                    }
                    P::Fixed => break,
                    _ => {
                        cb = Some(g.content_box);
                        break;
                    }
                }
            }
            a = dom.parent_element(p);
        }
        // A sticky box's insets resolve against the scrollport of its nearest scroll container in
        // each axis (css-position-3 §3.4), the viewport's when there is none.
        let sticky_basis = (position == P::Sticky).then(|| {
            let (mut w, mut h) = (None, None);
            let mut a = dom.parent_element(node);
            let root = dom.document_element(dom.node_document(node));
            while let Some(p) = a {
                if Some(p) == root || (w.is_some() && h.is_some()) {
                    break;
                }
                if let (Some(s), Some(g)) = (doc.data.styles(p), doc.layout.geometry(p)) {
                    let b = s.get_box();
                    let scrolls = |o: style::values::specified::box_::Overflow| o.is_scrollable();
                    if w.is_none() && scrolls(b.overflow_x) {
                        w = Some(g.content_box.width);
                    }
                    if h.is_none() && scrolls(b.overflow_y) {
                        h = Some(g.content_box.height);
                    }
                }
                a = dom.parent_element(p);
            }
            (w.unwrap_or(viewport.width), h.unwrap_or(viewport.height))
        });
        let mut used = NodeUsedValues::new(values, geometry, cb.unwrap_or(icb))
            .with_grid(doc.layout.grid_track_lists(node));
        used.sticky_basis = sticky_basis;
        Some(used)
    }

    /// The computed value of property `name` of `node` (without layout's used values), serialized:
    /// what `Animation.commitStyles()` writes to the inline style.
    pub fn computed_property(&self, node: NodeId, name: &str) -> Option<String> {
        let doc = self.doc.as_deref()?;
        let dom = doc.dom.borrow();
        let values = doc.engine.computed_style(&dom, &doc.data, node)?;
        let id = snowlark_style::computed::property_id(&snowlark_style::computed::physical_name(
            &values, name,
        ))?;
        Some(snowlark_style::computed::computed_property(&values, &id))
    }

    /// The physical property a (logical) longhand of `node` maps to in its writing mode.
    pub fn physical_property(&self, node: NodeId, name: &str) -> String {
        let Some(doc) = self.doc.as_deref() else {
            return name.to_owned();
        };
        let dom = doc.dom.borrow();
        match doc.engine.computed_style(&dom, &doc.data, node) {
            Some(values) => snowlark_style::computed::physical_name(&values, name),
            None => name.to_owned(),
        }
    }

    /// HTML "being rendered" as Web Animations' `commitStyles()` reads it: the element is connected
    /// and not in a `display: none` subtree (an element with `display: contents` counts).
    pub fn is_being_rendered(&self, node: NodeId) -> bool {
        let Some(doc) = self.doc.as_deref() else {
            return false;
        };
        let dom = doc.dom.borrow();
        if !dom.is_alive(node) || !dom.is_connected(node) {
            return false;
        }
        let mut n = Some(node);
        while let Some(id) = n {
            if dom.is_element(id) {
                match doc.data.styles(id) {
                    Some(s) if !s.get_box().display.is_none() => {}
                    _ => return false,
                }
            }
            n = dom.parent_element(id);
        }
        true
    }

    /// CSSOM `getComputedStyle(node).getPropertyValue(name)` with layout's used values.
    pub fn resolved_property(&self, node: NodeId, name: &str) -> Option<String> {
        let doc = self.doc.as_deref()?;
        let dom = doc.dom.borrow();
        let id = snowlark_style::computed::property_id(name)?;
        let values = doc.engine.computed_style(&dom, &doc.data, node)?;
        if let Some(v) = snowlark_style::computed::resolved_min_size_auto(&values, &id, || {
            snowlark_style::computed::is_flex_or_grid_item(&dom, &doc.data, node)
        }) {
            return Some(v);
        }
        let used = self.used_values(node);
        Some(snowlark_style::computed::resolved_property(
            &values,
            &id,
            used.as_ref()
                .map(|u| u as &dyn snowlark_style::computed::UsedValues),
        ))
    }

    /// Whether `node`'s style can depend on layout: it or an ancestor is a size container, so
    /// container queries and container units resolve against a laid-out box (CSS Conditional 5
    /// §7), and a style read needs layout first.
    pub fn style_depends_on_layout(&self, node: NodeId) -> bool {
        use style::computed_value_flags::ComputedValueFlags as F;
        self.doc.as_deref().is_some_and(|d| {
            d.data.styles(node).is_some_and(|v| {
                v.flags
                    .contains(F::SELF_OR_ANCESTOR_HAS_SIZE_CONTAINER_TYPE)
            })
        })
    }

    /// CSSOM `getComputedStyle(node).getPropertyValue(name)` for a property whose resolved value is
    /// the computed value (`snowlark_style::computed::needs_layout` is false): no layout needed.
    pub fn resolved_style_property(&self, node: NodeId, name: &str) -> Option<String> {
        let doc = self.doc.as_deref()?;
        let dom = doc.dom.borrow();
        let id = snowlark_style::computed::property_id(name)?;
        let values = doc.engine.computed_style(&dom, &doc.data, node)?;
        if let Some(v) = snowlark_style::computed::resolved_min_size_auto(&values, &id, || {
            snowlark_style::computed::is_flex_or_grid_item(&dom, &doc.data, node)
        }) {
            return Some(v);
        }
        Some(snowlark_style::computed::computed_property(&values, &id))
    }

    /// CSSOM `getComputedStyle(node, pseudo).getPropertyValue(name)` for a pseudo-element
    /// (`::before`, `::after`, `::marker`...): the computed value (pseudo-elements have no used
    /// values here). `None` when the element or the pseudo-element has no style.
    pub fn resolved_pseudo_property(
        &self,
        node: NodeId,
        pseudo: &snowlark_style::PseudoElement,
        name: &str,
    ) -> Option<String> {
        let doc = self.doc.as_deref()?;
        let dom = doc.dom.borrow();
        let id = snowlark_style::computed::property_id(name)?;
        let values = doc.engine.pseudo_style(&dom, &doc.data, node, pseudo)?;
        Some(snowlark_style::computed::computed_property(&values, &id))
    }

    /// The CSSOM's declaration block for `element`'s `style` attribute, about to be written as
    /// `css` ([`snowlark_style::StyleEngine::provide_style_attribute`]).
    pub fn provide_style_attribute(
        &self,
        element: NodeId,
        css: &str,
        block: &snowlark_style::DeclarationBlock,
    ) {
        if let Some(doc) = self.doc.as_deref() {
            doc.engine.provide_style_attribute(element, css, block);
        }
    }

    /// Whether the media query list `query` matches the device (`matchMedia`).
    pub fn evaluate_media(&self, query: &str) -> bool {
        let Some(doc) = self.doc.as_deref() else {
            return false;
        };
        let device = doc.engine.stylist().device();
        let url = style::stylesheets::UrlExtraData::from(doc.url.clone());
        srcset::media_matches(device, doc.engine.stylist().quirks_mode(), &url, query)
    }

    /// A count that changes whenever [`Renderer::evaluate_media`] may answer differently for some
    /// query: the device configuration changed (viewport, screen, device pixel ratio, preferred
    /// color scheme) or the document was set. While it stays the same, every media query list
    /// matches as it did.
    pub fn media_generation(&self) -> u64 {
        self.media_generation
    }

    /// `query` as a serialized media query list (`MediaQueryList.media`).
    pub fn serialize_media(&self, query: &str) -> String {
        let quirks = self
            .doc
            .as_deref()
            .map_or(style::context::QuirksMode::NoQuirks, |d| {
                d.engine.stylist().quirks_mode()
            });
        let url =
            style::stylesheets::UrlExtraData::from(Url::parse("about:blank").expect("valid URL"));
        srcset::serialize_media(&url, quirks, query)
    }

    /// The faces the document has, in cascade order: those its style sheets declare, then those a
    /// script added.
    pub fn faces(&mut self) -> Vec<FaceInfo> {
        let Some(doc) = self.doc.as_deref_mut() else {
            return Vec::new();
        };
        if Self::reconcile_faces(&self.text, &self.fonts, doc) {
            doc.env = Arc::new(StyleEnv {
                fonts: self.fonts.read().unwrap().clone(),
                images: self.image_table.clone(),
                motion_paths: doc.env.motion_paths.clone(),
            });
        }
        doc.face_order
            .iter()
            .map(|&i| {
                let f = &doc.faces[i];
                FaceInfo {
                    id: i,
                    family: f.face.family.clone(),
                    style: f.face.style.clone(),
                    weight: f.face.weight.clone(),
                    stretch: f.face.stretch.clone(),
                    unicode_range: f.face.unicode_range.clone(),
                    state: Self::face_state_of(f),
                    script: f.script,
                }
            })
            .collect()
    }

    fn face_state_of(f: &FaceLoad) -> FaceState {
        if f.loaded {
            FaceState::Loaded
        } else if !f.started {
            FaceState::Unloaded
        } else if f.source >= f.face.sources.len() {
            FaceState::Error
        } else {
            FaceState::Loading
        }
    }

    /// The state of face `id` (a removed or unknown face is in error).
    pub fn face_state(&self, id: usize) -> FaceState {
        self.doc
            .as_deref()
            .and_then(|d| d.faces.get(id))
            .filter(|f| f.active)
            .map_or(FaceState::Error, Self::face_state_of)
    }

    /// Adds a face a script made (`new FontFace(...)` added to `document.fonts`): its `src` list, or
    /// the bytes of a font it was made from. Returns its id.
    pub fn add_script_face(&mut self, face: FontFace, bytes: Option<&[u8]>) -> Option<usize> {
        let internal = bytes.map(|b| {
            crate::woff::to_sfnt(b)
                .ok()
                .and_then(|sfnt| self.text.add_font_data(&sfnt).ok())
        });
        let doc = self.doc.as_deref_mut()?;
        let mut load = FaceLoad {
            face,
            source: 0,
            loaded: false,
            active: true,
            web: None,
            script: true,
            started: bytes.is_some(),
            in_use: false,
        };
        match internal {
            Some(Some(internal)) => {
                load.loaded = true;
                load.web = Some(WebFace::described(
                    internal,
                    &load.face.weight,
                    &load.face.style,
                    &load.face.stretch,
                ));
            }
            Some(None) => load.source = load.face.sources.len(),
            None => {}
        }
        doc.faces.push(load);
        Some(doc.faces.len() - 1)
    }

    /// Puts a script's face in `document.fonts` (or takes it out): it takes part in font matching
    /// only while in.
    pub fn use_script_face(&mut self, id: usize, in_use: bool) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        let Some(f) = doc.faces.get_mut(id).filter(|f| f.script && f.active) else {
            return;
        };
        if f.in_use == in_use {
            return;
        }
        f.in_use = in_use;
        doc.face_order.retain(|&i| i != id);
        if in_use {
            doc.face_order.push(id);
        }
        Self::rebuild_fonts(&self.fonts, doc);
        doc.engine.sheets_changed();
        doc.needs_restyle = true;
        doc.env = Arc::new(StyleEnv {
            fonts: self.fonts.read().unwrap().clone(),
            images: self.image_table.clone(),
            motion_paths: doc.env.motion_paths.clone(),
        });
    }

    /// Replaces the descriptors of a script's face (its `family`, `weight`, ... changed).
    pub fn set_script_face_descriptors(&mut self, id: usize, face: FontFace) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        let Some(f) = doc.faces.get_mut(id).filter(|f| f.script && f.active) else {
            return;
        };
        f.face.family = face.family;
        f.face.weight = face.weight;
        f.face.style = face.style;
        f.face.stretch = face.stretch;
        f.face.unicode_range = face.unicode_range;
        f.face.display = face.display;
        if let Some(web) = &mut f.web {
            let internal = std::mem::take(&mut web.internal);
            *web = WebFace::described(internal, &f.face.weight, &f.face.style, &f.face.stretch);
        }
        let in_use = f.in_use;
        Self::rebuild_fonts(&self.fonts, doc);
        if in_use {
            doc.engine.sheets_changed();
            doc.needs_restyle = true;
            doc.env = Arc::new(StyleEnv {
                fonts: self.fonts.read().unwrap().clone(),
                images: self.image_table.clone(),
                motion_paths: doc.env.motion_paths.clone(),
            });
        }
    }

    /// Starts loading a script's face that has not begun.
    pub fn load_script_face(&mut self, id: usize) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        if doc.faces.get(id).is_some_and(|f| f.active && !f.started) {
            doc.faces[id].started = true;
            Self::next_font_source(&self.text, doc, id, 0);
        }
    }

    /// The faces that `families` (in order) select for the font properties and `text` (CSS Font
    /// Loading 3 "find the matching font faces"): of the faces declared for a family, those that
    /// match `weight`, `style` and `width` (in percent) best and whose `unicode-range` has a
    /// character of `text`.
    pub fn matching_faces(
        &mut self,
        families: &[String],
        weight: u16,
        style: &str,
        width: f32,
        text: &str,
    ) -> Vec<usize> {
        let faces = self.faces();
        let want = crate::env::Wanted {
            weight,
            style: crate::env::WantStyle::from_text(style),
            width,
        };
        let mut out = Vec::new();
        for family in families {
            let of: Vec<&FaceInfo> = faces
                .iter()
                .filter(|f| f.family.eq_ignore_ascii_case(family))
                .collect();
            let rank = |f: &FaceInfo| {
                crate::env::face_rank(
                    &WebFace::described(String::new(), &f.weight, &f.style, &f.stretch),
                    &want,
                )
            };
            let Some(best) = of.iter().map(|f| rank(f)).min() else {
                continue;
            };
            for f in of {
                if rank(f) == best
                    && (f.unicode_range.is_empty()
                        || text.chars().any(|c| unicode_range_has(&f.unicode_range, c)))
                {
                    out.push(f.id);
                }
            }
        }
        out
    }

    /// Web fonts (`@font-face` sources) still loading.
    pub fn fonts_loading(&mut self) -> usize {
        // (faces the style sheets declare since the last update count too)
        self.faces();
        self.doc.as_deref().map_or(0, |d| {
            d.faces
                .iter()
                .filter(|f| f.active && f.started && !f.loaded && f.source < f.face.sources.len())
                .count()
        })
    }

    /// A `<canvas>`'s bitmap changed: it shows `frame` (premultiplied BGRA) from the next paint.
    /// Whether the canvas element `node` has a frame here (a new frame then replaces it in place).
    pub fn has_canvas_frame(&self, node: NodeId) -> bool {
        self.doc
            .as_deref()
            .is_some_and(|d| d.canvases.contains_key(&node))
    }

    pub fn set_canvas_frame(&mut self, node: NodeId, frame: snowlark_paint::image::Frame) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        let (w, h) = (frame.width as f32, frame.height as f32);
        let image = snowlark_paint::image::DecodedImage {
            frames: vec![frame],
            orientation: 1,
            animation: None,
        };
        match doc.canvases.get(&node) {
            Some(&id) => self.images.replace(id, image),
            None => {
                let id = self.images.add(image);
                self.owned_images.push(id);
                doc.canvases.insert(node, id);
                doc.img_state
                    .insert(node, ImageState::Loaded(LoadedImage::bitmap(id, w, h)));
                doc.changes.restyled.push(ElementChange::reconstruct(node));
                doc.needs_restyle = true;
            }
        }
    }

    /// A media element's state from the page ([`crate::source::MediaState`]): its box is built
    /// again when it changed.
    pub fn set_media_state(&mut self, node: NodeId, state: crate::source::MediaState) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        if doc.media.get(&node) == Some(&state) {
            return;
        }
        doc.media.insert(node, state);
        doc.changes.restyled.push(ElementChange::reconstruct(node));
        doc.needs_restyle = true;
    }

    /// A video's current frame copied to memory (premultiplied BGRA): what rasterizers without
    /// the compositor's GPU frames draw.
    pub fn set_video_frame(&mut self, node: NodeId, frame: snowlark_paint::image::Frame) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        let image = snowlark_paint::image::DecodedImage {
            frames: vec![frame],
            orientation: 1,
            animation: None,
        };
        match doc.video_frames.get(&node) {
            Some(&id) => self.images.replace(id, image),
            None => {
                let id = self.images.add(image);
                self.owned_images.push(id);
                doc.video_frames.insert(node, id);
                doc.changes.restyled.push(ElementChange::reconstruct(node));
                doc.needs_restyle = true;
            }
        }
    }

    /// Sets or clears page-owned element state (`:hover`, `:active`, `:focus`, `:focus-visible`,
    /// `:focus-within`, `:checked`); the element restyles at the next restyle. Returns whether its
    /// state changed.
    /// An element's custom states (`ElementInternals.states`, `:state()`) changed.
    pub fn custom_states_changed(&mut self, node: NodeId) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        let dom_rc = doc.dom.clone();
        let dom = dom_rc.borrow();
        if !dom.is_alive(node) || dom.node_document(node) != doc.document {
            return;
        }
        doc.engine.custom_states_changed(&dom, &mut doc.data, node);
        doc.needs_restyle = true;
    }

    pub fn set_element_state(
        &mut self,
        node: NodeId,
        state: snowlark_style::ElementState,
        on: bool,
    ) -> bool {
        let Some(doc) = self.doc.as_deref_mut() else {
            return false;
        };
        let dom_rc = doc.dom.clone();
        let dom = dom_rc.borrow();
        if !dom.is_alive(node) || dom.node_document(node) != doc.document {
            return false;
        }
        let changed = doc.engine.set_state(&dom, &mut doc.data, node, state, on);
        if changed {
            doc.needs_restyle = true;
        }
        changed
    }

    /// [`Renderer::set_element_state`] for several bits at once: those of `mask` take the values
    /// `bits` has for them.
    pub fn set_element_states(
        &mut self,
        node: NodeId,
        mask: snowlark_style::ElementState,
        bits: snowlark_style::ElementState,
    ) -> bool {
        let Some(doc) = self.doc.as_deref_mut() else {
            return false;
        };
        let dom_rc = doc.dom.clone();
        let dom = dom_rc.borrow();
        if !dom.is_alive(node) || dom.node_document(node) != doc.document {
            return false;
        }
        let changed = doc.engine.set_states(&dom, &mut doc.data, node, mask, bits);
        if changed {
            doc.needs_restyle = true;
        }
        changed
    }

    /// Hands `state` bits of `node` back to the DOM-derived value (undoes
    /// [`Renderer::set_element_state`]).
    pub fn reset_element_state(
        &mut self,
        node: NodeId,
        state: snowlark_style::ElementState,
    ) -> bool {
        let Some(doc) = self.doc.as_deref_mut() else {
            return false;
        };
        let dom_rc = doc.dom.clone();
        let dom = dom_rc.borrow();
        if !dom.is_alive(node) || dom.node_document(node) != doc.document {
            return false;
        }
        doc.engine.reset_state(&dom, &mut doc.data, node, state);
        doc.needs_restyle = true;
        true
    }

    /// Detached nodes the page is about to free: what this renderer keeps for them goes.
    pub fn nodes_freed(&mut self, freed: &rustc_hash::FxHashSet<NodeId>) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        doc.links.retain(|n, _| !freed.contains(n));
        doc.img_src.retain(|n, _| !freed.contains(n));
        doc.page_imgs.retain(|n| !freed.contains(n));
        doc.posters_released.retain(|n| !freed.contains(n));
        doc.img_state.retain(|n, _| !freed.contains(n));
        doc.scroll.retain(|n, _| !freed.contains(n));
        doc.render_blocking.retain(|n| !freed.contains(n));
        doc.canvases.retain(|n, _| !freed.contains(n));
        doc.media.retain(|n, _| !freed.contains(n));
        doc.video_frames.retain(|n, _| !freed.contains(n));
        doc.control_states.retain(|n, _| !freed.contains(n));
        doc.changes.restyled.retain(|c| !freed.contains(&c.node));
        doc.changes.children.retain(|n| !freed.contains(n));
        doc.changes.text.retain(|n| !freed.contains(n));
    }

    /// The document's top layer changed (a modal dialog shown or closed, a popover...): the boxes
    /// are built again at the next layout, the top layer's over everything.
    pub fn set_top_layer(&mut self, top_layer: Vec<NodeId>) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        if doc.top_layer != top_layer {
            doc.top_layer = top_layer;
            doc.needs_build = true;
        }
    }

    /// The document's fullscreen element changed: the viewport's overflow (its scrollbar) follows
    /// at the next layout.
    pub fn set_fullscreen_element(&mut self, fullscreen: Option<NodeId>) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        if doc.fullscreen != fullscreen {
            doc.fullscreen = fullscreen;
            doc.needs_build = true;
        }
    }

    /// A shadow root was attached to `host` (DOM "attach a shadow root"): the host's light children
    /// leave the flat tree (the slotted ones come back through their slots), so its styles and the
    /// boxes inside it are made again.
    pub fn shadow_root_attached(&mut self, host: NodeId) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        let dom_rc = doc.dom.clone();
        let dom = dom_rc.borrow();
        if !dom.is_alive(host) || dom.node_document(host) != doc.document {
            return;
        }
        doc.engine.shadow_root_attached(&dom, &mut doc.data, host);
        doc.needs_restyle = true;
        doc.changes
            .restyled
            .push(ElementChange::restyled(host, BoxDamage::Repaint, true));
    }

    /// `slot.assign()` set a slot's manually assigned nodes: the flat tree under its shadow root
    /// changes, and the host's boxes with it.
    pub fn slot_assigned(&mut self, slot: NodeId, nodes: Vec<NodeId>) {
        let Some(doc) = self.doc.as_deref_mut() else {
            return;
        };
        let dom_rc = doc.dom.clone();
        let dom = dom_rc.borrow();
        if !dom.is_alive(slot) || dom.node_document(slot) != doc.document {
            return;
        }
        doc.engine
            .set_manual_slot_assignment(&dom, &mut doc.data, slot, nodes);
        doc.needs_restyle = true;
        if let Some(host) = dom.host(dom.root(slot)) {
            doc.changes
                .restyled
                .push(ElementChange::restyled(host, BoxDamage::Repaint, true));
        }
    }

    /// A form control's state from the page changed (its value, checkedness, selectedness, caret):
    /// its box shows it from the next layout. Returns whether it differs from the state its box had.
    pub fn set_control_state(
        &mut self,
        node: NodeId,
        state: crate::controls::ControlState,
    ) -> bool {
        let Some(doc) = self.doc.as_deref_mut() else {
            return false;
        };
        if doc.control_states.get(&node) == Some(&state) {
            return false;
        }
        doc.control_states.insert(node, state);
        doc.changes.restyled.push(ElementChange::reconstruct(node));
        true
    }

    /// The decoded pixels an `<img>` shows (for `drawImage`).
    pub fn image_pixels(&self, node: NodeId) -> Option<Arc<snowlark_paint::image::DecodedImage>> {
        let doc = self.doc.as_deref()?;
        match doc.img_state.get(&node)? {
            ImageState::Loaded(i) => self.images.get(i.id),
            _ => None,
        }
    }

    /// What canvas text is laid out with: this document's text engine and `@font-face` families, and
    /// (only for the CPU backend: loading them takes tens of milliseconds) usvg's system fonts.
    pub fn system_fonts(&self) -> crate::canvas::Fonts {
        crate::canvas::Fonts {
            text: self.text.clone(),
            env: self.doc.as_ref().map(|d| d.env.clone()).unwrap_or_default(),
            cache: self
                .doc
                .as_ref()
                .map(|d| d.canvas_text.clone())
                .unwrap_or_default(),
            cpu: match crate::canvas::Backend::from_env() {
                crate::canvas::Backend::Skia => Some(self.svg.fonts()),
                crate::canvas::Backend::Gpu(_) => None,
            },
        }
    }

    /// Whether an `@import` of the document's sheets is still being fetched.
    pub fn imports_pending(&self) -> bool {
        self.doc.as_ref().is_some_and(|d| {
            d.imports
                .entries
                .lock()
                .unwrap()
                .values()
                .any(|e| matches!(e, ImportEntry::Pending))
        })
    }

    /// Fetches in flight for fonts a script's `FontFace` asked for: a screenshot does not wait for
    /// them (script observes their arrival through `FontFace.loaded`).
    pub fn script_font_fetches(&self) -> usize {
        self.doc.as_ref().map_or(0, |d| {
            d.faces
                .iter()
                .filter(|f| {
                    f.script
                        && f.active
                        && f.started
                        && !f.loaded
                        && f.source < f.face.sources.len()
                })
                .count()
        })
    }

    /// Fetches in flight.
    pub fn pending(&self) -> usize {
        self.doc.as_ref().map_or(0, |d| d.fetcher.len())
    }

    /// Blocks until a fetch makes progress or `timeout` passes.
    pub fn wait(&self, timeout: Duration) {
        if let Some(d) = &self.doc {
            d.fetcher.wait(timeout);
        }
    }

    /// Pumps and updates the rendering's styles (as [`Renderer::update_rendering`] does: nothing while
    /// render-blocked, then the timeline moving with the clock) until no fetch is in flight (each
    /// arrival may start more: imports, images and fonts that styles reveal) or `timeout` passes.
    pub fn render_until_settled(&mut self, timeout: Duration) -> Settled {
        let start = Instant::now();
        loop {
            self.pump();
            self.update_style_for_rendering();
            let pending = self.pending();
            if pending == 0 {
                break;
            }
            let left = timeout.saturating_sub(start.elapsed());
            if left.is_zero() {
                break;
            }
            self.wait(left.min(Duration::from_millis(50)));
        }
        Settled {
            complete: self.pending() == 0,
            pending: self.pending(),
            requests: self.doc.as_ref().map_or(0, |d| d.requests),
        }
    }

    /// The height of the laid-out document (its scrollable overflow), in CSS px.
    pub fn document_height(&self) -> f32 {
        self.fragments()
            .map(|f| f.overflow.y + f.overflow.height)
            .unwrap_or(0.0)
    }

    /// Paints the last layout headless at `size` CSS px (`scale` device pixels per CSS px) with the
    /// viewport scrolled by `scroll`.
    pub fn rasterize(&mut self, size: Size, scroll: Point, scale: f32) -> Result<Pixmap, String> {
        let list = self.paint_scaled(size, scroll, scale);
        let t = Instant::now();
        let mut renderer = HeadlessRenderer::new().map_err(|e| e.to_string())?;
        let sources = Sources {
            text: &self.text,
            images: &self.images,
        };
        let view = Viewport {
            width: (size.width * scale).ceil() as u32,
            height: (size.height * scale).ceil() as u32,
            scale,
            background: self.canvas_color(),
        };
        let pixmap = renderer
            .render(&list, sources, &view)
            .map_err(|e| e.to_string())?;
        self.timings.raster_ms += ms(t);
        Ok(pixmap)
    }

    /// Lays out at `viewport`, paints and writes a PNG of `size` CSS px (at least the viewport; taller
    /// to show more of the page) scrolled by `scroll`.
    pub fn render_png(
        &mut self,
        path: &Path,
        viewport: Size,
        size: Size,
        scroll: Point,
    ) -> Result<(), String> {
        self.layout(viewport).ok_or("no document")?;
        let pixmap = self.rasterize(size, scroll, self.config.device_pixel_ratio)?;
        let t = Instant::now();
        pixmap.write_png(path).map_err(|e| e.to_string())?;
        self.timings.raster_ms += ms(t);
        Ok(())
    }
}

/// CSS Fonts 4 §4.? `font-weight` descriptor: one weight or a range; `normal`/`bold` keywords.
/// Whether `c` is in a `unicode-range` descriptor's text (`U+26`, `U+0-7F`, `U+4??`, ...).
fn unicode_range_has(range: &str, c: char) -> bool {
    let c = c as u32;
    range.split(',').any(|part| {
        let part = part.trim();
        let Some(body) = part.strip_prefix("U+").or_else(|| part.strip_prefix("u+")) else {
            return false;
        };
        let hex = |t: &str| u32::from_str_radix(t, 16).ok();
        if let Some((a, b)) = body.split_once('-') {
            matches!((hex(a), hex(b)), (Some(a), Some(b)) if a <= c && c <= b)
        } else if body.contains('?') {
            let lo = hex(&body.replace('?', "0"));
            let hi = hex(&body.replace('?', "F"));
            matches!((lo, hi), (Some(a), Some(b)) if a <= c && c <= b)
        } else {
            hex(body) == Some(c)
        }
    })
}

/// What an element's `clip-path: url()` names now.
#[derive(Default)]
pub(crate) struct ClipUse {
    /// Resolved at least once (`hash` means something).
    resolved: bool,
    /// Of what is written for the `clipPath`s: resolved again when that changes.
    hash: u64,
    /// The geometry, when the reference names a `clipPath`.
    pub(crate) reference: Option<Arc<snowlark_layout::style::ClipReference>>,
}

/// What the `url()`s of an element's `filter` and `backdrop-filter` name: each URL with its
/// `<filter>`, `None` when it names none.
pub(crate) type FilterReferences = Vec<(String, Option<Arc<crate::filter_ref::FilterRef>>)>;

/// A computed URL as written in [`FilterReferences`].
pub(crate) fn url_text(u: &style::values::computed::url::ComputedUrl) -> String {
    use style::values::computed::url::ComputedUrl;
    match u {
        ComputedUrl::Valid(url) => url.to_string(),
        ComputedUrl::Invalid(text) => text.to_string(),
    }
}

/// The URLs of an element's `filter` and `backdrop-filter` lists.
fn filter_urls(values: &ComputedValues) -> Vec<String> {
    use style::values::generics::effects::Filter as F;
    let e = values.get_effects();
    let mut out: Vec<String> = Vec::new();
    for f in e.filter.0.iter().chain(e.backdrop_filter.0.iter()) {
        if let F::Url(u) = f {
            let u = url_text(u);
            if !out.contains(&u) {
                out.push(u);
            }
        }
    }
    out
}

/// The URL of a computed `clip-path: url()`.
fn clip_url(values: &ComputedValues) -> Option<String> {
    use style::values::computed::url::ComputedUrl;
    match &values.get_svg().clip_path {
        style::values::generics::basic_shape::GenericClipPath::Url(u) => Some(match u {
            ComputedUrl::Valid(url) => url.to_string(),
            ComputedUrl::Invalid(text) => text.to_string(),
        }),
        _ => None,
    }
}

/// The URLs `image` loads; `dark`: the used color scheme is dark (`light-dark()` picks by it).
fn image_urls(image: &c::Image, dark: bool, out: &mut Vec<String>) {
    match image {
        c::Image::Url(u) => {
            if let Some(u) = u.url() {
                out.push(u.as_str().to_owned());
            }
        }
        c::Image::ImageSet(set) => {
            if let Some(item) = set.items.get(set.selected_index) {
                image_urls(&item.image, dark, out);
            }
        }
        c::Image::LightDark(ld) => image_urls(if dark { &ld.dark } else { &ld.light }, dark, out),
        c::Image::Fallback(f) => {
            if let Some(i) = &f.source {
                image_urls(i, dark, out);
            }
        }
        c::Image::CrossFade(cf) => {
            for e in cf.elements.iter() {
                if let style::values::generics::image::CrossFadeImage::Image(i) = &e.image {
                    image_urls(i, dark, out);
                }
            }
        }
        _ => {}
    }
}

/// The image URLs a computed style refers to, with whether each affects boxes (list-style-image,
/// `content: url()`) rather than only painting (backgrounds).
fn collect_style_urls(values: &ServoArc<ComputedValues>, out: &mut Vec<(String, bool)>) {
    use style::values::generics::counters::{GenericContent, GenericContentItem};
    let mut urls = Vec::new();
    let dark =
        crate::adapter::used_color_scheme(values) == snowlark_layout::style::ColorScheme::Dark;
    for image in values.get_background().background_image.0.iter() {
        image_urls(image, dark, &mut urls);
    }
    for image in values.get_svg().mask_image.0.iter() {
        image_urls(image, dark, &mut urls);
    }
    image_urls(&values.get_border().border_image_source, dark, &mut urls);
    out.extend(urls.drain(..).map(|u| (u, false)));
    if values.get_box().display.is_list_item() {
        image_urls(&values.get_list().list_style_image, dark, &mut urls);
    }
    if let GenericContent::Items(items) = &values.get_counters().content {
        for item in items.items.iter() {
            if let GenericContentItem::Image(i) = item {
                image_urls(i, dark, &mut urls);
            }
        }
    }
    out.extend(urls.drain(..).map(|u| (u, true)));
}

/// HTML §4.8.4.3.5 "update the source set" and §4.8.4.3.6 "select an image source" for an `img`:
/// the first matching `<source>` of a `<picture>` parent, else the `img`'s own `srcset` and `src`.
/// Returns the absolute URL (or the candidate as written when it does not resolve) and its density.
/// `auto` is the size an `auto` source size stands for when the `img` allows auto-sizes (see
/// [`srcset::parse_sizes`]); a `source` without `sizes` before such an `img` is `auto` then.
#[allow(clippy::too_many_arguments)]
fn select_img_source(
    dom: &Dom,
    img: NodeId,
    base: &Url,
    device: &style::device::Device,
    quirks: style::context::QuirksMode,
    url_data: &style::stylesheets::UrlExtraData,
    dpr: f32,
    auto: Option<f32>,
) -> Option<(String, f32)> {
    // (a candidate's URL is resolved as given: only ASCII white space was split off it; HTML
    // "encoding-parse-and-serialize a URL", with the document's encoding for the query)
    let encoding = dom.encoding(dom.node_document(img));
    let resolve = |s: &str| {
        crate::encoding_parse_url(
            base,
            encoding,
            s.trim_matches(|c: char| c.is_ascii_whitespace()),
        )
        .map(|u| u.to_string())
    };
    let choose = |srcset: Option<&str>, sizes: Option<&str>, src: Option<&str>| {
        let mut candidates = srcset.map(srcset::parse_srcset).unwrap_or_default();
        if let Some(src) = src.filter(|s| !s.trim().is_empty()) {
            // `src` is a 1x candidate unless one already has density 1.
            if !candidates
                .iter()
                .any(|c| c.density == Some(1.0) || c.width.is_some())
            {
                candidates.push(srcset::Candidate {
                    url: src.to_owned(),
                    density: Some(1.0),
                    width: None,
                });
            }
        }
        let size = match sizes {
            Some(s) => srcset::source_size(device, quirks, url_data, s, auto),
            None => auto.unwrap_or_else(|| device.au_viewport_size().width.to_f32_px()),
        };
        srcset::select(&candidates, size, dpr)
    };
    if let Some(parent) = dom.parent_element(img) {
        if dom.is_html_element_named(parent, &local_name!("picture")) {
            for s in dom.element_children(parent) {
                if s == img {
                    break;
                }
                if !dom.is_html_element_named(s, &local_name!("source")) {
                    continue;
                }
                let Some(srcset) = dom.get_attribute(s, "srcset") else {
                    continue;
                };
                if !srcset::media_matches(
                    device,
                    quirks,
                    url_data,
                    dom.get_attribute(s, "media").unwrap_or(""),
                ) {
                    continue;
                }
                if !srcset::supported_type(dom.get_attribute(s, "type").unwrap_or("")) {
                    continue;
                }
                if let Some((u, d)) = choose(Some(srcset), dom.get_attribute(s, "sizes"), None) {
                    return Some((resolve(&u).unwrap_or(u), d));
                }
            }
        }
    }
    let attr = |n| dom.attr(img, &ns!(), &n).filter(|_| true);
    let (u, d) = choose(
        attr(local_name!("srcset")),
        attr(local_name!("sizes")),
        attr(local_name!("src")),
    )?;
    // (a candidate that does not resolve stays as written: its request fails, HTML "update the
    // image data" step 10)
    Some((resolve(&u).unwrap_or(u), d))
}

/// Adds the counts of a second layout pass to the first's.
fn sum_layout_stats(a: &mut LayoutStats, b: LayoutStats) {
    a.laid_out += b.laid_out;
    a.reused += b.reused;
    a.relaid += b.relaid;
    a.shaped += b.shaped;
    a.shape_cache_hits += b.shape_cache_hits;
    a.words_shaped += b.words_shaped;
    a.words_cached += b.words_cached;
    a.containers_built += b.containers_built;
    a.algorithm_runs += b.algorithm_runs;
    a.prepare_us += b.prepare_us;
}

/// Whether [`Renderer::dom_changed`] needs the own `Inserted` and `Removed` reactions of an element
/// that is a descendant of an inserted or removed node (for an [`snowlark_dom::ReactionFilter`]):
/// `style` and `link` elements, whose sheets come and go with them. The inserted or removed node's
/// own reaction covers the rest of its subtree.
pub fn wants_descendant_reaction(e: &snowlark_dom::ElementData) -> bool {
    (e.name().ns == ns!(html)
        && matches!(e.name().local, local_name!("style") | local_name!("link")))
        || (e.name().ns == ns!(svg) && e.name().local == local_name!("style"))
}

/// How the viewport's scroll bar is drawn (see [`Renderer::viewport_scrollbar`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewportScrollbar {
    /// CSS px; 0 when no classic bar shows.
    pub width: f32,
    /// The root's used color scheme: the automatic colors follow it.
    pub color_scheme: snowlark_layout::style::ColorScheme,
    /// The root's `scrollbar-color` (`None`: `auto`).
    pub colors: Option<snowlark_layout::style::ScrollbarColors>,
}

/// The width of the viewport's classic scroll bar for the embedder's `full` width and the root's
/// `scrollbar-width` (CSS Scrollbars 1 §3: `thin` thinner, as Chrome's 11 px to 17, `none` none).
pub fn viewport_scrollbar_width(full: f32, width: snowlark_layout::style::ScrollbarWidth) -> f32 {
    use snowlark_layout::style::ScrollbarWidth as W;
    match width {
        W::Auto => full,
        W::Thin => (full * 11.0 / 17.0).round(),
        W::None => 0.0,
    }
}

/// The canvas surface's color for the root's used color scheme (CSS Color Adjust 1 §2.4).
pub fn canvas_color(scheme: snowlark_layout::style::ColorScheme) -> Color {
    match scheme {
        snowlark_layout::style::ColorScheme::Light => Color::WHITE,
        snowlark_layout::style::ColorScheme::Dark => Color::rgb(0x12, 0x12, 0x12),
    }
}
