// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`StyleEngine`]: one document's Stylist (device, user-agent, user and author sheets, shadow roots'
//! sheets), the DOM change hooks that turn mutations into snapshots and restyle hints, and the restyle.

use std::sync::{Arc as StdArc, Mutex, Once};

use rustc_hash::{FxHashMap, FxHashSet};
use selectors::matching::{
    ElementSelectorFlags, MatchingContext, MatchingForInvalidation, MatchingMode,
    NeedsSelectorFlags, SelectorCaches,
};
use selectors::parser::SelectorList;
use servo_arc::Arc;
use smallvec::SmallVec;
use snowlark_dom::{
    local_name, ns, CustomElementState, Dom, LocalName, Namespace, NodeId, Reaction,
};
use style::animation::{AnimationSetKey, DocumentAnimationSet};
use style::attr::{AttrIdentifier, AttrValue};
use style::context::{
    QuirksMode, RegisteredSpeculativePainter, RegisteredSpeculativePainters, SharedStyleContext,
    StyleContext, ThreadLocalStyleContext,
};
use style::data::ElementStyles;
use style::device::servo::FontMetricsProvider;
use style::device::Device;
use style::dom::{TElement, TNode};
use style::font_metrics::FontMetrics;
use style::global_style_data::{GLOBAL_STYLE_DATA, STYLE_THREAD_POOL};
use style::invalidation::element::invalidator::{InvalidationResult, SiblingTraversalMap};
use style::invalidation::element::relative_selector::{
    DomMutationOperation, RelativeSelectorInvalidator,
};
use style::invalidation::element::restyle_hints::RestyleHint;
use style::media_queries::MediaType;
use style::properties::style_structs::Font;
use style::properties::{
    parse_one_declaration_into, parse_style_attribute, ComputedValues, Importance, LonghandId,
    PropertyDeclaration, PropertyDeclarationBlock, PropertyId, SourcePropertyDeclaration,
};
use style::queries::values::PrefersColorScheme;
use style::selector_parser::{PseudoElement, SelectorImpl, SelectorParser, SnapshotMap};
use style::servo::selector_parser::ServoElementSnapshot;
use style::shared_lock::{Locked, SharedRwLock, StylesheetGuards};
use style::stylesheets::scope_rule::ImplicitScopeRoot;
use style::stylesheets::{CssRuleType, DocumentStyleSheet, Origin, UrlExtraData};
use style::stylist::{RuleInclusion, Stylist};
use style::thread_state::{self, ThreadState};
use style::traversal::{resolve_style, DomTraversal};
use style::traversal_flags::TraversalFlags;
use style::values::computed::font::{GenericFontFamily, QueryFontMetricsFlags};
use style::values::computed::{CSSPixelLength, Length};
use style::values::specified::color::ColorSchemeFlags;
use style::values::GenericAtomIdent;
use style::Atom;
use stylo_dom::ElementState;
use url::Url;

use crate::declarations::DeclarationBlock;
use crate::handle::{
    flat_parent, node_from_opaque, opaque_element, opaque_node, ShadowStyles, StyleElement,
    TreeView,
};
use crate::sheets::{
    self, FontFace, FontFaceHandler, ImportLoader, Sheet, SheetOrigin, QUIRKS_CSS, UA_CSS,
};
use crate::table::{bits, AttrCache, StyleData};
use crate::traversal::{Recalc, RestyleStats};

/// `prefers-color-scheme` (Media Queries 5 §11.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ColorScheme {
    #[default]
    Light,
    Dark,
}

/// The media type (Media Queries 4 §2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MediaKind {
    #[default]
    Screen,
    Print,
}

/// Why [`StyleEngine::register_property`] refused a registration (CSS Properties and Values API 1
/// §2.4: `InvalidModificationError` and `SyntaxError`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterPropertyError {
    /// The name is registered already.
    AlreadyRegistered,
    /// The name, the syntax or the initial value is invalid.
    Invalid,
}

/// Whether restyles use stylo's thread pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Parallelism {
    /// Every restyle is offered the pool: the traversal starts on the calling thread and shares a level
    /// of the tree with the pool's threads once that level has enough elements, so a small restyle
    /// never leaves the calling thread and a large one (however its elements became dirty) is shared.
    #[default]
    Parallel,
    /// Every restyle runs on the calling thread.
    Sequential,
}

/// Whether `element` is a `style` element whose text is a style sheet: HTML's (HTML §4.2.6) or
/// SVG's (SVG 2 §6.4, the same processing).
pub fn is_style_element(dom: &Dom, element: NodeId) -> bool {
    dom.element(element).is_some_and(|e| {
        e.name().local == local_name!("style")
            && (e.name().ns == ns!(html) || e.name().ns == ns!(svg))
    })
}

/// What media queries and units resolve against.
#[derive(Clone, Debug)]
pub struct DeviceConfig {
    /// Viewport width and height in CSS pixels (the initial containing block; `vw`/`vh`).
    pub viewport: (f32, f32),
    /// Device pixels per CSS pixel (`resolution`).
    pub device_pixel_ratio: f32,
    /// The output device's screen in CSS pixels (`device-width`, `device-height`,
    /// `device-aspect-ratio`): the top-level viewport, which a frame's document shares; `None` is
    /// the viewport itself.
    pub screen: Option<(f32, f32)>,
    /// The `medium` font size for proportional fonts, in CSS pixels.
    pub default_font_size: f32,
    /// The `medium` font size for `monospace`, in CSS pixels.
    pub default_monospace_font_size: f32,
    pub color_scheme: ColorScheme,
    pub media: MediaKind,
    /// Whether scripting is enabled for the document (HTML §8.1.3.4), which hides `noscript` (the UA
    /// sheet's `@media (scripting)` rule).
    pub scripting: bool,
    /// The pointing devices (`pointer`, `hover`, `any-pointer`, `any-hover`).
    pub input: crate::InputDevices,
    /// Font metrics for `ex`, `ch`, `cap`, `ic` (the paint crate's text engine). Without one, stylo's
    /// fallbacks apply (0.5em, 0.5em, ...).
    pub font_metrics: Option<StdArc<dyn FontMetricsProvider + Send>>,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        DeviceConfig {
            viewport: (1024.0, 768.0),
            device_pixel_ratio: 1.0,
            screen: None,
            default_font_size: 16.0,
            default_monospace_font_size: 13.0,
            color_scheme: ColorScheme::Light,
            media: MediaKind::Screen,
            scripting: true,
            input: crate::InputDevices::MOUSE,
            font_metrics: None,
        }
    }
}

#[derive(Debug)]
struct Metrics {
    proportional: f32,
    monospace: f32,
    external: Option<StdArc<dyn FontMetricsProvider + Send>>,
}

impl FontMetricsProvider for Metrics {
    fn query_font_metrics(
        &self,
        vertical: bool,
        font: &Font,
        base_size: CSSPixelLength,
        flags: QueryFontMetricsFlags,
    ) -> FontMetrics {
        match &self.external {
            Some(m) => m.query_font_metrics(vertical, font, base_size, flags),
            None => FontMetrics::default(),
        }
    }

    fn base_size_for_generic(&self, generic: GenericFontFamily) -> Length {
        Length::new(match generic {
            GenericFontFamily::Monospace => self.monospace,
            _ => self.proportional,
        })
    }
}

fn media_type(media: MediaKind) -> MediaType {
    match media {
        MediaKind::Screen => MediaType::screen(),
        MediaKind::Print => MediaType::print(),
    }
}

fn prefers_color_scheme(scheme: ColorScheme) -> PrefersColorScheme {
    match scheme {
        ColorScheme::Light => PrefersColorScheme::Light,
        ColorScheme::Dark => PrefersColorScheme::Dark,
    }
}

/// Whether two configurations have the same font metrics provider (the same object).
fn same_font_metrics(
    a: &Option<StdArc<dyn FontMetricsProvider + Send>>,
    b: &Option<StdArc<dyn FontMetricsProvider + Send>>,
) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => std::ptr::addr_eq(StdArc::as_ptr(a), StdArc::as_ptr(b)),
        _ => false,
    }
}

/// Records the root element's style on `device` for root-relative units, as stylo's restyle of the
/// root does when they change (`finish_restyle` in its `matching.rs`): its font
/// size (`rem`), its line height (`rlh`) and the style the font metrics of `rex`, `rch`, `rcap`
/// and `ric` are queried from.
fn seed_root(device: &Device, root: &Arc<ComputedValues>) {
    device.set_root_style(root);
    let zoom = root.effective_zoom;
    let size = root.get_font().clone_font_size().computed_size();
    device.set_root_font_size(zoom.unzoom(size.px()));
    let line_height = device
        .calc_line_height(root.get_font(), root.writing_mode, None)
        .0;
    device.set_root_line_height(zoom.unzoom(line_height.px()));
    if device.used_root_font_metrics() {
        device.update_root_font_metrics();
    }
}

fn make_device(config: &DeviceConfig, quirks_mode: QuirksMode, page: ColorSchemeFlags) -> Device {
    let viewport = euclid::Size2D::new(config.viewport.0, config.viewport.1);
    let screen = config.screen.unwrap_or(config.viewport);
    let mut device = Device::new(
        media_type(config.media),
        quirks_mode,
        viewport,
        euclid::Size2D::new(
            screen.0 * config.device_pixel_ratio,
            screen.1 * config.device_pixel_ratio,
        ),
        euclid::Scale::new(config.device_pixel_ratio),
        Box::new(Metrics {
            proportional: config.default_font_size,
            monospace: config.default_monospace_font_size,
            external: config.font_metrics.clone(),
        }),
        ComputedValues::initial_values_with_font_override(Font::initial_values()),
        prefers_color_scheme(config.color_scheme),
        config.input.primary_pointer(),
        config.input.all_pointers(),
    );
    device.set_page_color_scheme(page);
    device.set_scripting_enabled(config.scripting);
    device
}

/// The `meta` elements that may set the page color scheme, collected as elements are prepared
/// (a lock because preparation borrows the engine shared; only `meta` elements take it).
#[derive(Default)]
struct MetaCandidates {
    elements: Vec<NodeId>,
    /// One was inserted, removed or changed its `name` or `content` since the last restyle.
    dirty: bool,
}

struct NoPainters;

impl RegisteredSpeculativePainters for NoPainters {
    fn get(&self, _name: &Atom) -> Option<&dyn RegisteredSpeculativePainter> {
        None
    }
}

/// Serializes parallel traversals: stylo's per-worker caches (the style sharing cache, the bloom filter)
/// are thread-locals that a traversal borrows for its whole duration, so two traversals in flight on the
/// shared pool (two tabs, two tests) would collide on a worker. A lock, not state.
static PARALLEL_TRAVERSAL: Mutex<()> = Mutex::new(());

/// Creates stylo's thread pool from a short-lived thread. The pool adopts the thread that creates it as
/// worker 0 (`use_current_thread`); a page thread adopted that way would carry the worker thread state.
/// That worker never takes work: the thread that starts a traversal styles in its place.
fn ensure_pool() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = std::thread::spawn(|| {
            let _ = STYLE_THREAD_POOL.pool().is_some();
        })
        .join();
    });
}

/// An attribute's value before the first change since the last restyle: namespace, local name, value
/// (`None` when it was absent).
type OldAttr = (Namespace, LocalName, Option<String>);

/// Parsed presentational hints by the text of their declarations, so that elements with the same
/// presentation attributes share one block: one parse, and one rule node, which also lets them
/// share styles (the sharing check compares hint blocks by identity). The text is everything the
/// parse reads besides the document's URL and quirks mode and the lock, so a change of those
/// empties the cache. Bounded: it is emptied when it reaches [`HintBlocks::MAX`] entries.
/// (Blink keeps such a cache for HTML elements' presentation attribute style,
/// `PresentationAttributeCache`.)
///
/// SVG presentation attributes are parsed one by one, each as a value of its property, and the
/// parsed values are kept by property and value as written ([`HintValues`]): an element whose
/// attributes are new as a whole (a chart's `path`, with its own `d`) parses only its new values.
/// SVG blocks are found by a hash of the element's attributes, compared in place on a hit (no key
/// text is built), and kept with their key only from the second element that has those attributes:
/// a chart's shapes mostly have attributes of their own, and keeping a key for each would fill the
/// cache and empty it again. The first element's block waits by hash alone and is given to the
/// second when their declarations are the same. (The reference engine keys its cache by a hash of
/// the attributes, checked
/// against them, and keeps no SVG element's: each parses its presentation attributes alone, into
/// its own property set.)
#[derive(Default)]
struct HintBlocks {
    lock: Option<SharedRwLock>,
    url: String,
    quirks_mode: Option<QuirksMode>,
    map: FxHashMap<Box<str>, Arc<Locked<PropertyDeclarationBlock>>>,
    /// SVG blocks by the hash of their attributes ([`svg_key_hash`]): each with its key text
    /// ([`svg_key_text`]), compared with an element's attributes on a hit.
    svg: FxHashMap<u64, SmallVec<[KeptSvgBlock; 1]>>,
    svg_len: usize,
    /// The blocks of SVG attribute sets seen once, by hash (no key text), with the element they
    /// were built for: given to the second element with the same declarations, and then kept in
    /// `svg`.
    svg_seen: FxHashMap<u64, (NodeId, Arc<Locked<PropertyDeclarationBlock>>)>,
    /// The element whose block, its own until now, the last [`HintBlocks::svg_block`] gave to
    /// another element (see [`StyleEngine::note_hints_shared`]).
    shared_first: Option<(NodeId, Arc<Locked<PropertyDeclarationBlock>>)>,
    values: HintValues,
}

/// An element's presentational hints as one block, from [`StyleEngine::hints_cache`].
struct Hints {
    block: Option<Arc<Locked<PropertyDeclarationBlock>>>,
    /// The block was built for this element and no other element holds it.
    unshared: bool,
}

/// A kept SVG hint block with its key text ([`svg_key_text`]).
type KeptSvgBlock = (Box<str>, Arc<Locked<PropertyDeclarationBlock>>);

/// The hash of an SVG element's presentation attributes and language, the key of
/// [`HintBlocks::svg`].
fn svg_key_hash(list: &[(&'static str, &str)], lang: Option<&str>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    for (property, value) in list {
        property.hash(&mut h);
        value.hash(&mut h);
    }
    lang.hash(&mut h);
    h.finish()
}

/// The key text of an SVG element's presentation attributes and language: each property, `:`, its
/// value as written and `;`, then the language after a NUL.
fn svg_key_text(list: &[(&'static str, &str)], lang: Option<&str>) -> Box<str> {
    let mut key = String::with_capacity(
        list.iter()
            .map(|(p, v)| p.len() + v.len() + 2)
            .sum::<usize>()
            + lang.map_or(0, |l| l.len() + 1),
    );
    for (property, value) in list {
        key.push_str(property);
        key.push(':');
        key.push_str(value);
        key.push(';');
    }
    if let Some(lang) = lang {
        key.push('\0');
        key.push_str(lang);
    }
    key.into_boxed_str()
}

/// Whether `key` is [`svg_key_text`] of these attributes and language, compared in place.
fn svg_key_is(key: &str, list: &[(&'static str, &str)], lang: Option<&str>) -> bool {
    let mut rest = key;
    for (property, value) in list {
        let Some(r) = rest.strip_prefix(property) else {
            return false;
        };
        let Some(r) = r.strip_prefix(':') else {
            return false;
        };
        let Some(r) = r.strip_prefix(value) else {
            return false;
        };
        let Some(r) = r.strip_prefix(';') else {
            return false;
        };
        rest = r;
    }
    match lang {
        None => rest.is_empty(),
        Some(lang) => rest.strip_prefix('\0') == Some(lang),
    }
}

/// Parsed values of SVG presentation attributes: per property, its id (`None`: not one this build
/// knows) and the declarations each value text parsed to (none: invalid).
#[derive(Default)]
struct HintValues {
    by_property: FxHashMap<&'static str, (Option<PropertyId>, ParsedValues)>,
    len: usize,
}

/// One property's values as written and the declarations each parsed to.
type ParsedValues = FxHashMap<Box<str>, Box<[PropertyDeclaration]>>;

impl HintBlocks {
    const MAX: usize = 4096;
    /// SVG blocks kept, at most (then emptied, as the HTML ones are).
    const MAX_SVG: usize = 4096;
    /// SVG attribute sets seen once, at most (then forgotten).
    const MAX_SVG_SEEN: usize = 4096;
    /// Parsed values kept, at most (then emptied, as the blocks are).
    const MAX_VALUES: usize = 4096;
    /// Longer values are parsed every time and not kept: they are a path's `d` or a long list,
    /// rarely the same on two elements.
    const MAX_VALUE_LEN: usize = 64;

    /// Empties the cache unless its blocks were parsed with this lock, URL and quirks mode.
    fn validate(&mut self, lock: &SharedRwLock, url: &str, quirks_mode: QuirksMode) {
        let same = self.lock.as_ref().is_some_and(|l| l.same_lock(lock))
            && self.url == url
            && self.quirks_mode == Some(quirks_mode);
        if !same {
            self.map.clear();
            self.svg.clear();
            self.svg_len = 0;
            self.svg_seen.clear();
            self.shared_first = None;
            self.values = HintValues::default();
            self.lock = Some(lock.clone());
            url.clone_into(&mut self.url);
            self.quirks_mode = Some(quirks_mode);
        }
    }

    fn insert(&mut self, css: String, block: Arc<Locked<PropertyDeclarationBlock>>) {
        if self.map.len() >= Self::MAX {
            self.map.clear();
        }
        self.map.insert(css.into_boxed_str(), block);
    }

    /// An SVG element's presentation attributes as one block (and `lang`'s `-x-lang`), shared by
    /// the elements whose attributes are the same.
    fn svg_block(
        &mut self,
        dom: &Dom,
        id: NodeId,
        lang: Option<String>,
        url: &UrlExtraData,
        quirks_mode: QuirksMode,
        lock: &SharedRwLock,
    ) -> Hints {
        let shared = |block| Hints {
            block: Some(block),
            unshared: false,
        };
        let mut list = SmallVec::new();
        crate::hints::svg_presentation_list(dom, id, &mut list);
        if list.is_empty() && lang.is_none() {
            return Hints {
                block: None,
                unshared: false,
            };
        }
        let lang = lang.as_deref();
        let hash = svg_key_hash(&list, lang);
        if let Some(entries) = self.svg.get(&hash) {
            if let Some((_, block)) = entries.iter().find(|(k, _)| svg_key_is(k, &list, lang)) {
                return shared(block.clone());
            }
        }
        let mut block = PropertyDeclarationBlock::new();
        for &(property, value) in &list {
            self.declare(&mut block, property, value, url, quirks_mode);
        }
        if let Some(lang) = lang {
            block.push(
                PropertyDeclaration::XLang(style::values::specified::font::XLang(
                    style::Atom::from(lang),
                )),
                Importance::Normal,
            );
        }
        if let Some((first_id, first)) = self.svg_seen.remove(&hash) {
            // The second element with these attributes (or a hash collision): the first one's
            // block for both when its declarations are these, kept from now on.
            let same = {
                let guard = lock.read();
                first.read_with(&guard).declarations() == block.declarations()
            };
            let kept = if same {
                self.shared_first = Some((first_id, first.clone()));
                first
            } else {
                Arc::new(lock.wrap(block))
            };
            if self.svg_len >= Self::MAX_SVG {
                self.svg.clear();
                self.svg_len = 0;
            }
            self.svg
                .entry(hash)
                .or_default()
                .push((svg_key_text(&list, lang), kept.clone()));
            self.svg_len += 1;
            // (a block kept in `svg` can be given to any later element: not unshared even when
            // it is new, from a collision)
            return shared(kept);
        }
        let block = Arc::new(lock.wrap(block));
        if self.svg_seen.len() >= Self::MAX_SVG_SEEN {
            self.svg_seen.clear();
        }
        self.svg_seen.insert(hash, (id, block.clone()));
        Hints {
            block: Some(block),
            unshared: true,
        }
    }

    /// Adds to `block` the declarations of a presentation attribute that sets `property` and has the
    /// value `value` (as written, trimmed), parsed or kept.
    fn declare(
        &mut self,
        block: &mut PropertyDeclarationBlock,
        property: &'static str,
        value: &str,
        url: &UrlExtraData,
        quirks_mode: QuirksMode,
    ) {
        if property == "d" {
            // (path data, not CSS text: parsed straight into the property's value)
            if let Some(d) = crate::hints::d_declaration(value) {
                block.push(d, Importance::Normal);
            }
            return;
        }
        let keep = value.len() <= Self::MAX_VALUE_LEN;
        if keep && self.values.len >= Self::MAX_VALUES {
            self.values = HintValues::default();
        }
        let (id, parsed) = self
            .values
            .by_property
            .entry(property)
            .or_insert_with(|| (crate::computed::property_id(property), FxHashMap::default()));
        if let Some(declarations) = parsed.get(value) {
            for declaration in declarations.iter() {
                block.push(declaration.clone(), Importance::Normal);
            }
            return;
        }
        let Some(id) = id.clone() else {
            return;
        };
        let mut source = SourcePropertyDeclaration::default();
        let valid = crate::hints::svg_presentation_css(property, value).is_some_and(|css| {
            parse_one_declaration_into(
                &mut source,
                id,
                &css,
                Origin::Author,
                url,
                None,
                style_traits::ParsingMode::DEFAULT,
                quirks_mode,
                CssRuleType::Style,
            )
            .is_ok()
        });
        if !keep {
            if valid {
                for declaration in source.drain().declarations {
                    block.push(declaration, Importance::Normal);
                }
            }
            return;
        }
        let declarations: Box<[PropertyDeclaration]> = if valid {
            source.drain().declarations.collect()
        } else {
            Box::new([])
        };
        for declaration in declarations.iter() {
            block.push(declaration.clone(), Importance::Normal);
        }
        parsed.insert(value.into(), declarations);
        self.values.len += 1;
    }
}

/// A stylesheet with the node that owns it, in tree order within its scope.
struct OwnedSheet {
    owner: NodeId,
    sheet: Sheet,
}

/// The sheets of one shadow root.
#[derive(Default)]
struct ShadowSheets {
    tree: Vec<OwnedSheet>,
    adopted: Vec<Sheet>,
}

/// The style system of one document.
pub struct StyleEngine {
    document: NodeId,
    stylist: Stylist,
    config: DeviceConfig,
    url_string: String,
    url: UrlExtraData,
    /// Parsed presentational hints by their declarations' text ([`HintBlocks`]).
    hint_blocks: std::cell::RefCell<HintBlocks>,
    /// Styles kept from earlier restyles, reused by elements whose rules, name and parents equal
    /// a kept one's (stylo's `MatchedPropertiesCache`); emptied when a change may give any
    /// element another style ([`Self::global_style_change_pending`]).
    matched_properties: style::rule_cache::MatchedPropertiesCache,
    /// Where a traversal leaves the styles it kept, moved to [`Self::matched_properties`] after it.
    matched_properties_sink: std::sync::Mutex<style::rule_cache::MatchedPropertiesCache>,
    quirks_sheet: Option<DocumentStyleSheet>,
    noscript_sheet: bool,
    user_sheets: Vec<Sheet>,
    author: Vec<OwnedSheet>,
    adopted: Vec<Sheet>,
    /// Counts the changes of the document's sheets and of the device (what [`StyleEngine::font_faces`]
    /// reads), so callers can tell when to read them again.
    sheets_generation: u64,
    shadow_sheets: FxHashMap<NodeId, ShadowSheets>,
    shadows: FxHashMap<NodeId, ShadowStyles>,
    dirty_shadows: Vec<NodeId>,
    snapshots: SnapshotMap,
    /// For each element with attribute changes since the last restyle, the first old value of each
    /// changed attribute (namespace, local name, value).
    pending_attrs: FxHashMap<NodeId, SmallVec<[OldAttr; 2]>>,
    animations: DocumentAnimationSet,
    /// The phases of CSS animations and transitions as of the last event scan.
    css_events: crate::web_animations::css::CssEventTracker,
    /// A restyle may have started or canceled CSS animations or transitions since the last event scan.
    css_events_due: bool,
    /// Elements one of whose CSS animations or transitions script has driven through its object
    /// (`css_control`): their animations stay with the page's timeline, not a compositor's.
    css_controlled: FxHashSet<NodeId>,
    /// Nodes DOM's move (`moveBefore`) moved since the last restyle ([`StyleEngine::node_moved`]).
    moved: FxHashSet<NodeId>,
    /// What the compositor animates of each element or pseudo-element, as of the last frame
    /// painted for one ([`StyleEngine::set_on_compositor`]).
    on_compositor: FxHashMap<(NodeId, Option<PseudoElement>), SmallVec<[LonghandId; 2]>>,
    /// Each element's and pseudo-element's [`StyleEngine::animation_flags`] as of the last restyle
    /// (only those not 0).
    animation_flags: FxHashMap<(NodeId, Option<PseudoElement>), u8>,
    /// The page takes the events ([`StyleEngine::css_animation_events`] has been called).
    css_events_taken: bool,
    /// Events found by scans, for the page to take.
    css_events_out: Vec<crate::web_animations::CssAnimationEvent>,
    /// Script animations (Web Animations) in effect, shared with the traversal.
    script_animations: std::sync::RwLock<crate::web_animations::ScriptAnimations>,
    /// The document timeline's current time (seconds) as of the last [`StyleEngine::restyle_at`]: the
    /// time of the style change events of [`StyleEngine::restyle`].
    timeline_time: f64,
    /// The timeline time of the last "update animations".
    animations_updated_at: f64,
    import_loader: Option<StdArc<dyn ImportLoader>>,
    font_face_handler: Option<FontFaceHandler>,
    parallelism: Parallelism,
    prepared: bool,
    viewport_changed: bool,
    /// The device was replaced: the next restyle records the root's style on it first
    /// ([`StyleEngine::replace_device`]).
    reseed_root: bool,
    /// The next restyle restyles every element.
    restyle_all: bool,
    /// Inserted elements whose descendants are prepared at the next restyle, not at insertion
    /// (see [`StyleEngine::node_inserted`]). Entries can be stale (removed, prepared since).
    deferred: Vec<NodeId>,
    /// When `deferred` is next pruned of stale entries.
    deferred_prune_at: usize,
    /// The contents parsed from `style` elements' text, by text, base URL and quirks mode: every
    /// `style` element with the same text shares them (and so, in identical shadow trees, stylo's
    /// cascade data built from them). Entries no sheet uses any more are dropped when it grows.
    style_text_cache:
        FxHashMap<(String, String, bool), Arc<style::stylesheets::StylesheetContents>>,
    /// When `style_text_cache` is next pruned.
    style_text_prune_at: usize,
    /// Elements whose presentational hints are parsed again at the next restyle (their slots have
    /// `HINTS_DIRTY`). Entries can be stale (removed, prepared since).
    dirty_hints: Vec<NodeId>,
    /// When `dirty_hints` is next pruned of stale entries.
    dirty_hints_prune_at: usize,
    /// `process_reactions`' set of inserted nodes, kept between calls (empty) so most calls, which
    /// report one or two nodes, allocate nothing.
    batch_inserted: FxHashSet<NodeId>,
    /// `prepare_missing` found unprepared elements once (it logs the first time).
    reported_missing: bool,
    /// The embedder's Content Security Policy check of `style` attributes (see
    /// [`StyleAttributeCheck`]).
    style_attribute_check: Option<StyleAttributeCheck>,
    /// The page color scheme (HTML `<meta name=color-scheme>`, CSS Color Adjust 1 §2.1) the device
    /// was made with.
    page_color_scheme: ColorSchemeFlags,
    metas: Mutex<MetaCandidates>,
    /// Declaration blocks the CSSOM set as elements' `style` attributes, with the attribute text
    /// they serialize to (see [`StyleEngine::provide_style_attribute`]).
    cssom_style_attributes: Mutex<FxHashMap<NodeId, (String, PropertyDeclarationBlock)>>,
    /// The used color schemes may have changed (the preferred or the page color scheme): the
    /// next restyle restyles every element.
    color_schemes_changed: bool,
}

/// Whether an element's `style` attribute (with this value) applies: CSP `style-src-attr` (the
/// embedder reports the violation). Not set: always.
pub type StyleAttributeCheck = StdArc<dyn Fn(NodeId, &str) -> bool + Send + Sync>;

impl std::fmt::Debug for StyleEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StyleEngine")
            .field("document", &self.document)
            .field("author_sheets", &self.author.len())
            .finish_non_exhaustive()
    }
}

fn quirks_of(dom: &Dom, document: NodeId) -> QuirksMode {
    match dom.quirks_mode(document) {
        snowlark_dom::QuirksMode::NoQuirks => QuirksMode::NoQuirks,
        snowlark_dom::QuirksMode::Quirks => QuirksMode::Quirks,
        snowlark_dom::QuirksMode::LimitedQuirks => QuirksMode::LimitedQuirks,
    }
}

fn document_url(dom: &Dom, document: NodeId) -> Url {
    Url::parse(dom.document_url(document))
        .unwrap_or_else(|_| Url::parse("about:blank").expect("valid URL"))
}

/// The parsed user-agent sheets, shared by every engine (they are immutable).
struct UaSheets {
    html: DocumentStyleSheet,
    quirks: DocumentStyleSheet,
    noscript: DocumentStyleSheet,
}

/// HTML §15.3.1 `@media (scripting) { noscript { display: none !important; } }`, as a sheet the engine
/// adds while [`DeviceConfig::scripting`] is on (stylo has no `scripting` media feature).
const NOSCRIPT_CSS: &str =
    "@namespace \"http://www.w3.org/1999/xhtml\"; noscript { display: none !important; }";

fn ua_sheets() -> &'static UaSheets {
    crate::prefs::init();
    static SHEETS: std::sync::LazyLock<UaSheets> = std::sync::LazyLock::new(|| {
        let lock = SharedRwLock::read_only();
        let parse = |css: &str| {
            sheets::parse_sheet(
                css,
                sheets::ua_url(),
                "",
                SheetOrigin::UserAgent,
                &lock,
                QuirksMode::NoQuirks,
                None,
            )
            .0
        };
        UaSheets {
            html: parse(UA_CSS),
            quirks: parse(QUIRKS_CSS),
            noscript: parse(NOSCRIPT_CSS),
        }
    });
    &SHEETS
}

/// HTML §4.10.5 input types whose value is editable text (`:read-write` when not readonly or disabled).
fn is_text_input_type(t: &str) -> bool {
    [
        "text",
        "search",
        "url",
        "tel",
        "email",
        "password",
        "date",
        "month",
        "week",
        "time",
        "datetime-local",
        "number",
    ]
    .iter()
    .any(|k| t.eq_ignore_ascii_case(k))
}

/// The element state that follows from the DOM alone (HTML §4.16 "Pseudo-classes"): `:link`,
/// `:defined`, `:enabled`/`:disabled` from the `disabled` attribute, `:required`/`:optional`,
/// `:read-write`/`:read-only`, default `:checked`, `:open`, `:indeterminate` for `progress`,
/// `:placeholder-shown`. The page layer overrides any of these bits with `StyleEngine::set_state` once it
/// owns them (checkedness after user interaction, fieldset-disabled controls, values).
fn derived_state(dom: &Dom, id: NodeId) -> ElementState {
    let mut s = ElementState::empty();
    let Some(e) = dom.element(id) else {
        return s;
    };
    if matches!(
        dom.custom_element_state(id),
        CustomElementState::Uncustomized | CustomElementState::Custom
    ) {
        s |= ElementState::DEFINED;
    }
    let attr = |name: &LocalName| dom.attr(id, &ns!(), name);
    let has = |name: &LocalName| attr(name).is_some();
    let mut read_write = matches!(
        attr(&local_name!("contenteditable"))
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("" | "true" | "plaintext-only")
    );
    if e.name().ns == ns!(html) {
        let local = &e.name().local;
        match *local {
            local_name!("a") | local_name!("area") if has(&local_name!("href")) => {
                s |= ElementState::UNVISITED;
            }
            _ => {}
        }
        let form_control =
            matches!(
                *local,
                local_name!("button")
                    | local_name!("input")
                    | local_name!("select")
                    | local_name!("textarea")
                    | local_name!("optgroup")
                    | local_name!("option")
                    | local_name!("fieldset")
            ) || dom.element_flags(id) & snowlark_dom::element_flags::FORM_ASSOCIATED_CUSTOM != 0;
        if form_control {
            let mut disabled = has(&local_name!("disabled"));
            if *local == local_name!("option") {
                disabled |= dom.parent(id).is_some_and(|p| {
                    dom.is_html_element_named(p, &local_name!("optgroup"))
                        && dom.attr(p, &ns!(), &local_name!("disabled")).is_some()
                });
            }
            s |= if disabled {
                ElementState::DISABLED
            } else {
                ElementState::ENABLED
            };
        }
        let input_type = attr(&local_name!("type")).unwrap_or("text");
        match *local {
            local_name!("input") | local_name!("select") | local_name!("textarea") => {
                s |= if has(&local_name!("required")) {
                    ElementState::REQUIRED
                } else {
                    ElementState::OPTIONAL_
                };
            }
            _ => {}
        }
        match *local {
            local_name!("input") => {
                let editable = is_text_input_type(input_type)
                    || !attr(&local_name!("type")).is_some_and(|t| {
                        [
                            "hidden", "checkbox", "radio", "file", "submit", "image", "reset",
                            "button", "color", "range",
                        ]
                        .iter()
                        .any(|k| t.eq_ignore_ascii_case(k))
                    });
                read_write =
                    editable && !has(&local_name!("readonly")) && !has(&local_name!("disabled"));
                let checkable = input_type.eq_ignore_ascii_case("checkbox")
                    || input_type.eq_ignore_ascii_case("radio");
                if checkable && has(&local_name!("checked")) {
                    s |= ElementState::CHECKED | ElementState::DEFAULT;
                }
                if has(&local_name!("placeholder"))
                    && attr(&local_name!("value")).is_none_or(str::is_empty)
                {
                    s |= ElementState::PLACEHOLDER_SHOWN;
                }
            }
            local_name!("textarea") => {
                read_write = !has(&local_name!("readonly")) && !has(&local_name!("disabled"));
                if has(&local_name!("placeholder")) && dom.first_child(id).is_none() {
                    s |= ElementState::PLACEHOLDER_SHOWN;
                }
            }
            local_name!("option") if has(&local_name!("selected")) => {
                s |= ElementState::CHECKED | ElementState::DEFAULT;
            }
            local_name!("details") | local_name!("dialog") if has(&local_name!("open")) => {
                s |= ElementState::OPEN;
            }
            local_name!("progress") if !has(&local_name!("value")) => {
                s |= ElementState::INDETERMINATE;
            }
            _ => {}
        }
    }
    s |= if read_write {
        ElementState::READWRITE
    } else {
        ElementState::READONLY
    };
    s
}

/// Splits on ASCII whitespace into atoms (HTML "split a string on ASCII whitespace").
/// CSS Shadow Parts 1 §4.2.2 "parse a part mapping list": comma-separated mappings, each `ident` (to
/// itself) or `inner: outer`; a mapping that does not parse is left out.
pub(crate) fn part_mappings(value: Option<&str>) -> SmallVec<[(Atom, Atom); 1]> {
    let mut out = SmallVec::new();
    let Some(value) = value else {
        return out;
    };
    for item in value.split(',') {
        let mut rest = item.trim_matches(|c: char| c.is_ascii_whitespace());
        let token = |s: &str| -> usize {
            s.find(|c: char| c.is_ascii_whitespace() || c == ':')
                .unwrap_or(s.len())
        };
        let n = token(rest);
        if n == 0 {
            continue;
        }
        let first = &rest[..n];
        rest = rest[n..].trim_start_matches(|c: char| c.is_ascii_whitespace());
        if rest.is_empty() {
            out.push((Atom::from(first), Atom::from(first)));
            continue;
        }
        let Some(after) = rest.strip_prefix(':') else {
            continue;
        };
        let after = after.trim_start_matches(|c: char| c.is_ascii_whitespace());
        let m = token(after);
        if m == 0 || !after[m..].trim().is_empty() {
            continue;
        }
        out.push((Atom::from(first), Atom::from(&after[..m])));
    }
    out
}

/// The `id` attribute as an atom, when it has a value.
fn id_attribute(dom: &Dom, id: NodeId) -> Option<Atom> {
    dom.attr(id, &ns!(), &local_name!("id"))
        .filter(|v| !v.is_empty())
        .map(Atom::from)
}

/// The `exportparts` attribute. (By its text: it has no static atom, and interning one for every
/// element prepared costs more than reading the few attributes an element has.)
fn exportparts_attribute(dom: &Dom, id: NodeId) -> Option<&str> {
    dom.element(id)?
        .attrs()
        .iter()
        .find(|a| *a.namespace() == ns!() && &**a.local_name() == "exportparts")
        .map(|a| a.value())
}

fn tokens<const N: usize>(value: Option<&str>) -> SmallVec<[Atom; N]> {
    value
        .map(|v| v.split_ascii_whitespace().map(Atom::from).collect())
        .unwrap_or_default()
}

impl StyleEngine {
    /// The style engine of `document` (an HTML or XML Document node in `dom`).
    pub fn new(dom: &Dom, document: NodeId, config: DeviceConfig) -> StyleEngine {
        crate::prefs::init();
        let quirks_mode = quirks_of(dom, document);
        let mut stylist = Stylist::new(
            make_device(&config, quirks_mode, ColorSchemeFlags::empty()),
            quirks_mode,
        );
        let ua_lock = SharedRwLock::read_only();
        stylist.append_stylesheet(ua_sheets().html.clone(), &ua_lock.read());
        let url = document_url(dom, document);
        let mut engine = StyleEngine {
            document,
            stylist,
            config,
            url_string: dom.document_url(document).to_owned(),
            url: UrlExtraData::from(url),
            hint_blocks: Default::default(),
            quirks_sheet: None,
            noscript_sheet: false,
            user_sheets: Vec::new(),
            author: Vec::new(),
            adopted: Vec::new(),
            sheets_generation: 0,
            matched_properties: Default::default(),
            matched_properties_sink: Default::default(),
            shadow_sheets: FxHashMap::default(),
            shadows: FxHashMap::default(),
            dirty_shadows: Vec::new(),
            snapshots: SnapshotMap::new(),
            pending_attrs: FxHashMap::default(),
            animations: DocumentAnimationSet::default(),
            script_animations: Default::default(),
            css_events: Default::default(),
            css_events_due: false,
            css_controlled: FxHashSet::default(),
            moved: FxHashSet::default(),
            on_compositor: FxHashMap::default(),
            animation_flags: FxHashMap::default(),
            css_events_taken: false,
            css_events_out: Vec::new(),
            timeline_time: 0.0,
            animations_updated_at: 0.0,
            import_loader: None,
            font_face_handler: None,
            parallelism: Parallelism::Parallel,
            prepared: false,
            viewport_changed: false,
            reseed_root: false,
            restyle_all: false,
            deferred: Vec::new(),
            deferred_prune_at: DEFERRED_PRUNE_MIN,
            style_text_cache: FxHashMap::default(),
            style_text_prune_at: STYLE_TEXT_PRUNE_MIN,
            dirty_hints: Vec::new(),
            dirty_hints_prune_at: DEFERRED_PRUNE_MIN,
            batch_inserted: FxHashSet::default(),
            reported_missing: false,
            style_attribute_check: None,
            page_color_scheme: ColorSchemeFlags::empty(),
            metas: Mutex::new(MetaCandidates::default()),
            cssom_style_attributes: Mutex::new(FxHashMap::default()),
            color_schemes_changed: false,
        };
        engine.sync_quirks(dom);
        engine.sync_noscript();
        engine
    }

    /// The document this engine styles.
    pub fn document(&self) -> NodeId {
        self.document
    }

    /// The stylo Stylist (rule counts, cascade data), for diagnostics and the CSSOM layer.
    pub fn stylist(&self) -> &Stylist {
        &self.stylist
    }

    /// The current device configuration.
    pub fn device_config(&self) -> &DeviceConfig {
        &self.config
    }

    /// Changes the device (viewport resize, zoom, color scheme, print). Media queries are re-evaluated and
    /// viewport-relative units recomputed at the next restyle.
    pub fn set_device(&mut self, data: &StyleData, config: DeviceConfig) {
        self.sheets_generation += 1;
        let viewport_changed = config.viewport != self.config.viewport;
        // (system colors and `light-dark()` of elements that support both schemes follow the
        // preferred one, which no selector or media query invalidation covers)
        self.color_schemes_changed |= config.color_scheme != self.config.color_scheme;
        // (a stylo device's font metrics provider and `medium` font sizes are fixed)
        let fonts_changed = config.default_font_size != self.config.default_font_size
            || config.default_monospace_font_size != self.config.default_monospace_font_size
            || !same_font_metrics(&config.font_metrics, &self.config.font_metrics);
        self.config = config;
        if fonts_changed {
            self.replace_device(data);
        } else {
            self.apply_device(data);
        }
        self.viewport_changed |= viewport_changed;
        self.sync_noscript();
    }

    /// Brings the Stylist's device up to date with the configuration and the page color scheme, in
    /// place, and marks the origins whose media queries changed results for a restyle. The device
    /// is kept, not rebuilt: it also holds what the last restyle of the root element recorded for
    /// root-relative units (the root's font size for `rem`, its line height for `rlh`, its style
    /// for `rex`, `rch`, `rcap` and `ric`) and the body's color (the tables quirk), which stylo
    /// records only when the root's (the body's) values change. A new device has the initial ones
    /// (`rem` at 16px) for every element restyled after it whose root was not restyled with a new
    /// font size: after a resize, every element using viewport units.
    fn apply_device(&mut self, data: &StyleData) {
        let c = &self.config;
        let device = self.stylist.device_mut();
        device.set_media_type(media_type(c.media));
        device.set_viewport_size(euclid::Size2D::new(c.viewport.0, c.viewport.1));
        let screen = c.screen.unwrap_or(c.viewport);
        device.set_device_size(euclid::Size2D::new(
            screen.0 * c.device_pixel_ratio,
            screen.1 * c.device_pixel_ratio,
        ));
        device.set_scripting_enabled(c.scripting);
        device.set_device_pixel_ratio(euclid::Scale::new(c.device_pixel_ratio));
        device.set_color_scheme(prefers_color_scheme(c.color_scheme));
        device.set_page_color_scheme(self.page_color_scheme);
        device.set_primary_pointer_capabilities(c.input.primary_pointer());
        device.set_all_pointer_capabilities(c.input.all_pointers());
        self.media_features_changed(data);
    }

    /// Gives the Stylist a new device made from the configuration, for a change a device cannot
    /// take in place (the font metrics, the `medium` font sizes). The next restyle records the
    /// root's style on it first (what [`StyleEngine::apply_device`] keeps) and restyles every
    /// element (`medium` may have changed).
    fn replace_device(&mut self, data: &StyleData) {
        let quirks_mode = self.stylist.quirks_mode();
        let device = make_device(&self.config, quirks_mode, self.page_color_scheme);
        device.set_body_text_color(self.stylist.device().body_text_color());
        *self.stylist.device_mut() = device;
        self.media_features_changed(data);
        self.reseed_root = true;
        self.restyle_all = true;
    }

    /// Marks the origins whose media queries evaluate differently on the current device for a
    /// restyle.
    fn media_features_changed(&mut self, data: &StyleData) {
        let ua_lock = SharedRwLock::read_only();
        let ua_guard = ua_lock.read();
        let author_guard = data.lock.read();
        let origins = {
            let guards = StylesheetGuards {
                author: &author_guard,
                ua_or_user: &ua_guard,
            };
            self.stylist
                .media_features_change_changed_style(&guards, self.stylist.device())
        };
        if !origins.is_empty() {
            self.stylist.force_stylesheet_origins_dirty(origins);
        }
    }

    /// The page color scheme (the `color-scheme` value of the page's `<meta name=color-scheme>`)
    /// as of the last restyle; empty when the page sets none.
    pub fn page_color_scheme(&self) -> ColorSchemeFlags {
        self.page_color_scheme
    }

    /// HTML §4.2.5.1: the page's supported color-schemes are determined again when a `meta`
    /// element is inserted or removed or its `name` or `content` changes. A new value restyles
    /// the document (every element whose `color-scheme` is `normal` uses it).
    fn sync_page_color_scheme(&mut self, dom: &Dom, data: &mut StyleData) {
        let page = {
            let mut metas = self.metas.lock().unwrap_or_else(|e| e.into_inner());
            if !std::mem::take(&mut metas.dirty) {
                return;
            }
            crate::color_scheme::page_color_scheme(dom, self.document, &mut metas.elements)
        };
        if page == self.page_color_scheme {
            return;
        }
        self.page_color_scheme = page;
        self.apply_device(data);
        self.color_schemes_changed = true;
    }

    /// Notes a `meta` element being prepared (inserted) or changed (see
    /// [`StyleEngine::sync_page_color_scheme`]).
    fn note_meta(&self, dom: &Dom, id: NodeId) {
        if dom.is_html_element_named(id, &local_name!("meta")) {
            let mut metas = self.metas.lock().unwrap_or_else(|e| e.into_inner());
            metas.elements.push(id);
            metas.dirty = true;
        }
    }

    /// Adds or removes the `noscript` sheet to follow [`DeviceConfig::scripting`].
    fn sync_noscript(&mut self) {
        if self.config.scripting == self.noscript_sheet {
            return;
        }
        let ua_lock = SharedRwLock::read_only();
        let sheet = ua_sheets().noscript.clone();
        if self.config.scripting {
            self.stylist.append_stylesheet(sheet, &ua_lock.read());
        } else {
            self.stylist.remove_stylesheet(sheet, &ua_lock.read());
        }
        self.noscript_sheet = self.config.scripting;
    }

    /// Sets how restyles use stylo's thread pool.
    pub fn set_parallelism(&mut self, parallelism: Parallelism) {
        self.parallelism = parallelism;
    }

    /// Sets the `@import` loader; without one, `@import` rules are ignored.
    pub fn set_import_loader(&mut self, loader: Option<StdArc<dyn ImportLoader>>) {
        self.import_loader = loader;
    }

    /// Sets the handler that receives the `@font-face` rules of each sheet as it is added (the paint
    /// crate loads the fonts).
    pub fn set_font_face_handler(&mut self, handler: Option<FontFaceHandler>) {
        self.font_face_handler = handler;
    }

    fn sync_quirks(&mut self, dom: &Dom) {
        let quirks_mode = quirks_of(dom, self.document);
        if quirks_mode != self.stylist.quirks_mode() {
            self.stylist.set_quirks_mode(quirks_mode);
        }
        let want = quirks_mode == QuirksMode::Quirks;
        let ua_lock = SharedRwLock::read_only();
        match (&self.quirks_sheet, want) {
            (None, true) => {
                let sheet = ua_sheets().quirks.clone();
                self.stylist
                    .append_stylesheet(sheet.clone(), &ua_lock.read());
                self.quirks_sheet = Some(sheet);
            }
            (Some(sheet), false) => {
                self.stylist
                    .remove_stylesheet(sheet.clone(), &ua_lock.read());
                self.quirks_sheet = None;
            }
            _ => {}
        }
    }

    fn sync_url(&mut self, dom: &Dom) {
        if dom.document_url(self.document) != self.url_string {
            self.url_string = dom.document_url(self.document).to_owned();
            self.url = UrlExtraData::from(document_url(dom, self.document));
        }
    }

    // ----- stylesheets -------------------------------------------------------------------------------------

    /// Parses an author stylesheet. `base_url` resolves its relative URLs (the document's base URL for a
    /// `<style>`, the sheet's own URL for a `<link>`); `media` is the `media` attribute ("" for none).
    pub fn parse_sheet(&self, data: &StyleData, css: &str, base_url: &Url, media: &str) -> Sheet {
        sheets::parse_sheet(
            css,
            base_url,
            media,
            SheetOrigin::Author,
            &data.lock,
            self.stylist.quirks_mode(),
            self.import_loader.as_deref(),
        )
    }

    /// Parses a sheet of any origin (user sheets for user styles, extensions).
    pub fn parse_sheet_with_origin(
        &self,
        data: &StyleData,
        css: &str,
        base_url: &Url,
        media: &str,
        origin: SheetOrigin,
    ) -> Sheet {
        sheets::parse_sheet(
            css,
            base_url,
            media,
            origin,
            &data.lock,
            self.stylist.quirks_mode(),
            self.import_loader.as_deref(),
        )
    }

    fn report_font_faces(&self, data: &StyleData, sheet: &Sheet) {
        if let Some(handler) = &self.font_face_handler {
            let guard = data.lock.read();
            for face in sheets::font_faces(sheet.stylo(), self.stylist.device(), &guard) {
                handler(&face);
            }
        }
    }

    /// Changes with every change of the document's sheets (added, removed, edited through the
    /// CSSOM, adopted) and of the device: [`StyleEngine::font_faces`] may differ only then.
    pub fn sheets_generation(&self) -> u64 {
        self.sheets_generation
    }

    /// Every `@font-face` rule that currently applies, in cascade order (later wins among equals).
    pub fn font_faces(&self, data: &StyleData) -> Vec<FontFace> {
        let guard = data.lock.read();
        let device = self.stylist.device();
        let mut order = sheets::LayerOrder::default();
        let mut out = Vec::new();
        for sheet in self
            .user_sheets
            .iter()
            .chain(self.author.iter().map(|s| &s.sheet))
            .chain(self.adopted.iter())
        {
            if !sheet.disabled() {
                out.extend(sheets::ranked_font_faces(
                    sheet.stylo(),
                    device,
                    &guard,
                    &mut order,
                ));
            }
        }
        // Cascade-layer order, then source order (a layer's own rules after its sublayers, rules
        // outside any layer last).
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out.into_iter().map(|(_, f)| f).collect()
    }

    /// CSS Properties and Values API 1 §2.4 `registerProperty()`: registers `name` with its
    /// syntax, inheritance and initial value (resolved against the document's URL) in the
    /// document's set of registered properties, and restyles the document, whose custom property
    /// values now compute by the registration.
    pub fn register_property(
        &mut self,
        name: &str,
        syntax: &str,
        inherits: bool,
        initial_value: Option<&str>,
    ) -> Result<(), RegisterPropertyError> {
        use style::stylist::RegisterCustomPropertyResult as R;
        match self.stylist.register_custom_property(
            &self.url,
            name,
            syntax,
            inherits,
            initial_value,
        ) {
            R::SuccessfullyRegistered => {
                self.restyle_all = true;
                Ok(())
            }
            R::AlreadyRegistered => Err(RegisterPropertyError::AlreadyRegistered),
            R::InvalidName
            | R::InvalidSyntax
            | R::NoInitialValue
            | R::InvalidInitialValue
            | R::InitialValueNotComputationallyIndependent => Err(RegisterPropertyError::Invalid),
        }
    }

    /// Parses `css` as a user-origin sheet and adds it (an embedder's: the content blocker's
    /// cosmetic filters). The sheet is under a read-only lock: the cascade reads user and UA sheets
    /// with the read-only guard, never the document's. A rule whose selector does not parse is
    /// dropped alone (one selector per rule keeps the others).
    pub fn add_user_css(&mut self, data: &StyleData, css: &str) {
        self.sheets_generation += 1;
        let base = Url::parse("about:blank").expect("about:blank");
        let sheet = sheets::parse_sheet(
            css,
            &base,
            "",
            SheetOrigin::User,
            &SharedRwLock::read_only(),
            self.stylist.quirks_mode(),
            None,
        );
        self.add_user_sheet(data, sheet);
    }

    /// Adds a user-origin sheet (after earlier user sheets).
    pub fn add_user_sheet(&mut self, data: &StyleData, sheet: Sheet) {
        self.sheets_generation += 1;
        self.report_font_faces(data, &sheet);
        self.stylist
            .append_stylesheet(sheet.0.clone(), &data.lock.read());
        self.user_sheets.push(sheet);
    }

    /// Adds `sheet` as owned by `owner` (a `style` or `link` element, or the node a processing
    /// instruction sheet hangs off). The sheet's scope is the owner's root: the document, or the shadow root
    /// the owner is in. Within a scope, sheets apply in the tree order of their owners (CSSOM §6.2
    /// "document or shadow root CSS style sheets"), then the adopted sheets.
    pub fn add_sheet(&mut self, dom: &Dom, data: &StyleData, owner: NodeId, sheet: Sheet) {
        self.sheets_generation += 1;
        self.report_font_faces(data, &sheet);
        let root = dom.root(owner);
        // CSS Cascade 6 §2.1.1: an `@scope` without `<scope-start>` scopes to the owner node's parent
        // element, or the root of its tree when there is none (a shadow root: its host).
        let parent = dom.parent(owner);
        sheet.set_implicit_scope_root(Some(match parent {
            Some(p) if dom.is_element(p) => {
                if dom.is_shadow_root(root) {
                    ImplicitScopeRoot::InShadowTree(opaque_element(p))
                } else {
                    ImplicitScopeRoot::InLightTree(opaque_element(p))
                }
            }
            Some(p) if dom.is_shadow_root(p) => match dom.host(p) {
                Some(host) => ImplicitScopeRoot::ShadowHost(opaque_element(host)),
                None => ImplicitScopeRoot::DocumentElement,
            },
            _ => ImplicitScopeRoot::DocumentElement,
        }));
        if dom.is_shadow_root(root) {
            let list = self.shadow_sheets.entry(root).or_default();
            let pos = list
                .tree
                .iter()
                .position(|s| dom.tree_order(owner, s.owner).is_lt())
                .unwrap_or(list.tree.len());
            list.tree.insert(pos, OwnedSheet { owner, sheet });
            self.dirty_shadows.push(root);
            return;
        }
        let pos = self
            .author
            .iter()
            .position(|s| dom.tree_order(owner, s.owner).is_lt())
            .unwrap_or(self.author.len());
        let before = self
            .author
            .get(pos)
            .map(|s| s.sheet.clone())
            .or_else(|| self.adopted.first().cloned());
        let guard = data.lock.read();
        match before {
            Some(before) => {
                self.stylist
                    .insert_stylesheet_before(sheet.0.clone(), before.0, &guard)
            }
            None => self.stylist.append_stylesheet(sheet.0.clone(), &guard),
        }
        drop(guard);
        self.author.insert(pos, OwnedSheet { owner, sheet });
    }

    /// The sheet `owner` (a `style` or `link` element) contributes, if any (`LinkStyle.sheet`).
    pub fn sheet_of(&self, owner: NodeId) -> Option<Sheet> {
        if let Some(s) = self.author.iter().find(|s| s.owner == owner) {
            return Some(s.sheet.clone());
        }
        self.shadow_sheets
            .values()
            .find_map(|l| l.tree.iter().find(|s| s.owner == owner))
            .map(|s| s.sheet.clone())
    }

    /// The document's (or a shadow root's) sheets in tree order with their owners
    /// (`DocumentOrShadowRoot.styleSheets`).
    pub fn tree_sheets(&self, scope: NodeId) -> Vec<(NodeId, Sheet)> {
        if scope == self.document {
            return self
                .author
                .iter()
                .map(|s| (s.owner, s.sheet.clone()))
                .collect();
        }
        self.shadow_sheets
            .get(&scope)
            .map(|l| l.tree.iter().map(|s| (s.owner, s.sheet.clone())).collect())
            .unwrap_or_default()
    }

    /// Removes the sheets owned by `owner` (the `style` element was removed or its text changed).
    pub fn remove_sheets_of(&mut self, data: &StyleData, owner: NodeId) {
        self.sheets_generation += 1;
        let guard = data.lock.read();
        let mut i = 0;
        while i < self.author.len() {
            if self.author[i].owner == owner {
                let removed = self.author.remove(i);
                self.stylist.remove_stylesheet(removed.sheet.0, &guard);
            } else {
                i += 1;
            }
        }
        for (root, list) in self.shadow_sheets.iter_mut() {
            let before = list.tree.len();
            list.tree.retain(|s| s.owner != owner);
            if list.tree.len() != before {
                self.dirty_shadows.push(*root);
            }
        }
    }

    /// HTML §4.2.6 "update a `style` block" for the `style` element `element` (an SVG `style`
    /// element's sheet follows the same steps, SVG 2 §6.4): drops its old sheet and,
    /// when the element is connected and its `type` is empty, absent or `text/css`, parses its child text
    /// content with its `media` attribute and adds the result in tree order. The page layer calls this when
    /// the element is inserted, removed, its children change, or its `type`/`media` change. Returns the new
    /// sheet (for `HTMLStyleElement.sheet`).
    pub fn update_style_block(
        &mut self,
        dom: &Dom,
        data: &StyleData,
        element: NodeId,
    ) -> Option<Sheet> {
        // 1-3. Remove the old sheet.
        self.remove_sheets_of(data, element);
        if !dom.is_alive(element) || !dom.is_style_element(element) {
            return None;
        }
        // 4. Not connected: no sheet. (The shadow root case is handled by add_sheet's scoping.)
        if !dom.is_connected(element) || dom.node_document(element) != self.document {
            return None;
        }
        // 5. A `type` other than "" or text/css.
        if dom
            .attr(element, &ns!(), &local_name!("type"))
            .is_some_and(|t| !t.is_empty() && !t.eq_ignore_ascii_case("text/css"))
        {
            return None;
        }
        // 6-7. Content Security Policy is the page layer's check, before calling this.
        // 8. Create a CSS style sheet: the child text content, the `media` attribute, the document's
        // base URL.
        self.sync_url(dom);
        let text = dom.child_text_content(element);
        let media = dom
            .attr(element, &ns!(), &local_name!("media"))
            .unwrap_or("")
            .to_owned();
        let base = document_url(dom, self.document);
        let sheet = self.style_block_sheet(data, text, &base, &media);
        self.add_sheet(dom, data, element, sheet.clone());
        Some(sheet)
    }

    /// The sheet of a `style` element with child text `text`: over the contents parsed for another
    /// `style` element of this document with the same text, base URL and quirks mode when there is
    /// one, else parsed now. Parsing gives the same rules for the same text, so only the sheet
    /// objects (each with its own media list) differ; the CSSOM gives a sheet a copy of its own
    /// before it reaches the rules ([`Sheet::ensure_unique_contents`]).
    fn style_block_sheet(
        &mut self,
        data: &StyleData,
        text: String,
        base: &Url,
        media: &str,
    ) -> Sheet {
        let quirks = self.stylist.quirks_mode() == QuirksMode::Quirks;
        let key = (text, base.as_str().to_owned(), quirks);
        if let Some(contents) = self.style_text_cache.get(&key) {
            return Sheet::sharing(contents.clone(), media, &data.lock);
        }
        let sheet = self.parse_sheet(data, &key.0, base, media);
        let contents = {
            let guard = data.lock.read();
            sheet.shareable_contents(self.stylist.device(), &guard)
        };
        if let Some(contents) = contents {
            if self.style_text_cache.len() >= self.style_text_prune_at {
                // (only the cache holds these)
                self.style_text_cache
                    .retain(|_, c| !servo_arc::Arc::is_unique(c));
                self.style_text_prune_at =
                    (self.style_text_cache.len() * 2).max(STYLE_TEXT_PRUNE_MIN);
            }
            sheet.mark_shared();
            self.style_text_cache.insert(key, contents);
        }
        sheet
    }

    /// Runs [`StyleEngine::update_style_block`] for every `style` element of the document, shadow trees
    /// included (a document parsed without the page layer, as the CLI and tests do).
    pub fn update_all_style_blocks(&mut self, dom: &Dom, data: &StyleData) {
        let styles: Vec<NodeId> = dom
            .shadow_including_descendants(self.document)
            .filter(|&n| dom.is_style_element(n))
            .collect();
        for element in styles {
            self.update_style_block(dom, data, element);
        }
    }

    /// Replaces the adopted stylesheets (`adoptedStyleSheets`) of `scope`, the document or a shadow root.
    pub fn set_adopted_sheets(
        &mut self,
        dom: &Dom,
        data: &StyleData,
        scope: NodeId,
        adopted: Vec<Sheet>,
    ) {
        self.sheets_generation += 1;
        if dom.is_shadow_root(scope) {
            self.shadow_sheets.entry(scope).or_default().adopted = adopted;
            self.dirty_shadows.push(scope);
            return;
        }
        let guard = data.lock.read();
        for sheet in self.adopted.drain(..) {
            self.stylist.remove_stylesheet(sheet.0, &guard);
        }
        for sheet in &adopted {
            self.stylist.append_stylesheet(sheet.0.clone(), &guard);
        }
        drop(guard);
        for sheet in &adopted {
            self.report_font_faces(data, sheet);
        }
        self.adopted = adopted;
    }

    /// Something inside the sheets changed (CSSOM rule insertion, `disabled`): rebuild the cascade data
    /// and restyle what it affects.
    pub fn sheets_changed(&mut self) {
        self.sheets_generation += 1;
        self.stylist
            .force_stylesheet_origins_dirty(Origin::Author.into());
        let roots: Vec<NodeId> = self.shadow_sheets.keys().copied().collect();
        self.dirty_shadows.extend(roots);
    }

    /// Rebuilds the cascade data of the shadow roots whose sheets changed, and restyles their hosts'
    /// subtrees (`:host` rules match the host).
    fn flush_shadows(&mut self, dom: &Dom, data: &StyleData) {
        if self.dirty_shadows.is_empty() {
            return;
        }
        let mut roots = std::mem::take(&mut self.dirty_shadows);
        roots.sort();
        roots.dedup();
        // stylo caches cascade data by the identity of the sheets' contents, and CSSOM changes
        // (`replaceSync`, `insertRule`...) change the rules inside the same contents: an entry is
        // stale once its sheets change. Every shadow root using a changed sheet is rebuilt here, so
        // the entries only they held go (with every other one no shadow root uses any more); one
        // other shadow roots share stays, as their sheets have not changed.
        for root in &roots {
            self.shadows.remove(root);
        }
        self.stylist.remove_unique_author_data_cache_entries();
        let guard = data.lock.read();
        for root in roots {
            if !dom.is_alive(root) || !dom.is_shadow_root(root) {
                self.shadow_sheets.remove(&root);
                self.shadows.remove(&root);
                continue;
            }
            let list = self.shadow_sheets.entry(root).or_default();
            let mut styles = ShadowStyles::new();
            let custom_media = Default::default();
            for sheet in list
                .tree
                .iter()
                .map(|s| &s.sheet)
                .chain(list.adopted.iter())
            {
                styles
                    .stylesheets
                    .append_stylesheet(None, &custom_media, sheet.0.clone(), &guard);
            }
            styles.flush(&mut self.stylist, &guard);
            self.shadows.insert(root, styles);
            if let Some(host) = dom.host(root) {
                add_hint(data, host, RestyleHint::restyle_subtree());
                mark_ancestors_dirty(dom, data, host);
            }
        }
    }

    // ----- element preparation -------------------------------------------------------------------------

    /// Sets the check `style` attributes go through from now on (elements prepared before keep what
    /// they have until their attribute changes).
    pub fn set_style_attribute_check(&mut self, check: Option<StyleAttributeCheck>) {
        self.style_attribute_check = check;
    }

    /// CSSOM §6.6.1 "update style attribute for": the declarations of `element`'s inline style as
    /// the CSSOM holds them, which become its style attribute's declarations when the attribute is
    /// next read with the text `css` (the block serialized), instead of that text parsed again.
    /// Serialization rounds (`calc(1cm)` is `calc(37.7953px)`), so a reparse would compute a
    /// slightly different value than the declaration script set.
    pub fn provide_style_attribute(&self, element: NodeId, css: &str, block: &DeclarationBlock) {
        if let Ok(mut map) = self.cssom_style_attributes.lock() {
            map.insert(element, (css.to_owned(), block.block().clone()));
        }
    }

    /// The element's `style` attribute, parsed.
    fn style_attribute_cache(
        &self,
        dom: &Dom,
        lock: &SharedRwLock,
        id: NodeId,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        dom.attr(id, &ns!(), &local_name!("style"))
            .filter(|_| {
                let ns = dom.namespace(id);
                *ns == ns!(html) || *ns == ns!(svg) || *ns == ns!(mathml)
            })
            .filter(|css| {
                self.style_attribute_check
                    .as_ref()
                    .is_none_or(|check| check(id, css))
            })
            .map(|css| {
                let provided = self
                    .cssom_style_attributes
                    .lock()
                    .ok()
                    .and_then(|mut map| map.remove(&id));
                let block = match provided {
                    Some((text, block)) if text == css => block,
                    _ => parse_style_attribute(
                        css,
                        &self.url,
                        None,
                        self.stylist.quirks_mode(),
                        CssRuleType::Style,
                    ),
                };
                Arc::new(lock.wrap(block))
            })
    }

    /// The element's presentational hints, parsed: elements whose hints have the same text share
    /// one block ([`HintBlocks`]).
    fn hints_cache(&self, dom: &Dom, lock: &SharedRwLock, id: NodeId) -> Hints {
        let quirks_mode = self.stylist.quirks_mode();
        let lang = crate::hints::content_language_hint(dom, id);
        if *dom.namespace(id) == ns!(svg) {
            let mut blocks = self.hint_blocks.borrow_mut();
            blocks.validate(lock, &self.url_string, quirks_mode);
            return blocks.svg_block(dom, id, lang, &self.url, quirks_mode, lock);
        }
        Hints {
            block: self.html_hints(dom, lock, id, lang, quirks_mode),
            unshared: false,
        }
    }

    /// An HTML element's presentational hints as one block, shared by the elements whose hints
    /// have the same text.
    fn html_hints(
        &self,
        dom: &Dom,
        lock: &SharedRwLock,
        id: NodeId,
        lang: Option<String>,
        quirks_mode: QuirksMode,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        let css = crate::hints::presentational_hints(dom, id, quirks_mode == QuirksMode::Quirks);
        // (the language, which no author CSS can set, keys the cache after a NUL)
        let key = match (&css, &lang) {
            (None, None) => return None,
            (Some(css), None) => css.clone(),
            (css, Some(lang)) => format!("{}\0{lang}", css.as_deref().unwrap_or("")),
        };
        let mut blocks = self.hint_blocks.borrow_mut();
        blocks.validate(lock, &self.url_string, quirks_mode);
        if let Some(block) = blocks.map.get(key.as_str()) {
            return Some(block.clone());
        }
        let mut declarations = parse_style_attribute(
            css.as_deref().unwrap_or(""),
            &self.url,
            None,
            quirks_mode,
            CssRuleType::Style,
        );
        if let Some(lang) = lang {
            declarations.push(
                PropertyDeclaration::XLang(style::values::specified::font::XLang(
                    style::Atom::from(lang),
                )),
                Importance::Normal,
            );
        }
        let block = Arc::new(lock.wrap(declarations));
        blocks.insert(key, block.clone());
        Some(block)
    }

    /// The element's [`AttrCache`], and whether its hints' block is its own alone
    /// ([`bits::HINTS_UNSHARED`]).
    fn attr_cache(&self, dom: &Dom, lock: &SharedRwLock, id: NodeId) -> (AttrCache, bool) {
        let hints = self.hints_cache(dom, lock, id);
        let cache = AttrCache {
            id: id_attribute(dom, id),
            classes: tokens(dom.presented_attr(id, &local_name!("class"))),
            parts: tokens(dom.attr(id, &ns!(), &local_name!("part"))),
            exportparts: part_mappings(exportparts_attribute(dom, id)),
            style_attribute: self.style_attribute_cache(dom, lock, id),
            hints: hints.block,
        };
        (cache, hints.unshared)
    }

    /// After a hints lookup: an element whose block that lookup gave to another element is no
    /// longer alone with it.
    fn note_hints_shared(&self, data: &StyleData) {
        let Some((first, block)) = self.hint_blocks.borrow_mut().shared_first.take() else {
            return;
        };
        if let Some(slot) = data.slot(first) {
            if slot
                .attrs
                .hints
                .as_ref()
                .is_some_and(|b| Arc::ptr_eq(b, &block))
            {
                slot.clear(bits::HINTS_UNSHARED);
            }
        }
    }

    /// Brings the element's [`AttrCache`] up to date with a change of the attribute `local` in no
    /// namespace (the cache reads no other): only what that attribute decides is parsed again, and
    /// the presentational hints not before the next restyle. Returns whether this change marked
    /// the hints for that.
    fn refresh_attr_cache(
        &self,
        dom: &Dom,
        data: &mut StyleData,
        id: NodeId,
        local: &LocalName,
    ) -> bool {
        match *local {
            local_name!("style") => {
                let style = self.style_attribute_cache(dom, &data.lock, id);
                data.slot_mut(id).expect("prepared").attrs.style_attribute = style;
            }
            local_name!("id") => {
                data.slot_mut(id).expect("prepared").attrs.id = id_attribute(dom, id);
            }
            local_name!("class") => {
                data.slot_mut(id).expect("prepared").attrs.classes =
                    tokens(dom.presented_attr(id, &local_name!("class")));
            }
            local_name!("part") => {
                data.slot_mut(id).expect("prepared").attrs.parts =
                    tokens(dom.attr(id, &ns!(), &local_name!("part")));
            }
            _ if &**local == "exportparts" => {
                data.slot_mut(id).expect("prepared").attrs.exportparts =
                    part_mappings(exportparts_attribute(dom, id));
            }
            _ if crate::hints::can_change_hints(dom, id, local) => {
                // Blink marks the presentation attribute style dirty
                // (`SetPresentationAttributeStyleIsDirty`) and builds it at the style update: an
                // element whose attributes are set one by one is parsed once.
                let slot = data.slot(id).expect("prepared");
                if !slot.has(bits::HINTS_DIRTY) {
                    slot.set(bits::HINTS_DIRTY);
                    return true;
                }
            }
            _ => {}
        }
        false
    }

    /// Records that `element`'s presentational hints wait for the next restyle.
    fn note_dirty_hints(&mut self, data: &StyleData, element: NodeId) {
        self.dirty_hints.push(element);
        if self.dirty_hints.len() >= self.dirty_hints_prune_at {
            // (no restyle for a while: drop the entries removed or prepared since)
            self.dirty_hints
                .retain(|&id| data.slot(id).is_some_and(|s| s.has(bits::HINTS_DIRTY)));
            self.dirty_hints_prune_at = (self.dirty_hints.len() * 2).max(DEFERRED_PRUNE_MIN);
        }
    }

    /// Parses the presentational hints that attribute changes marked since the last restyle, before
    /// the traversal reads them. An element whose hints were or are now present restyles.
    fn flush_dirty_hints(&mut self, dom: &Dom, data: &mut StyleData) {
        if self.dirty_hints.is_empty() {
            return;
        }
        let mut list = std::mem::take(&mut self.dirty_hints);
        for id in list.drain(..) {
            if !data.slot(id).is_some_and(|s| s.has(bits::HINTS_DIRTY))
                || !dom.is_alive(id)
                || !dom.is_connected(id)
                || dom.node_document(id) != self.document
            {
                // (an element out of the document keeps the mark: it is prepared again, which
                // parses its hints, when it is inserted)
                continue;
            }
            let Hints {
                block: hints,
                unshared,
            } = self.hints_cache(dom, &data.lock, id);
            self.note_hints_shared(data);
            let slot = data.slot_mut(id).expect("prepared");
            slot.clear(bits::HINTS_DIRTY);
            if unshared {
                slot.set(bits::HINTS_UNSHARED);
            } else {
                slot.clear(bits::HINTS_UNSHARED);
            }
            // (the same block is the same declarations: nothing to restyle)
            let changed = match (&slot.attrs.hints, &hints) {
                (Some(old), Some(new)) => !Arc::ptr_eq(old, new),
                (None, None) => false,
                _ => true,
            };
            slot.attrs.hints = hints;
            if changed {
                add_hint(data, id, RestyleHint::RESTYLE_SELF);
                mark_ancestors_dirty(dom, data, id);
            }
        }
        self.dirty_hints = list;
        self.dirty_hints_prune_at = DEFERRED_PRUNE_MIN;
    }

    /// Prepares one element's slot: resets it for a new node, fills the attribute caches and the derived
    /// state.
    fn prepare_element(
        &self,
        dom: &Dom,
        data: &mut StyleData,
        id: NodeId,
        containing_shadow: Option<NodeId>,
    ) {
        data.prepare_slot(id);
        // (a slot prepared again keeps no assigned nodes from where it was: its shadow tree's
        // assignment, computed after its preparation, gives it those it has now)
        data.set_slotted_nodes(id, Vec::new());
        self.note_meta(dom, id);
        let (cache, unshared) = self.attr_cache(dom, &data.lock, id);
        self.note_hints_shared(data);
        let state = derived_state(dom, id);
        let slot = data.slot_mut(id).expect("prepared");
        slot.reset_data();
        slot.attrs = cache;
        slot.derived_state = state;
        slot.containing_shadow = containing_shadow;
        slot.assigned_slot = None;
        let flags = slot.flags.get_mut();
        *flags &= !(bits::SELECTOR_FLAGS
            | bits::UNPREPARED_DESCENDANTS
            | bits::HINTS_DIRTY
            | bits::HINTS_UNSHARED);
        if unshared {
            *flags |= bits::HINTS_UNSHARED;
        }
    }

    /// Prepares the descendants of the elements inserted since the last restyle whose preparation
    /// [`StyleEngine::node_inserted`] deferred, if they are still in the document. Elements inserted
    /// and removed again in between (a list rebuilt several times between two frames) are never
    /// prepared.
    fn prepare_deferred(&mut self, dom: &Dom, data: &mut StyleData) {
        if self.deferred.is_empty() {
            return;
        }
        data.reserve(dom.arena_len());
        for root in std::mem::take(&mut self.deferred) {
            if self.is_deferred(dom, data, root) {
                // (a host's child keeps the slot its insertion assigned it: preparing it again
                // clears that, and the slot still lists it)
                let assigned = data.slot(root).and_then(|s| s.assigned_slot);
                self.prepare_subtree(dom, data, root);
                if let Some(slot) = data.slot_mut(root) {
                    slot.assigned_slot = assigned;
                }
                // A subtree in a shadow tree can hold slots of that tree, which its slot
                // assignment at the insertion could not record (they had no slots then): it is
                // computed again now that they do (Blink: `SlotAssignment::RecalcAssignment` at
                // the style update, after `SetNeedsAssignmentRecalc` at the insertion).
                let tree_root = dom.root(root);
                if dom.is_shadow_root(tree_root) {
                    apply_slot_assignment(dom, data, tree_root);
                }
            }
        }
        self.deferred_prune_at = DEFERRED_PRUNE_MIN;
    }

    /// The restyle's safety net: prepares any element the traversal is about to reach that has no
    /// slot (a DOM change that did not reach [`StyleEngine::process_reactions`]), so the traversal
    /// never meets one. It follows the traversal's way down: into the children of an element with
    /// dirty descendants, and through the whole subtree of one with no styles yet or a restyle
    /// hint. Finding one is a bug, logged once per engine (and a panic in debug builds).
    fn prepare_missing(&mut self, dom: &Dom, data: &mut StyleData) {
        let Some(root) = dom.document_element(self.document) else {
            return;
        };
        let mut missing = Vec::new();
        let mut stack: Vec<(NodeId, bool)> = vec![(root, false)];
        while let Some((id, whole)) = stack.pop() {
            let Some(slot) = data.slot(id) else {
                missing.push(id);
                continue;
            };
            let whole = whole || !slot.has(bits::HAS_DATA) || !slot.data.borrow().hint.is_empty();
            if !whole && !slot.has(bits::DIRTY_DESCENDANTS) {
                continue;
            }
            let shadow = dom.shadow_root(id);
            for parent in std::iter::once(id).chain(shadow) {
                stack.extend(
                    dom.children(parent)
                        .filter(|&c| dom.is_element(c))
                        .map(|c| (c, whole)),
                );
            }
        }
        if missing.is_empty() {
            return;
        }
        debug_assert!(
            missing.is_empty(),
            "snowlark-style: elements not prepared before a restyle: {missing:?}"
        );
        if !self.reported_missing {
            self.reported_missing = true;
            eprintln!(
                "snowlark-style: {} element(s) of the document were not prepared (a DOM change not reported), first {:?}; prepared now",
                missing.len(),
                missing[0]
            );
        }
        data.reserve(dom.arena_len());
        for id in missing {
            self.prepare_subtree(dom, data, id);
        }
    }

    /// Whether `root` (an entry of `deferred`) still waits for its descendants' preparation (or,
    /// with no slot, for its own too).
    fn is_deferred(&self, dom: &Dom, data: &StyleData, root: NodeId) -> bool {
        dom.is_alive(root)
            && dom.is_connected(root)
            && dom.node_document(root) == self.document
            && dom.is_element(root)
            && data
                .slot(root)
                .is_none_or(|s| s.has(bits::UNPREPARED_DESCENDANTS))
    }

    /// Prepares `element` alone if it is in a subtree whose preparation waits for the next restyle
    /// (page-owned state set on it now must have a slot to live in; the restyle's preparation of
    /// the subtree keeps it).
    fn prepare_waiting_element(&mut self, dom: &Dom, data: &mut StyleData, element: NodeId) {
        if !self.has_deferred()
            || data.slot(element).is_some()
            || !dom.is_alive(element)
            || !dom.is_element(element)
            || !dom.is_connected(element)
            || dom.node_document(element) != self.document
        {
            return;
        }
        // A connected element of this document without a slot waits: inside a subtree whose
        // root's descendants wait (the root has the flag, or no slot when it waits whole), or as
        // such a root itself, whose descendants still wait once it is prepared alone.
        let inside = dom.ancestors(element).any(|a| {
            data.slot(a)
                .map_or(dom.is_element(a), |s| s.has(bits::UNPREPARED_DESCENDANTS))
        });
        data.reserve(dom.arena_len());
        let shadow = containing_shadow(dom, element);
        self.prepare_element(dom, data, element, shadow);
        if !inside {
            if let Some(slot) = data.slot(element) {
                slot.set(bits::UNPREPARED_DESCENDANTS);
            }
        }
    }

    /// Whether some element of this document has descendants that are not prepared yet.
    pub(crate) fn has_deferred(&self) -> bool {
        !self.deferred.is_empty()
    }

    /// Prepares `root` and its shadow-including descendants, then computes the slot assignment of every
    /// shadow root in it.
    fn prepare_subtree(&self, dom: &Dom, data: &mut StyleData, root: NodeId) {
        let tree_root = dom.root(root);
        let shadow = dom.is_shadow_root(tree_root).then_some(tree_root);
        let mut stack = vec![(root, shadow)];
        let mut shadows = Vec::new();
        while let Some((id, shadow)) = stack.pop() {
            if dom.is_element(id) {
                self.prepare_element(dom, data, id, shadow);
                if let Some(sr) = dom.shadow_root(id) {
                    shadows.push(sr);
                    stack.extend(dom.children_rev(sr).map(|c| (c, Some(sr))));
                }
            } else if dom.is_shadow_root(id) {
                shadows.push(id);
            }
            if dom.is_element(id) || dom.is_document(id) || dom.is_document_fragment(id) {
                stack.extend(dom.children_rev(id).map(|c| (c, shadow)));
            }
        }
        for sr in shadows {
            apply_slot_assignment(dom, data, sr);
        }
    }

    // ----- change hooks ------------------------------------------------------------------------------------

    /// Records the old state of an element before a change, for stylo's invalidator. `None` when the
    /// element has no styles yet (it will be styled from scratch).
    fn snapshot(&mut self, data: &StyleData, id: NodeId) -> Option<&mut ServoElementSnapshot> {
        let slot = data.slot(id)?;
        if !slot.has(bits::HAS_DATA) || slot.primary.is_none() {
            return None;
        }
        slot.set(bits::HAS_SNAPSHOT);
        slot.clear(bits::HANDLED_SNAPSHOT);
        Some(self.snapshots.entry(opaque_node(id)).or_default())
    }

    /// The attribute change hook (DOM "attribute change steps"): `old_value` is the value before the
    /// change (`None` when the attribute was added). Updates the attribute caches and the derived state and
    /// records the old values for the invalidator, which turns them into restyle hints from the selectors
    /// that depend on the attribute.
    pub fn attribute_changed(
        &mut self,
        dom: &Dom,
        data: &mut StyleData,
        element: NodeId,
        namespace: &Namespace,
        local: &LocalName,
        old_value: Option<&str>,
    ) {
        if !dom.is_alive(element) || !dom.is_element(element) || data.slot(element).is_none() {
            return;
        }
        let null_ns = *namespace == ns!();
        if self.snapshot(data, element).is_some() {
            let entry = self.pending_attrs.entry(element).or_default();
            if !entry.iter().any(|(n, l, _)| n == namespace && l == local) {
                entry.push((
                    namespace.clone(),
                    local.clone(),
                    old_value.map(str::to_owned),
                ));
            }
            let snapshot = self
                .snapshots
                .get_mut(&opaque_node(element))
                .expect("snapshot");
            match *local {
                local_name!("class") if null_ns => snapshot.class_changed = true,
                local_name!("id") if null_ns => snapshot.id_changed = true,
                _ => snapshot.other_attributes_changed = true,
            }
            let name = GenericAtomIdent(local.clone());
            if !snapshot.changed_attrs.contains(&name) {
                snapshot.changed_attrs.push(name);
            }
        }
        self.sync_url(dom);
        let old_state = data
            .slot(element)
            .map(|s| s.state())
            .unwrap_or(ElementState::empty());
        if null_ns && self.refresh_attr_cache(dom, data, element, local) {
            self.note_dirty_hints(data, element);
        }
        let state = derived_state(dom, element);
        let slot = data.slot_mut(element).expect("prepared");
        slot.derived_state = state | (slot.derived_state & ElementState::HAS_SLOTTED);
        let new_state = data
            .slot(element)
            .map(|s| s.state())
            .unwrap_or(ElementState::empty());
        if new_state != old_state {
            if let Some(snapshot) = self.snapshot(data, element) {
                snapshot.state.get_or_insert(old_state);
            }
        }
        if null_ns && *local == local_name!("style") {
            add_hint(data, element, RestyleHint::RESTYLE_STYLE_ATTRIBUTE);
        }
        // HTML 15.5.5: a `details` element's content slot (`::details-content`, a lazily resolved
        // pseudo-element whose `content-visibility` follows `open`) is rebuilt when `open` changes;
        // the element's own style does not tell layout.
        if null_ns
            && *local == local_name!("open")
            && dom.is_html_element_named(element, &local_name!("details"))
        {
            data.log_restyle(crate::damage::ElementRestyle {
                element,
                change: crate::damage::StyleChange::Unchanged,
                contents: crate::damage::StyleChange::Reconstruct,
                counters: false,
            });
        }
        // CSS Shadow Parts 1: what a host exports decides which outer `::part()` rules match the
        // parts in its shadow tree (and in the trees nested in it).
        if null_ns && &**local == "exportparts" {
            if let Some(root) = dom.shadow_root(element) {
                let kids: Vec<NodeId> = dom.children(root).filter(|&c| dom.is_element(c)).collect();
                for c in kids {
                    if data.slot(c).is_some() {
                        add_hint(data, c, RestyleHint::restyle_subtree());
                        mark_ancestors_dirty(dom, data, c);
                    }
                }
            }
        }
        if null_ns && matches!(*local, local_name!("name") | local_name!("content")) {
            self.note_meta(dom, element);
        }
        // A table's `cellpadding` is a hint on its cells.
        if null_ns
            && *local == local_name!("cellpadding")
            && dom.is_html_element_named(element, &local_name!("table"))
        {
            let cells: Vec<NodeId> = dom
                .descendants(element)
                .filter(|&c| {
                    (dom.is_html_element_named(c, &local_name!("td"))
                        || dom.is_html_element_named(c, &local_name!("th")))
                        && crate::hints::cell_table(dom, c) == Some(element)
                })
                .collect();
            for cell in cells {
                if data.slot(cell).is_none() {
                    continue;
                }
                // (an HTML cell: its hints are never [`bits::HINTS_UNSHARED`])
                let (cache, _) = self.attr_cache(dom, &data.lock, cell);
                data.slot_mut(cell).expect("slot").attrs = cache;
                add_hint(data, cell, RestyleHint::RESTYLE_SELF);
                mark_ancestors_dirty(dom, data, cell);
            }
        }
        // Slot assignment depends on `slot` (on a host's children) and `name` (on slots).
        if null_ns && *local == local_name!("slot") {
            if let Some(parent) = dom.parent(element) {
                if let Some(sr) = dom.shadow_root(parent) {
                    self.reassign_slots(dom, data, sr);
                }
            }
        }
        if null_ns
            && *local == local_name!("name")
            && dom.is_html_element_named(element, &local_name!("slot"))
        {
            let root = dom.root(element);
            if dom.is_shadow_root(root) {
                self.reassign_slots(dom, data, root);
            }
        }
        mark_ancestors_dirty(dom, data, element);
    }

    /// Sets (`on`) or clears page-owned element state: `:hover`, `:active`, `:focus`, `:focus-within`,
    /// `:focus-visible`, `:checked`, `:target`, `:modal`, `:popover-open`, validity... Once the page layer
    /// sets a bit it owns it: the DOM-derived value of that bit no longer applies. Returns whether the
    /// element's state changed.
    pub fn set_state(
        &mut self,
        dom: &Dom,
        data: &mut StyleData,
        element: NodeId,
        state: ElementState,
        on: bool,
    ) -> bool {
        let bits = if on { state } else { ElementState::empty() };
        self.set_states(dom, data, element, state, bits)
    }

    /// [`StyleEngine::set_state`] for several bits at once: the bits of `mask` become page-owned,
    /// with the values `bits` has for them. Returns whether the element's state changed.
    pub fn set_states(
        &mut self,
        dom: &Dom,
        data: &mut StyleData,
        element: NodeId,
        mask: ElementState,
        bits: ElementState,
    ) -> bool {
        self.prepare_waiting_element(dom, data, element);
        let Some(slot) = data.slot(element) else {
            return false;
        };
        let old = slot.state();
        let slot = data.slot_mut(element).expect("slot");
        slot.set_mask |= mask;
        slot.set_state = (slot.set_state & !mask) | (bits & mask);
        let new = slot.state();
        if new != old {
            if let Some(snapshot) = self.snapshot(data, element) {
                snapshot.state.get_or_insert(old);
            }
            mark_ancestors_dirty(dom, data, element);
        }
        new != old
    }

    /// Hands `state` bits back to the DOM-derived value (undoes [`StyleEngine::set_state`]).
    pub fn reset_state(
        &mut self,
        dom: &Dom,
        data: &mut StyleData,
        element: NodeId,
        state: ElementState,
    ) {
        self.prepare_waiting_element(dom, data, element);
        let Some(slot) = data.slot(element) else {
            return;
        };
        let old = slot.state();
        let slot = data.slot_mut(element).expect("slot");
        slot.set_mask &= !state;
        slot.set_state &= !state;
        let new = slot.state();
        if new != old {
            if let Some(snapshot) = self.snapshot(data, element) {
                snapshot.state.get_or_insert(old);
            }
            mark_ancestors_dirty(dom, data, element);
        }
    }

    /// Something about `element` other than its attributes changed that its derived state depends on
    /// (custom element upgrade, a child added to a `textarea`).
    pub fn element_changed(&mut self, dom: &Dom, data: &mut StyleData, element: NodeId) {
        let Some(slot) = data.slot(element) else {
            return;
        };
        let old = slot.state();
        let derived = derived_state(dom, element);
        let slot = data.slot_mut(element).expect("slot");
        // (`:has-slotted` follows the slot assignment, not the element)
        slot.derived_state = derived | (slot.derived_state & ElementState::HAS_SLOTTED);
        let new = slot.state();
        if new != old {
            if let Some(snapshot) = self.snapshot(data, element) {
                snapshot.state.get_or_insert(old);
            }
            mark_ancestors_dirty(dom, data, element);
        }
    }

    /// The hook for "attach a shadow root" (`Element.attachShadow`; the parser's declarative shadow roots
    /// are covered by the insertion of the host): the host's children now belong to the shadow tree's
    /// slots, and the host restyles over its new flat-tree children.
    pub fn shadow_root_attached(&mut self, dom: &Dom, data: &mut StyleData, host: NodeId) {
        let Some(shadow) = dom.shadow_root(host) else {
            return;
        };
        if data.slot(host).is_none() {
            return;
        }
        // The light children leave the flat tree; the reassignment puts the slotted ones back.
        for child in dom.children(host) {
            drop_styles(dom, data, child);
        }
        self.reassign_slots(dom, data, shadow);
        // (the host's flat-tree children are the shadow tree's now, whatever the assignment)
        add_hint(data, host, RestyleHint::restyle_subtree());
        mark_ancestors_dirty(dom, data, host);
    }

    /// `slot.assign(...nodes)` (HTML §4.12.4): the slot's manually assigned nodes (a node assigned
    /// to another slot before moves here); the flat tree under its shadow root is recomputed.
    pub fn set_manual_slot_assignment(
        &mut self,
        dom: &Dom,
        data: &mut StyleData,
        slot: NodeId,
        nodes: Vec<NodeId>,
    ) {
        for other in data.manual_slots.values_mut() {
            other.retain(|n| !nodes.contains(n));
        }
        data.manual_slots.insert(slot, nodes);
        let shadow = dom.root(slot);
        if dom.is_shadow_root(shadow) && dom.host(shadow).is_some_and(|h| data.slot(h).is_some()) {
            self.reassign_slots(dom, data, shadow);
        }
    }

    fn reassign_slots(&mut self, dom: &Dom, data: &mut StyleData, shadow: NodeId) {
        let Some(host) = dom.host(shadow) else {
            return;
        };
        // When a node's slot or a slot's assigned nodes changed, the flat tree changed under the
        // host: restyle it all (children inherit from new parents, a slot's fallback content enters
        // or leaves it). A change that leaves the assignment as it was (a node inserted in or
        // removed from the shadow tree away from its slots, a host child no slot takes) leaves the
        // flat tree around the slots as it was, and the host is not restyled for it.
        if apply_slot_assignment(dom, data, shadow) {
            add_hint(data, host, RestyleHint::restyle_subtree());
            mark_ancestors_dirty(dom, data, host);
        }
    }

    /// The insertion hook (DOM "insertion steps"), for `node` (and its subtree) just inserted. Prepares the
    /// new elements, recomputes slot assignment, and restyles the siblings that sibling-sensitive selectors
    /// (`:nth-child`, `:first-child`, `+`, `~`, `:empty`, `:has()`) make depend on the change.
    pub fn node_inserted(&mut self, dom: &Dom, data: &mut StyleData, node: NodeId) {
        self.prepare_inserted(dom, data, node);
        self.inserted_prepared(dom, data, node);
    }

    /// The preparation part of [`StyleEngine::node_inserted`]: the inserted element's slot (and its
    /// descendants', now or at the next restyle).
    fn prepare_inserted(&mut self, dom: &Dom, data: &mut StyleData, node: NodeId) {
        if !dom.is_alive(node) || !dom.is_connected(node) {
            return;
        }
        if dom.node_document(node) != self.document {
            return;
        }
        data.reserve(dom.arena_len());
        self.sync_url(dom);
        if dom.is_element(node) {
            // A new element (never prepared: no slot) that is not a host's child (whose slot holds
            // its assigned slot) waits whole, itself included: a subtree inserted and removed
            // again before the next restyle (a list rebuilt on every change) costs nothing here:
            // the insertion only marks it for the next style update.
            let whole = data.slot(node).is_none()
                && dom
                    .parent(node)
                    .is_some_and(|p| dom.shadow_root(p).is_none());
            if (whole || dom.first_child(node).is_some())
                && self.can_defer_descendants(dom, data, node)
            {
                // Its descendants (and the element itself when `whole`) at the next restyle, if it
                // is still in the document then (`prepare_deferred`). Until then nothing reads
                // their slots: invalidation skips elements without one, and they are styled from
                // scratch.
                if !whole {
                    let shadow = containing_shadow(dom, node);
                    self.prepare_element(dom, data, node, shadow);
                    if let Some(slot) = data.slot(node) {
                        slot.set(bits::UNPREPARED_DESCENDANTS);
                    }
                }
                self.deferred.push(node);
                if self.deferred.len() >= self.deferred_prune_at {
                    // (no restyle for a while: drop the entries removed since)
                    let deferred = std::mem::take(&mut self.deferred);
                    self.deferred = deferred
                        .into_iter()
                        .filter(|&r| self.is_deferred(dom, data, r))
                        .collect();
                    self.deferred_prune_at = (self.deferred.len() * 2).max(DEFERRED_PRUNE_MIN);
                }
            } else {
                self.prepare_subtree(dom, data, node);
            }
        }
    }

    /// The rest of [`StyleEngine::node_inserted`], for an inserted node already prepared: slot
    /// assignment and the restyles the insertion causes around it.
    fn inserted_prepared(&mut self, dom: &Dom, data: &mut StyleData, node: NodeId) {
        if !dom.is_alive(node) || !dom.is_connected(node) {
            return;
        }
        if dom.node_document(node) != self.document || dom.is_document_fragment(node) {
            return;
        }
        let Some(parent) = dom.parent(node) else {
            return;
        };
        if let Some(sr) = dom.shadow_root(parent) {
            self.reassign_slots(dom, data, sr);
        }
        if dom.is_html_element_named(node, &local_name!("slot")) || dom.is_element(node) {
            let root = dom.root(node);
            if dom.is_shadow_root(root)
                && dom.may_hold_slots(node)
                && dom
                    .descendants(node)
                    .chain(std::iter::once(node))
                    .any(|n| dom.is_html_element_named(n, &local_name!("slot")))
            {
                self.reassign_slots(dom, data, root);
            }
        }
        if dom.is_element(node) {
            mark_ancestors_dirty(dom, data, node);
            self.invalidate_relative_selectors(dom, data, node, DomMutationOperation::Insert);
            if dom.is_element(parent) {
                let (prev, next) = (
                    dom.previous_element_sibling(node),
                    dom.next_element_sibling(node),
                );
                self.invalidate_has_for_sibling_indices(dom, data, parent, prev, next);
                if let Some(next) = next {
                    self.invalidate_has_for_new_previous_sibling(dom, data, next, prev);
                }
            }
        }
        self.invalidate_has_for_emptiness(dom, data, parent);
        self.restyle_for_child_change(dom, data, parent, Some(node));
    }

    /// Whether the preparation of a just inserted element's descendants can wait for the next
    /// restyle, as Blink leaves all of an inserted subtree's style work to the style update
    /// (`Node::InsertedInto` only marks it): not when `:has()` invalidation for the insertion
    /// matches against the new subtree now. Shadow trees do not stop it: the slot assignment of a
    /// shadow tree inside the subtree is computed when the subtree is prepared, and that of the
    /// shadow tree around it again then (`prepare_deferred`); a host outside the subtree has only
    /// the inserted element itself among its children, and that is prepared now.
    fn can_defer_descendants(&self, dom: &Dom, data: &StyleData, node: NodeId) -> bool {
        self.relative_selector_flags(dom, data, node).is_empty()
    }

    /// The removal hook (DOM "removing steps"), for `node` just removed from `old_parent`. Drops the styles
    /// of the removed subtree and restyles the old siblings that depend on the change.
    ///
    /// `reinserted`: the same batch inserted `node` again (a move), and the batch's insertions were
    /// prepared before its removals are handled, so the subtree's slot assignment is the one its new
    /// place gave it: it is kept. Otherwise the subtree leaves the flat tree, and its slots their
    /// assigned nodes (DOM §4.2.2: a slot outside a shadow tree has none).
    fn node_removed(
        &mut self,
        dom: &Dom,
        data: &mut StyleData,
        node: NodeId,
        old_parent: Option<NodeId>,
        old_siblings: (Option<NodeId>, Option<NodeId>),
        reinserted: bool,
    ) {
        if !dom.is_alive(node) {
            return;
        }
        {
            let mut metas = self.metas.lock().unwrap_or_else(|e| e.into_inner());
            if !metas.elements.is_empty() {
                metas.dirty = true;
            }
        }
        // (the selector flags matching left on the removed node: a `:has()` search went through it)
        let removed_flags = data
            .slot(node)
            .map_or(0, |s| s.flags.load(std::sync::atomic::Ordering::Relaxed));
        // (the batch's ancestor marks are of connected elements: the resets below of a subtree
        // out of the document cannot touch them)
        let marked = data
            .marked_ancestors()
            .filter(|&m| !dom.is_connected(node) && dom.is_alive(m) && dom.is_connected(m));
        let mut stack: SmallVec<[NodeId; 16]> = smallvec::smallvec![node];
        while let Some(id) = stack.pop() {
            let mut had_snapshot = false;
            let mut descendants_unprepared = false;
            let mut clear_list = false;
            if let Some(slot) = data.slot_mut(id) {
                had_snapshot = slot.has(bits::HAS_SNAPSHOT);
                // (inserted since the last restyle: its descendants have no slots to clear)
                descendants_unprepared = slot.has(bits::UNPREPARED_DESCENDANTS);
                slot.reset_data();
                if !reinserted {
                    slot.assigned_slot = None;
                    clear_list = slot.slotted_list.is_some();
                }
            } else if dom.is_element(id) {
                // (never prepared: inserted whole since the last restyle, so its descendants
                // have no slots either, or out of the document since it was made)
                descendants_unprepared = true;
            }
            if clear_list {
                data.set_slotted_nodes(id, Vec::new());
            }
            // (the map holds an element only while its slot has the flag)
            if had_snapshot {
                self.snapshots.remove(&opaque_node(id));
            }
            if !self.pending_attrs.is_empty() {
                self.pending_attrs.remove(&id);
            }
            if descendants_unprepared {
                continue;
            }
            stack.extend(dom.children(id));
            if let Some(sr) = dom.shadow_root(id) {
                stack.push(sr);
            }
        }
        if let Some(m) = marked {
            data.set_marked_ancestors(m);
        }
        let Some(parent) = old_parent.filter(|&p| dom.is_alive(p)) else {
            return;
        };
        if let Some(sr) = dom.shadow_root(parent) {
            self.reassign_slots(dom, data, sr);
        }
        let root = dom.root(parent);
        if dom.is_shadow_root(root) {
            self.reassign_slots(dom, data, root);
        }
        if dom.is_element(parent) {
            self.invalidate_has_for_removal(dom, data, parent, removed_flags);
            self.invalidate_has_for_emptiness(dom, data, parent);
            if dom.is_element(node) {
                // (the element siblings on either side of where it was)
                let near = |s: Option<NodeId>, back: bool| {
                    let s = s.filter(|&s| dom.is_alive(s) && dom.parent(s) == Some(parent))?;
                    if dom.is_element(s) {
                        Some(s)
                    } else if back {
                        dom.previous_element_sibling(s)
                    } else {
                        dom.next_element_sibling(s)
                    }
                };
                let (prev, next) = (near(old_siblings.0, true), near(old_siblings.1, false));
                self.invalidate_has_for_sibling_indices(dom, data, parent, prev, next);
            }
        }
        self.restyle_for_child_change(dom, data, parent, None);
    }

    /// Selectors 4 §4.4 `:has()` after a removal from `parent`. The removed subtree is gone, so the
    /// relative selector invalidator (which starts from the mutated element) cannot run; instead the
    /// anchors whose `:has()` search could have reached the removed node are restyled: anchors are
    /// marked by matching (`ANCHORS_RELATIVE_SELECTOR`, `_NON_SUBJECT`), and a search reaches
    /// `parent`'s children from `parent` and its ancestors (descendant arguments) and from earlier
    /// siblings of those (sibling arguments, `:has(~ x)`, `:has(+ x .y)`).
    fn invalidate_has_for_removal(
        &mut self,
        dom: &Dom,
        data: &mut StyleData,
        parent: NodeId,
        removed_flags: u32,
    ) {
        use std::sync::atomic::Ordering::Relaxed;
        let flags_of = |id: NodeId| data.slot(id).map_or(0, |s| s.flags.load(Relaxed));
        let search =
            ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING.bits() as u32;
        let anchor = ElementSelectorFlags::ANCHORS_RELATIVE_SELECTOR.bits() as u32;
        let non_subject = ElementSelectorFlags::ANCHORS_RELATIVE_SELECTOR_NON_SUBJECT.bits() as u32;
        if (flags_of(parent) | removed_flags) & (search | anchor | non_subject) == 0 {
            return;
        }
        // (the siblings that became adjacent: `:has(+ x)` relations through them)
        self.invalidate_relative_selectors(
            dom,
            data,
            parent,
            DomMutationOperation::SideEffectNextSibling,
        );
        let flags_of = |id: NodeId| data.slot(id).map_or(0, |s| s.flags.load(Relaxed));
        let mut anchors: SmallVec<[(NodeId, bool); 8]> = SmallVec::new();
        let consider = |id: NodeId, anchors: &mut SmallVec<[(NodeId, bool); 8]>| {
            let f = flags_of(id);
            if f & non_subject != 0 {
                anchors.push((id, true));
            } else if f & anchor != 0 {
                anchors.push((id, false));
            }
        };
        // (the removed node's former siblings: `:has(+ .x)`, `:has(~ .x)` anchors)
        if removed_flags & search != 0 {
            for c in dom.children(parent) {
                if dom.is_element(c) {
                    consider(c, &mut anchors);
                }
            }
        }
        let mut cur = Some(parent);
        while let Some(a) = cur {
            if !dom.is_element(a) {
                break;
            }
            consider(a, &mut anchors);
            if flags_of(a)
                & ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING.bits() as u32
                != 0
            {
                let mut prev = dom.previous_element_sibling(a);
                while let Some(p) = prev {
                    consider(p, &mut anchors);
                    prev = dom.previous_element_sibling(p);
                }
            }
            cur = dom.parent_element(a);
        }
        for (a, non_subject) in anchors {
            // (a subject anchor restyles itself and below; one that is not the subject, as in
            // `div:has(.x) > #s` or `div:has(.x) ~ #s`, also what follows it)
            let mut next = Some(a);
            while let Some(n) = next {
                if data.slot(n).is_some() {
                    add_hint(data, n, RestyleHint::restyle_subtree());
                    mark_ancestors_dirty(dom, data, n);
                }
                next = if non_subject {
                    dom.next_element_sibling(n)
                } else {
                    None
                };
            }
        }
    }

    /// The children-changed hook for text changes inside `parent` (`:empty`, `:placeholder-shown` of a
    /// `textarea`).
    pub fn children_changed(&mut self, dom: &Dom, data: &mut StyleData, parent: NodeId) {
        if !dom.is_alive(parent) || !dom.is_element(parent) {
            return;
        }
        let Some(slot) = data.slot(parent) else {
            return;
        };
        let flags = ElementSelectorFlags::from_bits_truncate(
            (slot.flags.load(std::sync::atomic::Ordering::Relaxed) & bits::SELECTOR_FLAGS) as usize,
        );
        if flags.contains(ElementSelectorFlags::HAS_EMPTY_SELECTOR) {
            add_hint(data, parent, RestyleHint::restyle_subtree());
            mark_ancestors_dirty(dom, data, parent);
        }
        if dom.is_html_element_named(parent, &local_name!("textarea")) {
            self.element_changed(dom, data, parent);
        }
    }

    /// Applies the DOM's reaction queue: attribute changes, insertions, removals, children changes,
    /// adoption.
    pub fn process_reactions<'r>(
        &mut self,
        dom: &Dom,
        data: &mut StyleData,
        reactions: impl IntoIterator<Item = &'r Reaction> + Clone,
    ) {
        data.begin_batch();
        self.handle_reactions(dom, data, reactions);
        data.end_batch();
        self.batch_done();
    }

    /// The end of a reaction batch the caller handled without [`StyleEngine::process_reactions`]
    /// (none of its changes was in the document).
    #[inline]
    pub fn batch_done(&mut self) {
        // What CSSOM provided was for the attribute change of this batch. An element without styles
        // never read its entry, and an element that goes must not leave one behind.
        if let Ok(map) = self.cssom_style_attributes.get_mut() {
            if !map.is_empty() {
                map.clear();
            }
        }
    }

    fn handle_reactions<'r>(
        &mut self,
        dom: &Dom,
        data: &mut StyleData,
        reactions: impl IntoIterator<Item = &'r Reaction> + Clone,
    ) {
        // The nodes this batch inserted, so each insertion asks whether its parent was inserted too
        // in constant time.
        let mut inserted = std::mem::take(&mut self.batch_inserted);
        for r in reactions.clone() {
            if let Reaction::Inserted { node, .. } = r {
                inserted.insert(*node);
            }
        }
        // The insertions are prepared before any reaction of the batch is handled: the batch is
        // handled after the whole operation, so a removal's invalidation (`:has()`, sibling
        // selectors) can already meet the nodes the same operation inserted (a parent's children
        // replaced at once).
        let inserted_roots = |r: &Reaction| match r {
            Reaction::Inserted {
                node,
                connected: true,
                root: true,
                ..
            } if !dom.parent(*node).is_some_and(|p| inserted.contains(&p)) => Some(*node),
            _ => None,
        };
        for node in reactions.clone().into_iter().filter_map(inserted_roots) {
            self.prepare_inserted(dom, data, node);
        }
        // An element inserted in this batch (itself, or with an ancestor: a reaction filter can
        // leave descendants out) is prepared from scratch by its insertion.
        let new_in_batch = |el: NodeId| {
            !inserted.is_empty() && dom.inclusive_ancestors(el).any(|a| inserted.contains(&a))
        };
        for r in reactions {
            match r {
                // An element inserted in this batch is prepared from scratch by its insertion (its
                // attributes as they are now), and a disconnected one has no styles: their
                // attribute changes need nothing more.
                Reaction::AttributeChanged { element, .. }
                    if !dom.is_connected(*element) || new_in_batch(*element) => {}
                Reaction::AttributeChanged {
                    element,
                    local_name,
                    namespace,
                    old_value,
                    ..
                } => {
                    let ns = namespace.clone().unwrap_or(ns!());
                    self.attribute_changed(
                        dom,
                        data,
                        *element,
                        &ns,
                        local_name,
                        old_value.as_deref(),
                    );
                }
                Reaction::Inserted {
                    node,
                    connected,
                    root,
                    ..
                } => {
                    // Preparing the inserted node covers its descendants (whose reactions, if any,
                    // need nothing), and a node inserted into one inserted earlier in the batch;
                    // preparing again is harmless.
                    let parent_inserted = dom.parent(*node).is_some_and(|p| inserted.contains(&p));
                    if *connected && *root && !parent_inserted {
                        // (prepared above)
                        self.inserted_prepared(dom, data, *node);
                    }
                }
                Reaction::Removed {
                    node,
                    old_parent: Some(parent),
                    old_previous,
                    old_next,
                    ..
                } => {
                    // (moved: removed and inserted again by this batch, and prepared above)
                    let reinserted =
                        dom.is_alive(*node) && dom.is_connected(*node) && new_in_batch(*node);
                    self.node_removed(
                        dom,
                        data,
                        *node,
                        Some(*parent),
                        (*old_previous, *old_next),
                        reinserted,
                    )
                }
                Reaction::ChildrenChanged { parent } => self.children_changed(dom, data, *parent),
                _ => {}
            }
        }
        // (kept, emptied, for the next call; a big batch's set is not)
        if inserted.capacity() > 1024 {
            inserted = FxHashSet::default();
        } else {
            inserted.clear();
        }
        self.batch_inserted = inserted;
    }

    /// `element`'s custom states (`:state()`) changed. Stylo's snapshots do not carry custom
    /// states, so the element, its following siblings and their subtrees (what sibling combinators
    /// and `:nth-child(of)` can reach) restyle in full: the subtree of its parent.
    pub fn custom_states_changed(&mut self, dom: &Dom, data: &mut StyleData, element: NodeId) {
        let target = dom
            .parent(element)
            .filter(|&p| dom.is_element(p))
            .unwrap_or(element);
        self.invalidate_subtree(dom, data, target);
    }

    /// Forces a full restyle of `element`'s subtree (a size query container changed size, a custom
    /// property registration changed).
    pub fn invalidate_subtree(&mut self, dom: &Dom, data: &mut StyleData, element: NodeId) {
        add_hint(data, element, RestyleHint::restyle_subtree());
        mark_ancestors_dirty(dom, data, element);
    }

    /// The children of `parent` changed (an insertion of `changed`, or a removal): restyle the children
    /// whose matching can depend on their position (Selectors 4 §14 structural pseudo-classes, sibling
    /// combinators), per the selector flags matching left on `parent`.
    fn restyle_for_child_change(
        &mut self,
        dom: &Dom,
        data: &mut StyleData,
        parent: NodeId,
        changed: Option<NodeId>,
    ) {
        if !dom.is_element(parent) {
            return;
        }
        let Some(slot) = data.slot(parent) else {
            return;
        };
        let flags = ElementSelectorFlags::from_bits_truncate(
            (slot.flags.load(std::sync::atomic::Ordering::Relaxed) & bits::SELECTOR_FLAGS) as usize,
        );
        if flags.is_empty() {
            return;
        }
        let mut targets: SmallVec<[NodeId; 8]> = SmallVec::new();
        if flags.contains(ElementSelectorFlags::HAS_EMPTY_SELECTOR) {
            add_hint(data, parent, RestyleHint::RESTYLE_SELF);
        }
        let all = flags.intersects(
            ElementSelectorFlags::HAS_SLOW_SELECTOR
                | ElementSelectorFlags::MAY_HAVE_TREE_COUNTING_FUNCTION,
        ) || (changed.is_none()
            && flags.contains(ElementSelectorFlags::HAS_SLOW_SELECTOR_LATER_SIBLINGS));
        if all && dom.shadow_root(parent).is_none() {
            // Every child: the parent's descendants restyle, which is the children's subtrees,
            // marked once rather than child by child for each change (a list emptied or rebuilt
            // one child at a time would otherwise walk the children once per child). Blink marks
            // the parent the same way when positional rules depend on its children. (A host's
            // children are slotted: each is reached through its slot, so they are marked alone.)
            add_hint(data, parent, RestyleHint::RESTYLE_DESCENDANTS);
        } else if all {
            targets.extend(dom.element_children(parent));
        } else {
            if flags.contains(ElementSelectorFlags::HAS_SLOW_SELECTOR_LATER_SIBLINGS) {
                if let Some(c) = changed {
                    let mut next = dom.next_element_sibling(c);
                    while let Some(n) = next {
                        targets.push(n);
                        next = dom.next_element_sibling(n);
                    }
                }
            }
            if flags.contains(ElementSelectorFlags::HAS_EDGE_CHILD_SELECTOR) {
                targets.extend(dom.first_element_child(parent));
                targets.extend(dom.last_element_child(parent));
                if let Some(c) = changed {
                    targets.extend(dom.previous_element_sibling(c));
                    targets.extend(dom.next_element_sibling(c));
                }
            }
        }
        let mut marked = false;
        for t in targets {
            if Some(t) != changed {
                add_hint(data, t, RestyleHint::restyle_subtree());
                // Siblings of one parent share their flat-tree ancestors unless they are slotted.
                if !marked || data.slot(t).is_some_and(|s| s.assigned_slot.is_some()) {
                    mark_ancestors_dirty(dom, data, t);
                    marked = true;
                }
            }
        }
        mark_ancestors_dirty(dom, data, parent);
        if flags.contains(ElementSelectorFlags::HAS_EMPTY_SELECTOR) {
            if let Some(gp) = flat_parent(dom, data, parent) {
                mark_ancestors_dirty(dom, data, gp);
            }
        }
    }

    /// `:has()` invalidation for a DOM mutation at `element` (stylo's relative selector invalidator).
    fn invalidate_relative_selectors(
        &mut self,
        dom: &Dom,
        data: &mut StyleData,
        element: NodeId,
        operation: DomMutationOperation,
    ) {
        let inherited = self.relative_selector_flags(dom, data, element);
        if inherited.is_empty() {
            return;
        }
        // (the invalidator walks subtrees: no element in them may wait for its preparation)
        self.prepare_deferred(dom, data);
        let view = TreeView::new(dom, data, &self.shadows, self.document, false);
        let invalidator = RelativeSelectorInvalidator {
            element: view.element(element),
            quirks_mode: self.stylist.quirks_mode(),
            snapshot_table: None,
            invalidated: relative_invalidated,
            sibling_traversal_map: SiblingTraversalMap::default(),
            _marker: std::marker::PhantomData,
        };
        invalidator.invalidate_relative_selectors_for_dom_mutation(
            true,
            &self.stylist,
            inherited,
            operation,
        );
    }

    /// Selectors 4 §4.4 with §14.3-4: a child inserted into or removed from `parent` moves its later
    /// siblings' indices and can make a new first or last child, which `:nth-*()`,
    /// `:first-child` and `:last-child` inside a `:has()` argument read. The relative selector
    /// invalidator starts from the mutated element, whose own dependencies those are not, so the
    /// siblings whose position changed are invalidated for their tree-structural dependencies:
    /// `prev` and `next` are the element siblings on either side of the change (Gecko's restyle
    /// manager does this for each later sibling, and for the edges).
    fn invalidate_has_for_sibling_indices(
        &mut self,
        dom: &Dom,
        data: &mut StyleData,
        parent: NodeId,
        prev: Option<NodeId>,
        next: Option<NodeId>,
    ) {
        use style::invalidation::element::invalidation_map::TSStateForInvalidation as Ts;
        let Some(slot) = data.slot(parent) else {
            return;
        };
        let flags = ElementSelectorFlags::from_bits_truncate(
            (slot.flags.load(std::sync::atomic::Ordering::Relaxed) & bits::SELECTOR_FLAGS) as usize,
        );
        let nth = flags.intersects(
            ElementSelectorFlags::HAS_SLOW_SELECTOR
                | ElementSelectorFlags::HAS_SLOW_SELECTOR_LATER_SIBLINGS,
        );
        let edge = flags.contains(ElementSelectorFlags::HAS_EDGE_CHILD_SELECTOR);
        if !nth && !edge {
            return;
        }
        let mut targets: SmallVec<[(NodeId, Ts); 8]> = SmallVec::new();
        if nth {
            let mut sibling = next;
            while let Some(s) = sibling {
                targets.push((s, Ts::NTH));
                sibling = dom.next_element_sibling(s);
            }
        }
        if edge {
            // (`:only-child` is an edge on both sides)
            let both = Ts::NTH_EDGE_FIRST | Ts::NTH_EDGE_LAST;
            if let Some(n) = next.filter(|_| prev.is_none()) {
                targets.push((n, both));
            }
            if let Some(p) = prev.filter(|_| next.is_none()) {
                targets.push((p, both));
            }
        }
        self.invalidate_has_tree_structural(dom, data, &targets);
    }

    /// Selectors 4 §4.4 with §14.2: a child added to or removed from `parent` can change its
    /// `:empty`, which a `:has()` argument may test (matching marked it).
    fn invalidate_has_for_emptiness(&mut self, dom: &Dom, data: &mut StyleData, parent: NodeId) {
        use style::invalidation::element::invalidation_map::TSStateForInvalidation as Ts;
        if !dom.is_element(parent) {
            return;
        }
        let empty = data.slot(parent).is_some_and(|s| {
            s.flags.load(std::sync::atomic::Ordering::Relaxed)
                & ElementSelectorFlags::HAS_EMPTY_SELECTOR.bits() as u32
                != 0
        });
        if empty {
            self.invalidate_has_tree_structural(dom, data, &[(parent, Ts::EMPTY)]);
        }
    }

    /// `:has()` invalidation for elements whose tree-structural pseudo-classes (`:empty`, `:nth-*`,
    /// the edges) may have changed, each for the given dependencies.
    fn invalidate_has_tree_structural(
        &mut self,
        dom: &Dom,
        data: &mut StyleData,
        targets: &[(
            NodeId,
            style::invalidation::element::invalidation_map::TSStateForInvalidation,
        )],
    ) {
        if targets
            .iter()
            .all(|&(t, _)| self.relative_selector_flags(dom, data, t).is_empty())
        {
            return;
        }
        self.prepare_deferred(dom, data);
        let view = TreeView::new(dom, data, &self.shadows, self.document, false);
        for &(target, states) in targets {
            if view.data.slot(target).is_none() {
                continue;
            }
            let invalidator = RelativeSelectorInvalidator {
                element: view.element(target),
                quirks_mode: self.stylist.quirks_mode(),
                snapshot_table: None,
                invalidated: relative_invalidated,
                sibling_traversal_map: SiblingTraversalMap::default(),
                _marker: std::marker::PhantomData,
            };
            invalidator.invalidate_relative_selectors_for_this(
                &self.stylist,
                |element, scope, data, quirks_mode, collector| {
                    data.relative_invalidation_map_attributes()
                        .ts_state_to_selector
                        .lookup_with_additional(
                            *element,
                            quirks_mode,
                            None,
                            &[],
                            ElementState::empty(),
                            |dependency| {
                                if dependency.state.intersects(states) {
                                    collector.add_dependency(&dependency.dep, *element, scope);
                                }
                                true
                            },
                        );
                },
            );
        }
    }

    /// Selectors 4 §4.4: an element inserted just before `next` separates it from `old_prev`, an
    /// anchor whose `:has(+ x)` (or `:has(+ x > y)`) it satisfied. The next-sibling side-effect
    /// invalidation runs from `next` with the sibling it had before the insertion as its previous
    /// one, the case stylo's removal path names (`.anchor + .removed + .a`), the other way round.
    fn invalidate_has_for_new_previous_sibling(
        &mut self,
        dom: &Dom,
        data: &mut StyleData,
        next: NodeId,
        old_prev: Option<NodeId>,
    ) {
        let flags = data.slot(next).map_or(0, |s| {
            s.flags.load(std::sync::atomic::Ordering::Relaxed) & bits::SELECTOR_FLAGS
        });
        let search = ElementSelectorFlags::from_bits_truncate(flags as usize)
            & ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING;
        if search.is_empty() || old_prev.is_none() {
            return;
        }
        self.prepare_deferred(dom, data);
        let view = TreeView::new(dom, data, &self.shadows, self.document, false);
        let element = view.element(next);
        let map = SiblingTraversalMap::new(
            element,
            old_prev.map(|p| view.element(p)),
            dom.next_element_sibling(next).map(|n| view.element(n)),
        );
        let invalidator = RelativeSelectorInvalidator {
            element,
            quirks_mode: self.stylist.quirks_mode(),
            snapshot_table: None,
            invalidated: relative_invalidated,
            sibling_traversal_map: map,
            _marker: std::marker::PhantomData,
        };
        invalidator.invalidate_relative_selectors_for_dom_mutation(
            false,
            &self.stylist,
            ElementSelectorFlags::empty(),
            DomMutationOperation::SideEffectNextSibling,
        );
    }

    /// The `:has()` search directions a mutation at `element` must invalidate for, from the
    /// selector flags matching left on its parent and previous sibling (empty: none).
    fn relative_selector_flags(
        &self,
        dom: &Dom,
        data: &StyleData,
        element: NodeId,
    ) -> ElementSelectorFlags {
        let parent = dom.parent_element(element);
        let mut inherited = ElementSelectorFlags::empty();
        let flags_of = |id: NodeId| {
            data.slot(id)
                .map(|s| {
                    ElementSelectorFlags::from_bits_truncate(
                        (s.flags.load(std::sync::atomic::Ordering::Relaxed) & bits::SELECTOR_FLAGS)
                            as usize,
                    )
                })
                .unwrap_or(ElementSelectorFlags::empty())
        };
        if let Some(p) = parent {
            inherited |= flags_of(p)
                & ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING;
            if flags_of(p).contains(ElementSelectorFlags::ANCHORS_RELATIVE_SELECTOR) {
                inherited |= ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR;
            }
        }
        // (both directions from the previous sibling: a sibling search that goes on into the
        // siblings' subtrees, `:has(~ .a .b)`, reaches the new element's descendants through it)
        if let Some(prev) = dom.previous_element_sibling(element) {
            inherited |= flags_of(prev)
                & ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING;
        }
        inherited
    }

    /// `:has()` invalidation for the attribute and state changes recorded since the last restyle (what
    /// Gecko's `Servo_StyleSet_MaybeInvalidateRelativeSelector*Dependency` do at mutation time; here at
    /// restyle time, when the snapshots are complete). The relative selector invalidation map's
    /// dependencies on what changed decide which anchors are restyled.
    fn invalidate_relative_selectors_for_snapshots<'v>(&self, view: &'v TreeView<'v>) {
        use style::invalidation::element::element_wrapper::ElementSnapshot;
        for (node, snapshot) in self.snapshots.iter() {
            let Some(id) = node_from_opaque(*node) else {
                continue;
            };
            if !view.dom.is_alive(id) || view.data.slot(id).is_none() {
                continue;
            }
            let element = view.element(id);
            // (an element off every `:has()` search path still matters when an argument holds a
            // complex selector reaching above the anchor, `.a:has(#d:is(.x .b))` with `.x` on an
            // ancestor of `.a`: the invalidation map's dependencies decide, not the search path)
            let changed_states = snapshot
                .state()
                .map(|old| old ^ element.state())
                .unwrap_or(ElementState::empty());
            let mut classes: SmallVec<[Atom; 4]> = SmallVec::new();
            if snapshot.class_changed {
                let case = self
                    .stylist
                    .quirks_mode()
                    .classes_and_ids_case_sensitivity();
                snapshot.each_class(|c| {
                    if !selectors::Element::has_class(&element, c, case) {
                        classes.push(c.0.clone());
                    }
                });
                element.each_class(|c| {
                    if !snapshot.has_class(c, case) {
                        classes.push(c.0.clone());
                    }
                });
            }
            let mut ids: SmallVec<[Atom; 2]> = SmallVec::new();
            if snapshot.id_changed {
                ids.extend(snapshot.id_attr().cloned());
                ids.extend(TElement::id(&element).cloned());
            }
            let attrs = &snapshot.changed_attrs;
            if changed_states.is_empty() && classes.is_empty() && ids.is_empty() && attrs.is_empty()
            {
                continue;
            }
            let invalidator = RelativeSelectorInvalidator {
                element,
                quirks_mode: self.stylist.quirks_mode(),
                snapshot_table: Some(&self.snapshots),
                invalidated: relative_invalidated,
                sibling_traversal_map: SiblingTraversalMap::default(),
                _marker: std::marker::PhantomData,
            };
            invalidator.invalidate_relative_selectors_for_this(
                &self.stylist,
                |element, scope, data, quirks_mode, collector| {
                    let map = data.relative_selector_invalidation_map();
                    for class in &classes {
                        if let Some(deps) = map.class_to_selector.get(class, quirks_mode) {
                            for dep in deps {
                                collector.add_dependency(dep, *element, scope);
                            }
                        }
                    }
                    for id in &ids {
                        if let Some(deps) = map.id_to_selector.get(id, quirks_mode) {
                            for dep in deps {
                                collector.add_dependency(dep, *element, scope);
                            }
                        }
                    }
                    for attr in attrs {
                        if let Some(deps) = map.other_attribute_affecting_selectors.get(attr) {
                            for dep in deps {
                                collector.add_dependency(dep, *element, scope);
                            }
                        }
                    }
                    if !changed_states.is_empty() {
                        map.state_affecting_selectors.lookup_with_additional(
                            *element,
                            quirks_mode,
                            None,
                            &[],
                            changed_states,
                            |dep| {
                                if dep.state.intersects(changed_states) {
                                    collector.add_dependency(&dep.dep, *element, scope);
                                }
                                true
                            },
                        );
                    }
                },
            );
        }
    }

    // ----- restyle -----------------------------------------------------------------------------------------

    fn build_snapshot_attrs(&mut self, dom: &Dom) {
        for (element, originals) in self.pending_attrs.drain() {
            if !dom.is_alive(element) {
                continue;
            }
            let Some(snapshot) = self.snapshots.get_mut(&opaque_node(element)) else {
                continue;
            };
            let mut attrs: Vec<(AttrIdentifier, AttrValue)> = dom
                .attrs(element)
                .iter()
                .map(|a| {
                    (
                        ident(a.namespace(), a.local_name(), a.prefix()),
                        value(a.namespace(), a.local_name(), a.value()),
                    )
                })
                .collect();
            for (ns, local, old) in originals {
                let pos = attrs
                    .iter()
                    .position(|(i, _)| i.local_name.0 == local && i.namespace.0 == ns);
                match (pos, old) {
                    (Some(i), Some(v)) => attrs[i].1 = value(&ns, &local, &v),
                    (Some(i), None) => {
                        attrs.remove(i);
                    }
                    (None, Some(v)) => {
                        attrs.push((ident(&ns, &local, None), value(&ns, &local, &v)))
                    }
                    (None, None) => {}
                }
            }
            snapshot.attrs = Some(attrs);
        }
        // State-only snapshots still need an attribute list: stylo's stylesheet invalidation reads
        // every snapshot's classes and id (`invalidation_kind_for`) without checking `has_attrs`, and
        // servo's snapshot unwraps the list. Snapshots of nodes that died are dropped.
        self.snapshots
            .retain(|node, _| node_from_opaque(*node).is_some_and(|id| dom.is_alive(id)));
        for (node, snapshot) in self.snapshots.iter_mut() {
            if snapshot.attrs.is_none() {
                if let Some(id) = node_from_opaque(*node).filter(|&id| dom.is_alive(id)) {
                    snapshot.attrs = Some(
                        dom.attrs(id)
                            .iter()
                            .map(|a| {
                                (
                                    ident(a.namespace(), a.local_name(), a.prefix()),
                                    value(a.namespace(), a.local_name(), a.value()),
                                )
                            })
                            .collect(),
                    );
                }
            }
        }
    }

    /// Brings every element of the document up to date: flushes stylesheet changes, turns the recorded
    /// snapshots into restyle hints, and runs stylo's traversal over the dirty part of the flat tree.
    ///
    /// This is a style change event (CSS Transitions 1 §3) at the timeline's current time as of the
    /// last [`StyleEngine::restyle_at`] (0 before the first): what script-driven style reads
    /// (`getComputedStyle`) need. It does not move the timeline.
    pub fn restyle(&mut self, dom: &Dom, data: &mut StyleData) -> RestyleStats {
        let stats = self.restyle_inner(dom, data);
        self.moved.clear();
        stats
    }

    /// DOM's move (`moveBefore`) moved `node`: it stayed connected, so its subtree's CSS animations
    /// and transitions go on, though their elements have no styles until the next restyle.
    pub fn node_moved(&mut self, node: NodeId) {
        self.moved.insert(node);
    }

    /// The document timeline moves to `now` (seconds since the document's time origin; it never moves
    /// back, an earlier `now` leaves it where it is), then [`StyleEngine::restyle`]: Web Animations 1
    /// §4.4 "update animations" moves the transitions and animations to their states at that time
    /// (pending to running, the next iteration, finished), the elements whose animated values change
    /// are restyled with their values at that time, and transitions started by the restyle start then.
    /// HTML's "update the rendering" calls this once per frame; [`StyleEngine::animations_active`]
    /// then says whether another frame is wanted.
    pub fn restyle_at(&mut self, dom: &Dom, data: &mut StyleData, now: f64) -> RestyleStats {
        if now > self.timeline_time {
            self.timeline_time = now;
        }
        // (before what finished is dropped: its end is an event)
        if !self.animations.sets.read().is_empty() || self.css_events_due {
            let now = self.timeline_time;
            self.scan_css_events(dom, data, now);
            // (what this scan found waits for the next rendering update, which takes it:
            // `css_animation_events`)
            self.hold_css_events();
        }
        self.update_animations(dom, data);
        let stats = self.restyle_inner(dom, data);
        self.moved.clear();
        stats
    }

    /// Web Animations: the keyframe effects in effect with an element target, in composite order, as
    /// the page sampled them at the timeline's current time. The targets whose samples changed restyle
    /// at the next restyle. Returns the previous samples (a buffer for the next call).
    pub fn set_script_effects(
        &mut self,
        dom: &Dom,
        data: &mut StyleData,
        samples: Vec<crate::web_animations::EffectSample>,
    ) -> Vec<crate::web_animations::EffectSample> {
        let (old, changed) = match self.script_animations.get_mut() {
            Ok(store) => store.replace(samples),
            Err(_) => return Vec::new(),
        };
        for id in changed {
            if !dom.is_alive(id) || !dom.is_connected(id) || data.slot(id).is_none() {
                continue;
            }
            add_hint(data, id, RestyleHint::RESTYLE_SELF);
            mark_ancestors_dirty(dom, data, id);
        }
        old
    }

    /// CSS Animations 2 §4.1, CSS Transitions 2 §5.2: the events of the CSS animations and
    /// transitions whose phase changed since the last call, at timeline time `now` (s): the page's
    /// "update animations and send events" queues them.
    pub fn css_animation_events(
        &mut self,
        dom: &Dom,
        data: &StyleData,
        now: f64,
    ) -> Vec<crate::web_animations::CssAnimationEvent> {
        self.css_events_taken = true;
        self.scan_css_events(dom, data, now);
        std::mem::take(&mut self.css_events_out)
    }

    /// Compares the CSS animations' and transitions' phases at `now` (s) with the last scan's and
    /// keeps the events. Runs at each rendering update before stylo drops what finished.
    fn scan_css_events(&mut self, dom: &Dom, data: &StyleData, now: f64) {
        let sets = self.animations.sets.read();
        let out = &mut self.css_events_out;
        for (key, set) in sets.iter() {
            let Some(id) = node_from_opaque(key.node) else {
                continue;
            };
            if !dom.is_alive(id) {
                continue;
            }
            // (an element that left the document, or does not display, has its animations
            // canceled: as of now)
            let canceled = !dom.is_connected(id) || !displayed(dom, data, &self.moved, id);
            for (k, timing, start, local, paused) in crate::web_animations::css::entries(
                id,
                &key.pseudo_element,
                &set.animations,
                &set.transitions,
                now,
            ) {
                if canceled {
                    self.css_events.note_active(&k, &timing, local);
                } else {
                    self.css_events.see(k, &timing, start, local, paused, out);
                }
            }
        }
        drop(sets);
        self.css_events.finish(now, &mut self.css_events_out);
        self.css_events_due = false;
    }

    /// The CSS animations and transitions running now (timeline time `now`, s), with what their
    /// `CSSAnimation`/`CSSTransition` objects show.
    pub fn css_timings(&self, dom: &Dom, now: f64) -> Vec<crate::web_animations::CssTiming> {
        let mut out = Vec::new();
        for (key, set) in self.animations.sets.read().iter() {
            let Some(id) = node_from_opaque(key.node) else {
                continue;
            };
            if !dom.is_alive(id) {
                continue;
            }
            crate::web_animations::css::timings(
                id,
                &key.pseudo_element,
                &set.animations,
                &set.transitions,
                now,
                &mut out,
            );
        }
        out
    }

    /// CSS Animations 2 §4.3: the keyframes of `element`'s CSS animation `name` as its `CSSAnimation`'s
    /// effect shows them (`getKeyframes()`): each `@keyframes` step with its declared values (the
    /// synthesized 0% and 100% steps with the element's computed values), its
    /// `animation-timing-function` (else the element's for the animation) and its declared
    /// `animation-composition`. Empty when the rule animates nothing.
    pub fn css_animation_keyframes(
        &self,
        dom: &Dom,
        data: &StyleData,
        element: NodeId,
        name: &str,
    ) -> Vec<crate::web_animations::RawKeyframe> {
        use crate::web_animations::CompositeOperation;
        use style::properties::longhands::animation_composition::computed_value::single_value::T as Composition;
        use style::properties::{LonghandId, PropertyDeclarationId};
        use style::stylesheets::keyframes_rule::KeyframesStepValue;
        use style_traits::ToCss;
        let Some(style) = data.styles_arc(element) else {
            return Vec::new();
        };
        let ui = style.get_ui();
        let index = ui
            .animation_name_iter()
            .position(|n| n.as_atom().is_some_and(|a| &**a == name));
        let easing = index.map_or_else(
            || "ease".to_owned(),
            |i| ui.animation_timing_function_mod(i).to_css_string(),
        );
        let composite = |c: Composition| match c {
            Composition::Replace => CompositeOperation::Replace,
            Composition::Add => CompositeOperation::Add,
            Composition::Accumulate => CompositeOperation::Accumulate,
        };
        let atom = Atom::from(name);
        self.guards_and_view(dom, data, |shared, view| {
            let Some(animation) = self.stylist.lookup_keyframes(&atom, view.element(element))
            else {
                return Vec::new();
            };
            if animation.properties_changed.is_empty() {
                return Vec::new();
            }
            let guard = shared.guards.author;
            let mut out = Vec::with_capacity(animation.steps.len());
            for step in &animation.steps {
                let values: Vec<(String, String)> = match &step.value {
                    KeyframesStepValue::Declarations { block } => block
                        .read_with(guard)
                        .normal_declaration_iter()
                        .filter(|d| match d.id() {
                            PropertyDeclarationId::Longhand(l) => {
                                l != LonghandId::Display && l.is_animatable()
                            }
                            PropertyDeclarationId::Custom(_) => true,
                        })
                        .map(|d| {
                            let mut name = String::new();
                            let _ = d.id().to_css(&mut style_traits::CssWriter::new(&mut name));
                            let mut value = String::new();
                            let _ = d.to_css(&mut value);
                            (name, value)
                        })
                        .collect(),
                    KeyframesStepValue::ComputedValues => animation
                        .properties_changed
                        .iter()
                        .map(|p| {
                            let mut name = String::new();
                            let _ = p.to_css(&mut style_traits::CssWriter::new(&mut name));
                            let value = crate::computed::computed_property_by_name(&style, &name)
                                .unwrap_or_default();
                            (name, value)
                        })
                        .collect(),
                };
                out.push(crate::web_animations::RawKeyframe {
                    offset: Some(step.start_offset.percentage.0 as f64),
                    easing: step
                        .get_animation_timing_function(guard)
                        .map_or_else(|| easing.clone(), |t| t.to_css_string()),
                    composite: step.get_animation_composition(guard).map(composite),
                    values,
                });
            }
            out
        })
    }

    /// Script controls a CSS animation or transition through its object: it is put at local time
    /// `local` (ms; `None` cancels it), paused or not, at timeline time `now` (s). Returns whether it
    /// was found. Its element restyles.
    pub fn css_control(
        &mut self,
        dom: &Dom,
        data: &mut StyleData,
        key: &crate::web_animations::CssKey,
        now: f64,
        local: Option<f64>,
        paused: bool,
    ) -> bool {
        let set_key = AnimationSetKey::new(opaque_node(key.element), key.pseudo);
        self.css_controlled.insert(key.element);
        let found = {
            let mut sets = self.animations.sets.write();
            let Some(set) = sets.get_mut(&set_key) else {
                return false;
            };
            let mut found = false;
            if key.transition {
                for t in set.transitions.iter_mut() {
                    if t.start_time.to_bits() != key.index
                        || matches!(t.state, style::animation::AnimationState::Canceled)
                    {
                        continue;
                    }
                    match local {
                        Some(l) => crate::web_animations::css::control_transition(t, now, l),
                        None => {
                            let (timing, start) = crate::web_animations::css::transition_timing(t);
                            self.css_events
                                .note_active(key, &timing, (now - start) * 1000.0);
                            // (as a completed transition with its end value: stylo's next
                            // restyle neither applies it nor starts it again, as it would from a
                            // canceled one whose value is still in the before-change style)
                            t.state = style::animation::AnimationState::Finished;
                        }
                    }
                    found = true;
                }
            } else {
                let mut n = 0;
                for a in set.animations.iter_mut() {
                    if *a.name != *key.name {
                        continue;
                    }
                    let index = n;
                    n += 1;
                    if index != key.index
                        || matches!(a.state, style::animation::AnimationState::Canceled)
                    {
                        continue;
                    }
                    match local {
                        Some(l) => crate::web_animations::css::control_animation(a, now, l, paused),
                        None => {
                            let (timing, start) = crate::web_animations::css::animation_timing(a);
                            let local = crate::web_animations::css::animation_local(a, start, now);
                            self.css_events.note_active(key, &timing, local);
                            a.state = style::animation::AnimationState::Canceled;
                        }
                    }
                    found = true;
                }
            }
            found
        };
        // (a seek, pause or cancel changes its phase: its events come at the next scan)
        self.css_events_due |= found;
        if found && dom.is_alive(key.element) && data.slot(key.element).is_some() {
            add_hint(data, key.element, RestyleHint::RESTYLE_SELF);
            mark_ancestors_dirty(dom, data, key.element);
        }
        found
    }

    /// Whether a CSS animation or transition event may come at a later scan: something started,
    /// was canceled, or has not ended (the caller keeps asking for rendering updates).
    pub fn css_events_pending(&self) -> bool {
        self.css_events_due
            || self
                .css_events
                .unfinished_where(|k| !self.css_key_on_compositor(k))
    }

    /// Whether a scan for CSS animation events is due (something started, was canceled, seeked or
    /// paused since the last): work the event loop has left, unlike a running animation.
    pub fn css_events_due(&self) -> bool {
        self.css_events_due
    }

    /// After a scan that is not the rendering update's own: the events it found wait for the next
    /// rendering update, which [`StyleEngine::css_events_pending`] then asks for.
    /// (Only when someone takes the events: a renderer without a page never does.)
    fn hold_css_events(&mut self) {
        self.css_events_due |= self.css_events_taken && !self.css_events_out.is_empty();
    }

    /// The animations a compositor runs: the property each animation of a frame painted for one
    /// animates, by element (`docs/compositor-animations.md`, S2). Until the next such frame, the
    /// page does not service the animations and transitions they show
    /// ([`StyleEngine::animations_need_page_ticks`]), only their changes
    /// ([`StyleEngine::compositor_next_change`]).
    pub fn set_on_compositor(
        &mut self,
        entries: impl IntoIterator<Item = (NodeId, Option<PseudoElement>, LonghandId)>,
    ) {
        self.on_compositor.clear();
        for (element, pseudo, id) in entries {
            let on = self.on_compositor.entry((element, pseudo)).or_default();
            if !on.contains(&id) {
                on.push(id);
            }
        }
    }

    /// The properties of the element or pseudo-element `key` names a compositor animates (empty
    /// for none).
    fn compositor_properties(&self, key: &AnimationSetKey) -> &[LonghandId] {
        if self.on_compositor.is_empty() {
            return &[];
        }
        node_from_opaque(key.node)
            .and_then(|id| self.on_compositor.get(&(id, key.pseudo_element)))
            .map_or(&[], |on| on.as_slice())
    }

    /// Whether the CSS animation or transition `key` names runs on a compositor (Blink's
    /// `Animation::HasActiveAnimationsOnCompositor`).
    pub fn css_key_on_compositor(&self, key: &crate::web_animations::CssKey) -> bool {
        if self.css_controlled.contains(&key.element) {
            return false;
        }
        let Some(on) = self.on_compositor.get(&(key.element, key.pseudo)) else {
            return false;
        };
        let sets = self.animations.sets.read();
        let Some(set) = sets.get(&AnimationSetKey::new(opaque_node(key.element), key.pseudo))
        else {
            return false;
        };
        if key.transition {
            set.transitions.iter().any(|t| {
                t.created_at.to_bits() == key.index
                    && crate::animations::transition_on_compositor(set, t, on)
            })
        } else {
            set.animations
                .iter()
                .filter(|a| &*a.name == key.name.as_str())
                .nth(key.index as usize)
                .is_some_and(|a| crate::animations::animation_on_compositor(set, a, on))
        }
    }

    /// What changes when a restyle starts, cancels, pauses, resumes or retimes a CSS animation or
    /// transition: each unfinished one's state and timing, summed (dropping finished ones changes
    /// nothing).
    fn animations_signature(&self) -> u64 {
        use style::animation::AnimationState as S;
        let entry = |s: &S, start: f64, duration: f64| -> u64 {
            let state = match s {
                S::Finished => return 0,
                S::Canceled => 1u64,
                S::Pending => 2,
                S::Running => 3,
                S::Paused(p) => 4 ^ p.to_bits().rotate_left(7),
            };
            (state.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ start.to_bits().rotate_left(17))
                .wrapping_add(duration.to_bits().rotate_left(31))
                | 1
        };
        let mut sig = 0u64;
        for set in self.animations.sets.read().values() {
            for a in &set.animations {
                sig = sig.wrapping_add(entry(&a.state, a.started_at, a.duration));
            }
            for t in &set.transitions {
                sig =
                    sig.wrapping_add(entry(&t.state, t.start_time, t.property_animation.duration));
            }
        }
        sig
    }

    /// Whether a transition or animation the page runs itself (not a compositor) is in its delay or
    /// active phase, so the next frame looks different: [`StyleEngine::animations_active`] leaving
    /// out what [`StyleEngine::set_on_compositor`] names.
    pub fn animations_need_page_ticks(&self) -> bool {
        let now = self.timeline_time;
        self.animations.sets.read().iter().any(|(key, set)| {
            crate::animations::needs_page_ticks(set, self.compositor_properties(key), now)
        })
    }

    /// The earliest timeline time (s) at which an animation a compositor runs changes in a way the
    /// page must service (it starts, ends, or, with `iteration_events`, iterates):
    /// `crate::animations::compositor_next_change`.
    pub fn compositor_next_change(&self, iteration_events: bool) -> Option<f64> {
        if self.on_compositor.is_empty() {
            return None;
        }
        self.animations
            .sets
            .read()
            .iter()
            .filter_map(|(key, set)| {
                let on = self.compositor_properties(key);
                if on.is_empty() {
                    return None;
                }
                crate::animations::compositor_next_change(set, on, iteration_events)
            })
            .reduce(f64::min)
    }

    /// The elements with an animation in effect of a property a compositor animates (CSS
    /// animations and transitions, and script animations), sorted by id, and in `pseudos` the
    /// pseudo-elements with such a CSS animation or transition: Blink's
    /// `HasCurrentTransformAnimation` and `HasCurrentOpacityAnimation`, which
    /// `CompositingReasonFinder::CompositingReasonsForAnimation` turns into a layer (a
    /// pseudo-element's own, as it has a layout object of its own).
    pub fn composited_animation_targets(
        &self,
        out: &mut Vec<NodeId>,
        pseudos: &mut Vec<(NodeId, PseudoElement)>,
    ) {
        out.clear();
        pseudos.clear();
        if let Ok(script) = self.script_animations.read() {
            script.targets_animating(&crate::animations::composited_property, out);
        }
        for (key, set) in self.animations.sets.read().iter() {
            let Some(id) = node_from_opaque(key.node) else {
                continue;
            };
            if crate::animations::has_composited_animation(set) {
                match &key.pseudo_element {
                    None => out.push(id),
                    Some(p) => pseudos.push((id, *p)),
                }
            }
        }
        out.sort_unstable_by_key(|n| n.to_bits());
        out.dedup();
    }

    /// What the animations of `element` that are current or in effect animate, as
    /// [`ANIMATES_OPACITY`](crate::ANIMATES_OPACITY) and
    /// [`ANIMATES_TRANSFORM`](crate::ANIMATES_TRANSFORM) bits, as of the last restyle: Web
    /// Animations 1 §5.7 ("side effects of animation": the element acts as if `will-change` named
    /// those properties), as Blink's `ComputedStyle::HasCurrentOpacityAnimation` and
    /// `HasCurrentTransformRelatedAnimation` make it a stacking context
    /// (`CalculateIsStackingContextWithoutContainment`, `HasNonInitialOpacity`), and, for a
    /// transform, a containing block (`HasTransform`).
    pub fn animation_flags(&self, element: NodeId) -> u8 {
        self.animation_flags
            .get(&(element, None))
            .copied()
            .unwrap_or(0)
    }

    /// [`StyleEngine::animation_flags`] of a pseudo-element of `element` (CSS animations and
    /// transitions; script animations of pseudo-elements are left out).
    pub fn pseudo_animation_flags(&self, element: NodeId, pseudo: &PseudoElement) -> u8 {
        if self.animation_flags.is_empty() {
            return 0;
        }
        self.animation_flags
            .get(&(element, Some(*pseudo)))
            .copied()
            .unwrap_or(0)
    }

    /// Recomputes [`StyleEngine::animation_flags`] after a restyle and logs, for each element
    /// whose flags changed, the change layout must make: a repaint for `opacity` (a stacking
    /// context, as an opacity below 1 makes one), a relayout for a transform (also a containing
    /// block for positioned descendants).
    fn update_animation_flags(&mut self, data: &mut StyleData) {
        let mut flags: FxHashMap<(NodeId, Option<PseudoElement>), u8> = FxHashMap::default();
        if let Ok(script) = self.script_animations.read() {
            script.each_animated(&mut |target, id| {
                let f = crate::animations::animation_flag(id);
                if f != 0 {
                    *flags.entry((target, None)).or_default() |= f;
                }
            });
        }
        for (key, set) in self.animations.sets.read().iter() {
            let f = crate::animations::current_flags(set);
            if let (Some(id), true) = (node_from_opaque(key.node), f != 0) {
                *flags.entry((id, key.pseudo_element)).or_default() |= f;
            }
        }
        if flags.is_empty() && self.animation_flags.is_empty() {
            return;
        }
        // (a pseudo-element's change is its element's restyle: its boxes are the element's)
        let changed = |(id, _): (NodeId, Option<PseudoElement>), old: u8, new: u8| {
            (old != new).then_some(crate::damage::ElementRestyle {
                element: id,
                change: if (old ^ new) & crate::animations::ANIMATES_TRANSFORM != 0 {
                    crate::damage::StyleChange::Relayout
                } else {
                    crate::damage::StyleChange::Repaint
                },
                contents: crate::damage::StyleChange::Unchanged,
                counters: false,
            })
        };
        let mut records = Vec::new();
        for (key, &new) in &flags {
            records.extend(changed(
                *key,
                self.animation_flags.get(key).copied().unwrap_or(0),
                new,
            ));
        }
        for (key, &old) in &self.animation_flags {
            if !flags.contains_key(key) {
                records.extend(changed(*key, old, 0));
            }
        }
        for r in records {
            if data.slot(r.element).is_some() {
                data.log_restyle(r);
            }
        }
        self.animation_flags = flags;
    }

    /// How many effects on `element` (CSS animations and transitions that are current or in
    /// effect, script animations' effects current or in effect) animate `id`.
    fn effects_on(
        &self,
        element: NodeId,
        set: Option<&style::animation::ElementAnimationSet>,
        script: Option<&crate::web_animations::ScriptAnimations>,
        id: LonghandId,
    ) -> usize {
        set.map_or(0, |s| crate::animations::effects_on(s, id))
            + script.map_or(0, |s| {
                s.count_animating(
                    element,
                    style::properties::PropertyDeclarationId::Longhand(id),
                )
            })
    }

    /// Whether the effect on `element` that animates `id` (`transform` or `opacity`) is the only
    /// effect there that could change what `id` shows: one effect animates it, and, for
    /// `transform`, none animates `translate`, `rotate` or `scale` (Blink's
    /// `HasIncompatibleAnimations`, `compositor_animations.cc`, over all the element's animations,
    /// CSS and script alike).
    fn alone_on(
        &self,
        element: NodeId,
        set: Option<&style::animation::ElementAnimationSet>,
        script: Option<&crate::web_animations::ScriptAnimations>,
        id: LonghandId,
    ) -> bool {
        self.effects_on(element, set, script, id) == 1
            && (id != LonghandId::Transform
                || [LonghandId::Translate, LonghandId::Rotate, LonghandId::Scale]
                    .into_iter()
                    .all(|i| self.effects_on(element, set, script, i) == 0))
    }

    /// The animations of `element` that a compositor may run, each with the property it animates
    /// (`docs/compositor-animations.md`, S2; Blink's
    /// `CompositorAnimations::CheckCanStartEffectOnCompositor` and `CheckCanStartElementOnCompositor`):
    /// CSS animations and transitions running or in their delay, animating only `transform` and
    /// `opacity`, with a finite iteration duration above 0, unless script drove them; script
    /// animations' effects as [`ScriptAnimations`](crate::web_animations::ScriptAnimations) chooses
    /// them, every property of the effect passing; each the only effect on its property
    /// ([`StyleEngine::alone_on`]). Animations (not transitions, which sit above them in the
    /// cascade) of a property with an important declaration on the element (`data` its document's
    /// style data, `values` its computed values) are left out whole, as Blink's
    /// `KeyframeEffect::CheckCanStartAnimationOnCompositor` does with `AffectsImportantProperty`
    /// (`keyframe_effect.cc`): the page shows the important value (CSS Cascade 5 §6.1).
    /// With `pseudo`, those of that pseudo-element of `element` (`values` its computed values):
    /// its CSS animations and transitions (script animations of pseudo-elements stay with the
    /// page). The caller checks the values (keyframes it can interpolate).
    pub fn compositor_candidates(
        &self,
        element: NodeId,
        pseudo: Option<&PseudoElement>,
        data: &StyleData,
        values: &ComputedValues,
        f: &mut dyn FnMut(crate::animations::CompositorCandidate<'_>),
    ) {
        let script = self.script_animations.read().ok();
        let script = script.as_deref().filter(|_| pseudo.is_none());
        let key = AnimationSetKey::new(opaque_node(element), pseudo.copied());
        let sets = self.animations.sets.read();
        let set = sets.get(&key);
        let alone = |id: LonghandId| self.alone_on(element, set, script, id);
        let important = |id: LonghandId| important_declaration(data, values, id);
        if let Some(set) = set.filter(|_| !self.css_controlled.contains(&element)) {
            crate::animations::compositor_candidates(set, &alone, &important, f);
        }
        if let Some(script) = script.filter(|s| s.has_target(element)) {
            let animation_ok = |id: LonghandId| alone(id) && !important(id);
            script.compositor_candidates(element, &animation_ok, &mut |c| {
                f(crate::animations::CompositorCandidate::Script(c))
            });
        }
    }

    /// Whether a compositor runs the script animation effect `id`
    /// ([`EffectSample::id`](crate::web_animations::EffectSample)): Blink's
    /// `KeyframeEffect::HasActiveAnimationsOnCompositor`. The page then services it only at its
    /// next change, which it computes from the animation's timing.
    pub fn script_effect_on_compositor(&self, id: u64) -> bool {
        if self.on_compositor.is_empty() {
            return false;
        }
        let Ok(script) = self.script_animations.read() else {
            return false;
        };
        let sets = self.animations.sets.read();
        let on = |n: NodeId, p: LonghandId| {
            self.on_compositor
                .get(&(n, None))
                .is_some_and(|o| o.contains(&p))
        };
        let alone = |n: NodeId, p: LonghandId| {
            let set = sets.get(&AnimationSetKey::new_for_non_pseudo(opaque_node(n)));
            self.alone_on(n, set, Some(&script), p)
        };
        script.effect_on_compositor(id, &on, &alone)
    }

    pub fn has_script_effects(&self) -> bool {
        self.script_animations.read().is_ok_and(|s| !s.is_empty())
    }

    /// The document timeline's current time, in seconds.
    pub fn timeline_time(&self) -> f64 {
        self.timeline_time
    }

    /// Whether a transition or animation is in its delay or active phase at the timeline's current
    /// time, so its values change as the timeline moves on (the caller schedules another frame).
    pub fn animations_active(&self) -> bool {
        let now = self.timeline_time;
        self.animations
            .sets
            .read()
            .values()
            .any(|set| crate::animations::needs_ticks(set, now))
    }

    /// The CSS animations (by name, one entry per element animated) and the number of transitions
    /// that need ticks at the timeline's current time: what [`StyleEngine::animations_active`]
    /// answers for (profiling).
    pub fn ticking_animations(&self) -> (Vec<String>, usize) {
        let now = self.timeline_time;
        let mut names = Vec::new();
        let mut transitions = 0;
        for set in self.animations.sets.read().values() {
            transitions += crate::animations::ticking(set, now, &mut names);
        }
        (names, transitions)
    }

    /// Web Animations 1 §4.4 "update animations" for the CSS transitions and animations of the
    /// document at the timeline's current time, and a restyle hint on each element whose animated
    /// values may have changed. Sets of elements that left the document, or have no style (inside a
    /// `display: none` subtree, where stylo does not see them), are dropped: their animations are
    /// canceled (CSS Animations 1 §3, CSS Transitions 1 §3).
    fn update_animations(&mut self, dom: &Dom, data: &mut StyleData) {
        let now = self.timeline_time;
        let prev = std::mem::replace(&mut self.animations_updated_at, now);
        let mut dirty: SmallVec<[NodeId; 8]> = SmallVec::new();
        {
            let mut sets = self.animations.sets.write();
            if sets.is_empty() {
                return;
            }
            sets.retain(|key, set| {
                let Some(id) = node_from_opaque(key.node) else {
                    return false;
                };
                if !dom.is_alive(id)
                    || !dom.is_connected(id)
                    || !displayed(dom, data, &self.moved, id)
                {
                    self.css_controlled.remove(&id);
                    // (its animations are canceled: their events are due)
                    self.css_events_due = true;
                    return false;
                }
                if crate::animations::update_set(set, prev, now) {
                    dirty.push(id);
                }
                !set.is_empty()
            });
        }
        for id in dirty {
            add_hint(data, id, RestyleHint::RESTYLE_SELF);
            mark_ancestors_dirty(dom, data, id);
        }
    }

    fn restyle_inner(&mut self, dom: &Dom, data: &mut StyleData) -> RestyleStats {
        // (the traversal clears dirty bits)
        data.forget_marked_ancestors();
        let now = self.timeline_time;
        self.sync_quirks(dom);
        self.sync_url(dom);
        data.reserve(dom.arena_len());
        let Some(root) = dom.document_element(self.document) else {
            return RestyleStats::default();
        };
        if !self.prepared || data.slot(root).is_none() {
            self.prepare_subtree(dom, data, self.document);
            self.prepared = true;
            self.deferred.clear();
        }
        self.prepare_deferred(dom, data);
        self.prepare_missing(dom, data);
        self.flush_dirty_hints(dom, data);
        data.reserve(dom.arena_len());
        self.sync_page_color_scheme(dom, data);
        if self.global_style_change_pending() {
            self.matched_properties.clear();
        }
        if std::mem::take(&mut self.reseed_root) {
            if let Some(style) = data.styles_arc(root) {
                seed_root(self.stylist.device(), &style);
            }
        }
        if std::mem::take(&mut self.color_schemes_changed) | std::mem::take(&mut self.restyle_all) {
            add_hint(data, root, RestyleHint::restyle_subtree());
            mark_ancestors_dirty(dom, data, root);
        }
        self.flush_shadows(dom, data);
        self.build_snapshot_attrs(dom);

        thread_state::enter(ThreadState::LAYOUT);
        let lock = data.lock.clone();
        let author_guard = lock.read();
        let ua_lock = SharedRwLock::read_only();
        let ua_guard = ua_lock.read();
        let guards = StylesheetGuards {
            author: &author_guard,
            ua_or_user: &ua_guard,
        };
        let invalidations = self.stylist.flush(&guards);

        let scripted = self.has_script_effects();
        let animating = scripted || !self.animations.sets.read().is_empty();
        let started_or_canceled = self.animations_signature();
        let stats = {
            let mut view = TreeView::new(dom, data, &self.shadows, self.document, animating);
            if scripted {
                view.script = Some(&self.script_animations);
            }
            let root_el = view.element(root);
            invalidations.process_style(root_el, Some(&self.snapshots));
            self.invalidate_relative_selectors_for_snapshots(&view);
            if std::mem::take(&mut self.viewport_changed) {
                style::invalidation::viewport_units::invalidate(root_el);
            }
            let shared = SharedStyleContext {
                stylist: &self.stylist,
                visited_styles_enabled: false,
                options: GLOBAL_STYLE_DATA.options.clone(),
                guards,
                current_time_for_animations: now,
                traversal_flags: TraversalFlags::empty(),
                snapshot_map: &self.snapshots,
                animations: self.animations.clone(),
                registered_speculative_painters: &NoPainters,
                matched_properties: Some(&self.matched_properties),
                matched_properties_sink: Some(&self.matched_properties_sink),
            };
            let token = Recalc::pre_traverse(root_el, &shared);
            let parallel = self.parallelism == Parallelism::Parallel;
            if token.should_traverse() {
                let traversal = Recalc::new(shared);
                data.restyled_len
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                if parallel {
                    ensure_pool();
                    let _serial = PARALLEL_TRAVERSAL.lock().unwrap_or_else(|e| e.into_inner());
                    let pool = STYLE_THREAD_POOL.pool();
                    style::driver::traverse_dom(&traversal, token, pool.as_ref());
                } else {
                    style::driver::traverse_dom(&traversal, token, None);
                }
                traversal.stats()
            } else {
                RestyleStats::default()
            }
        };
        drop(author_guard);
        thread_state::exit(ThreadState::LAYOUT);
        self.matched_properties.take_from(
            self.matched_properties_sink
                .get_mut()
                .unwrap_or_else(|e| e.into_inner()),
        );
        // (animations and transitions the traversal started or canceled have events to come)
        self.css_events_due |= self.animations_signature() != started_or_canceled;

        // Copy the new primary styles to where `StyleData::styles` reads them.
        let n = (data
            .restyled_len
            .swap(0, std::sync::atomic::Ordering::Relaxed) as usize)
            .min(data.restyled.len());
        for i in 0..n {
            let bits = data.restyled[i].load(std::sync::atomic::Ordering::Relaxed);
            let Some(id) = NodeId::from_bits(bits) else {
                continue;
            };
            if let Some(slot) = data.slot_mut(id) {
                let primary = slot.data.borrow().styles.primary.clone();
                slot.primary = primary;
            }
            let change = data.restyled_change[i].load(std::sync::atomic::Ordering::Relaxed);
            data.log_restyle(crate::damage::ElementRestyle::unpack(id, change));
        }
        self.update_animation_flags(data);
        for node in self.snapshots.keys() {
            if let Some(slot) = node_from_opaque(*node).and_then(|id| data.slot(id)) {
                slot.clear(bits::HAS_SNAPSHOT | bits::HANDLED_SNAPSHOT);
            }
        }
        self.snapshots.clear();
        self.stylist.rule_tree().maybe_gc();
        stats
    }

    /// Whether a change waits for the next restyle that may change the style of any element, whatever
    /// its own state: new or changed rules, a new device (media queries, viewport units, fonts), the
    /// color schemes, the quirks mode, shadow roots' sheets.
    fn global_style_change_pending(&self) -> bool {
        self.reseed_root
            || self.restyle_all
            || self.color_schemes_changed
            || self.viewport_changed
            || !self.dirty_shadows.is_empty()
            || self.stylist.stylesheets_have_changed()
    }

    fn guards_and_view<R>(
        &self,
        dom: &Dom,
        data: &StyleData,
        f: impl for<'v> FnOnce(&SharedStyleContext, &'v TreeView<'v>) -> R,
    ) -> R {
        let author_guard = data.lock.read();
        let ua_lock = SharedRwLock::read_only();
        let ua_guard = ua_lock.read();
        let guards = StylesheetGuards {
            author: &author_guard,
            ua_or_user: &ua_guard,
        };
        let view = TreeView::new(dom, data, &self.shadows, self.document, false);
        let shared = SharedStyleContext {
            stylist: &self.stylist,
            visited_styles_enabled: false,
            options: GLOBAL_STYLE_DATA.options.clone(),
            guards,
            current_time_for_animations: self.timeline_time,
            traversal_flags: TraversalFlags::empty(),
            snapshot_map: &self.snapshots,
            animations: self.animations.clone(),
            registered_speculative_painters: &NoPainters,
            // (until the next restyle empties it, kept styles may predate a change)
            matched_properties: (!self.global_style_change_pending())
                .then_some(&self.matched_properties),
            matched_properties_sink: None,
        };
        f(&shared, &view)
    }

    /// The computed style of `element` for `getComputedStyle`: the stored style, or (for an element in a
    /// `display: none` subtree, or not styled yet) one resolved on demand without storing it. `None` for
    /// elements outside this document or not connected.
    pub fn computed_style(
        &self,
        dom: &Dom,
        data: &StyleData,
        element: NodeId,
    ) -> Option<Arc<ComputedValues>> {
        if !dom.is_alive(element) || !dom.is_element(element) || !dom.is_connected(element) {
            return None;
        }
        if dom.node_document(element) != self.document {
            return None;
        }
        if let Some(s) = data.styles_arc(element) {
            return Some(s);
        }
        data.slot(element)?;
        // (in a subtree whose preparation waits for the next restyle: no style before it)
        if self.has_deferred()
            && dom
                .ancestors(element)
                .any(|a| dom.is_element(a) && data.slot(a).is_none())
        {
            return None;
        }
        Some(self.resolve_on_demand(dom, data, element).primary().clone())
    }

    /// Resolves the styles of an element the traversal did not style (in a `display: none` subtree, or
    /// outside the flat tree), from its ancestors' styles, without storing them.
    fn resolve_on_demand(&self, dom: &Dom, data: &StyleData, element: NodeId) -> ElementStyles {
        let entered = !thread_state::get().is_layout();
        if entered {
            thread_state::enter(ThreadState::LAYOUT);
        }
        let styles = self.guards_and_view(dom, data, |shared, view| {
            let mut tlc = ThreadLocalStyleContext::new();
            let mut context = StyleContext {
                shared,
                thread_local: &mut tlc,
            };
            resolve_style(
                &mut context,
                view.element(element),
                RuleInclusion::All,
                None,
                None,
            )
        });
        if entered {
            thread_state::exit(ThreadState::LAYOUT);
        }
        styles
    }

    /// The style of a pseudo-element of `element` (`::before`, `::after`, `::marker`, `::placeholder`,
    /// `::selection`, `::first-letter`, `::backdrop`, `::file-selector-button`...): the eagerly cascaded
    /// ones from the element's data, the others resolved on demand. `None` when no rule creates it (for
    /// `::before`/`::after`, when `content` computes to `none`; for a `::-webkit-scrollbar` part, when
    /// no rule matches it).
    pub fn pseudo_style(
        &self,
        dom: &Dom,
        data: &StyleData,
        element: NodeId,
        pseudo: &PseudoElement,
    ) -> Option<Arc<ComputedValues>> {
        let primary = self.computed_style(dom, data, element)?;
        if pseudo.is_eager() {
            // Eager pseudo-elements are cascaded with their element: stored with it, or resolved with
            // it on demand.
            let style = if data.styles(element).is_some() {
                data.slot(element)?
                    .data
                    .borrow()
                    .styles
                    .pseudos
                    .get(pseudo)
                    .cloned()
            } else {
                self.resolve_on_demand(dom, data, element)
                    .pseudos
                    .get(pseudo)
                    .cloned()
            };
            return style.filter(|s| !pseudo.is_before_or_after() || pseudo.should_exist(s));
        }
        if !pseudo.is_lazy() {
            // Anonymous-box pseudo-elements belong to layout.
            return None;
        }
        let entered = !thread_state::get().is_layout();
        if entered {
            thread_state::enter(ThreadState::LAYOUT);
        }
        let style = self.guards_and_view(dom, data, |shared, view| {
            self.stylist.lazily_compute_pseudo_element_style(
                &shared.guards,
                view.element(element),
                pseudo,
                RuleInclusion::All,
                &primary,
                // (a scroll bar part no rule styles is not drawn: no style for it)
                pseudo.is_before_or_after() || pseudo.is_webkit_scrollbar_part(),
                None,
            )
        });
        if entered {
            thread_state::exit(ThreadState::LAYOUT);
        }
        let style = style?;
        if pseudo.is_before_or_after() && !pseudo.should_exist(&style) {
            return None;
        }
        Some(style)
    }

    /// The style `getComputedStyle(element, pseudo)` reads (CSSOM §6.7.1: a pseudo-element's
    /// computed values whether or not it generates a box): [`StyleEngine::pseudo_style`]'s, and
    /// for a pseudo-element no rule creates (a `::before` with no rule, or whose `content` is
    /// `none`), the style it has with no rules: inherited from the element, initial otherwise.
    /// `None` for an element without a style, or an anonymous-box pseudo-element.
    pub fn cssom_pseudo_style(
        &self,
        dom: &Dom,
        data: &StyleData,
        element: NodeId,
        pseudo: &PseudoElement,
    ) -> Option<Arc<ComputedValues>> {
        if pseudo.is_highlight_pseudo() {
            return self.highlight_style(dom, data, element, pseudo);
        }
        let primary = self.computed_style(dom, data, element)?;
        let style = if pseudo.is_eager() {
            if data.styles(element).is_some() {
                data.slot(element)?
                    .data
                    .borrow()
                    .styles
                    .pseudos
                    .get(pseudo)
                    .cloned()
            } else {
                self.resolve_on_demand(dom, data, element)
                    .pseudos
                    .get(pseudo)
                    .cloned()
            }
        } else if pseudo.is_lazy() {
            let entered = !thread_state::get().is_layout();
            if entered {
                thread_state::enter(ThreadState::LAYOUT);
            }
            let style = self.guards_and_view(dom, data, |shared, view| {
                self.stylist.lazily_compute_pseudo_element_style(
                    &shared.guards,
                    view.element(element),
                    pseudo,
                    RuleInclusion::All,
                    &primary,
                    false,
                    None,
                )
            });
            if entered {
                thread_state::exit(ThreadState::LAYOUT);
            }
            style
        } else {
            return None;
        };
        Some(style.unwrap_or_else(|| {
            style::properties::StyleBuilder::for_inheritance(
                self.stylist.device(),
                Some(&self.stylist),
                Some(&primary),
                Some(pseudo),
            )
            .build()
        }))
    }

    /// The style of a highlight pseudo-element of `element` (`::selection`, `::target-text`,
    /// `::spelling-error`, `::grammar-error`), as `getComputedStyle()` reads it. CSS
    /// Pseudo-Elements 4 §3.5 (highlight inheritance): a property that applies to highlights (§3.2)
    /// and has no value from the cascade takes the value of the same highlight of the originating
    /// element's parent, inherited or not; custom properties inherit from the originating element,
    /// and every other property is the originating element's (they do not apply, so font-relative
    /// units resolve against its font). `color: currentcolor` is the originating element's color.
    /// `None` for an element without a style.
    pub fn highlight_style(
        &self,
        dom: &Dom,
        data: &StyleData,
        element: NodeId,
        pseudo: &PseudoElement,
    ) -> Option<Arc<ComputedValues>> {
        let primary = self.computed_style(dom, data, element)?;
        let parent = crate::handle::flat_parent(dom, data, element)
            .filter(|&p| dom.is_element(p))
            .and_then(|p| self.highlight_style(dom, data, p, pseudo));
        let device = self.stylist.device();
        // What the cascade inherits from: the originating element, with the parent highlight's
        // inherited highlight properties.
        let base = match &parent {
            Some(parent) => {
                let mut b = style::properties::StyleBuilder::for_derived_style(
                    device,
                    Some(&self.stylist),
                    &primary,
                    None,
                );
                b.rules = primary.rules.clone();
                let text = parent.get_inherited_text();
                let t = b.mutate_inherited_text();
                t.copy_color_from(text);
                t.copy_text_shadow_from(text);
                t.copy_text_underline_offset_from(text);
                t.copy_text_underline_position_from(text);
                let svg = parent.get_inherited_svg();
                let s = b.mutate_inherited_svg();
                s.copy_fill_from(svg);
                s.copy_stroke_from(svg);
                s.copy_stroke_width_from(svg);
                b.build()
            }
            None => primary.clone(),
        };
        let entered = !thread_state::get().is_layout();
        if entered {
            thread_state::enter(ThreadState::LAYOUT);
        }
        let own = self.guards_and_view(dom, data, |shared, view| {
            if pseudo.is_eager() {
                // (cascaded with the element: its rules are those of the stored style)
                let inputs = match self.pseudo_style(dom, data, element, pseudo) {
                    Some(s) => style::context::CascadeInputs::new_from_style(&s),
                    None => {
                        let mut i = style::context::CascadeInputs::new_from_style(&primary);
                        i.rules = None;
                        i.visited_rules = None;
                        i
                    }
                };
                Some(self.stylist.compute_pseudo_element_style_with_inputs(
                    inputs,
                    pseudo,
                    &shared.guards,
                    Some(&base),
                    Some(view.element(element)),
                ))
            } else {
                self.stylist.lazily_compute_pseudo_element_style(
                    &shared.guards,
                    view.element(element),
                    pseudo,
                    RuleInclusion::All,
                    &base,
                    false,
                    None,
                )
            }
        });
        if entered {
            thread_state::exit(ThreadState::LAYOUT);
        }
        let own = own.unwrap_or_else(|| {
            style::properties::StyleBuilder::for_inheritance(
                device,
                Some(&self.stylist),
                Some(&base),
                Some(pseudo),
            )
            .build()
        });
        let color = declared_kind(data, &own, LonghandId::Color);
        let reset = [
            LonghandId::BackgroundColor,
            LonghandId::TextDecorationLine,
            LonghandId::TextDecorationStyle,
            LonghandId::TextDecorationColor,
            LonghandId::TextDecorationThickness,
        ]
        .map(|id| parent.is_some() && declared_kind(data, &own, id) == DeclaredColor::No);
        if color != DeclaredColor::CurrentColor && !reset.contains(&true) {
            return Some(own);
        }
        let mut b = style::properties::StyleBuilder::for_derived_style(
            device,
            Some(&self.stylist),
            &own,
            None,
        );
        b.rules = own.rules.clone();
        if color == DeclaredColor::CurrentColor {
            b.mutate_inherited_text()
                .copy_color_from(primary.get_inherited_text());
        }
        if let Some(parent) = &parent {
            if reset[0] {
                b.mutate_background()
                    .copy_background_color_from(parent.get_background());
            }
            let text = parent.get_text();
            let t = b.mutate_text();
            if reset[1] {
                t.copy_text_decoration_line_from(text);
            }
            if reset[2] {
                t.copy_text_decoration_style_from(text);
            }
            if reset[3] {
                t.copy_text_decoration_color_from(text);
            }
            if reset[4] {
                t.copy_text_decoration_thickness_from(text);
            }
        }
        Some(b.build())
    }

    /// The style of a part of `element`'s internal tree (`::-webkit-progress-bar`, the meter's value
    /// part...): the rules for `pseudo` matched against `element`, inheriting from `parent` (the part
    /// it is in, or the element itself), with `inline`, the declarations the element sets on the part,
    /// at the precedence of a style attribute (CSS Cascade 5 §6.1: above normal author rules, below
    /// important ones). `None` for an element without a style.
    pub fn part_style(
        &self,
        dom: &Dom,
        data: &StyleData,
        element: NodeId,
        pseudo: &PseudoElement,
        parent: &ComputedValues,
        inline: Option<&str>,
    ) -> Option<Arc<ComputedValues>> {
        use style::applicable_declarations::ApplicableDeclarationList;
        use style::context::CascadeInputs;
        use style::rule_tree::RuleCascadeFlags;
        let primary = self.computed_style(dom, data, element)?;
        let inline = inline.map(|css| {
            Arc::new(data.lock.wrap(parse_style_attribute(
                css,
                &self.url,
                None,
                self.stylist.quirks_mode(),
                CssRuleType::Style,
            )))
        });
        let entered = !thread_state::get().is_layout();
        if entered {
            thread_state::enter(ThreadState::LAYOUT);
        }
        let style = self.guards_and_view(dom, data, |shared, view| {
            let e = view.element(element);
            let mut declarations = ApplicableDeclarationList::new();
            let mut caches = SelectorCaches::default();
            let mut context = MatchingContext::<SelectorImpl>::new(
                MatchingMode::ForStatelessPseudoElement,
                None,
                &mut caches,
                self.stylist.quirks_mode(),
                NeedsSelectorFlags::Yes,
                MatchingForInvalidation::No,
            );
            context.extra_data.originating_element_style = Some(&primary);
            self.stylist.push_applicable_declarations(
                e,
                Some(pseudo),
                inline.as_ref().map(|b| b.borrow_arc()),
                None,
                Default::default(),
                RuleInclusion::All,
                &mut declarations,
                &mut context,
            );
            let flags = context.extra_data.cascade_input_flags;
            let rules = self
                .stylist
                .rule_tree()
                .compute_rule_node(&mut declarations, &shared.guards);
            let inputs = CascadeInputs {
                rules: Some(rules),
                visited_rules: None,
                flags,
                included_cascade_flags: RuleCascadeFlags::empty(),
            };
            self.stylist.compute_pseudo_element_style_with_inputs(
                inputs,
                pseudo,
                &shared.guards,
                Some(parent),
                Some(e),
            )
        });
        if entered {
            thread_state::exit(ThreadState::LAYOUT);
        }
        Some(style)
    }

    /// Whether an author or user rule, or a shadow tree's sheet, may style `::marker`. Without one, a
    /// list item whose `list-style` gives no marker has none, and layout need not resolve its
    /// `::marker` style (a cascade) to find out.
    pub fn may_style_markers(&self) -> bool {
        !self.shadow_sheets.is_empty()
            || self.stylist.iter_origins().any(|(data, origin)| {
                origin != style::stylesheets::Origin::UserAgent
                    && data
                        .normal_rules(&[PseudoElement::Marker])
                        .is_some_and(|m| !m.is_empty())
            })
    }

    /// Parses a selector list against this document (`querySelector`, the CLI).
    pub fn parse_selector(&self, selector: &str) -> Result<SelectorList<SelectorImpl>, String> {
        SelectorParser::parse_author_origin_no_namespace(selector, &self.url)
            .map_err(|e| format!("{:?}", e.kind))
    }

    /// The elements of `root`'s inclusive descendants (not crossing into shadow trees) that match
    /// `selectors`, in tree order (`querySelectorAll`).
    pub fn query_selector_all(
        &self,
        dom: &Dom,
        data: &StyleData,
        root: NodeId,
        selectors: &SelectorList<SelectorImpl>,
    ) -> Vec<NodeId> {
        let view = TreeView::new(dom, data, &self.shadows, self.document, false);
        let mut caches = SelectorCaches::default();
        let mut context = MatchingContext::new(
            MatchingMode::Normal,
            None,
            &mut caches,
            self.stylist.quirks_mode(),
            NeedsSelectorFlags::No,
            MatchingForInvalidation::No,
        );
        dom.descendants(root)
            .filter(|&n| dom.is_element(n) && data.slot(n).is_some())
            .filter(|&n| {
                selectors::matching::matches_selector_list(
                    selectors,
                    &view.element(n),
                    &mut context,
                )
            })
            .collect()
    }
}

/// `RelativeSelectorInvalidator`'s callback: the invalidated element now has a hint; make the traversal
/// reach it.
fn relative_invalidated(element: StyleElement<'_>, result: &InvalidationResult) {
    let start = if result.has_invalidated_siblings() {
        match element.as_node().traversal_parent() {
            Some(p) => p,
            None => return,
        }
    } else {
        element
    };
    if result.has_invalidated_descendants() || result.has_invalidated_siblings() {
        start.set_dirty_bit();
    }
    let mut current = start.as_node().traversal_parent();
    while let Some(p) = current {
        p.set_dirty_bit();
        current = p.as_node().traversal_parent();
    }
}

fn ident(
    ns: &Namespace,
    local: &LocalName,
    prefix: Option<&snowlark_dom::Prefix>,
) -> AttrIdentifier {
    AttrIdentifier {
        local_name: GenericAtomIdent(local.clone()),
        name: GenericAtomIdent(local.clone()),
        namespace: GenericAtomIdent(ns.clone()),
        prefix: prefix.map(|p| GenericAtomIdent(p.clone())),
    }
}

fn value(ns: &Namespace, local: &LocalName, v: &str) -> AttrValue {
    if *ns == ns!() && *local == local_name!("class") {
        AttrValue::from_serialized_tokenlist(v.to_owned())
    } else if *ns == ns!() && (*local == local_name!("id") || *local == local_name!("part")) {
        if *local == local_name!("part") {
            AttrValue::from_serialized_tokenlist(v.to_owned())
        } else {
            AttrValue::from_atomic(v.to_owned())
        }
    } else {
        AttrValue::String(v.to_owned())
    }
}

/// Adds `hint` to a styled element's restyle hint.
/// The initial size of [`StyleEngine`]'s deferred list before it is pruned.
const DEFERRED_PRUNE_MIN: usize = 1024;

/// The size of [`StyleEngine`]'s `style` text cache at which it is first pruned.
const STYLE_TEXT_PRUNE_MIN: usize = 64;

/// Whether an element has a style that displays (CSS Animations 1 §3: `display: none` on it or an
/// ancestor cancels its animations; elements inside such a subtree have no style).
/// Whether the element `id` is rendered as far as styles tell (not `display: none`). One without
/// styles is not, unless DOM's move (`moveBefore`, in `moved`) moved it or an ancestor since the last
/// restyle and its nearest styled flat-tree ancestor is rendered: the move kept it connected, and
/// its animations with it, and the next restyle styles it.
fn displayed(dom: &Dom, data: &StyleData, moved: &FxHashSet<NodeId>, id: NodeId) -> bool {
    let mut n = id;
    let mut in_moved = false;
    loop {
        if let Some(s) = data.styles(n) {
            return (n == id || in_moved) && !s.get_box().display.is_none();
        }
        in_moved |= moved.contains(&n);
        if moved.is_empty() {
            return false;
        }
        let in_flat_tree = data.slot(n).is_some_and(|s| s.assigned_slot.is_some())
            || dom
                .parent(n)
                .is_none_or(|p| !dom.is_element(p) || dom.shadow_root(p).is_none());
        match crate::handle::flat_parent(dom, data, n) {
            Some(p) if in_flat_tree && dom.is_element(p) => n = p,
            _ => return false,
        }
    }
}

/// The shadow root whose tree `node` is in, if it is in one.
fn containing_shadow(dom: &Dom, node: NodeId) -> Option<NodeId> {
    let root = dom.root(node);
    dom.is_shadow_root(root).then_some(root)
}

fn add_hint(data: &StyleData, id: NodeId, hint: RestyleHint) {
    if let Some(slot) = data.slot(id) {
        if slot.has(bits::HAS_DATA) {
            slot.data.borrow_mut().hint.insert(hint);
        }
    }
}

/// Sets the dirty-descendants bit on every flat-tree ancestor of `id`, so the traversal reaches it.
/// In a reaction batch the walk stops at an element an earlier walk of the batch marked with its
/// ancestors ([`StyleData::marked_ancestors`]): the children of one parent changed at once mark the
/// same ancestors once, not once per child.
fn mark_ancestors_dirty(dom: &Dom, data: &StyleData, id: NodeId) {
    let done = data.marked_ancestors();
    if done == Some(id) {
        return;
    }
    let first = flat_parent(dom, data, id);
    let mut current = first;
    while let Some(p) = current {
        if Some(p) == done {
            break;
        }
        if let Some(slot) = data.slot(p) {
            slot.set(bits::DIRTY_DESCENDANTS);
        }
        current = flat_parent(dom, data, p);
    }
    if let Some(p) = first {
        data.set_marked_ancestors(p);
    }
}

/// Stores the slot assignment of `shadow` in the table. A host child whose slot changed is restyled from
/// scratch (it inherits from its new flat-tree parent, and `::slotted()` rules may now apply); one that
/// left the flat tree loses its styles, as an element outside the flat tree has none.
/// Applies `shadow`'s slot assignment; returns whether it changed (a node's assigned slot, or a
/// slot's assigned nodes).
fn apply_slot_assignment(dom: &Dom, data: &mut StyleData, shadow: NodeId) -> bool {
    let assignment = crate::slots::assign(dom, shadow, &data.manual_slots);
    let mut changed = false;
    for (child, slot) in assignment.children {
        let Some(s) = data.slot_mut(child) else {
            continue;
        };
        if s.assigned_slot == slot {
            continue;
        }
        changed = true;
        s.assigned_slot = slot;
        if slot.is_some() {
            add_hint(data, child, RestyleHint::restyle_subtree());
        } else {
            drop_styles(dom, data, child);
        }
    }
    for (slot, nodes) in assignment.slots {
        if data.slotted_nodes(slot).unwrap_or(&[]) != nodes.as_slice() {
            changed = true;
            data.set_slotted_nodes(slot, nodes);
            update_has_slotted(dom, data, slot);
        }
    }
    changed
}

/// CSS Scoping 1 `:has-slotted`: sets `slot`'s state to whether its flattened assigned nodes are
/// non-empty (an assigned slot of an outer shadow tree counts when it has some itself; fallback
/// content does not count), then that of the slots it is assigned to in turn. A slot whose state
/// changes here is restyled with its host's subtree: its assigned nodes changed, and so did the
/// flat tree under that host.
fn update_has_slotted(dom: &Dom, data: &mut StyleData, slot: NodeId) {
    let mut current = Some(slot);
    while let Some(slot) = current {
        let has = data.slotted_nodes(slot).unwrap_or(&[]).iter().any(|&n| {
            if dom.is_html_element_named(n, &local_name!("slot")) && dom.is_shadow_root(dom.root(n))
            {
                data.slot(n)
                    .is_some_and(|s| s.derived_state.contains(ElementState::HAS_SLOTTED))
            } else {
                true
            }
        });
        let Some(s) = data.slot_mut(slot) else {
            return;
        };
        if s.derived_state.contains(ElementState::HAS_SLOTTED) == has {
            return;
        }
        s.derived_state.set(ElementState::HAS_SLOTTED, has);
        current = s.assigned_slot;
    }
}

/// Drops the styles of `root`'s flat-tree subtree (it left the flat tree or was removed).
fn drop_styles(dom: &Dom, data: &mut StyleData, root: NodeId) {
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if let Some(slot) = data.slot_mut(id) {
            slot.reset_data();
        }
        stack.extend(dom.children(id));
        if let Some(sr) = dom.shadow_root(id) {
            stack.push(sr);
        }
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;

/// Whether the cascaded value of `id` for the element styled `values` comes from an important
/// declaration (of any origin; CSS Cascade 5 §6.1: such a declaration beats animations): Blink's
/// `ComputedStyle::GetBaseImportantSet` (`computed_style.cc`), read from the element's rule node
/// with the guards of `data`'s document and the UA sheets, as stylo's cascade reads them.
/// What the declarations of a highlight pseudo-element's own style give one of its colors
/// ([`highlight_colors_declared`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclaredColor {
    /// Nothing, or `inherit`/`unset`: the value is the parent's highlight style's (CSS
    /// Pseudo-Elements 4 §3.5).
    No,
    /// `currentcolor`.
    CurrentColor,
    /// Any other value: the style's computed one.
    Value,
}

/// What the author and user declarations behind `values`, a highlight pseudo-element's style
/// (`::selection`), give its `color` and its `background-color`. CSS Pseudo-Elements 4 §3.5: a
/// highlight's property with no value from the cascade takes the parent's highlight's; §3.3.1: the
/// user agent's highlight colors apply only where neither of the two has one. Read from the style's
/// rule node, as the cascade reads it (the first declaration found is the one that wins).
pub fn highlight_colors_declared(
    data: &StyleData,
    values: &ComputedValues,
) -> (DeclaredColor, DeclaredColor) {
    (
        declared_kind(data, values, LonghandId::Color),
        declared_kind(data, values, LonghandId::BackgroundColor),
    )
}

/// What the author and user declarations behind `values` give the longhand `id` (the first one
/// found from the rule node up is the one the cascade uses): see [`DeclaredColor`].
fn declared_kind(data: &StyleData, values: &ComputedValues, id: LonghandId) -> DeclaredColor {
    use style::properties::{CSSWideKeyword, PropertyDeclarationId};
    use style::values::specified::Color;
    let Some(rules) = values.rules.as_ref() else {
        return DeclaredColor::No;
    };
    let ua_lock = SharedRwLock::read_only();
    let ua_guard = ua_lock.read();
    let author_guard = data.lock.read();
    let guards = StylesheetGuards {
        author: &author_guard,
        ua_or_user: &ua_guard,
    };
    rules
        .self_and_ancestors()
        .find_map(|node| {
            let level = node.cascade_level();
            if matches!(level.origin(), style::rule_tree::CascadeOrigin::UA) {
                return None;
            }
            let source = node.style_source()?;
            let block = source.read(level.guard(&guards));
            let (declaration, _) = block.get(PropertyDeclarationId::Longhand(id))?;
            Some(match declaration {
                PropertyDeclaration::CSSWideKeyword(w)
                    if matches!(w.keyword, CSSWideKeyword::Inherit | CSSWideKeyword::Unset) =>
                {
                    DeclaredColor::No
                }
                PropertyDeclaration::Color(c) if c.0 == Color::CurrentColor => {
                    DeclaredColor::CurrentColor
                }
                PropertyDeclaration::BackgroundColor(c) if *c == Color::CurrentColor => {
                    DeclaredColor::CurrentColor
                }
                _ => DeclaredColor::Value,
            })
        })
        .unwrap_or(DeclaredColor::No)
}

fn important_declaration(data: &StyleData, values: &ComputedValues, id: LonghandId) -> bool {
    let Some(rules) = values.rules.as_ref() else {
        return false;
    };
    if !rules
        .self_and_ancestors()
        .any(|node| node.cascade_level().is_important())
    {
        return false;
    }
    let ua_lock = SharedRwLock::read_only();
    let ua_guard = ua_lock.read();
    let author_guard = data.lock.read();
    let guards = StylesheetGuards {
        author: &author_guard,
        ua_or_user: &ua_guard,
    };
    rules.self_and_ancestors().any(|node| {
        let level = node.cascade_level();
        level.is_important()
            && node.style_source().is_some_and(|source| {
                source
                    .read(level.guard(&guards))
                    .get(style::properties::PropertyDeclarationId::Longhand(id))
                    .is_some_and(|(_, importance)| importance.important())
            })
    })
}
